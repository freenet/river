//! Per-viewer avatar colour salt.
//!
//! Each message author's avatar is coloured per VIEWER: the hue is
//! [`avatar_hue`]`(salt, member_vk)`, a BLAKE3 keyed hash under a salt only
//! this viewer knows.
//!
//! Colour is a weak HINT, not an identity check. With 10 drawn hues a blind
//! guess matches a given member's colour 1 time in 10, and keys are free, so
//! an impersonator can try sockpuppets until one happens to match for some
//! viewer. What the salt stops is an attacker CHOOSING or COMPUTING the colour
//! a particular viewer sees for a key: without that viewer's seed they cannot
//! aim. The real defence against lookalikes is the confusable-name warning;
//! never present the colour as more than a hint.
//!
//! The salt derives from a random SEED that only this viewer holds: 32 bytes
//! from the CSPRNG, stored in the chat delegate under
//! [`AVATAR_SEED_STORAGE_KEY`], read once per session. Each room's salt is
//! [`room_salt`]`(seed, room_owner_vk)`, computed locally. It deliberately
//! does NOT derive from the member signing key: the inviter generates that key
//! and ships its private half in the invitation, so anyone who invited this
//! viewer could compute a key-derived salt and aim a lookalike at them.
//!
//! A room whose salt is absent (the seed could not be read) renders neutral
//! avatars; there is no fallback to any key.
//!
//! The seed is per device: identity export does not carry it. It normally
//! survives a chat-delegate re-key: the migration walk only finds it (earliest
//! seed winning), and the walk driver carries it after the walk's journal
//! status is recorded and the journal lock released, in a detached task
//! bounded by its requests' 10 s timeouts
//! (`delegate_migration::carry_found_seed_with`, [`import_seed`]). So it never
//! holds up the migration of real data, the identity-import gate or the
//! sweep's journal gate; a failed carry changes the palette once.

use crate::components::app::freenet_api::delegate_migration::RiverDelegateChannel;
// Never `{:?}` a reply here: a seed-bearing reply would print the seed.
use crate::components::app::freenet_api::response_handler::describe_delegate_response;
use dioxus::logger::tracing::warn;
use dioxus::prelude::*;
use freenet_stdlib::prelude::DelegateKey;
use river_core::chat_delegate::{
    CasStoreResult, ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg, RoomKey,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

/// This viewer's avatar colour salt per room (keyed by the room owner's
/// verifying-key bytes), filled by [`ensure_avatar_salt`]. A room with no
/// entry renders neutral avatars.
///
/// Read it with `try_read()`, and in a memo or effect with the
/// `util::signal_guard` anchor and nudge (dioxus-signal-safety.md).
pub static AVATAR_SALTS: GlobalSignal<HashMap<RoomKey, [u8; 32]>> = Global::new(HashMap::new);

// ---------------------------------------------------------------------------
// The seed and the per-room salt
// ---------------------------------------------------------------------------

/// Chat-delegate storage key of the viewer's [`AvatarSeedRecord`].
pub(crate) const AVATAR_SEED_STORAGE_KEY: &[u8] = b"river_avatar_colour_seed_v1";

/// Domain tag hashed ahead of the room key in [`room_salt`].
const ROOM_SALT_DOMAIN: &[u8] = b"freenet river avatar colour salt v1";

/// The viewer's avatar seed as stored in the chat delegate (CBOR).
///
/// `created_at_ms` orders two seeds that meet (a re-key's walk importing a
/// predecessor's seed into a successor that already made one): the earliest
/// wins, ties broken by the seed bytes, so every importer converges on the
/// user's original palette ([`AvatarSeedRecord::precedes`]).
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AvatarSeedRecord {
    pub(crate) created_at_ms: u64,
    pub(crate) seed: [u8; 32],
}

/// Redacts the seed: it is the secret that keeps the palette private.
impl std::fmt::Debug for AvatarSeedRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AvatarSeedRecord")
            .field("created_at_ms", &self.created_at_ms)
            .finish_non_exhaustive()
    }
}

impl AvatarSeedRecord {
    /// A fresh seed from the CSPRNG (the one invitations use, which in the
    /// browser is `crypto.getRandomValues`).
    #[cfg_attr(all(feature = "example-data", feature = "no-sync"), allow(dead_code))]
    fn generate() -> Self {
        use rand::RngCore;
        let mut seed = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut seed);
        Self {
            created_at_ms: crate::components::app::sync_info::now_ms() as u64,
            seed,
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        ciborium::ser::into_writer(self, &mut out).expect("a seed record always encodes");
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        ciborium::from_reader(bytes).ok()
    }

    /// Whether `self` wins over `other` when two seeds meet.
    pub(crate) fn precedes(&self, other: &Self) -> bool {
        (self.created_at_ms, self.seed) < (other.created_at_ms, other.seed)
    }
}

