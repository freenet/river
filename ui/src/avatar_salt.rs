//! Per-viewer avatar colour salt.
//!
//! Each message author's avatar is coloured per VIEWER: the hue is
//! [`avatar_hue`]`(salt, member_vk)`, a BLAKE3 keyed hash under a salt only
//! this viewer knows. An impersonator therefore cannot pick a key whose avatar
//! colour matches someone else's, because they cannot compute anyone's colours.
//!
//! The salt comes from the chat delegate (`ChatDelegateRequestMsg::GetAvatarSalt`),
//! which derives it from this viewer's member signing key for the room, so the
//! display code never touches the private key. There is deliberately no
//! fallback to `RoomData::self_sk`: a room whose salt is absent (the delegate
//! did not answer, or holds no key for it) renders neutral avatars.
//!
//! The salt follows the key the delegate holds, so `crate::signing` reports
//! every write of a room's signing key ([`signing_key_written`]) and every
//! confirmation that it is current ([`signing_key_confirmed`]).

use crate::components::app::chat_delegate::{generate_request_id, send_delegate_request};
use dioxus::logger::tracing::warn;
use dioxus::prelude::*;
use river_core::chat_delegate::{ChatDelegateRequestMsg, ChatDelegateResponseMsg, RoomKey};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

/// This viewer's avatar colour salt per room (keyed by the room owner's
/// verifying-key bytes), filled by [`ensure_avatar_salt`]. A room with no
/// entry renders neutral avatars.
///
/// Read it with `try_read()`, and in a memo or effect with the
/// `util::signal_guard` anchor and nudge (dioxus-signal-safety.md).
pub static AVATAR_SALTS: GlobalSignal<HashMap<RoomKey, [u8; 32]>> = Global::new(HashMap::new);

/// Fetch attempts per claim before the room is left without a salt.
const MAX_AVATAR_SALT_ATTEMPTS: u8 = 3;

/// Wait before attempt `n + 1` after attempt `n` fails.
const AVATAR_SALT_RETRY_DELAYS_MS: [u64; (MAX_AVATAR_SALT_ATTEMPTS - 1) as usize] = [2_000, 8_000];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FetchState {
    /// A task is fetching, or waiting to retry.
    InFlight,
    /// The salt was fetched and stored.
    Done,
    /// Every attempt failed. Stays absent until the delegate's key for the
    /// room is confirmed or written.
    Exhausted,
}

/// One room's fetch, owned by the task whose `epoch` it carries. Epochs are
/// never reused, so a task whose claim was dropped (the room's signing key
/// changed) can tell, and its result is discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SaltClaim {
    epoch: u64,
    state: FetchState,
}

/// What a task does after an attempt.
#[derive(Debug, PartialEq, Eq)]
enum AfterAttempt {
    /// Store the salt.
    Store,
    /// Try again after this many milliseconds.
    RetryAfterMs(u64),
    /// The last attempt failed; leave the room without a salt.
    GiveUp,
    /// The claim is no longer this task's; do nothing.
    Stale,
}

/// Claim `slot` for a new fetch with `epoch`, unless a fetch is running, done
/// or exhausted.
fn claim_fetch(slot: &mut Option<SaltClaim>, epoch: u64) -> bool {
    if slot.is_some() {
        return false;
    }
    *slot = Some(SaltClaim {
        epoch,
        state: FetchState::InFlight,
    });
    true
}

/// Whether the task with `epoch` still owns a running fetch.
fn owns_fetch(slot: &Option<SaltClaim>, epoch: u64) -> bool {
    *slot
        == Some(SaltClaim {
            epoch,
            state: FetchState::InFlight,
        })
}

/// Record the outcome of attempt number `attempt` (1-based).
fn after_attempt(slot: &mut Option<SaltClaim>, epoch: u64, attempt: u8, ok: bool) -> AfterAttempt {
    if !owns_fetch(slot, epoch) {
        return AfterAttempt::Stale;
    }
    let claim = slot.as_mut().expect("owns_fetch checked it");
    if ok {
        claim.state = FetchState::Done;
        AfterAttempt::Store
    } else if attempt < MAX_AVATAR_SALT_ATTEMPTS {
        AfterAttempt::RetryAfterMs(AVATAR_SALT_RETRY_DELAYS_MS[usize::from(attempt) - 1])
    } else {
        claim.state = FetchState::Exhausted;
        AfterAttempt::GiveUp
    }
}