/// This viewer's salt for the room owned by `room_key`: a BLAKE3 keyed hash,
/// keyed by the seed, of [`ROOM_SALT_DOMAIN`] followed by the room key.
pub(crate) fn room_salt(seed: &[u8; 32], room_key: &RoomKey) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_keyed(seed);
    hasher.update(ROOM_SALT_DOMAIN);
    hasher.update(room_key);
    *hasher.finalize().as_bytes()
}

// ---------------------------------------------------------------------------
// Delegate storage
// ---------------------------------------------------------------------------

/// Compare-and-swap rounds for one seed write. Each conflict means another
/// session wrote the seed in between; a handful is plenty.
const SEED_CAS_ATTEMPTS: u32 = 3;

/// Serialises every seed read and write in this tab. The session's read and
/// the migration walk's import ask the SAME delegate for the SAME key, so their
/// requests share one correlation slot in the single-waiter `PENDING_REQUESTS`
/// registry: run together, the second registration displaces the first's
/// waiter, or the replies cross and each acts on the other's answer.
///
/// Residual this does not close: a reply that arrives AFTER its request's
/// 10 s timeout finds the slot empty
/// or, if the next seed request has registered by then, fills that request's
/// waiter. The reply's key is checked and matches; the protocol carries no
/// request id for these operations and the generation alone cannot tell two
/// requests for one key apart, so there is no cheap guard. Worst case: one
/// operation acts on a stale read or a stale store result, and this device's
/// palette changes once (the CAS still refuses any write against a stale
/// generation, so no seed is overwritten blindly).
static SEED_IO: futures::lock::Mutex<()> = futures::lock::Mutex::new(());

/// Serialises every test that takes [`SEED_IO`] (in this module and in the
/// migration, journal and registry tests). `SEED_IO` is process-wide, so under
/// a parallel `cargo test` one test holding it (the hanging-carry test holds it
/// until it ends) reorders another's turns or stalls it. Take this FIRST, before
/// any other test guard, so the lock order is the same everywhere.
#[cfg(test)]
pub(crate) fn seed_io_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static SEED_IO_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SEED_IO_TESTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn seed_key() -> ChatDelegateKey {
    ChatDelegateKey::new(AVATAR_SEED_STORAGE_KEY.to_vec())
}

/// The stored seed bytes and their CAS generation on `delegate`.
async fn read_seed_versioned<C: RiverDelegateChannel>(
    channel: &C,
    delegate: &DelegateKey,
) -> Result<(Option<Vec<u8>>, u64), String> {
    let key = seed_key();
    match channel
        .request(
            delegate,
            ChatDelegateRequestMsg::GetVersionedRequest { key: key.clone() },
        )
        .await?
    {
        Some(ChatDelegateResponseMsg::GetVersionedResponse {
            key: reply_key,
            value,
            generation,
        }) if reply_key == key => Ok((value, generation)),
        Some(other) => Err(format!(
            "unexpected reply to seed GetVersionedRequest: {}",
            describe_delegate_response(&other)
        )),
        None => Err("seed GetVersionedRequest timed out".to_string()),
    }
}

/// Store `record` on `delegate` if its generation is still `generation`.
async fn cas_seed<C: RiverDelegateChannel>(
    channel: &C,
    delegate: &DelegateKey,
    record: &AvatarSeedRecord,
    generation: u64,
) -> Result<CasStoreResult, String> {
    let key = seed_key();
    match channel
        .request(
            delegate,
            ChatDelegateRequestMsg::CasStoreRequest {
                key: key.clone(),
                value: record.encode(),
                expected_generation: generation,
            },
        )
        .await?
    {
        Some(ChatDelegateResponseMsg::CasStoreResponse {
            key: reply_key,
            result,
        }) if reply_key == key => Ok(result),
        Some(other) => Err(format!(
            "unexpected reply to seed CasStoreRequest: {}",
            describe_delegate_response(&other)
        )),
        None => Err("seed CasStoreRequest timed out".to_string()),
    }
}

/// The seed stored on `delegate`, storing `fresh` first if there is none.
///
/// The store is a compare-and-swap, so two tabs cannot both create one: the
/// one that loses adopts the winner's. A stored value that does not decode is
/// replaced (the key is versioned; a different format gets a new key).
#[cfg_attr(all(feature = "example-data", feature = "no-sync"), allow(dead_code))]
pub(crate) async fn load_or_create_seed<C: RiverDelegateChannel>(
    channel: &C,
    delegate: &DelegateKey,
    fresh: AvatarSeedRecord,
) -> Result<AvatarSeedRecord, String> {
    let _io = SEED_IO.lock().await;
    let (mut value, mut generation) = read_seed_versioned(channel, delegate).await?;
    for _ in 0..SEED_CAS_ATTEMPTS {
        if let Some(record) = value.as_deref().and_then(AvatarSeedRecord::decode) {
            return Ok(record);
        }
        match cas_seed(channel, delegate, &fresh, generation).await? {
            CasStoreResult::Stored { .. } => return Ok(fresh),
            CasStoreResult::Conflict {
                current_generation,
                current_value,
            } => {
                value = current_value;
                generation = current_generation;
            }
            CasStoreResult::Failed(e) => return Err(format!("delegate refused seed store: {e}")),
        }
    }
    // The last conflict may have carried a seed another tab stored: adopt it.
    value
        .as_deref()
        .and_then(AvatarSeedRecord::decode)
        .ok_or_else(|| "seed store kept conflicting".to_string())
}