/// Whether a salt fetched under `epoch` may still be stored.
fn may_store(slot: &Option<SaltClaim>, epoch: u64) -> bool {
    *slot
        == Some(SaltClaim {
            epoch,
            state: FetchState::Done,
        })
}

/// The delegate confirmed it holds the key the room already had: a room
/// whose attempts were exhausted gets a fresh claim. Returns whether the
/// room should be fetched again.
fn rearm_if_exhausted(slot: &mut Option<SaltClaim>) -> bool {
    if matches!(slot, Some(c) if c.state == FetchState::Exhausted) {
        *slot = None;
        true
    } else {
        false
    }
}

/// The delegate's key for the room was written: any salt or running fetch
/// belongs to the old key. Returns whether the room had been asked for (and
/// so should be fetched again).
fn invalidate(slot: &mut Option<SaltClaim>) -> bool {
    slot.take().is_some()
}

static AVATAR_SALT_CLAIMS: LazyLock<Mutex<HashMap<RoomKey, SaltClaim>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Run `f` against `room_key`'s claim slot.
fn with_claim<R>(room_key: RoomKey, f: impl FnOnce(&mut Option<SaltClaim>) -> R) -> R {
    let mut claims = AVATAR_SALT_CLAIMS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut slot = claims.get(&room_key).copied();
    let out = f(&mut slot);
    match slot {
        Some(claim) => claims.insert(room_key, claim),
        None => claims.remove(&room_key),
    };
    out
}

/// Fetch this viewer's avatar salt for `room_key` from the delegate and
/// store it in [`AVATAR_SALTS`]. One fetch per room runs at a time; it makes
/// up to [`MAX_AVATAR_SALT_ATTEMPTS`] attempts (waiting 2 s, then 8 s), after
/// which the room's salt stays absent until the delegate's key for the room
/// is confirmed or written (see [`signing_key_confirmed`] and
/// [`signing_key_written`]). A stored salt is not fetched again.
///
/// Safe to call from render: it writes no signal synchronously; the store is
/// deferred.
pub fn ensure_avatar_salt(room_key: RoomKey) {
    let epoch = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed);
    if !with_claim(room_key, |slot| claim_fetch(slot, epoch)) {
        return;
    }
    crate::util::safe_spawn_local(async move {
        let mut attempt = 1u8;
        loop {
            if !with_claim(room_key, |slot| owns_fetch(slot, epoch)) {
                return;
            }
            let result = fetch_avatar_salt(room_key).await;
            if let Err(e) = &result {
                warn!(
                    "No avatar salt for room {:?} (attempt {}): {}",
                    room_key, attempt, e
                );
            }
            match with_claim(room_key, |slot| {
                after_attempt(slot, epoch, attempt, result.is_ok())
            }) {
                AfterAttempt::Store => {
                    let salt = result.expect("Store follows a fetched salt");
                    crate::util::defer(move || {
                        if with_claim(room_key, |slot| may_store(slot, epoch)) {
                            AVATAR_SALTS.write().insert(room_key, salt);
                        }
                    });
                    return;
                }
                AfterAttempt::RetryAfterMs(ms) => {
                    crate::util::sleep(crate::util::millis(ms)).await;
                    attempt += 1;
                }
                AfterAttempt::GiveUp | AfterAttempt::Stale => return,
            }
        }
    });
}

/// The delegate already held the room's signing key (`migrate_signing_key`
/// found it current). A room whose fetch attempts were exhausted, typically
/// because they ran before the key reached the delegate, is fetched again.
/// A room the UI never asked about is left alone.
pub fn signing_key_confirmed(room_key: RoomKey) {
    if with_claim(room_key, rearm_if_exhausted) {
        ensure_avatar_salt(room_key);
    }
}

/// The delegate's signing key for the room was just written (a key stored
/// for the first time, a stale key overwritten, an identity import
/// (freenet/river#414), a new room). The salt derives from that key, so the
/// room's salt and any running fetch are dropped, and the room is fetched
/// again if the UI had asked for it. A room the UI never asked about is left
/// alone, so a startup migration sends no extra delegate request.
pub fn signing_key_written(room_key: RoomKey) {
    if with_claim(room_key, invalidate) {
        crate::util::defer(move || {
            AVATAR_SALTS.write().remove(&room_key);
        });
        ensure_avatar_salt(room_key);
    }
}

/// Ask the chat delegate for this viewer's avatar salt for `room_key`.
pub async fn fetch_avatar_salt(room_key: RoomKey) -> Result<[u8; 32], String> {
    let request = ChatDelegateRequestMsg::GetAvatarSalt {
        room_key,
        request_id: generate_request_id(),
    };
    extract_avatar_salt(send_delegate_request(request).await)
}