/// What [`import_seed`] did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SeedImport {
    /// The incoming seed now stands on the successor.
    Replaced,
    /// The successor's seed precedes (or is) the incoming one, and stands.
    Kept,
}

/// Carry a predecessor's seed onto `successor`: written unless the successor
/// already holds a seed that precedes it ([`AvatarSeedRecord::precedes`]).
pub(crate) async fn import_seed<C: RiverDelegateChannel>(
    channel: &C,
    successor: &DelegateKey,
    incoming: AvatarSeedRecord,
) -> Result<SeedImport, String> {
    let _io = SEED_IO.lock().await;
    let (mut value, mut generation) = read_seed_versioned(channel, successor).await?;
    let stands = |value: &Option<Vec<u8>>| {
        value
            .as_deref()
            .and_then(AvatarSeedRecord::decode)
            .is_some_and(|held| !incoming.precedes(&held))
    };
    for _ in 0..SEED_CAS_ATTEMPTS {
        if stands(&value) {
            return Ok(SeedImport::Kept);
        }
        match cas_seed(channel, successor, &incoming, generation).await? {
            CasStoreResult::Stored { .. } => return Ok(SeedImport::Replaced),
            CasStoreResult::Conflict {
                current_generation,
                current_value,
            } => {
                value = current_value;
                generation = current_generation;
            }
            CasStoreResult::Failed(e) => return Err(format!("delegate refused seed import: {e}")),
        }
    }
    // The last conflict may have stored a seed at least as early: it stands.
    if stands(&value) {
        return Ok(SeedImport::Kept);
    }
    Err("seed import kept conflicting".to_string())
}

// ---------------------------------------------------------------------------
// The session: one seed read, rooms filled as they are asked for
// ---------------------------------------------------------------------------

/// Seed read attempts before the session is left without a seed.
const MAX_SEED_ATTEMPTS: u8 = 3;

/// Wait before attempt `n + 1` after attempt `n` fails.
const SEED_RETRY_DELAYS_MS: [u64; (MAX_SEED_ATTEMPTS - 1) as usize] = [2_000, 8_000];

/// Where the session's seed read stands. (No `Debug`: `Ready` holds the seed.)
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SeedFetch {
    /// Not asked for yet.
    #[default]
    Idle,
    /// The task with this epoch is reading, or waiting to retry.
    InFlight { epoch: u64 },
    /// Read.
    Ready { seed: [u8; 32] },
    /// Every attempt failed; re-armed when the delegate is known present.
    Exhausted,
}

/// Salts to store, by room.
type Salts = Vec<(RoomKey, [u8; 32])>;

#[derive(Default)]
struct SeedSession {
    fetch: SeedFetch,
    /// Rooms the UI has asked for.
    wanted: BTreeSet<RoomKey>,
    /// Rooms whose salt has been handed out for the current seed, so a room
    /// asked for on every render is stored once.
    filled: BTreeSet<RoomKey>,
}

/// What [`SeedSession::want`] asks the caller to do.
#[derive(PartialEq, Eq)]
enum Want {
    Fill(Salts),
    Fetch(u64),
    Nothing,
}

/// What a fetch task does after an attempt.
#[derive(PartialEq, Eq)]
enum AfterAttempt {
    Fill(Salts),
    RetryAfterMs(u64),
    GiveUp,
    /// The session moved on (the seed was replaced, or re-armed); do nothing.
    Stale,
}

impl SeedSession {
    fn salts(seed: &[u8; 32], rooms: impl IntoIterator<Item = RoomKey>) -> Salts {
        rooms
            .into_iter()
            .map(|room| (room, room_salt(seed, &room)))
            .collect()
    }

    /// The UI asks for `room`'s salt; `epoch` is used if a read starts.
    fn want(&mut self, room: RoomKey, epoch: u64) -> Want {
        self.wanted.insert(room);
        match self.fetch {
            SeedFetch::Ready { seed } if self.filled.insert(room) => {
                Want::Fill(Self::salts(&seed, [room]))
            }
            SeedFetch::Idle => {
                self.fetch = SeedFetch::InFlight { epoch };
                Want::Fetch(epoch)
            }
            _ => Want::Nothing,
        }
    }

    /// Whether the task with `epoch` still owns the read.
    fn owns(&self, epoch: u64) -> bool {
        self.fetch == SeedFetch::InFlight { epoch }
    }