/// Extract the salt from a delegate response.
fn extract_avatar_salt(
    response: Result<ChatDelegateResponseMsg, String>,
) -> Result<[u8; 32], String> {
    match response {
        Ok(ChatDelegateResponseMsg::AvatarSaltResponse { salt, .. }) => salt,
        Ok(other) => Err(format!("Unexpected response: {:?}", other)),
        Err(e) => Err(e),
    }
}

/// The avatar hue (`0..360`) this viewer gives the member with verifying-key
/// bytes `member_vk_bytes`: a BLAKE3 keyed hash of those bytes under `salt`,
/// its first 8 bytes read little-endian, reduced mod 360.
pub fn avatar_hue(salt: &[u8; 32], member_vk_bytes: &[u8]) -> u16 {
    let hash = blake3::keyed_hash(salt, member_vk_bytes);
    let mut head = [0u8; 8];
    head.copy_from_slice(&hash.as_bytes()[..8]);
    (u64::from_le_bytes(head) % 360) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the construction (keyed hash, byte order, reduction). Expected
    /// value computed independently: `b3sum --keyed` over 32 bytes of 0x02
    /// with a key of 32 bytes of 0x01, first 8 bytes little-endian, mod 360.
    #[test]
    fn avatar_hue_golden_vector() {
        assert_eq!(avatar_hue(&[1u8; 32], &[2u8; 32]), 262);
    }

    #[test]
    fn avatar_hue_is_in_range_and_deterministic() {
        let salt = [7u8; 32];
        for i in 0..=255u8 {
            let vk = [i; 32];
            let hue = avatar_hue(&salt, &vk);
            assert!(hue < 360, "hue {hue} out of range");
            assert_eq!(hue, avatar_hue(&salt, &vk));
        }
    }

    /// The point of the salt: another viewer's salt gives a different
    /// colour mapping. Checked over many members, since any one hue can
    /// collide by chance.
    #[test]
    fn avatar_hue_depends_on_salt_and_member() {
        let members: Vec<[u8; 32]> = (0..64u8).map(|i| [i; 32]).collect();
        let a: Vec<u16> = members.iter().map(|m| avatar_hue(&[1u8; 32], m)).collect();
        let b: Vec<u16> = members.iter().map(|m| avatar_hue(&[2u8; 32], m)).collect();
        assert_ne!(a, b, "a different salt must change the mapping");
        let distinct: std::collections::HashSet<u16> = a.iter().copied().collect();
        assert!(
            distinct.len() > 32,
            "64 members should spread over many hues, got {}",
            distinct.len()
        );
    }

    #[test]
    fn extract_avatar_salt_passes_the_delegate_answer_through() {
        let ok = ChatDelegateResponseMsg::AvatarSaltResponse {
            room_key: [3u8; 32],
            request_id: 1,
            salt: Ok([9u8; 32]),
        };
        assert_eq!(extract_avatar_salt(Ok(ok)), Ok([9u8; 32]));

        let refused = ChatDelegateResponseMsg::AvatarSaltResponse {
            room_key: [3u8; 32],
            request_id: 1,
            salt: Err("Signing key not found for this room".into()),
        };
        assert!(extract_avatar_salt(Ok(refused)).is_err());

        let other = ChatDelegateResponseMsg::GetPublicKeyResponse {
            room_key: [3u8; 32],
            public_key: None,
        };
        assert!(extract_avatar_salt(Ok(other)).is_err());
        assert!(extract_avatar_salt(Err("timeout".into())).is_err());
    }

    #[test]
    fn a_room_is_claimed_by_one_fetch_at_a_time() {
        let mut slot = None;
        assert!(claim_fetch(&mut slot, 1));
        assert!(!claim_fetch(&mut slot, 2), "a running fetch keeps the room");
        assert!(owns_fetch(&slot, 1));
        assert!(!owns_fetch(&slot, 2));
    }

    #[test]
    fn a_fetch_retries_twice_with_backoff_then_gives_up() {
        let mut slot = None;
        assert!(claim_fetch(&mut slot, 1));
        assert_eq!(
            after_attempt(&mut slot, 1, 1, false),
            AfterAttempt::RetryAfterMs(2_000)
        );
        assert!(owns_fetch(&slot, 1), "a retrying fetch keeps its claim");
        assert_eq!(
            after_attempt(&mut slot, 1, 2, false),
            AfterAttempt::RetryAfterMs(8_000)
        );
        assert_eq!(after_attempt(&mut slot, 1, 3, false), AfterAttempt::GiveUp);
        assert_eq!(slot.unwrap().state, FetchState::Exhausted);
        assert!(!claim_fetch(&mut slot, 2), "exhausted stays absent");
    }

    #[test]
    fn a_fetched_salt_is_stored_once_and_not_fetched_again() {
        let mut slot = None;
        assert!(claim_fetch(&mut slot, 1));
        assert_eq!(after_attempt(&mut slot, 1, 2, true), AfterAttempt::Store);
        assert!(may_store(&slot, 1));
        assert!(!claim_fetch(&mut slot, 2));
    }

    #[test]
    fn confirming_the_key_rearms_only_an_exhausted_room() {
        let mut never_asked = None;
        assert!(!rearm_if_exhausted(&mut never_asked));

        let mut running = Some(SaltClaim {
            epoch: 1,
            state: FetchState::InFlight,
        });
        assert!(!rearm_if_exhausted(&mut running));
        assert!(owns_fetch(&running, 1));

        let mut done = Some(SaltClaim {
            epoch: 1,
            state: FetchState::Done,
        });
        assert!(!rearm_if_exhausted(&mut done));

        let mut exhausted = Some(SaltClaim {
            epoch: 1,
            state: FetchState::Exhausted,
        });
        assert!(rearm_if_exhausted(&mut exhausted));
        assert!(claim_fetch(&mut exhausted, 2));
    }

    /// A new key supersedes everything fetched or fetching under the old one:
    /// the old task's result is dropped, it stops retrying, and a salt it
    /// already queued is not stored over the new claim.
    #[test]
    fn writing_the_key_supersedes_the_old_fetch() {
        let mut never_asked = None;
        assert!(
            !invalidate(&mut never_asked),
            "an unasked room is not fetched"
        );

        let mut slot = None;
        assert!(claim_fetch(&mut slot, 1));
        assert!(invalidate(&mut slot));
        assert!(claim_fetch(&mut slot, 2));
        assert!(!owns_fetch(&slot, 1), "the old task stops before retrying");
        assert_eq!(after_attempt(&mut slot, 1, 1, true), AfterAttempt::Stale);
        assert_eq!(after_attempt(&mut slot, 1, 1, false), AfterAttempt::Stale);
        assert!(
            owns_fetch(&slot, 2),
            "a stale result leaves the new claim alone"
        );

        // The old task fetched and queued its store; then the key changed.
        let mut slot = None;
        assert!(claim_fetch(&mut slot, 3));
        assert_eq!(after_attempt(&mut slot, 3, 1, true), AfterAttempt::Store);
        assert!(invalidate(&mut slot));
        assert!(claim_fetch(&mut slot, 4));
        assert!(!may_store(&slot, 3));
    }

    /// The salt follows the delegate's key only if every write of that key
    /// reports it. Pins the two `migrate_signing_key` hooks (inside their
    /// branches) and room creation's.
    #[test]
    fn every_signing_key_write_reports_to_the_avatar_salt() {
        use crate::util::source_scan::production_only;

        let signing = production_only(include_str!("signing.rs"));
        let start = signing
            .find("pub async fn migrate_signing_key(")
            .expect("migrate_signing_key exists");
        let body = &signing[start..];
        let body = &body[..body.find("\n}\n").expect("function end")];

        let confirmed = body
            .find("crate::avatar_salt::signing_key_confirmed(room_key);")
            .expect("migrate_signing_key reports a current key");
        let already_current = body
            .find("return MigrationResult::AlreadyCurrent;")
            .expect("the AlreadyCurrent return");
        assert!(
            confirmed < already_current && !body[confirmed..already_current].contains('}'),
            "the confirmation must sit in the AlreadyCurrent branch"
        );

        let store = body
            .find("match store_signing_key(room_key, signing_key).await")
            .expect("the store");
        let written = body
            .find("crate::avatar_salt::signing_key_written(room_key);")
            .expect("migrate_signing_key reports a written key");
        let verify = store
            + body[store..]
                .find("get_public_key(room_key)")
                .expect("the verifying read");
        assert!(
            store < written && written < verify,
            "the write must be reported on the stored branch, before verification"
        );

        let create = production_only(include_str!("components/room_list/create_room_modal.rs"));
        let stored = create
            .find("store_signing_key(room_key_bytes, &sk_clone).await")
            .expect("room creation stores the key");
        assert!(
            create[stored..].contains("crate::avatar_salt::signing_key_written(room_key_bytes);"),
            "room creation must report the key it stored"
        );
    }
}