    /// Record attempt number `attempt` (1-based): the seed, or `None`.
    fn after_attempt(&mut self, epoch: u64, attempt: u8, seed: Option<[u8; 32]>) -> AfterAttempt {
        if !self.owns(epoch) {
            return AfterAttempt::Stale;
        }
        match seed {
            Some(seed) => {
                self.fetch = SeedFetch::Ready { seed };
                let rooms: Vec<RoomKey> = self.wanted.difference(&self.filled).copied().collect();
                self.filled.extend(rooms.iter().copied());
                AfterAttempt::Fill(Self::salts(&seed, rooms))
            }
            None if attempt < MAX_SEED_ATTEMPTS => {
                AfterAttempt::RetryAfterMs(SEED_RETRY_DELAYS_MS[usize::from(attempt) - 1])
            }
            None => {
                self.fetch = SeedFetch::Exhausted;
                AfterAttempt::GiveUp
            }
        }
    }

    /// The delegate is known present: an exhausted read starts again with
    /// `epoch` if any room is waiting. Returns the epoch to run.
    fn rearm_if_exhausted(&mut self, epoch: u64) -> Option<u64> {
        if self.fetch != SeedFetch::Exhausted {
            return None;
        }
        if self.wanted.is_empty() {
            self.fetch = SeedFetch::Idle;
            return None;
        }
        self.fetch = SeedFetch::InFlight { epoch };
        Some(epoch)
    }

    /// The migration walk replaced the stored seed: every room asked for so
    /// far is recoloured under the new one, and a read still running is stale.
    fn replace_seed(&mut self, seed: [u8; 32]) -> Salts {
        self.fetch = SeedFetch::Ready { seed };
        self.filled.extend(self.wanted.iter().copied());
        Self::salts(&seed, self.filled.iter().copied())
    }
}

static SESSION: LazyLock<Mutex<SeedSession>> = LazyLock::new(|| Mutex::new(SeedSession::default()));

static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

fn with_session<R>(f: impl FnOnce(&mut SeedSession) -> R) -> R {
    let mut session = SESSION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut session)
}

fn next_epoch() -> u64 {
    NEXT_EPOCH.fetch_add(1, Ordering::Relaxed)
}

/// Store `salts` in [`AVATAR_SALTS`], deferred.
fn store_salts(salts: Salts) {
    if salts.is_empty() {
        return;
    }
    crate::util::defer(move || {
        let mut map = AVATAR_SALTS.write();
        for (room, salt) in salts {
            map.insert(room, salt);
        }
    });
}

/// Make `room_key`'s salt available in [`AVATAR_SALTS`].
///
/// The first call of the session reads the seed (one delegate request, two
/// the first time a device ever runs: the read and the creating store), up
/// to 3 attempts 2 s then 8 s apart; every later call costs no request.
/// Safe to call from render: it writes no signal synchronously, and a room
/// already filled is not stored again.
pub fn ensure_avatar_salt(room_key: RoomKey) {
    match with_session(|s| s.want(room_key, next_epoch())) {
        Want::Fill(salts) => store_salts(salts),
        Want::Fetch(epoch) => spawn_seed_read(epoch),
        Want::Nothing => {}
    }
}

/// Called where the outbound-DM load is sent, once per load attempt: after the
/// current delegate's room list was answered, after it was registered, or by
/// the 30 s fallback when neither happened (`DM_LOAD_FALLBACK_MS`). The
/// delegate is therefore probably, not certainly, present. Every reconnect and
/// refresh-on-wake re-runs `set_up_chat_delegate`, a new attempt, so this runs
/// again then too. A seed read that gave up (typically one sent before the
/// delegate was registered) starts again; nothing else does, so the cost is
/// at most one 3-attempt read per load attempt, and only after a failure.
pub(crate) fn current_delegate_present() {
    if let Some(epoch) = with_session(|s| s.rearm_if_exhausted(next_epoch())) {
        spawn_seed_read(epoch);
    }
}

/// The migration walk stored a predecessor's earlier seed over this
/// session's: recolour every room under it.
pub(crate) fn seed_replaced(record: AvatarSeedRecord) {
    store_salts(with_session(|s| s.replace_seed(record.seed)));
}

fn spawn_seed_read(epoch: u64) {
    crate::util::safe_spawn_local(async move {
        let mut attempt = 1u8;
        loop {
            if !with_session(|s| s.owns(epoch)) {
                return;
            }
            let seed = match read_seed().await {
                Ok(seed) => Some(seed),
                Err(e) => {
                    warn!("No avatar seed (attempt {}): {}", attempt, e);
                    None
                }
            };
            match with_session(|s| s.after_attempt(epoch, attempt, seed)) {
                AfterAttempt::Fill(salts) => {
                    store_salts(salts);
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

/// This viewer's seed, from the current chat delegate.
#[cfg(not(all(feature = "example-data", feature = "no-sync")))]
async fn read_seed() -> Result<[u8; 32], String> {
    use crate::components::app::chat_delegate::current_delegate_key;
    use crate::components::app::freenet_api::delegate_migration::NodeDelegateChannel;
    load_or_create_seed(
        &NodeDelegateChannel,
        &current_delegate_key(),
        AvatarSeedRecord::generate(),
    )
    .await
    .map(|record| record.seed)
}

/// The offline example build has no delegate, so it stands in a fixed seed
/// and its avatars are coloured as a real viewer's would be. Tests clear a
/// room's salt with `__riverTest.setAvatarSalt`.
#[cfg(all(feature = "example-data", feature = "no-sync"))]
async fn read_seed() -> Result<[u8; 32], String> {
    Ok(*blake3::hash(b"river example-data avatar seed").as_bytes())
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

/// Steps the displayed hue is quantised to, [`AVATAR_HUE_STEP_DEGREES`] apart.
pub const AVATAR_HUE_STEPS: u16 = 10;
/// Degrees between two displayed avatar hues.
pub const AVATAR_HUE_STEP_DEGREES: u16 = 360 / AVATAR_HUE_STEPS;

/// The hue an avatar is DRAWN with: [`avatar_hue`]'s raw output rounded to
/// the nearest of [`AVATAR_HUE_STEPS`] evenly spaced hues. At the avatars'
/// low chroma two hues 15° apart look the same, so two discs are either the
/// same colour or clearly different, never confusably close.
pub fn avatar_hue_step(hue: u16) -> u16 {
    let half = AVATAR_HUE_STEP_DEGREES / 2;
    ((hue % 360 + half) / AVATAR_HUE_STEP_DEGREES % AVATAR_HUE_STEPS) * AVATAR_HUE_STEP_DEGREES
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::app::freenet_api::delegate_migration::tests_support::{
        successor_key, DelegateSim, MockChannel,
    };
    use futures::executor::block_on;

    #[test]
    fn avatar_hue_step_rounds_to_the_nearest_of_ten_hues() {
        assert_eq!(avatar_hue_step(0), 0);
        assert_eq!(avatar_hue_step(17), 0);
        assert_eq!(avatar_hue_step(18), 36);
        assert_eq!(avatar_hue_step(53), 36);
        assert_eq!(avatar_hue_step(262), 252);
        // The top wraps to red, not to a 360th hue.
        assert_eq!(avatar_hue_step(342), 0);
        assert_eq!(avatar_hue_step(359), 0);
        let steps: std::collections::BTreeSet<u16> = (0..360).map(avatar_hue_step).collect();
        assert_eq!(steps.len(), AVATAR_HUE_STEPS as usize);
        assert!(steps.iter().all(|h| h % AVATAR_HUE_STEP_DEGREES == 0));
    }

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

    fn record(created_at_ms: u64, fill: u8) -> AvatarSeedRecord {
        AvatarSeedRecord {
            created_at_ms,
            seed: [fill; 32],
        }
    }

    fn channel_with(store: Option<AvatarSeedRecord>) -> MockChannel {
        let mut sim = DelegateSim::default();
        if let Some(r) = store {
            sim.store
                .insert(AVATAR_SEED_STORAGE_KEY.to_vec(), r.encode());
            sim.generations.insert(AVATAR_SEED_STORAGE_KEY.to_vec(), 1);
        }
        MockChannel::default().with_delegate(&successor_key(), sim)
    }

    fn stored(channel: &MockChannel) -> Option<AvatarSeedRecord> {
        channel
            .stored(&successor_key(), AVATAR_SEED_STORAGE_KEY)
            .and_then(|v| AvatarSeedRecord::decode(&v))
    }

    /// Pins the construction. Expected value computed independently:
    /// `b3sum --keyed`, key 32 bytes of 0x01, over the domain tag followed by
    /// 32 bytes of 0x02.
    #[test]
    fn room_salt_golden_vector() {
        let hex: String = room_salt(&[1u8; 32], &[2u8; 32])
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex,
            "9845ac3806756122f0e80c3d6c8560559366d7b9053f12ea8cb6f21e7c942e97"
        );
    }

    #[test]
    fn per_room_salts_differ_and_depend_on_the_seed() {
        let seed = [7u8; 32];
        assert_ne!(room_salt(&seed, &[1u8; 32]), room_salt(&seed, &[2u8; 32]));
        assert_ne!(
            room_salt(&seed, &[1u8; 32]),
            room_salt(&[8u8; 32], &[1u8; 32])
        );
        assert_eq!(room_salt(&seed, &[1u8; 32]), room_salt(&seed, &[1u8; 32]));
    }

    #[test]
    fn a_seed_is_created_once_and_read_back() {
        let _seed_io = seed_io_test_guard();
        let channel = channel_with(None);
        let created = block_on(load_or_create_seed(
            &channel,
            &successor_key(),
            record(5, 1),
        ))
        .unwrap();
        assert_eq!(created, record(5, 1));
        assert_eq!(stored(&channel), Some(record(5, 1)));

        let again = block_on(load_or_create_seed(
            &channel,
            &successor_key(),
            record(9, 2),
        ))
        .unwrap();
        assert_eq!(again, record(5, 1), "a stored seed is read, never replaced");
        assert_eq!(stored(&channel), Some(record(5, 1)));
    }

    /// Two tabs read "no seed" at once; the second's store conflicts and it
    /// adopts the first's, so the device has one palette.
    #[test]
    fn a_tab_that_loses_the_create_race_adopts_the_stored_seed() {
        let _seed_io = seed_io_test_guard();
        let channel = channel_with(None);
        channel
            .delegates
            .borrow_mut()
            .get_mut(successor_key().bytes())
            .unwrap()
            .cas_interleave = Some((AVATAR_SEED_STORAGE_KEY.to_vec(), record(3, 9).encode()));
        let got = block_on(load_or_create_seed(
            &channel,
            &successor_key(),
            record(5, 1),
        ))
        .unwrap();
        assert_eq!(got, record(3, 9));
        assert_eq!(stored(&channel), Some(record(3, 9)));
    }

    #[test]
    fn an_undecodable_stored_seed_is_replaced() {
        let _seed_io = seed_io_test_guard();
        let mut sim = DelegateSim::default();
        sim.store
            .insert(AVATAR_SEED_STORAGE_KEY.to_vec(), vec![0xFF, 0x00]);
        sim.generations.insert(AVATAR_SEED_STORAGE_KEY.to_vec(), 1);
        let channel = MockChannel::default().with_delegate(&successor_key(), sim);
        let got = block_on(load_or_create_seed(
            &channel,
            &successor_key(),
            record(5, 1),
        ))
        .unwrap();
        assert_eq!(got, record(5, 1));
        assert_eq!(stored(&channel), Some(record(5, 1)));
    }

    #[test]
    fn a_silent_delegate_is_an_error_not_a_new_seed() {
        let _seed_io = seed_io_test_guard();
        let channel = channel_with(None);
        channel.silence(&successor_key(), true);
        assert!(block_on(load_or_create_seed(
            &channel,
            &successor_key(),
            record(5, 1)
        ))
        .is_err());
        channel.silence(&successor_key(), false);
        assert_eq!(stored(&channel), None);
    }

    /// Replays fixed replies, in order, whatever is asked.
    struct Scripted(std::cell::RefCell<std::collections::VecDeque<ChatDelegateResponseMsg>>);

    impl Scripted {
        fn new(replies: Vec<ChatDelegateResponseMsg>) -> Self {
            Self(std::cell::RefCell::new(replies.into()))
        }
    }

    impl RiverDelegateChannel for Scripted {
        async fn request(
            &self,
            _target: &DelegateKey,
            _request: ChatDelegateRequestMsg,
        ) -> Result<Option<ChatDelegateResponseMsg>, String> {
            Ok(self.0.borrow_mut().pop_front())
        }
        async fn request_all(
            &self,
            requests: Vec<(DelegateKey, ChatDelegateRequestMsg)>,
        ) -> Vec<Result<Option<ChatDelegateResponseMsg>, String>> {
            let mut out = Vec::new();
            for (target, request) in requests {
                out.push(self.request(&target, request).await);
            }
            out
        }
    }

    fn versioned(value: Option<Vec<u8>>, generation: u64) -> ChatDelegateResponseMsg {
        ChatDelegateResponseMsg::GetVersionedResponse {
            key: seed_key(),
            value,
            generation,
        }
    }

    fn cas(result: CasStoreResult) -> ChatDelegateResponseMsg {
        ChatDelegateResponseMsg::CasStoreResponse {
            key: seed_key(),
            result,
        }
    }

    fn conflict(value: Vec<u8>, generation: u64) -> ChatDelegateResponseMsg {
        cas(CasStoreResult::Conflict {
            current_generation: generation,
            current_value: Some(value),
        })
    }

    /// The read, then exactly `SEED_CAS_ATTEMPTS` conflicting stores.
    fn three_conflicts(last: Vec<u8>) -> Vec<ChatDelegateResponseMsg> {
        assert_eq!(SEED_CAS_ATTEMPTS, 3);
        vec![
            versioned(None, 0),
            conflict(vec![0xFF], 1),
            conflict(vec![0xFF], 2),
            conflict(last, 3),
        ]
    }

    #[test]
    fn a_valid_seed_in_the_last_conflict_is_adopted() {
        let _seed_io = seed_io_test_guard();
        let channel = Scripted::new(three_conflicts(record(3, 9).encode()));
        let got = block_on(load_or_create_seed(
            &channel,
            &successor_key(),
            record(5, 1),
        ));
        assert_eq!(got, Ok(record(3, 9)));

        // The importer: the last conflict holds an earlier seed, which stands.
        let channel = Scripted::new(three_conflicts(record(3, 9).encode()));
        let got = block_on(import_seed(&channel, &successor_key(), record(5, 1)));
        assert_eq!(got, Ok(SeedImport::Kept));
    }

    #[test]
    fn conflicts_that_never_settle_are_an_error() {
        let _seed_io = seed_io_test_guard();
        let channel = Scripted::new(three_conflicts(vec![0xFF]));
        assert!(block_on(load_or_create_seed(
            &channel,
            &successor_key(),
            record(5, 1)
        ))
        .is_err());

        // The importer: the last conflict holds a LATER seed, which must not
        // be reported as kept.
        let channel = Scripted::new(three_conflicts(record(9, 2).encode()));
        assert!(block_on(import_seed(&channel, &successor_key(), record(5, 1))).is_err());
    }

    #[test]
    fn a_refused_store_is_an_error() {
        let _seed_io = seed_io_test_guard();
        let refused = || {
            vec![
                versioned(None, 0),
                cas(CasStoreResult::Failed("no space".into())),
            ]
        };
        let channel = Scripted::new(refused());
        assert!(block_on(load_or_create_seed(
            &channel,
            &successor_key(),
            record(5, 1)
        ))
        .is_err());
        let channel = Scripted::new(refused());
        assert!(block_on(import_seed(&channel, &successor_key(), record(5, 1))).is_err());
    }

    #[test]
    fn importing_keeps_the_earliest_seed() {
        let _seed_io = seed_io_test_guard();
        // Absent: the incoming seed lands.
        let channel = channel_with(None);
        assert_eq!(
            block_on(import_seed(&channel, &successor_key(), record(5, 1))),
            Ok(SeedImport::Replaced)
        );
        assert_eq!(stored(&channel), Some(record(5, 1)));

        // A newer held seed (made before the walk got here) gives way.
        let channel = channel_with(Some(record(9, 2)));
        assert_eq!(
            block_on(import_seed(&channel, &successor_key(), record(5, 1))),
            Ok(SeedImport::Replaced)
        );
        assert_eq!(stored(&channel), Some(record(5, 1)));

        // An earlier, or the same, held seed stands.
        for held in [record(3, 7), record(5, 1)] {
            let channel = channel_with(Some(held));
            assert_eq!(
                block_on(import_seed(&channel, &successor_key(), record(5, 1))),
                Ok(SeedImport::Kept)
            );
            assert_eq!(stored(&channel), Some(held));
        }
    }

    #[test]
    fn the_first_room_starts_one_read_and_later_rooms_wait_for_it() {
        let mut s = SeedSession::default();
        assert!(s.want([1; 32], 10) == Want::Fetch(10));
        assert!(s.want([2; 32], 11) == Want::Nothing, "one read per session");
        assert!(s.owns(10) && !s.owns(11));
        match s.after_attempt(10, 1, Some([7; 32])) {
            AfterAttempt::Fill(salts) => {
                let rooms: Vec<RoomKey> = salts.iter().map(|(r, _)| *r).collect();
                assert_eq!(rooms, vec![[1; 32], [2; 32]]);
                assert_eq!(salts[0].1, room_salt(&[7; 32], &[1; 32]));
            }
            _ => panic!("a read seed fills every waiting room"),
        }
        assert!(
            s.want([1; 32], 12) == Want::Nothing,
            "a filled room is stored once"
        );
        match s.want([3; 32], 13) {
            Want::Fill(salts) => assert_eq!(salts, vec![([3; 32], room_salt(&[7; 32], &[3; 32]))]),
            _ => panic!("a new room fills from the ready seed"),
        }
    }

    #[test]
    fn a_read_retries_twice_with_backoff_then_gives_up_until_rearmed() {
        let mut s = SeedSession::default();
        assert!(s.want([1; 32], 1) == Want::Fetch(1));
        assert!(s.after_attempt(1, 1, None) == AfterAttempt::RetryAfterMs(2_000));
        assert!(s.owns(1), "a retrying read keeps its claim");
        assert!(s.after_attempt(1, 2, None) == AfterAttempt::RetryAfterMs(8_000));
        assert!(s.after_attempt(1, 3, None) == AfterAttempt::GiveUp);
        assert!(
            s.want([2; 32], 2) == Want::Nothing,
            "exhausted stays absent"
        );
        assert_eq!(s.rearm_if_exhausted(3), Some(3));
        assert!(s.owns(3));
        assert_eq!(
            s.rearm_if_exhausted(4),
            None,
            "only an exhausted read re-arms"
        );
        assert!(
            matches!(s.after_attempt(3, 1, Some([7; 32])), AfterAttempt::Fill(f) if f.len() == 2)
        );
    }

    #[test]
    fn an_exhausted_session_nobody_waits_on_starts_no_read() {
        let mut s = SeedSession {
            fetch: SeedFetch::Exhausted,
            ..SeedSession::default()
        };
        assert_eq!(s.rearm_if_exhausted(1), None);
        assert!(s.want([1; 32], 2) == Want::Fetch(2));
    }

    #[test]
    fn a_replaced_seed_recolours_every_room_and_stales_a_running_read() {
        let mut s = SeedSession::default();
        assert!(s.want([1; 32], 1) == Want::Fetch(1));
        let salts = s.replace_seed([9; 32]);
        assert_eq!(salts, vec![([1; 32], room_salt(&[9; 32], &[1; 32]))]);
        assert!(s.after_attempt(1, 1, Some([7; 32])) == AfterAttempt::Stale);
        assert!(s.want([1; 32], 2) == Want::Nothing);
    }

    #[test]
    fn the_seed_record_never_prints_its_seed() {
        let printed = format!("{:?}", record(5, 0xAB));
        assert!(!printed.contains("171"), "the seed bytes leaked: {printed}");
        assert!(printed.contains("created_at_ms"));
    }

    /// A seed read that gave up is re-armed only if the delegate-present
    /// point reports in. Fails if the call is dropped from it.
    #[test]
    fn the_delegate_present_point_rearms_the_seed_read() {
        // Not `production_only`: this file's test module sits above the
        // function. Anchored at a line start, so the copy of the signature a
        // test there quotes as a string cannot match.
        let src = crate::util::strip_comments(include_str!("components/app/chat_delegate.rs"));
        let start = src
            .find("\nasync fn fire_load_outbound_dms_request(attempt: u32) {\n")
            .expect("the outbound-DM load");
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").expect("function end")];
        assert!(body.contains("crate::avatar_salt::current_delegate_present();"));
    }

    const KEY_WORDS: [&str; 3] = ["self_sk", "SigningKey", "signing_key"];
    const SALT_CALLS: [&str; 5] = [
        "display_hue(",
        "avatar_hue(",
        "avatar_hue_step(",
        "room_salt(",
        "ensure_avatar_salt(",
    ];

    /// Every mention of a signing key at a salt or hue call site in `src`
    /// (comments stripped): the call's arguments, and the statement leading
    /// up to it (back to the previous `;`, `{` or `}`), as `"name: text"`.
    fn key_mentions_at_salt_calls(name: &str, src: &str) -> Vec<String> {
        let mut out = Vec::new();
        for call in SALT_CALLS {
            let mut from = 0;
            while let Some(i) = src[from..].find(call) {
                let at = from + i;
                from = at + call.len();
                // `fn display_hue(` is the definition, not a call.
                if src[..at].trim_end().ends_with("fn") {
                    continue;
                }
                let lead = src[..at].rfind([';', '{', '}']).map_or(0, |j| j + 1);
                let mut depth = 1usize;
                let mut end = from;
                for (k, c) in src[from..].char_indices() {
                    match c {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                end = from + k;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let site = &src[lead..end];
                for word in KEY_WORDS {
                    if site.contains(word) {
                        out.push(format!("{name}: {word} in `{}`", site.trim()));
                    }
                }
            }
        }
        out
    }

    /// The salt must never be derived from a signing key: the inviter
    /// generated the invitee's key, so anything derived from it is computable
    /// by them. Scans `avatar_salt.rs` outside its tests whole, and every salt
    /// or hue call site in the UI. A text scan sees only the call's own
    /// statement: a key bound in an EARLIER statement and passed by name is
    /// not caught. What stops that is the module scan, since nothing in the
    /// salt API takes or names a key.
    #[test]
    fn no_salt_or_hue_call_site_mentions_a_signing_key() {
        use crate::util::source_scan::{production_only, rust_files};
        let module = production_only(include_str!("avatar_salt.rs"));
        for word in KEY_WORDS {
            assert!(!module.contains(word), "avatar_salt.rs mentions {word}");
        }
        let mut sites = 0;
        let mut found = Vec::new();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for path in rust_files(&root) {
            let src = production_only(&std::fs::read_to_string(&path).unwrap());
            sites += SALT_CALLS
                .iter()
                .map(|c| src.matches(c).count())
                .sum::<usize>();
            found.extend(key_mentions_at_salt_calls(
                &path.display().to_string(),
                &src,
            ));
        }
        assert!(
            sites >= 8,
            "the scan found only {sites} call sites; it is looking in the wrong place"
        );
        assert!(found.is_empty(), "{found:#?}");
    }

    /// The scan above is not vacuous: a copy of a real call site, mutated to
    /// salt with the room's signing key, is caught, and so is a key in the
    /// module itself.
    #[test]
    fn the_signing_key_scan_catches_a_mutated_call_site() {
        let real =
            crate::util::strip_comments(include_str!("components/room_list/dm_rail_section.rs"));
        assert!(key_mentions_at_salt_calls("dm_rail_section.rs", &real).is_empty());
        let call = "display_hue(&salt, &vk)";
        assert!(real.contains(call), "the fixture call site moved");
        let mutated = real.replacen(call, "display_hue(&rd.self_sk_bytes(), &vk)", 1);
        assert_eq!(key_mentions_at_salt_calls("mutated", &mutated).len(), 1);

        let in_statement = real.replacen(
            "crate::avatar_salt::ensure_avatar_salt(room.to_bytes());",
            "crate::avatar_salt::ensure_avatar_salt(rd.self_sk.as_ref().unwrap().to_bytes());",
            1,
        );
        assert_ne!(in_statement, real, "the fixture ensure call moved");
        assert!(!key_mentions_at_salt_calls("in_statement", &in_statement).is_empty());

        let module = crate::util::source_scan::production_only(include_str!("avatar_salt.rs"))
            + "fn derive(k: &SigningKey) {}";
        assert!(KEY_WORDS.iter().any(|w| module.contains(w)));
    }
}
