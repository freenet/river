use crate::components::app::{CURRENT_ROOM, ROOMS, WEB_API};
use crate::room_data::{RoomData, RoomSlot, RoomsMeta};
use dioxus::logger::tracing::{debug, error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use freenet_scaffold::ComposableState;
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::prelude::{
    CodeHash, Delegate, DelegateCode, DelegateContainer, DelegateKey, DelegateWasmAPIVersion,
    Parameters,
};
use futures::channel::oneshot;
use futures::future::{select, Either};
use river_core::chat_delegate::{
    CasStoreResult, ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg,
    HiddenDmThreadEntry, OutboundDmEntry, OutboundDmStore, RequestId, RoomKey,
};
use river_core::room_state::direct_messages::{PurgeToken, MAX_DM_MESSAGES_PER_PAIR};
use river_core::room_state::member::MemberId;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

// Legacy single-blob rooms key. Still read for the one-time client-side
// migration into per-room keys (and for legacy-delegate migration); no longer
// written. Per-room storage uses `room:<base58(owner_vk)>` keys + `rooms_meta`
// (freenet/river#345 / #65).
pub const ROOMS_STORAGE_KEY: &[u8] = b"rooms_data";

/// Delegate key holding the list-level [`RoomsMeta`] (current room, per-room
/// notification prefs, room order). Membership/tombstones live in the per-room
/// `room:<vk>` keys.
pub const ROOMS_META_KEY: &[u8] = b"rooms_meta";

/// Prefix for per-room delegate keys: `room:<base58(owner_vk)>`. Base58, NOT
/// raw bytes — the delegate's `create_origin_key` is utf8-lossy, so a raw
/// 32-byte VK would be corrupted.
pub const ROOM_KEY_PREFIX: &str = "room:";

/// Build the per-room delegate key for a room owner verifying key.
pub fn room_storage_key(owner_vk: &ed25519_dalek::VerifyingKey) -> Vec<u8> {
    format!(
        "{ROOM_KEY_PREFIX}{}",
        bs58::encode(owner_vk.to_bytes()).into_string()
    )
    .into_bytes()
}

/// Parse a per-room delegate key back into its owner verifying key, or `None`
/// if `key` isn't a well-formed `room:<base58(vk)>` key.
pub fn parse_room_storage_key(key: &[u8]) -> Option<ed25519_dalek::VerifyingKey> {
    let s = std::str::from_utf8(key).ok()?;
    let b58 = s.strip_prefix(ROOM_KEY_PREFIX)?;
    let bytes = bs58::decode(b58).into_vec().ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    ed25519_dalek::VerifyingKey::from_bytes(&arr).ok()
}

// Re-export so other UI modules don't have to reach into `river_core` for the key.
pub use river_core::chat_delegate::OUTBOUND_DMS_STORAGE_KEY;

/// Cached `DelegateKey` for the chat delegate.
///
/// The delegate WASM is `include_bytes!`-bundled, so the bytes are
/// `'static` and the derived key is invariant for the entire session.
/// Computing it requires allocating the 710 KB WASM as a `Vec<u8>` and
/// BLAKE3-hashing it. Previously every `send_delegate_request` (and a
/// few other paths) repeated that work per call — on cold open with N
/// rooms that adds up to tens of MB of pure waste in the burst window
/// (freenet/river#246). Computing it once via `LazyLock` removes that
/// contribution entirely.
static CHAT_DELEGATE_KEY: LazyLock<DelegateKey> = LazyLock::new(|| {
    let bytes = include_bytes!("../../../public/contracts/chat_delegate.wasm");
    let delegate_code = DelegateCode::from(bytes.to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    delegate.key().clone()
});

/// Bundled coalesce-state for one save path. The three correlated
/// pieces (mutex, dirty flag, last-result store) must agree on which
/// path they belong to — packing them into a struct makes mismatches
/// across call sites impossible at the type level (skeptical-review
/// M3 on PR #311). Each save path declares one `static` of this type
/// and hands a `&CoalesceState` to [`coalesce_save`].
struct CoalesceState {
    /// Serializes the critical section so at most one save runs at a
    /// time. `futures::lock::Mutex` (async-aware) is held across the
    /// entire dirty-check loop, which closes the TOCTOU window the
    /// earlier `IN_FLIGHT` + `DIRTY` atomic-pair shape had on PR #259:
    /// in that window a concurrent caller could see `IN_FLIGHT==true`,
    /// set `DIRTY=true`, and return, while the in-flight save released
    /// `IN_FLIGHT` without re-checking `DIRTY` — stranding the dirty
    /// update with no save running. The mutex+dirty-flag pattern this
    /// struct embodies has no such window.
    mutex: futures::lock::Mutex<()>,
    /// "There is unsaved state." Set by every caller before queueing,
    /// drained by the in-flight save's loop. Mutex + dirty flag is
    /// the proven coalesce shape this primitive extracts.
    dirty: AtomicBool,
    /// Most recent save iteration's result. Read by callers whose own
    /// loop runs zero iterations (because another caller's catch-up
    /// drained `dirty` between our store and our mutex acquisition) —
    /// see the `last_result` paragraph on [`coalesce_save`].
    last_result: Mutex<Result<(), String>>,
}

impl CoalesceState {
    /// `const`-callable so the struct can live in a `static`. Verified
    /// const-callable: `futures::lock::Mutex::new`, `AtomicBool::new`,
    /// and `std::sync::Mutex::new` are all `const` (the last one since
    /// Rust 1.63).
    const fn new() -> Self {
        Self {
            mutex: futures::lock::Mutex::new(()),
            dirty: AtomicBool::new(false),
            last_result: Mutex::new(Ok(())),
        }
    }
}

/// Generic coalescing primitive for the fire-and-forget delegate save
/// paths. Mirrors the `save_outbound_dms_to_delegate` rationale: many
/// callers `spawn_local(save_*_to_delegate())` during burst events
/// (cold-start state ingestion, catch-up deltas, etc.), and without
/// coalescing each one re-clones / re-serializes its full payload,
/// allocating MB-scale chunks per call — exactly the symptom that
/// produced the 100-400 MB/s open-time burst Ivvor reported in
/// freenet/river#246.
///
/// Mark `dirty` first (so a caller that lands *after* we swap below
/// re-triggers the loop), take the mutex to serialize, then drain
/// `dirty` in a loop. A chain of N rapid calls produces at most 2
/// actual saves (the in-flight one, plus one final catch-up that
/// observes `dirty == true` after the in-flight save returned).
///
/// **`do_save` contract — snapshot inside, not before:** the `do_save`
/// closure MUST snapshot the shared state it's persisting from inside
/// its own body, not via captured-closure state. The loop calls
/// `do_save()` multiple times; if the snapshot were taken before the
/// call and reused, the catch-up save would write a stale snapshot and
/// defeat the entire coalescing rationale (re-introducing the burst).
/// See `do_save_rooms_to_delegate` for the canonical shape.
///
/// **Memory ordering** is consciously written for portability: the
/// `Release` store + `AcqRel` swap synchronizes via the dirty flag, and
/// `futures::lock::Mutex::lock().await` adds a task-level happens-before
/// edge. On the default `wasm32-unknown-unknown` target (single-threaded,
/// no `+atomics` feature) LLVM lowers the orderings to plain loads and
/// stores, so they cost nothing — the mutex's task serialization carries
/// correctness on its own. The orderings exist so the helper would also
/// be correct on multi-threaded targets (`wasm32` with `+atomics`, or
/// any future native build).
///
/// **`last_result` — propagate failures to queued callers.** When a
/// queued caller (whose `dirty.store(true)` ran before another
/// caller's loop drained the flag) acquires the mutex it may find
/// `dirty == false` and run zero iterations. Without consulting
/// `last_result` such a caller would return the synthetic `Ok(())`
/// initialized at the top, even if the catch-up save that covered its
/// mutation actually failed. That false-`Ok` would silently strand
/// callers that gate on success (e.g. the legacy-migration
/// `mark_legacy_migration_done` branch in `response_handler.rs`). The
/// store is updated after every save iteration; a zero-iteration
/// caller returns the most recent stored result instead.
///
/// Over-pessimism is possible but safe: if a more recent unrelated
/// save has since failed, a zero-iteration caller reads that `Err`
/// even though its own mutation may have been covered by an earlier
/// successful save. Downstream consumers gated on `Ok` (the
/// legacy-migration path) handle this conservatively — they retry on
/// next startup. False `Err` is strictly better than false `Ok`.
///
/// **Poisoning recovery.** `state.last_result` is `std::sync::Mutex`.
/// On the default `panic = abort` release WASM profile poisoning is
/// unreachable. On `panic = unwind` (used by `cargo test` and dev
/// builds) a panic in a `Mutex`-holding closure would otherwise
/// permanently poison it; we recover via `PoisonError::into_inner` at
/// both lock sites so the bug-fix this helper exists for doesn't
/// silently regress in non-release profiles.
async fn coalesce_save<F, Fut>(
    state: &CoalesceState,
    label: &'static str,
    do_save: F,
) -> Result<(), String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    state.dirty.store(true, Ordering::Release);
    let _guard = state.mutex.lock().await;

    let mut ran_any = false;
    let mut local_result: Result<(), String> = Ok(());
    while state.dirty.swap(false, Ordering::AcqRel) {
        let result = do_save().await;
        if let Err(e) = &result {
            // The error is surfaced to the caller below. The loop re-runs only
            // if another caller has since re-marked `dirty` (a newer snapshot
            // is pending) — it does NOT auto-retry this same failed snapshot.
            warn!("{} save failed mid-coalesce: {}", label, e);
        }
        // Publish this iteration's result so any caller whose own loop
        // observes dirty=false (drained by us) returns the real outcome
        // instead of a synthetic Ok(()). Recover from a (test-profile)
        // poisoned mutex so the publish doesn't silently no-op and
        // re-introduce the false-Ok bug.
        let mut slot = match state.last_result.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *slot = result.clone();
        drop(slot);
        local_result = result;
        ran_any = true;
    }
    if ran_any {
        local_result
    } else {
        // Loop ran zero iterations: a concurrent caller's save drained
        // dirty AFTER our `dirty.store(true)` at entry but BEFORE we
        // acquired the mutex. That save's snapshot included our
        // mutation (it was visible by then), so its result is the
        // authoritative outcome for our caller too.
        match state.last_result.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }
}

// =============================================================================
// LEGACY DELEGATE MIGRATION
// When the delegate WASM changes (dependency updates, code changes), the delegate
// key changes and old secrets become inaccessible. This migration code attempts
// to load room data from known previous delegate keys and migrate it to the
// current delegate.
//
// Legacy entries are defined in legacy_delegates.toml at the repo root.
// The build.rs generates the const array at compile time.
// To add a new entry: cargo make add-migration
// =============================================================================

/// Previous delegate keys for migration, generated from legacy_delegates.toml.
mod generated_legacy {
    include!(concat!(env!("OUT_DIR"), "/legacy_delegates.rs"));
}
use generated_legacy::LEGACY_DELEGATES;

/// Check if a delegate key matches any known legacy delegate
pub fn is_legacy_delegate_key(key_bytes: &[u8]) -> bool {
    LEGACY_DELEGATES
        .iter()
        .any(|(dk, _)| dk.as_slice() == key_bytes)
}

// Prefixes for different pending request types
const SIGNING_KEY_PREFIX: &[u8] = b"__signing_key:";
const PUBLIC_KEY_PREFIX: &[u8] = b"__public_key:";
const SIGN_PREFIX: &[u8] = b"__sign:";
/// Tracking-key prefix for an in-flight `EnsureRoomSubscription` request.
/// Must match the bytes built in `get_request_key()` so `send_delegate_request`
/// and the response router agree on the lookup key.
const ROOM_SUBSCRIPTION_PREFIX: &[u8] = b"__room_subscription:";

/// Atomic counter for generating unique request IDs
static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a unique request ID for signing requests
pub fn generate_request_id() -> RequestId {
    REQUEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Registry for pending delegate requests.
/// Maps request keys to oneshot senders that will receive the response.
static PENDING_REQUESTS: std::sync::LazyLock<
    Mutex<HashMap<Vec<u8>, oneshot::Sender<ChatDelegateResponseMsg>>>,
> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Complete a pending delegate request with the given response.
/// Called by the response handler when a delegate response is received.
pub fn complete_pending_request(key: &ChatDelegateKey, response: ChatDelegateResponseMsg) -> bool {
    let key_bytes = key.as_bytes().to_vec();
    complete_pending_request_bytes(&key_bytes, response)
}

/// Complete a pending signing key store request.
pub fn complete_pending_signing_key_request(
    room_key: &RoomKey,
    response: ChatDelegateResponseMsg,
) -> bool {
    let mut key_bytes = SIGNING_KEY_PREFIX.to_vec();
    key_bytes.extend_from_slice(room_key);
    complete_pending_request_bytes(&key_bytes, response)
}

/// Complete a pending public key request.
pub fn complete_pending_public_key_request(
    room_key: &RoomKey,
    response: ChatDelegateResponseMsg,
) -> bool {
    let mut key_bytes = PUBLIC_KEY_PREFIX.to_vec();
    key_bytes.extend_from_slice(room_key);
    complete_pending_request_bytes(&key_bytes, response)
}

/// Complete a pending signing request using room_key and request_id for correlation.
pub fn complete_pending_sign_request(
    room_key: &RoomKey,
    request_id: RequestId,
    response: ChatDelegateResponseMsg,
) -> bool {
    let mut key_bytes = SIGN_PREFIX.to_vec();
    key_bytes.extend_from_slice(room_key);
    key_bytes.extend_from_slice(&request_id.to_le_bytes());
    complete_pending_request_bytes(&key_bytes, response)
}

/// Complete a pending `EnsureRoomSubscription` request.
///
/// Routed from the response handler so callers awaiting the delegate's
/// acknowledgement actually receive it. Without this routing the response is
/// silently dropped (the pending-request entry would leak and the awaited
/// future would never resolve), which is exactly why the load-time
/// EnsureRoomSubscription path was previously fire-and-forget — and which
/// caused Bug #6 (owner's delegate not back-filling encrypted_secrets for
/// newly-invited members after a delegate-WASM migration race).
///
/// The lookup key includes `request_id` so concurrent or sequential calls
/// for the same `room_owner_vk` cannot collide on the same registry slot.
/// Without the request_id mix-in, a 10-second timeout on an earlier call
/// could clear that call's pending entry, a fresh second call could install
/// its sender into the same slot, and a late-arriving response for the
/// first call would resolve the second caller with the wrong epoch's ACK —
/// see PR #276 review feedback.
pub fn complete_pending_room_subscription_request(
    room_owner_vk: &RoomKey,
    request_id: RequestId,
    response: ChatDelegateResponseMsg,
) -> bool {
    let mut key_bytes = ROOM_SUBSCRIPTION_PREFIX.to_vec();
    key_bytes.extend_from_slice(room_owner_vk);
    key_bytes.extend_from_slice(&request_id.to_le_bytes());
    complete_pending_request_bytes(&key_bytes, response)
}

/// Internal function to complete a pending request by key bytes.
fn complete_pending_request_bytes(key_bytes: &[u8], response: ChatDelegateResponseMsg) -> bool {
    if let Ok(mut pending) = PENDING_REQUESTS.lock() {
        if let Some(sender) = pending.remove(key_bytes) {
            if sender.send(response).is_ok() {
                info!(
                    "Completed pending request for key: {:?}",
                    String::from_utf8_lossy(key_bytes)
                );
                return true;
            }
        }
    }
    false
}

pub async fn set_up_chat_delegate() -> Result<(), String> {
    let delegate = create_chat_delegate_container();

    // Get a write lock on the API and use it directly
    let api_result = {
        let mut web_api = WEB_API.write();
        if let Some(api) = web_api.as_mut() {
            // Perform the operation while holding the lock
            info!("Registering chat delegate");
            api.send(DelegateOp(DelegateRequest::RegisterDelegate {
                delegate,
                cipher: DelegateRequest::DEFAULT_CIPHER,
                nonce: DelegateRequest::DEFAULT_NONCE,
            }))
            .await
        } else {
            Err(freenet_stdlib::client_api::Error::ConnectionClosed)
        }
    };

    match api_result {
        Ok(_) => {
            info!("Chat delegate registered successfully");
            // NOTE: We don't await load_rooms_from_delegate() here because it would
            // deadlock - it waits for a response that comes through the same message
            // loop that called us. Instead, we fire off the request and let the
            // response be handled by the response_handler through the message loop.
            //
            // The response handler will process GetResponse and populate ROOMS.
            //
            // Legacy migration is NOT fired here. It is gated on the current
            // delegate's response: if the current delegate has data, migration is
            // skipped (current is authoritative); if it is empty, migration is
            // fired then. This prevents legacy responses from racing with the
            // current delegate and clobbering newer state (freenet/river#253).
            fire_list_rooms_request().await;
            fire_load_outbound_dms_request().await;

            Ok(())
        }
        Err(e) => Err(format!("Failed to register chat delegate: {}", e)),
    }
}

/// Per-session dedup set for `EnsureRoomSubscription` calls. The UI may
/// re-fire its load-rooms path on every `rooms_data` reload, but we only
/// need to ask the delegate to (re-)subscribe each room once per session —
/// the delegate's secret store keeps the sub_index across `process()`
/// invocations.
static ENSURE_SUBSCRIPTION_SENT: std::sync::LazyLock<Mutex<HashSet<RoomKey>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// Reset the per-session dedup set. Used in tests; not called in production.
#[cfg(test)]
pub(crate) fn reset_ensure_subscription_dedup() {
    if let Ok(mut s) = ENSURE_SUBSCRIPTION_SENT.lock() {
        s.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// freenet/river#345 round-9 (pure decision): a `Present` save overwrites a
    /// remote tombstone ONLY when the user explicitly rejoined this session; a
    /// background update to a room left in another tab adopts the leave instead
    /// of resurrecting it. This is what per-room keys + this rule give us that
    /// the single-blob design could not.
    #[test]
    fn present_action_resolves_rejoin_vs_remote_leave() {
        assert_eq!(
            present_action(SlotKind::Absent, false),
            PresentAction::StoreFresh
        );
        assert_eq!(
            present_action(SlotKind::Absent, true),
            PresentAction::StoreFresh
        );
        assert_eq!(
            present_action(SlotKind::Present, false),
            PresentAction::MergeState
        );
        assert_eq!(
            present_action(SlotKind::Present, true),
            PresentAction::MergeState
        );
        // The round-9 crux:
        assert_eq!(
            present_action(SlotKind::Tombstone, false),
            PresentAction::AbortAdoptLeave,
            "a background update must NOT resurrect a room left in another tab"
        );
        assert_eq!(
            present_action(SlotKind::Tombstone, true),
            PresentAction::StoreFresh,
            "an explicit rejoin overwrites the tombstone"
        );
    }

    fn empty_rooms() -> crate::room_data::Rooms {
        use std::collections::{HashMap, HashSet};
        crate::room_data::Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: HashSet::new(),
            notification_modes: HashMap::new(),
            room_order: Vec::new(),
            migrated_rooms: Vec::new(),
        }
    }

    /// `cas_write_key` stores the reconciled bytes at the generation just read.
    #[test]
    fn cas_write_key_stores_reconciled_value() {
        let stored = std::cell::RefCell::new(None);
        futures::executor::block_on(cas_write_key(
            |_current| Ok(Some(b"v1".to_vec())),
            || async { Ok::<_, String>((None, 0u64)) },
            |value, expected| {
                assert_eq!(expected, 0, "stores at the generation just read");
                *stored.borrow_mut() = Some(value);
                async move { Ok(CasStoreResult::Stored { generation: 1 }) }
            },
        ))
        .unwrap_or_else(|e| panic!("cas_write_key failed: {e}"));
        assert_eq!(*stored.borrow(), Some(b"v1".to_vec()));
    }

    /// `reconcile` returning `None` aborts the write — nothing is stored. This
    /// is how a room save adopts a remote leave instead of resurrecting it.
    #[test]
    fn cas_write_key_abort_does_not_store() {
        let stores = std::cell::RefCell::new(0u32);
        futures::executor::block_on(cas_write_key(
            |_current| Ok(None),
            || async { Ok::<_, String>((Some(b"remote".to_vec()), 5u64)) },
            |_value, _expected| {
                *stores.borrow_mut() += 1;
                async move { Ok(CasStoreResult::Stored { generation: 6 }) }
            },
        ))
        .unwrap_or_else(|e| panic!("cas_write_key failed: {e}"));
        assert_eq!(*stores.borrow(), 0, "abort (None) must not store");
    }

    /// On a `Conflict` the key is RE-READ and `reconcile` re-runs against the
    /// fresh value/generation, storing at the new generation.
    #[test]
    fn cas_write_key_retries_on_conflict() {
        let gets = std::cell::RefCell::new(0u32);
        let reconciles = std::cell::RefCell::new(0u32);
        let get_gens = std::cell::RefCell::new(vec![7u64, 0u64]); // popped: 0 then 7
        let store_script = std::cell::RefCell::new(vec![
            CasStoreResult::Stored { generation: 8 },
            CasStoreResult::Conflict {
                current_generation: 7,
                current_value: None,
            },
        ]);
        let store_expected = std::cell::RefCell::new(Vec::new());

        futures::executor::block_on(cas_write_key(
            |_current| {
                *reconciles.borrow_mut() += 1;
                Ok(Some(b"v".to_vec()))
            },
            || {
                *gets.borrow_mut() += 1;
                let g = get_gens.borrow_mut().pop().expect("get gen");
                async move { Ok::<_, String>((None, g)) }
            },
            |_value, expected| {
                store_expected.borrow_mut().push(expected);
                let r = store_script.borrow_mut().pop().expect("store response");
                async move { Ok(r) }
            },
        ))
        .unwrap_or_else(|e| panic!("cas_write_key failed: {e}"));

        assert_eq!(*gets.borrow(), 2, "must re-read on conflict");
        assert_eq!(*reconciles.borrow(), 2, "reconcile re-runs on conflict");
        assert_eq!(
            *store_expected.borrow(),
            vec![0, 7],
            "the retry stores at the re-read generation"
        );
    }

    /// Exhaustion after `ROOMS_CAS_MAX_ATTEMPTS` consecutive conflicts errors.
    #[test]
    fn cas_write_key_exhaustion_errors() {
        let stores = std::cell::RefCell::new(0u32);
        let result = futures::executor::block_on(cas_write_key(
            |_current| Ok(Some(b"v".to_vec())),
            || async { Ok::<_, String>((None, 0u64)) },
            |_value, _expected| {
                *stores.borrow_mut() += 1;
                async move {
                    Ok(CasStoreResult::Conflict {
                        current_generation: 1,
                        current_value: None,
                    })
                }
            },
        ));
        assert!(result.is_err());
        assert_eq!(*stores.borrow(), ROOMS_CAS_MAX_ATTEMPTS);
    }

    /// A `Failed` store propagates immediately.
    #[test]
    fn cas_write_key_propagates_failed() {
        let result = futures::executor::block_on(cas_write_key(
            |_current| Ok(Some(b"v".to_vec())),
            || async { Ok::<_, String>((None, 0u64)) },
            |_value, _expected| async move {
                Ok(CasStoreResult::Failed("secret store error".to_string()))
            },
        ));
        match result {
            Err(e) => assert_eq!(e, "secret store error"),
            Ok(_) => panic!("expected the Failed result to propagate as an error"),
        }
    }

    /// `reconcile_room_tombstone`: absent → writes a Tombstone slot; already a
    /// Tombstone → no-op (None), so we don't churn.
    #[test]
    fn reconcile_room_tombstone_writes_then_noops() {
        let out = reconcile_room_tombstone(None)
            .unwrap()
            .expect("absent slot should store a tombstone");
        assert!(matches!(
            ciborium::from_reader::<RoomSlot, _>(out.as_slice()),
            Ok(RoomSlot::Tombstone)
        ));
        assert!(
            reconcile_room_tombstone(Some(&out)).unwrap().is_none(),
            "already-tombstoned slot must be a no-op"
        );
    }

    /// `reconcile_meta`: this tab's prefs win, but a remote notification-mode
    /// for a room we don't track is absorbed (not lost on a concurrent write).
    #[test]
    fn reconcile_meta_absorbs_remote_notification_modes() {
        use crate::room_data::NotificationMode;
        use ed25519_dalek::SigningKey;

        let vk_local = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
        let vk_remote = SigningKey::from_bytes(&[2u8; 32]).verifying_key();

        let mut local = RoomsMeta::default();
        local
            .notification_modes
            .insert(vk_local, NotificationMode::default());
        let mut remote = RoomsMeta::default();
        remote
            .notification_modes
            .insert(vk_remote, NotificationMode::default());
        let mut rb = Vec::new();
        ciborium::ser::into_writer(&remote, &mut rb).unwrap();

        let out = reconcile_meta(Some(&rb), &local)
            .unwrap()
            .expect("stores merged meta");
        let merged: RoomsMeta = ciborium::from_reader(out.as_slice()).unwrap();
        assert!(merged.notification_modes.contains_key(&vk_local));
        assert!(
            merged.notification_modes.contains_key(&vk_remote),
            "a concurrent tab's notification pref must not be lost"
        );
    }

    /// freenet/river#345 (M1): CAS/GetVersioned ops MUST register under a
    /// different pending-request key than a plain Get/Store for the same
    /// storage key, or a concurrent plain `GetResponse` (the cold-start load)
    /// could steal an in-flight CAS save's response slot. A regression that
    /// reverted to the bare key bytes would silently reintroduce slot-stealing
    /// and no other test would fail.
    #[test]
    fn cas_correlation_keys_diverge_from_plain_storage_keys() {
        let key = ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec());
        let plain_get = get_request_key(&ChatDelegateRequestMsg::GetRequest { key: key.clone() });
        let cas = get_request_key(&ChatDelegateRequestMsg::CasStoreRequest {
            key: key.clone(),
            value: vec![],
            expected_generation: 0,
        });
        let getver =
            get_request_key(&ChatDelegateRequestMsg::GetVersionedRequest { key: key.clone() });
        assert_ne!(
            cas, plain_get,
            "CAS store must not share the plain Get slot"
        );
        assert_ne!(
            getver, plain_get,
            "GetVersioned must not share the plain Get slot"
        );
        assert_ne!(cas, getver, "CAS and GetVersioned must use distinct slots");
    }

    /// The request side (`get_request_key`) must produce the EXACT bytes the
    /// response side rebuilds from the echoed key, or responses can't correlate
    /// and the awaiting save hangs. Pins both sides to the shared helpers.
    #[test]
    fn cas_correlation_keys_round_trip_request_to_response() {
        let key = ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec());
        assert_eq!(
            get_request_key(&ChatDelegateRequestMsg::CasStoreRequest {
                key: key.clone(),
                value: vec![1, 2, 3],
                expected_generation: 9,
            }),
            cas_store_correlation_key(&key),
        );
        assert_eq!(
            get_request_key(&ChatDelegateRequestMsg::GetVersionedRequest { key: key.clone() }),
            get_versioned_correlation_key(&key),
        );
    }

    /// Pins the delegate's envelope-tag invariant from the client side: a real
    /// CBOR `Rooms` blob is a map (major type 5, first byte `0xA0..=0xBF`) and
    /// never starts with the delegate's `ENVELOPE_TAG` (`0x01`), so the
    /// defensive raw-vs-enveloped decode can never misread stored rooms.
    #[test]
    fn rooms_cbor_never_collides_with_envelope_tag() {
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&empty_rooms(), &mut bytes).unwrap();
        assert!(!bytes.is_empty());
        assert_ne!(
            bytes[0], 0x01,
            "Rooms CBOR must not start with ENVELOPE_TAG"
        );
        assert!(
            (0xA0..=0xBF).contains(&bytes[0]),
            "Rooms serializes as a CBOR map (got first byte {:#x})",
            bytes[0]
        );
    }

    /// Current delegate returned no `rooms_data` value at all — fire migration.
    #[test]
    fn decide_value_absent_fires_migration() {
        assert_eq!(
            decide_legacy_migration_action(false, false, false),
            LegacyMigrationAction::FireMigration,
        );
    }

    /// Current delegate returned `Some(serialized empty Rooms)` (placeholder
    /// written by save_rooms_to_delegate before any room existed). This is
    /// indistinguishable from "user has no data yet" — fire migration so users
    /// with rooms only under a legacy delegate aren't stranded.
    #[test]
    fn decide_value_present_but_empty_fires_migration() {
        assert_eq!(
            decide_legacy_migration_action(true, false, false),
            LegacyMigrationAction::FireMigration,
        );
    }

    /// Current delegate has actual rooms — it is the source of truth.
    /// Block legacy migration permanently to prevent the freenet/river#253
    /// race where stale legacy data overwrites it.
    #[test]
    fn decide_has_rooms_marks_done() {
        assert_eq!(
            decide_legacy_migration_action(true, true, false),
            LegacyMigrationAction::MarkDone,
        );
    }

    /// Current delegate has only tombstones (user left all rooms). Still
    /// authoritative state — the tombstones must be preserved, and a legacy
    /// migration that re-introduces the abandoned rooms would defeat the
    /// freenet/river#247 tombstone fix.
    #[test]
    fn decide_has_tombstones_only_marks_done() {
        assert_eq!(
            decide_legacy_migration_action(true, false, true),
            LegacyMigrationAction::MarkDone,
        );
    }

    /// Both rooms and tombstones present — definitely authoritative.
    #[test]
    fn decide_has_rooms_and_tombstones_marks_done() {
        assert_eq!(
            decide_legacy_migration_action(true, true, true),
            LegacyMigrationAction::MarkDone,
        );
    }

    /// Codex P1 finding on PR #259: the legacy-migration-done
    /// localStorage flag MUST be scoped to the current
    /// `LEGACY_DELEGATES` set, otherwise every WASM bump that adds
    /// a new legacy entry is silently blocked for any user who
    /// already migrated under the previous set. Pinning behaviour:
    /// the fingerprint must be a non-empty hex string and the full
    /// key must carry the prefix so the storage namespace stays
    /// recognizable. If `LEGACY_DELEGATES` ever changes shape, the
    /// fingerprint will change too — which is exactly the property
    /// this whole scheme depends on.
    #[test]
    fn legacy_migration_flag_is_scoped_to_set() {
        let fp = legacy_set_fingerprint();
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        let key = legacy_migration_flag_key();
        assert!(key.starts_with(LEGACY_MIGRATION_FLAG_PREFIX));
        assert!(key.ends_with(&fp));
    }

    // ===== resolve_hidden_thread_hydration tests (#261 Codex P3) =====
    use freenet_scaffold::util::FastHash;

    fn sk(seed: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
    }

    fn make_hidden_entry(
        room_vk: ed25519_dalek::VerifyingKey,
        peer: MemberId,
        ts: u64,
    ) -> HiddenDmThreadEntry {
        HiddenDmThreadEntry {
            room_owner_vk: room_vk.to_bytes(),
            peer,
            hidden_at_ts: ts,
        }
    }

    /// Baseline: with no current entries and no suppression, every
    /// incoming entry is inserted.
    #[test]
    fn resolve_hidden_thread_hydration_inserts_into_empty() {
        let room_vk = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let incoming = vec![make_hidden_entry(room_vk, peer, 1_000)];
        let current = HashMap::new();
        let suppressed = HashSet::new();

        let (to_insert, diagnostics) =
            resolve_hidden_thread_hydration(incoming, &current, &suppressed);
        assert_eq!(to_insert.len(), 1);
        assert!(diagnostics.is_empty());
        assert_eq!(to_insert[0].0, (room_vk, peer));
        assert_eq!(to_insert[0].1.hidden_at_ts, 1_000);
    }

    /// Conflict resolution: in-memory entry's `hidden_at_ts >=`
    /// incoming → keep in-memory (no insert returned).
    #[test]
    fn resolve_hidden_thread_hydration_keeps_fresher_in_memory_entry() {
        let room_vk = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let incoming = vec![make_hidden_entry(room_vk, peer, 500)];
        let mut current = HashMap::new();
        current.insert((room_vk, peer), make_hidden_entry(room_vk, peer, 1_000));
        let suppressed = HashSet::new();

        let (to_insert, _) = resolve_hidden_thread_hydration(incoming, &current, &suppressed);
        assert!(
            to_insert.is_empty(),
            "fresher in-memory entry must NOT be overwritten by older incoming"
        );
    }

    /// Conflict resolution: incoming `hidden_at_ts >` in-memory →
    /// overwrite. Pins the "most recent hide wins" semantics.
    #[test]
    fn resolve_hidden_thread_hydration_takes_fresher_incoming_entry() {
        let room_vk = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let incoming = vec![make_hidden_entry(room_vk, peer, 2_000)];
        let mut current = HashMap::new();
        current.insert((room_vk, peer), make_hidden_entry(room_vk, peer, 1_000));
        let suppressed = HashSet::new();

        let (to_insert, _) = resolve_hidden_thread_hydration(incoming, &current, &suppressed);
        assert_eq!(to_insert.len(), 1);
        assert_eq!(to_insert[0].1.hidden_at_ts, 2_000);
    }

    /// Codex P3 regression: a `(room, peer)` in the suppression set
    /// MUST drop the incoming hide, even if it would otherwise
    /// overwrite a stale in-memory entry. This is what closes the
    /// "unhide racing late-hydration" window.
    #[test]
    fn resolve_hidden_thread_hydration_suppresses_recently_unhidden() {
        let room_vk = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let incoming = vec![make_hidden_entry(room_vk, peer, 5_000)];
        let current = HashMap::new();
        let mut suppressed = HashSet::new();
        suppressed.insert((room_vk, peer));

        let (to_insert, diagnostics) =
            resolve_hidden_thread_hydration(incoming, &current, &suppressed);
        assert!(
            to_insert.is_empty(),
            "suppressed pair must not be re-inserted by hydration"
        );
        assert!(diagnostics.is_empty());
    }

    /// Suppression is scoped per `(room, peer)`: an entry for a
    /// DIFFERENT peer in the same room must still be applied.
    #[test]
    fn resolve_hidden_thread_hydration_suppression_does_not_leak_across_peers() {
        let room_vk = sk(1).verifying_key();
        let suppressed_peer = MemberId(FastHash(11));
        let other_peer = MemberId(FastHash(22));
        let incoming = vec![
            make_hidden_entry(room_vk, suppressed_peer, 5_000),
            make_hidden_entry(room_vk, other_peer, 5_000),
        ];
        let current = HashMap::new();
        let mut suppressed = HashSet::new();
        suppressed.insert((room_vk, suppressed_peer));

        let (to_insert, _) = resolve_hidden_thread_hydration(incoming, &current, &suppressed);
        assert_eq!(to_insert.len(), 1);
        assert_eq!(to_insert[0].0, (room_vk, other_peer));
    }

    // Note: a unit test for the "invalid room VK" defensive branch
    // was attempted but `ed25519_dalek::VerifyingKey::from_bytes` does
    // not validate canonical encoding at decode time (validation
    // happens only when a signature is verified against the key), so
    // there is no easy way to synthesize a `[u8; 32]` value the
    // decoder rejects. The branch is defensive code against
    // genuinely-corrupt delegate data; integration testing against a
    // truncated delegate blob would be the right coverage if this
    // path matters.

    /// Codex round-3 Low: duplicate `(room, peer)` entries in the same
    /// incoming `Vec` must resolve to the entry with the largest
    /// `hidden_at_ts`. Without internal dedup, a newer-then-older
    /// ordering would let the caller's eventual `HashMap::insert` loop
    /// overwrite the newer cutoff with the older one (because both
    /// would be emitted into `to_insert`).
    #[test]
    fn resolve_hidden_thread_hydration_dedupes_duplicate_incoming() {
        let room_vk = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        // Same pair appears three times with varying timestamps.
        let incoming = vec![
            make_hidden_entry(room_vk, peer, 500),
            make_hidden_entry(room_vk, peer, 2_000),
            make_hidden_entry(room_vk, peer, 1_000),
        ];
        let current = HashMap::new();
        let suppressed = HashSet::new();

        let (to_insert, _) = resolve_hidden_thread_hydration(incoming, &current, &suppressed);
        assert_eq!(
            to_insert.len(),
            1,
            "duplicate (room, peer) entries must collapse to one"
        );
        assert_eq!(
            to_insert[0].1.hidden_at_ts, 2_000,
            "the largest hidden_at_ts must win"
        );
    }

    /// Reverse ordering of the above: the largest cutoff arriving
    /// last must still win (and identical-cutoff entries must not
    /// produce a duplicate emit).
    #[test]
    fn resolve_hidden_thread_hydration_dedup_handles_reverse_and_ties() {
        let room_vk = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let incoming = vec![
            make_hidden_entry(room_vk, peer, 5_000),
            make_hidden_entry(room_vk, peer, 5_000), // tie
            make_hidden_entry(room_vk, peer, 100),   // older, must not overwrite
        ];
        let current = HashMap::new();
        let suppressed = HashSet::new();

        let (to_insert, _) = resolve_hidden_thread_hydration(incoming, &current, &suppressed);
        assert_eq!(to_insert.len(), 1);
        assert_eq!(to_insert[0].1.hidden_at_ts, 5_000);
    }

    // ===== decide_hide_action + hide-unhide-rehide round-trip =====
    // Tests for the testing-reviewer's BLOCKING gap on PR #265: the
    // "click Hide again should re-hide, not no-op" path through
    // `hide_dm_thread` needs explicit regression coverage. We extract
    // the decision logic into `decide_hide_action` and test it
    // directly, then simulate the full hide → revive → re-hide
    // round-trip against a `HashMap` exactly the way `hide_dm_thread`
    // does at runtime.

    /// Baseline: no existing entry → `Insert` the incoming entry.
    #[test]
    fn decide_hide_action_no_existing_inserts() {
        let room = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let incoming = make_hidden_entry(room, peer, 1_000);
        match decide_hide_action(None, incoming.clone()) {
            HideAction::Insert(e) => assert_eq!(e, incoming),
            other => panic!("expected Insert, got {:?}", other),
        }
    }

    /// Existing entry's `hidden_at_ts == incoming` → `NoOp`. Avoids
    /// churning the delegate blob for an unchanged value.
    #[test]
    fn decide_hide_action_equal_existing_is_noop() {
        let room = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let existing = make_hidden_entry(room, peer, 1_000);
        let incoming = make_hidden_entry(room, peer, 1_000);
        assert_eq!(
            decide_hide_action(Some(&existing), incoming),
            HideAction::NoOp,
        );
    }

    /// Existing entry's `hidden_at_ts > incoming` → `NoOp`. The
    /// already-hidden state is at least as restrictive.
    #[test]
    fn decide_hide_action_newer_existing_is_noop() {
        let room = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let existing = make_hidden_entry(room, peer, 2_000);
        let incoming = make_hidden_entry(room, peer, 1_000);
        assert_eq!(
            decide_hide_action(Some(&existing), incoming),
            HideAction::NoOp,
        );
    }

    /// Existing entry's `hidden_at_ts < incoming` → `Insert` with the
    /// NEWER cutoff. This is the load-bearing branch the testing
    /// reviewer flagged: without it, a re-hide after a revive
    /// silently no-ops.
    #[test]
    fn decide_hide_action_older_existing_advances_cutoff() {
        let room = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let existing = make_hidden_entry(room, peer, 1_000);
        let incoming = make_hidden_entry(room, peer, 1_500);
        match decide_hide_action(Some(&existing), incoming.clone()) {
            HideAction::Insert(e) => assert_eq!(
                e.hidden_at_ts, 1_500,
                "re-hide must advance cutoff to incoming value"
            ),
            other => panic!("expected Insert, got {:?}", other),
        }
    }

    /// Round-trip test (testing-reviewer BLOCKING #2 on PR #265).
    ///
    /// Sequence the user-visible bug would manifest as:
    /// 1. User clicks Hide on a thread whose last message was at ts=1000.
    ///    `hide_dm_thread` writes entry { hidden_at_ts: 1000 }.
    /// 2. A new message arrives at ts=1500 → rail filter
    ///    (`filter_rail_entries`) sees `last_any_ts=1500 > 1000` and
    ///    revives the thread.
    /// 3. User clicks Hide AGAIN. `hide_dm_thread` is called with
    ///    `hidden_at_ts=1500`. The decision must `Insert(1500)` —
    ///    NOT `NoOp` — so the next render hides the thread.
    ///
    /// We simulate the HashMap mutation that `hide_dm_thread` does
    /// inside `HIDDEN_DM_THREADS.with_mut`, then call
    /// `filter_rail_entries` to confirm the rail-side observable
    /// state at each step.
    #[test]
    fn hide_unhide_rehide_round_trip() {
        use crate::components::direct_messages::is_thread_hidden_for;
        let room = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let key = (room, peer);

        // Step 1: hide at ts=1000.
        let mut hidden: HashMap<(ed25519_dalek::VerifyingKey, MemberId), HiddenDmThreadEntry> =
            HashMap::new();
        let incoming_1 = make_hidden_entry(room, peer, 1_000);
        let action_1 = decide_hide_action(hidden.get(&key), incoming_1.clone());
        match action_1 {
            HideAction::Insert(e) => {
                hidden.insert(key, e);
            }
            other => panic!("step 1 expected Insert, got {:?}", other),
        }
        // Confirm the thread is hidden when last_any_ts == cutoff.
        assert!(
            is_thread_hidden_for(&hidden, &room, peer, 1_000),
            "after step 1, thread at ts=1000 must be hidden"
        );

        // Step 2: message arrives at ts=1500. Filter view revives —
        // the map is unchanged, but the rail observable flips.
        assert!(
            !is_thread_hidden_for(&hidden, &room, peer, 1_500),
            "after step 2, thread at ts=1500 must revive (strict `>`)"
        );

        // Step 3: user clicks Hide AGAIN with the now-latest ts=1500.
        // This is the regression-pin: decide_hide_action MUST return
        // Insert (not NoOp), because existing.hidden_at_ts (1000) is
        // strictly less than incoming.hidden_at_ts (1500).
        let incoming_2 = make_hidden_entry(room, peer, 1_500);
        let action_2 = decide_hide_action(hidden.get(&key), incoming_2.clone());
        match action_2 {
            HideAction::Insert(e) => {
                assert_eq!(
                    e.hidden_at_ts, 1_500,
                    "re-hide must advance the cutoff to 1500"
                );
                hidden.insert(key, e);
            }
            HideAction::NoOp => panic!(
                "REGRESSION: re-hide after revival no-op'd — cutoff stuck at 1000, \
                 next render would surface the thread again"
            ),
        }

        // Final: thread must be hidden at ts=1500 (the new cutoff).
        assert!(
            is_thread_hidden_for(&hidden, &room, peer, 1_500),
            "after step 3, thread at ts=1500 must be hidden under new cutoff"
        );
    }

    // ===== Outbound DM revives hidden thread (Codex P1 fix) =====
    // The testing-reviewer's BLOCKING #3 on PR #265. The "explicit
    // unhide on outbound" path lives in `dm_thread_modal::do_send` /
    // `unhide_dm_thread`. Its pure-function effect is "remove the
    // entry from the map." We expose that effect through a tiny pure
    // helper so the user-visible invariant — *after a successful
    // outbound send, the (room, peer) pair must NOT remain in
    // HIDDEN_DM_THREADS* — has a regression-pin.

    /// Simulate the in-memory effect of [`unhide_dm_thread`] (the
    /// `HIDDEN_DM_THREADS.with_mut(|h| h.remove(...))` line). Returns
    /// the map after the removal. Exists purely for unit-testing the
    /// Codex P1 invariant on PR #265.
    fn process_outbound_send_for_hidden(
        mut hidden: HashMap<(ed25519_dalek::VerifyingKey, MemberId), HiddenDmThreadEntry>,
        room: ed25519_dalek::VerifyingKey,
        peer: MemberId,
    ) -> HashMap<(ed25519_dalek::VerifyingKey, MemberId), HiddenDmThreadEntry> {
        hidden.remove(&(room, peer));
        hidden
    }

    /// Codex P1 invariant: starting from a hidden entry for
    /// `(room, peer)`, after the outbound-send code path runs, the
    /// entry MUST be gone. Closes the "both `unix_now()` calls land
    /// in the same second → re-hides the thread right after the user
    /// sent a message" race.
    #[test]
    fn outbound_send_clears_hidden_entry_for_pair() {
        let room = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let mut hidden = HashMap::new();
        hidden.insert((room, peer), make_hidden_entry(room, peer, 1_000));
        assert_eq!(hidden.len(), 1, "precondition: (room, peer) is hidden");

        let after = process_outbound_send_for_hidden(hidden, room, peer);
        assert!(
            after.is_empty(),
            "after outbound send, (room, peer) must be unhidden"
        );
        assert!(
            !after.contains_key(&(room, peer)),
            "explicit (room, peer) key must be absent post-send"
        );
    }

    /// Scope check on the outbound-revive path: an outbound send to
    /// peer X MUST NOT unhide a different peer Y in the same room
    /// (nor the same peer X in a different room — same-shape lookup
    /// tuple as `filter_rail_entries`'s scope test).
    #[test]
    fn outbound_send_unhide_is_scoped_per_room_and_peer() {
        let room_a = sk(1).verifying_key();
        let room_b = sk(2).verifying_key();
        let peer_x = MemberId(FastHash(11));
        let peer_y = MemberId(FastHash(22));

        let mut hidden = HashMap::new();
        hidden.insert((room_a, peer_x), make_hidden_entry(room_a, peer_x, 1_000));
        hidden.insert((room_a, peer_y), make_hidden_entry(room_a, peer_y, 1_000));
        hidden.insert((room_b, peer_x), make_hidden_entry(room_b, peer_x, 1_000));

        // User sends an outbound DM to peer_x in room_a.
        let after = process_outbound_send_for_hidden(hidden, room_a, peer_x);

        assert!(
            !after.contains_key(&(room_a, peer_x)),
            "outbound to (room_a, peer_x) must unhide that pair"
        );
        assert!(
            after.contains_key(&(room_a, peer_y)),
            "outbound to peer_x must NOT affect peer_y in same room"
        );
        assert!(
            after.contains_key(&(room_b, peer_x)),
            "outbound to peer_x in room_a must NOT affect peer_x in room_b"
        );
    }

    /// Idempotency: calling the outbound-unhide on a pair that was
    /// not hidden is a no-op (matches `unhide_dm_thread`'s
    /// "idempotent: no-op when no entry exists" contract). Pins that
    /// `do_send`'s unconditional `unhide_dm_thread` call is safe
    /// regardless of whether the user previously hid the thread.
    #[test]
    fn outbound_send_unhide_is_idempotent() {
        let room = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        // Hidden map containing only an unrelated entry.
        let other_peer = MemberId(FastHash(22));
        let mut hidden = HashMap::new();
        hidden.insert(
            (room, other_peer),
            make_hidden_entry(room, other_peer, 1_000),
        );

        let after = process_outbound_send_for_hidden(hidden.clone(), room, peer);
        assert_eq!(
            after, hidden,
            "outbound unhide on a not-hidden pair must be a no-op"
        );
    }

    // ===== ENSURE_SUBSCRIPTION_SENT dedup-on-failure tests (Bug #6) =====
    //
    // Serializes the tests below that mutate the process-global
    // `ENSURE_SUBSCRIPTION_SENT` static. Each starts with
    // `reset_ensure_subscription_dedup()`, which clears the WHOLE set — so
    // without serialization a parallel test's reset wipes another test's
    // entries mid-run, a shared-mutable-state flake. `into_inner()` recovers
    // a poisoned lock so one failing test doesn't cascade-fail the rest.
    static DEDUP_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    //
    // These tests pin the contract that the per-session dedup must NOT
    // hold across a failed `EnsureRoomSubscription`. Before the Bug #6
    // fix, the dedup set was populated up-front and never cleared, so a
    // single transient failure (transport error, signing-key-not-on-file
    // race after a delegate-WASM migration) permanently disabled
    // delegate-side rotation for the affected room until the user
    // refreshed.
    //
    // The tests exercise `clear_ensure_subscription_sent` against the
    // module's `ENSURE_SUBSCRIPTION_SENT` static directly rather than
    // calling `ensure_room_subscription_once` end-to-end — the latter
    // depends on `WEB_API`, which requires WASM/browser plumbing to
    // instantiate. The dedup logic is the only piece that needed
    // changing for Bug #6 and is fully exercised here.

    /// Baseline: inserting a new VK returns true (lock guard's
    /// `insert` returns true on a fresh entry), and a second insert of
    /// the same VK in the same "session" returns false (dedup hit). This
    /// is the property `ensure_room_subscription_once` relies on for its
    /// `Ok(false)` no-op return.
    #[test]
    fn ensure_subscription_dedup_blocks_second_call() {
        let _dedup_guard = DEDUP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_ensure_subscription_dedup();
        let vk = sk(7).verifying_key().to_bytes();

        let inserted_first = {
            let mut sent = ENSURE_SUBSCRIPTION_SENT.lock().unwrap();
            sent.insert(vk)
        };
        let inserted_second = {
            let mut sent = ENSURE_SUBSCRIPTION_SENT.lock().unwrap();
            sent.insert(vk)
        };
        assert!(inserted_first, "first insert must succeed");
        assert!(!inserted_second, "second insert must be a dedup hit");
    }

    /// Bug #6 regression: after `clear_ensure_subscription_sent` runs,
    /// a subsequent insert MUST succeed. Without this property the
    /// owner's delegate stayed permanently unsubscribed on every cold
    /// load that lost the signing-key/EnsureRoomSubscription race.
    #[test]
    fn ensure_subscription_dedup_cleared_after_failure_allows_retry() {
        let _dedup_guard = DEDUP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_ensure_subscription_dedup();
        let vk = sk(8).verifying_key().to_bytes();

        // First "send" — claims the dedup slot.
        let first = {
            let mut sent = ENSURE_SUBSCRIPTION_SENT.lock().unwrap();
            sent.insert(vk)
        };
        assert!(first, "first insert must succeed");

        // Simulate the failure path in `ensure_room_subscription_once`:
        // delegate rejected (or transport failed) → clear the dedup so a
        // future call can retry.
        clear_ensure_subscription_sent(&vk);

        // Retry — must be allowed.
        let retry = {
            let mut sent = ENSURE_SUBSCRIPTION_SENT.lock().unwrap();
            sent.insert(vk)
        };
        assert!(
            retry,
            "after clear_ensure_subscription_sent the same VK must be insertable again \
             (Bug #6 regression — the owner's delegate stayed unsubscribed on \
             EnsureRoomSubscription failure because the dedup was never cleared)"
        );
    }

    /// Clearing dedup is scoped to the supplied VK — unrelated entries
    /// must NOT be cleared. Pins that a single room's retry doesn't
    /// silently re-enable retries for every other room in the session.
    #[test]
    fn clear_ensure_subscription_sent_is_scoped_to_supplied_vk() {
        let _dedup_guard = DEDUP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_ensure_subscription_dedup();
        let vk_a = sk(9).verifying_key().to_bytes();
        let vk_b = sk(10).verifying_key().to_bytes();

        {
            let mut sent = ENSURE_SUBSCRIPTION_SENT.lock().unwrap();
            sent.insert(vk_a);
            sent.insert(vk_b);
        }

        clear_ensure_subscription_sent(&vk_a);

        // vk_b must still be marked as sent — clear was scoped to vk_a.
        let b_retry = {
            let mut sent = ENSURE_SUBSCRIPTION_SENT.lock().unwrap();
            sent.insert(vk_b)
        };
        assert!(
            !b_retry,
            "clearing one VK must not clear an unrelated VK's dedup entry"
        );

        // And vk_a must now be insertable again.
        let a_retry = {
            let mut sent = ENSURE_SUBSCRIPTION_SENT.lock().unwrap();
            sent.insert(vk_a)
        };
        assert!(a_retry, "cleared VK must be re-insertable");
    }

    /// `complete_pending_room_subscription_request` must use the same
    /// `ROOM_SUBSCRIPTION_PREFIX + room_owner_vk + request_id.to_le_bytes()`
    /// that `get_request_key` builds, otherwise the response handler will
    /// never resolve the awaiting future and `ensure_room_subscription_once`
    /// will hang forever. Pins the key-bytes contract on both sides so a
    /// future rename can't silently re-introduce the Bug #6 hang.
    ///
    /// The `request_id` mix-in is what protects against the cross-epoch
    /// race described in PR #276 review feedback: without it, a 10s timeout
    /// on call A could free the slot, call C would install its sender at
    /// the same `room_owner_vk`-keyed slot, and a late response for call A
    /// would resolve call C with the wrong ACK. Including `request_id` in
    /// the key bytes makes each call's slot unique.
    #[test]
    fn room_subscription_pending_key_round_trips() {
        let vk = sk(11).verifying_key().to_bytes();
        let request_id: RequestId = 0x1234_5678_9abc_def0;
        let req = ChatDelegateRequestMsg::EnsureRoomSubscription {
            room_owner_vk: vk,
            request_id,
            contract_id: [0u8; 32],
        };
        let from_request = get_request_key(&req);

        let mut from_response = ROOM_SUBSCRIPTION_PREFIX.to_vec();
        from_response.extend_from_slice(&vk);
        from_response.extend_from_slice(&request_id.to_le_bytes());

        assert_eq!(
            from_request, from_response,
            "request-side and response-side lookup keys must agree, including request_id"
        );
    }

    /// Two concurrent `EnsureRoomSubscription` calls for the same
    /// `room_owner_vk` MUST produce distinct pending-request keys —
    /// otherwise the second caller would clobber the first's sender slot,
    /// and a late-arriving response for either call would be routed to
    /// the wrong awaiting future. This was the BLOCKING race documented
    /// in PR #276 review feedback.
    #[test]
    fn room_subscription_pending_keys_diverge_per_request_id() {
        let vk = sk(12).verifying_key().to_bytes();
        let req_a = ChatDelegateRequestMsg::EnsureRoomSubscription {
            room_owner_vk: vk,
            request_id: 1,
            contract_id: [0u8; 32],
        };
        let req_b = ChatDelegateRequestMsg::EnsureRoomSubscription {
            room_owner_vk: vk,
            request_id: 2,
            contract_id: [0u8; 32],
        };
        assert_ne!(
            get_request_key(&req_a),
            get_request_key(&req_b),
            "distinct request_ids for the same room_owner_vk must produce \
             distinct registry keys — otherwise a stale response could resolve \
             the wrong awaiting caller"
        );
    }

    /// The coalescing primitive must drain a dirty re-entry into exactly
    /// one extra save — modelling "another caller landed mid-save" by
    /// having the inner save flip the dirty flag back on its first
    /// iteration. This pins the open-time mitigation for freenet/river#246:
    /// without the coalesce, every `spawn_local(save_rooms_to_delegate())`
    /// re-runs the full-map clone/serialize and bursts memory at
    /// MB-per-ms during cold open.
    ///
    /// Function-local statics are used so the coalesce-state lifetimes are
    /// `'static` (matching production use) and so each `#[test]` owns
    /// fresh state.
    #[test]
    fn coalesce_save_drains_dirty_re_entry_into_one_extra_save() {
        use std::sync::atomic::AtomicUsize;

        static STATE: CoalesceState = CoalesceState::new();
        static COUNT: AtomicUsize = AtomicUsize::new(0);

        COUNT.store(0, Ordering::SeqCst);
        STATE.dirty.store(true, Ordering::SeqCst);
        *STATE.last_result.lock().unwrap() = Ok(());

        async fn save() -> Result<(), String> {
            let n = COUNT.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // Model a concurrent caller marking dirty mid-save.
                STATE.dirty.store(true, Ordering::Release);
            }
            Ok(())
        }

        let result = futures::executor::block_on(coalesce_save(&STATE, "Test", save));
        assert!(
            result.is_ok(),
            "coalesce_save propagated an error: {:?}",
            result
        );
        assert_eq!(
            COUNT.load(Ordering::SeqCst),
            2,
            "coalesce_save must run exactly twice when one re-entry lands mid-save"
        );
    }

    /// If the dirty flag is never re-set during the save, exactly one
    /// save runs. Pairs with the previous test to pin both bounds of the
    /// coalesce loop.
    #[test]
    fn coalesce_save_runs_once_when_no_re_entry() {
        use std::sync::atomic::AtomicUsize;

        static STATE: CoalesceState = CoalesceState::new();
        static COUNT: AtomicUsize = AtomicUsize::new(0);

        COUNT.store(0, Ordering::SeqCst);
        STATE.dirty.store(true, Ordering::SeqCst);
        *STATE.last_result.lock().unwrap() = Ok(());

        async fn save() -> Result<(), String> {
            COUNT.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        let result = futures::executor::block_on(coalesce_save(&STATE, "Test", save));
        assert!(result.is_ok());
        assert_eq!(COUNT.load(Ordering::SeqCst), 1);
    }

    /// When the last iteration's save errs, the helper must propagate
    /// that error (not the prior Ok). Pins the `last_result` contract
    /// for the legacy-migration `mark_legacy_migration_done()` branch.
    #[test]
    fn coalesce_save_returns_err_from_last_iteration() {
        use std::sync::atomic::AtomicUsize;

        static STATE: CoalesceState = CoalesceState::new();
        static COUNT: AtomicUsize = AtomicUsize::new(0);

        COUNT.store(0, Ordering::SeqCst);
        STATE.dirty.store(true, Ordering::SeqCst);
        *STATE.last_result.lock().unwrap() = Ok(());

        async fn save() -> Result<(), String> {
            let n = COUNT.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                STATE.dirty.store(true, Ordering::Release);
                Ok(())
            } else {
                Err("catch-up failed".to_string())
            }
        }

        let result = futures::executor::block_on(coalesce_save(&STATE, "Test", save));
        assert!(
            matches!(&result, Err(e) if e == "catch-up failed"),
            "expected Err from last iteration, got {:?}",
            result
        );
        assert_eq!(COUNT.load(Ordering::SeqCst), 2);
        // The store must also reflect the final outcome so a queued
        // caller landing now would see the error.
        assert!(
            matches!(STATE.last_result.lock().unwrap().clone(), Err(e) if e == "catch-up failed")
        );
    }

    /// When two callers race and the second is "covered" by the first's
    /// catch-up save (its loop sees dirty=false on entry), the second
    /// caller must see the actual save's result rather than synthetic
    /// `Ok(())`. This is the convergent-finding bug from Codex + the
    /// skeptical-reviewer pass on PR #311: without `state.last_result`,
    /// `response_handler.rs:739`'s `mark_legacy_migration_done()` could
    /// fire on a queued caller's false-`Ok` while the catch-up save
    /// that covered the migration's snapshot had actually failed,
    /// silently losing the migrated data.
    ///
    /// **Test-design note** (round-2 reviewers caught this empirically):
    /// the `save()` closure MUST yield via `futures::pending!()` after
    /// recording its call. Without that yield, `block_on(join(f1, f2))`
    /// polls f1 to completion before f2 even starts — nothing in f1's
    /// `await` chain returns `Pending` (an uncontended
    /// `futures::lock::Mutex::lock()` is `Ready` immediately), so f2
    /// runs its own iteration and the zero-iteration branch is never
    /// reached. The round-1 (buggy) code passed the test without the
    /// yield, which means the test wasn't pinning the bug. With the
    /// yield, the executor must interleave the two futures so f2's
    /// `dirty.store(true)` lands before f1's last `swap` drains it;
    /// f2 then takes the zero-iteration branch and reads
    /// `state.last_result`.
    ///
    /// **Assertion that f2 actually took the zero-iteration branch**:
    /// `COUNT == 2`. f1 runs exactly two iterations (its own + the
    /// catch-up that covers f2's mutation); if f2 ran its own
    /// iteration too, `COUNT` would be 3. If a future refactor removed
    /// the `state.last_result` propagation, either the `Err` assertion
    /// would fail (synthetic `Ok(())`) or f2 would run its own save
    /// (`COUNT == 3`) — both failure modes distinguishable.
    #[test]
    fn coalesce_save_queued_caller_sees_real_failure_not_synthetic_ok() {
        use std::sync::atomic::AtomicUsize;

        static STATE: CoalesceState = CoalesceState::new();
        static COUNT: AtomicUsize = AtomicUsize::new(0);

        COUNT.store(0, Ordering::SeqCst);
        STATE.dirty.store(false, Ordering::SeqCst);
        *STATE.last_result.lock().unwrap() = Ok(());

        // Yield exactly once on first poll, then complete. Crucially
        // calls `wake_by_ref` so `block_on`'s executor schedules us
        // again — `futures::pending!()` does NOT do this (it returns
        // `Pending` without arming a waker, which deadlocks the
        // executor on a single-threaded `block_on`).
        struct YieldOnce(bool);
        impl std::future::Future for YieldOnce {
            type Output = ();
            fn poll(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<()> {
                if self.0 {
                    std::task::Poll::Ready(())
                } else {
                    self.0 = true;
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            }
        }

        async fn save() -> Result<(), String> {
            COUNT.fetch_add(1, Ordering::SeqCst);
            // Yield once so a concurrently-polled coalesce_save
            // instance can run its `dirty.store(true)` before our
            // loop's `swap`, forcing it onto the zero-iteration path.
            // Without this yield the regression test passes for the
            // wrong reason — see the test-design note above.
            YieldOnce(false).await;
            Err("save_failure".to_string())
        }

        let f1 = coalesce_save(&STATE, "Test", save);
        let f2 = coalesce_save(&STATE, "Test", save);
        let (r1, r2) = futures::executor::block_on(futures::future::join(f1, f2));

        // Both must surface the failure. f2 specifically takes the
        // zero-iteration branch (proven by COUNT == 2) and returns
        // the value read from `state.last_result` — that's the
        // regression-pin for the convergent-finding bug.
        assert!(
            matches!(&r1, Err(e) if e == "save_failure"),
            "first caller: expected Err, got {:?}",
            r1
        );
        assert!(
            matches!(&r2, Err(e) if e == "save_failure"),
            "queued caller: expected Err propagated from state.last_result, got {:?}",
            r2
        );

        assert_eq!(
            COUNT.load(Ordering::SeqCst),
            2,
            "expected exactly 2 saves (f1 own + f1 catch-up); f2 must take \
             the zero-iteration branch"
        );
    }

    /// A caller that runs at least one iteration must return its OWN
    /// iteration's result, never inherit a stale `state.last_result`
    /// from an earlier unrelated call. Pins that `ran_any` actually
    /// gates the return value — without that gate, a stale `Err` from
    /// a prior failure would silently mask every subsequent caller's
    /// success.
    #[test]
    fn coalesce_save_running_caller_does_not_inherit_stale_last_result() {
        static STATE: CoalesceState = CoalesceState::new();

        STATE.dirty.store(true, Ordering::SeqCst);
        // Seed with a stale Err as if a prior unrelated call had failed.
        *STATE.last_result.lock().unwrap() = Err("stale_prior_error".to_string());

        async fn save() -> Result<(), String> {
            Ok(())
        }

        let result = futures::executor::block_on(coalesce_save(&STATE, "Test", save));
        assert!(
            result.is_ok(),
            "running caller must return its OWN iteration's Ok, not the stale store; got {:?}",
            result
        );
        // And the store should reflect the fresh success after our run.
        assert!(STATE.last_result.lock().unwrap().is_ok());
    }

    /// The cached `CHAT_DELEGATE_KEY` must equal what the previous
    /// per-call construction would produce. A drift here (parameters
    /// changed, WASM include path changed, BLAKE3 implementation
    /// changed) would silently misroute every delegate request to a
    /// non-existent delegate key — the same failure class as the
    /// "riverctl v0.1.34 used stale WASM" incident.
    #[test]
    fn cached_chat_delegate_key_matches_uncached_construction() {
        let bytes = include_bytes!("../../../public/contracts/chat_delegate.wasm");
        let uncached_code = DelegateCode::from(bytes.to_vec());
        let params = Parameters::from(Vec::<u8>::new());
        let uncached_delegate = Delegate::from((&uncached_code, &params));
        let uncached_key = uncached_delegate.key().clone();
        assert_eq!(
            *CHAT_DELEGATE_KEY, uncached_key,
            "cached CHAT_DELEGATE_KEY drifted from per-call construction"
        );
    }
}

/// Remove a `room_owner_vk` from the per-session ensure-subscription dedup
/// set so a future caller can retry. Used when the previous attempt failed
/// (transport error or delegate-side rejection) — without this, a single
/// transient failure poisoned the in-memory set for the rest of the
/// session and the owner's delegate stayed unsubscribed, which was the
/// owner-side root cause of Bug #6.
fn clear_ensure_subscription_sent(room_owner_vk: &RoomKey) {
    if let Ok(mut sent) = ENSURE_SUBSCRIPTION_SENT.lock() {
        sent.remove(room_owner_vk);
    }
}

/// Idempotent helper: ask the chat delegate to subscribe to a room
/// contract, but only if we haven't already done so this session.
///
/// Returns `Ok(true)` if the delegate ACKed a fresh subscription, `Ok(false)`
/// if it was a no-op because the subscription already succeeded this session.
/// On any failure the per-session dedup entry is cleared so a follow-up call
/// (e.g. after a subsequent `StoreSigningKey`) can retry. This matters
/// because the delegate refuses `EnsureRoomSubscription` if the owner
/// signing key is not on file, and the load-rooms path can race
/// `StoreSigningKey` against `EnsureRoomSubscription` (Bug #6).
pub(crate) async fn ensure_room_subscription_once(
    room_owner_vk: RoomKey,
    contract_id: [u8; 32],
) -> Result<bool, String> {
    {
        let mut sent = ENSURE_SUBSCRIPTION_SENT
            .lock()
            .map_err(|e| format!("Failed to lock ensure-subscription dedup set: {e}"))?;
        if !sent.insert(room_owner_vk) {
            return Ok(false);
        }
    }

    // Fresh per-call request_id so the pending-request registry doesn't
    // collide if the dedup gets cleared mid-flight (e.g. a previous call
    // timed out, the slot was reclaimed, and a late-arriving response for
    // the previous epoch would otherwise resolve the new call with stale
    // bytes — see PR #276 review feedback).
    let request_id = generate_request_id();
    let req = ChatDelegateRequestMsg::EnsureRoomSubscription {
        room_owner_vk,
        request_id,
        contract_id,
    };

    // Await the delegate's response so a delegate-side rejection (e.g.
    // "no signing key on file") clears the dedup entry and lets a retry
    // happen later in this session.
    let response = match send_delegate_request(req).await {
        Ok(resp) => resp,
        Err(e) => {
            clear_ensure_subscription_sent(&room_owner_vk);
            return Err(e);
        }
    };

    match response {
        ChatDelegateResponseMsg::EnsureRoomSubscriptionResponse { result, .. } => match result {
            Ok(()) => Ok(true),
            Err(e) => {
                clear_ensure_subscription_sent(&room_owner_vk);
                Err(format!("Delegate refused EnsureRoomSubscription: {e}"))
            }
        },
        other => {
            clear_ensure_subscription_sent(&room_owner_vk);
            Err(format!(
                "Unexpected response for EnsureRoomSubscription: {other:?}"
            ))
        }
    }
}

/// Fire a `ListRequest` to enumerate the current delegate's stored keys
/// without waiting for the response. The `ListResponse` is handled in
/// `response_handler.rs`, which spawns the per-room load orchestration
/// (freenet/river#345 / #65). Firing rather than awaiting avoids the deadlock
/// described in `set_up_chat_delegate`: the response arrives through the same
/// message loop that called us.
///
/// Replaces the old single-blob `GetRequest{rooms_data}` load: rooms are now
/// stored as independent `room:<vk>` keys (+ `rooms_meta`), so the load path
/// must first discover which keys exist before fetching them.
async fn fire_list_rooms_request() {
    info!("Firing ListRequest to enumerate delegate keys for room load");

    let request = ChatDelegateRequestMsg::ListRequest;

    // Serialize and send the request without waiting for response
    let mut payload = Vec::new();
    if let Err(e) = ciborium::ser::into_writer(&request, &mut payload) {
        error!("Failed to serialize list-rooms request: {}", e);
        return;
    }

    // Reuse the session-cached delegate key (freenet/river#246) — re-hashing
    // the 710 KB delegate WASM here on every call was a per-request fixed
    // cost that, multiplied across the cold-open burst, contributed
    // measurably to the memory peak. The delegate code is invariant for
    // the session, so the key only needs to be computed once.
    let delegate_key = CHAT_DELEGATE_KEY.clone();

    let app_msg = freenet_stdlib::prelude::ApplicationMessage::new(payload);

    let delegate_request = DelegateOp(DelegateRequest::ApplicationMessages {
        key: delegate_key,
        params: Parameters::from(Vec::<u8>::new()),
        inbound: vec![freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(app_msg)],
    });

    // Send without waiting for response
    let api_result = {
        let mut web_api = WEB_API.write();
        if let Some(api) = web_api.as_mut() {
            api.send(delegate_request).await
        } else {
            Err(freenet_stdlib::client_api::Error::ConnectionClosed)
        }
    };

    if let Err(e) = api_result {
        error!("Failed to send list-rooms request: {}", e);
    } else {
        info!("List-rooms request sent, response will be handled by message loop");
    }
}

/// Load rooms from the delegate storage (with response waiting - use outside message loop only)
#[allow(dead_code)]
pub async fn load_rooms_from_delegate() -> Result<(), String> {
    info!("Loading rooms from delegate storage");

    // Create a get request for the rooms data
    let request = ChatDelegateRequestMsg::GetRequest {
        key: ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
    };

    // Send the request to the delegate
    match send_delegate_request(request).await {
        Ok(_) => {
            info!("Sent request to load rooms from delegate");
            Ok(())
        }
        Err(e) => {
            warn!("Failed to load rooms from delegate: {}", e);
            // Don't fail the app if we can't load rooms
            Ok(())
        }
    }
}

/// Coalescing state for `save_rooms_to_delegate`. The whole point is
/// to keep cold-start bursts (many rooms' GET responses + catch-up
/// deltas each firing their own `spawn_local(save_rooms_to_delegate())`)
/// from each re-cloning the entire `ROOMS` map and re-serializing it
/// — freenet/river#246. See [`CoalesceState`] and [`coalesce_save`]
/// for the shared pattern (also used by [`save_outbound_dms_to_delegate`]).
static ROOMS_SAVE_STATE: CoalesceState = CoalesceState::new();

/// Maximum read-merge-write CAS attempts for a single rooms save. A retry
/// happens only when another writer advances the key between our read and our
/// store, so convergence normally takes 1–2 iterations even with several tabs
/// writing; the cap only guards against a pathological hot-loop of competing
/// writers.
const ROOMS_CAS_MAX_ATTEMPTS: u32 = 8;

/// Save rooms to the delegate storage.
///
/// Coalesced via [`coalesce_save`]: a chain of N rapid callers (e.g.
/// every per-room state notification on cold open) produces at most 2
/// actual saves. The expensive `ROOMS.read().clone()` +
/// `ciborium::ser::into_writer` happens inside
/// [`do_save_rooms_to_delegate`] and therefore only runs when a save
/// actually executes, not once per queued caller.
///
/// Callers must have mutated `ROOMS` (and/or `CURRENT_ROOM`) before
/// invoking; `Ok(())` guarantees the post-mutation snapshot was
/// persisted (either by this call's loop or by a concurrent caller's
/// catch-up that covered our snapshot — see `coalesce_save` for the
/// queued-caller propagation invariant).
pub async fn save_rooms_to_delegate() -> Result<(), String> {
    coalesce_save(&ROOMS_SAVE_STATE, "Rooms", do_save_rooms_to_delegate).await
}

/// What we last persisted for a room key, so a save only writes the rooms that
/// actually changed. Per-room isolation: a change to room X never rewrites room
/// R's key, so it can't resurrect R's tombstone (freenet/river#345 round-9).
#[derive(Clone, Copy, PartialEq)]
enum SavedSlot {
    /// Last-saved content hash of the room's serialized `RoomData`.
    Present(u64),
    /// We've persisted this room as a tombstone (the user left it).
    Tombstone,
}

thread_local! {
    /// Last-persisted state per per-room key, keyed by owner VK. Diffed each
    /// save so only changed rooms are written. Per-session; starts EMPTY on a
    /// fresh load (the load path deliberately does NOT pre-seed it). The first
    /// save after a load therefore re-serializes each room's CURRENT
    /// (post-hydrate) state and CAS-writes any that differ from the delegate —
    /// correct because hydration can legitimately change a room's serialized
    /// form (e.g. `regenerate_contract_key`, `remove_unverifiable_messages`), so
    /// seeding the baseline from the loaded bytes could wrongly skip a needed
    /// write. CAS reconcile makes the redundant same-content writes harmless.
    static ROOM_SLOT_STATE: std::cell::RefCell<HashMap<VerifyingKey, SavedSlot>> =
        std::cell::RefCell::new(HashMap::new());
    /// Last-persisted content hash of the `rooms_meta` value.
    static META_SLOT_HASH: std::cell::RefCell<Option<u64>> = const { std::cell::RefCell::new(None) };
    /// Rooms the user EXPLICITLY rejoined this session (cleared the tombstone +
    /// re-added). Only an explicit rejoin may overwrite a remote `Tombstone`
    /// slot with `Present`; a background content update to a remotely-left room
    /// adopts the leave rather than resurrecting it (freenet/river#345 round-9).
    static REJOINED_THIS_SESSION: std::cell::RefCell<HashSet<VerifyingKey>> =
        std::cell::RefCell::new(HashSet::new());
}

/// Record that the user explicitly rejoined `owner_vk` this session. Called
/// from the invitation-accept and identity-import rejoin paths (the sites that
/// clear the room's tombstone). See [`REJOINED_THIS_SESSION`].
pub fn mark_room_rejoined(owner_vk: VerifyingKey) {
    REJOINED_THIS_SESSION.with(|s| {
        s.borrow_mut().insert(owner_vk);
    });
}

fn content_hash(bytes: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(bytes);
    h.finish()
}

/// Generic read-merge-write CAS for a single delegate key (freenet/river#345).
///
/// `reconcile(current)` receives the delegate's current value (`None` if
/// absent) and returns `Ok(Some(bytes))` to store, `Ok(None)` to abort the
/// write as a no-op (e.g. adopt a remote tombstone), or `Err` to fail. On a CAS
/// `Conflict` (another writer advanced the key between our read and store) it
/// re-reads and re-reconciles. `get_versioned`/`cas_store` are injected so the
/// loop is unit-testable without the websocket.
async fn cas_write_key<R, G, GFut, S, SFut>(
    mut reconcile: R,
    get_versioned: G,
    mut cas_store: S,
) -> Result<(), String>
where
    R: FnMut(Option<&[u8]>) -> Result<Option<Vec<u8>>, String>,
    G: Fn() -> GFut,
    GFut: std::future::Future<Output = Result<(Option<Vec<u8>>, u64), String>>,
    S: FnMut(Vec<u8>, u64) -> SFut,
    SFut: std::future::Future<Output = Result<CasStoreResult, String>>,
{
    for _ in 0..ROOMS_CAS_MAX_ATTEMPTS {
        let (current, generation) = get_versioned().await?;
        let to_store = match reconcile(current.as_deref())? {
            Some(bytes) => bytes,
            None => return Ok(()),
        };
        match cas_store(to_store, generation).await? {
            CasStoreResult::Stored { .. } => return Ok(()),
            CasStoreResult::Conflict { .. } => continue,
            CasStoreResult::Failed(e) => return Err(e),
        }
    }
    Err(format!(
        "CAS exceeded {ROOMS_CAS_MAX_ATTEMPTS} attempts under concurrent writers"
    ))
}

/// [`cas_write_key`] bound to a real delegate key over the websocket.
async fn cas_write_delegate_key<R>(key: Vec<u8>, reconcile: R) -> Result<(), String>
where
    R: FnMut(Option<&[u8]>) -> Result<Option<Vec<u8>>, String>,
{
    let get_key = key.clone();
    cas_write_key(
        reconcile,
        move || {
            let key = get_key.clone();
            async move {
                let request = ChatDelegateRequestMsg::GetVersionedRequest {
                    key: ChatDelegateKey::new(key),
                };
                match send_delegate_request(request).await {
                    Ok(ChatDelegateResponseMsg::GetVersionedResponse {
                        value, generation, ..
                    }) => Ok((value, generation)),
                    Ok(other) => Err(format!("Unexpected response to GetVersioned: {other:?}")),
                    Err(e) => Err(e),
                }
            }
        },
        move |value, expected| {
            let key = key.clone();
            async move {
                let request = ChatDelegateRequestMsg::CasStoreRequest {
                    key: ChatDelegateKey::new(key),
                    value,
                    expected_generation: expected,
                };
                match send_delegate_request(request).await {
                    Ok(ChatDelegateResponseMsg::CasStoreResponse { result, .. }) => Ok(result),
                    Ok(other) => Err(format!("Unexpected response to CasStore: {other:?}")),
                    Err(e) => Err(e),
                }
            }
        },
    )
    .await
}

/// The delegate's current per-room slot, classified for the reconcile decision.
#[derive(Clone, Copy, PartialEq, Debug)]
enum SlotKind {
    Absent,
    Present,
    Tombstone,
}

/// What a `Present` save should do given the delegate's current slot and
/// whether the user explicitly rejoined this room this session. This is the
/// round-9 decision in pure form (unit-testable without a `RoomData`):
/// absent → store; present → CRDT-merge; tombstone + explicit rejoin → store
/// (overwrite the leave); tombstone + background update → abort (adopt the
/// leave, never resurrect a room left elsewhere).
#[derive(Clone, Copy, PartialEq, Debug)]
enum PresentAction {
    StoreFresh,
    MergeState,
    AbortAdoptLeave,
}

fn present_action(kind: SlotKind, explicitly_rejoined: bool) -> PresentAction {
    match kind {
        SlotKind::Absent => PresentAction::StoreFresh,
        SlotKind::Present => PresentAction::MergeState,
        SlotKind::Tombstone if explicitly_rejoined => PresentAction::StoreFresh,
        SlotKind::Tombstone => PresentAction::AbortAdoptLeave,
    }
}

/// Reconcile a `Present` save for room `owner_vk` against the delegate's
/// current slot. Returns the `RoomSlot` bytes to store, or `None` to abort
/// (adopt a remote leave on a background update — the round-9 fix).
fn reconcile_room_present(
    current: Option<&[u8]>,
    owner_vk: &VerifyingKey,
    local: &RoomData,
    explicitly_rejoined: bool,
) -> Result<Option<Vec<u8>>, String> {
    let (kind, remote): (SlotKind, Option<Box<RoomData>>) = match current {
        None => (SlotKind::Absent, None),
        Some(bytes) => match ciborium::from_reader::<RoomSlot, _>(bytes) {
            Ok(RoomSlot::Present(remote)) => (SlotKind::Present, Some(remote)),
            Ok(RoomSlot::Tombstone) => (SlotKind::Tombstone, None),
            Err(e) => return Err(format!("unparseable room slot for {owner_vk:?}: {e}")),
        },
    };

    let merged: RoomData = match present_action(kind, explicitly_rejoined) {
        PresentAction::AbortAdoptLeave => return Ok(None),
        PresentAction::StoreFresh => local.clone(),
        PresentAction::MergeState => {
            let remote = remote.expect("MergeState implies a Present remote slot");
            if local.self_sk != remote.self_sk {
                // Diverged identity for the same room — keep local's
                // (local-authoritative), don't merge the other identity's
                // state, and don't fail the whole save (M1 fix: scoped to this
                // one room rather than wedging all persistence).
                local.clone()
            } else {
                let mut m = local.clone();
                m.room_state
                    .merge(
                        &local.room_state,
                        &river_core::room_state::ChatRoomParametersV1 { owner: *owner_vk },
                        &remote.room_state,
                    )
                    .map_err(|e| format!("room_state merge failed: {e}"))?;
                m
            }
        }
    };

    let mut out = Vec::new();
    ciborium::ser::into_writer(&RoomSlot::Present(Box::new(merged)), &mut out)
        .map_err(|e| format!("serialize room slot: {e}"))?;
    Ok(Some(out))
}

/// Reconcile a `Tombstone` (leave) save: write a tombstone unless already one.
fn reconcile_room_tombstone(current: Option<&[u8]>) -> Result<Option<Vec<u8>>, String> {
    if let Some(bytes) = current {
        if matches!(
            ciborium::from_reader::<RoomSlot, _>(bytes),
            Ok(RoomSlot::Tombstone)
        ) {
            return Ok(None);
        }
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&RoomSlot::Tombstone, &mut out)
        .map_err(|e| format!("serialize tombstone: {e}"))?;
    Ok(Some(out))
}

/// Reconcile the `rooms_meta` save: this tab's view-prefs are authoritative;
/// absorb notification prefs the delegate has for rooms we don't track so a
/// concurrent tab's setting isn't lost.
fn reconcile_meta(current: Option<&[u8]>, local: &RoomsMeta) -> Result<Option<Vec<u8>>, String> {
    let mut merged = local.clone();
    if let Some(bytes) = current {
        if let Ok(remote) = ciborium::from_reader::<RoomsMeta, _>(bytes) {
            for (vk, mode) in remote.notification_modes {
                merged.notification_modes.entry(vk).or_insert(mode);
            }
        }
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&merged, &mut out).map_err(|e| format!("serialize meta: {e}"))?;
    Ok(Some(out))
}

async fn do_save_rooms_to_delegate() -> Result<(), String> {
    info!("Saving rooms to delegate storage (per-room CAS)");

    // Snapshot this tab's explicit state once. We never mutate the ROOMS signal
    // here; each per-room save reads that one room's delegate key and reconciles
    // (read-merge-write CAS), so a concurrent tab's rooms are preserved rather
    // than clobbered. Merged remote state is not written back into the ROOMS
    // signal (a delegate-deserialized room lacks rehydrated `#[serde(skip)]`
    // runtime state and isn't subscribed) — it reappears fully hydrated via the
    // load path on reload; live cross-tab surfacing is future work.
    let (rooms, meta) = {
        let mut rooms = ROOMS.read().clone();
        rooms.current_room_key = CURRENT_ROOM.read().owner_key;
        let meta = rooms.to_meta();
        (rooms, meta)
    };

    // 1. Present rooms — write only those whose serialized RoomData changed
    //    since the last save. Per-room isolation: an unchanged room is never
    //    rewritten, so a save can't touch (and thus can't resurrect) another
    //    room's tombstone (freenet/river#345 round-9).
    for (vk, room_data) in rooms.map.iter() {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(room_data, &mut buf)
            .map_err(|e| format!("serialize room {vk:?}: {e}"))?;
        let h = content_hash(&buf);
        if ROOM_SLOT_STATE.with(|c| c.borrow().get(vk) == Some(&SavedSlot::Present(h))) {
            continue;
        }
        let rejoined = REJOINED_THIS_SESSION.with(|s| s.borrow().contains(vk));
        let rd = room_data.clone();
        let owner = *vk;
        cas_write_delegate_key(room_storage_key(vk), move |current| {
            reconcile_room_present(current, &owner, &rd, rejoined)
        })
        .await?;
        ROOM_SLOT_STATE.with(|c| {
            c.borrow_mut().insert(*vk, SavedSlot::Present(h));
        });
    }

    // 2. Tombstones — rooms the user has left.
    for vk in rooms.removed_rooms.iter() {
        if ROOM_SLOT_STATE.with(|c| c.borrow().get(vk) == Some(&SavedSlot::Tombstone)) {
            continue;
        }
        cas_write_delegate_key(room_storage_key(vk), reconcile_room_tombstone).await?;
        ROOM_SLOT_STATE.with(|c| {
            c.borrow_mut().insert(*vk, SavedSlot::Tombstone);
        });
    }

    // 3. List-level view preferences (current room, notification modes, order).
    let mut meta_buf = Vec::new();
    ciborium::ser::into_writer(&meta, &mut meta_buf).map_err(|e| format!("serialize meta: {e}"))?;
    let mh = content_hash(&meta_buf);
    if META_SLOT_HASH.with(|c| *c.borrow()) != Some(mh) {
        cas_write_delegate_key(ROOMS_META_KEY.to_vec(), move |current| {
            reconcile_meta(current, &meta)
        })
        .await?;
        META_SLOT_HASH.with(|c| {
            *c.borrow_mut() = Some(mh);
        });
    }

    Ok(())
}

// =============================================================================
// OUTBOUND DM PLAINTEXT CACHE (issue freenet/river#256)
//
// The room contract carries DM bodies as ECIES ciphertext only the recipient
// can decrypt, so without a side-channel the sender's own UI/`riverctl` would
// render their own sent DMs as "sent — ciphertext only". We persist the
// sender's plaintext in the chat delegate (same backing store as room
// secrets / signing keys) so reloads and second devices recover it.
// =============================================================================

/// Fire a GetRequest for the outbound-DM cache without awaiting the
/// response. Mirrors [`fire_list_rooms_request`] — the response is
/// processed in `freenet_api::response_handler` and hydrates the
/// in-memory [`OUTBOUND_DMS`] signal.
async fn fire_load_outbound_dms_request() {
    info!("Firing request to load outbound DMs from delegate storage");

    let request = ChatDelegateRequestMsg::GetRequest {
        key: ChatDelegateKey::new(OUTBOUND_DMS_STORAGE_KEY.to_vec()),
    };

    let mut payload = Vec::new();
    if let Err(e) = ciborium::ser::into_writer(&request, &mut payload) {
        error!("Failed to serialize load outbound-DMs request: {}", e);
        return;
    }

    // Reuse the session-cached delegate key (freenet/river#246) — re-hashing
    // the 710 KB delegate WASM here on every call was a per-request fixed
    // cost that, multiplied across the cold-open burst, contributed
    // measurably to the memory peak. The delegate code is invariant for
    // the session, so the key only needs to be computed once.
    let delegate_key = CHAT_DELEGATE_KEY.clone();

    let app_msg = freenet_stdlib::prelude::ApplicationMessage::new(payload);
    let delegate_request = DelegateOp(DelegateRequest::ApplicationMessages {
        key: delegate_key,
        params: Parameters::from(Vec::<u8>::new()),
        inbound: vec![freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(app_msg)],
    });

    let api_result = {
        let mut web_api = WEB_API.write();
        if let Some(api) = web_api.as_mut() {
            api.send(delegate_request).await
        } else {
            Err(freenet_stdlib::client_api::Error::ConnectionClosed)
        }
    };

    match api_result {
        Ok(_) => info!("Outbound-DMs load request sent"),
        Err(e) => error!("Failed to send outbound-DMs load request: {}", e),
    }
}

/// Hydrate the [`OUTBOUND_DMS`] cache from a `Vec<OutboundDmEntry>`
/// loaded from a delegate. Per-pair caps are NOT re-applied here — we
/// trust the writer (us, on save). Returns the number of entries
/// loaded.
///
/// **Conflict resolution.** If an entry for the same `(room,
/// recipient, purge_token)` is ALREADY in the cache, keep whichever
/// has the larger `timestamp` (skeptical-review IMPORTANT: legacy
/// responses can arrive after the current delegate's response and
/// would otherwise overwrite the fresher current entries). Both
/// senders signed equivalently-derived tokens, so the timestamp is the
/// only authoritative recency signal.
pub fn hydrate_outbound_dms_cache(entries: Vec<OutboundDmEntry>) -> usize {
    use crate::components::direct_messages::OUTBOUND_DMS;
    let count = entries.len();
    crate::util::defer(move || {
        OUTBOUND_DMS.with_mut(|cache| {
            for entry in entries {
                let room_vk = match ed25519_dalek::VerifyingKey::from_bytes(&entry.room_owner_vk) {
                    Ok(vk) => vk,
                    Err(e) => {
                        warn!("Skipping outbound-DM entry with invalid room VK: {}", e);
                        continue;
                    }
                };
                let key = (room_vk, entry.recipient, entry.purge_token);
                match cache.by_token.get(&key) {
                    Some(existing) if existing.timestamp >= entry.timestamp => {
                        // Existing entry is at least as fresh — keep it.
                    }
                    _ => {
                        cache.by_token.insert(key, entry);
                    }
                }
            }
        });
    });
    count
}

/// Session-scoped tombstone set for `(room, peer)` pairs the local
/// user has explicitly un-hidden in this session. Consulted by
/// [`hydrate_hidden_dm_threads`] to suppress any late-arriving
/// delegate response that would otherwise resurrect the just-removed
/// hide entry (Codex P3 finding on #261, second review pass).
///
/// Scenario the tombstone closes: user clicks Hide, then Send. The
/// send's `Applied` arm calls `unhide_dm_thread` and the
/// `save_outbound_dms_to_delegate` writes the now-empty hidden list.
/// But if the delegate's GET response (or a legacy delegate's GET
/// response) is still in-flight when the user did all that, it will
/// arrive AFTER the unhide and re-insert the entry with its original
/// `hidden_at_ts`. The strict `<=` filter would then re-hide the
/// thread when the outbound DM's `unix_now()` happened to land in
/// the same second.
///
/// Without persistence: the tombstone is in-memory only, scoped to
/// the current session. That's deliberate — the persistent store
/// (`OutboundDmStore.hidden_threads`) is itself the source of truth
/// across sessions; the tombstone is a transient guard against
/// hydrate-vs-unhide races within a session. A new session starts
/// from the delegate's current `hidden_threads` snapshot (which by
/// construction has the post-unhide state because
/// `save_outbound_dms_to_delegate` was queued by `unhide_dm_thread`).
static RECENTLY_UNHIDDEN: std::sync::LazyLock<
    Mutex<HashSet<(ed25519_dalek::VerifyingKey, MemberId)>>,
> = std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// Reset the recently-unhidden tombstone set. For tests only;
/// production never calls this.
#[cfg(test)]
pub(crate) fn reset_recently_unhidden() {
    if let Ok(mut s) = RECENTLY_UNHIDDEN.lock() {
        s.clear();
    }
}

/// Hydrate the [`HIDDEN_DM_THREADS`] signal from a `Vec<HiddenDmThreadEntry>`
/// loaded from a delegate (issue freenet/river#261).
///
/// **Conflict resolution.** If an entry for the same `(room, peer)` is
/// already in the signal (e.g. the current delegate's response landed
/// first, then a legacy delegate response arrives), keep whichever has
/// the larger `hidden_at_ts` — the user's most-recent hide intent
/// wins. A legacy response with an older cutoff must NOT clobber a
/// newer hide.
///
/// **Recently-unhidden suppression (Codex P3).** Entries whose
/// `(room, peer)` appears in [`RECENTLY_UNHIDDEN`] are dropped — see
/// that static's doc-comment for the exact race. Without this guard,
/// a delegate response in-flight at the moment of `unhide_dm_thread`
/// would re-insert the just-removed entry, defeating the "outbound
/// always revives" guarantee from the Codex P1 fix.
///
/// Returns the number of entries observed (NOT the number actually
/// inserted, so callers can still gate "is_legacy_delegate && count > 0"
/// on the wire-side presence rather than on post-filter retention).
/// Pure helper: decide which incoming hydration entries to apply to a
/// hidden-threads map, given the current map state and the set of
/// recently-unhidden `(room, peer)` pairs.
///
/// Rules:
/// - Entries with an invalid `room_owner_vk` are dropped (caller logs).
/// - Entries whose `(room, peer)` is in `suppressed` are dropped — the
///   user explicitly un-hid this thread in the current session, so a
///   late-arriving delegate response must not resurrect the hide
///   (Codex P3 on #261 v2).
/// - Otherwise, take whichever entry has the larger `hidden_at_ts`:
///   the incoming entry overwrites the in-memory one when it is at
///   least as fresh; otherwise the in-memory entry is preserved.
///
/// Returns the `(key, entry)` pairs that should be `insert`-ed into
/// the in-memory `HashMap`. Errors-by-malformed-VK are returned
/// separately as a `Vec<String>` of human-readable diagnostics so the
/// caller can emit `warn!` log lines.
pub(crate) fn resolve_hidden_thread_hydration(
    incoming: Vec<HiddenDmThreadEntry>,
    current: &HashMap<(ed25519_dalek::VerifyingKey, MemberId), HiddenDmThreadEntry>,
    suppressed: &HashSet<(ed25519_dalek::VerifyingKey, MemberId)>,
) -> (
    Vec<((ed25519_dalek::VerifyingKey, MemberId), HiddenDmThreadEntry)>,
    Vec<String>,
) {
    // Fold incoming entries through a temporary map so duplicate
    // `(room, peer)` keys in the same `Vec` resolve to the entry with
    // the largest `hidden_at_ts` — without this, a malformed legacy
    // blob that contains the same pair twice (newer-then-older order)
    // would let the older cutoff overwrite the newer one when the
    // caller does `for (k,e) in to_insert { hidden.insert(k,e); }`
    // (Codex round-3 Low finding on #261).
    //
    // The normal UI write path goes through a `HashMap` snapshot so it
    // can't produce duplicates; this guard is for legacy / corrupted
    // delegate blobs from other writers (or pre-#261 vintages that
    // accidentally accumulated duplicates).
    let mut merged: HashMap<(ed25519_dalek::VerifyingKey, MemberId), HiddenDmThreadEntry> =
        HashMap::new();
    let mut diagnostics = Vec::new();
    for entry in incoming {
        let room_vk = match ed25519_dalek::VerifyingKey::from_bytes(&entry.room_owner_vk) {
            Ok(vk) => vk,
            Err(e) => {
                diagnostics.push(format!(
                    "Skipping hidden-DM-thread entry with invalid room VK: {e}"
                ));
                continue;
            }
        };
        let key = (room_vk, entry.peer);
        if suppressed.contains(&key) {
            continue;
        }
        // Compare against in-flight merged entries first, then against
        // the pre-existing `current` map. Keep whichever cutoff is
        // largest.
        let existing_ts = merged
            .get(&key)
            .map(|e| e.hidden_at_ts)
            .or_else(|| current.get(&key).map(|e| e.hidden_at_ts));
        match existing_ts {
            Some(ts) if ts >= entry.hidden_at_ts => {
                // Existing entry is at least as fresh — keep it.
            }
            _ => {
                merged.insert(key, entry);
            }
        }
    }
    let to_insert = merged.into_iter().collect();
    (to_insert, diagnostics)
}

pub fn hydrate_hidden_dm_threads(entries: Vec<HiddenDmThreadEntry>) -> usize {
    use crate::components::direct_messages::HIDDEN_DM_THREADS;
    let count = entries.len();
    if count == 0 {
        return 0;
    }
    crate::util::defer(move || {
        // Snapshot the tombstone set under its own lock so we don't
        // hold it across the signal write (the with_mut closure may
        // run subscriber notifications synchronously on Drop).
        let suppressed: HashSet<(ed25519_dalek::VerifyingKey, MemberId)> =
            match RECENTLY_UNHIDDEN.lock() {
                Ok(g) => g.clone(),
                Err(e) => {
                    warn!("RECENTLY_UNHIDDEN lock poisoned: {}", e);
                    HashSet::new()
                }
            };
        HIDDEN_DM_THREADS.with_mut(|hidden| {
            let (to_insert, diagnostics) =
                resolve_hidden_thread_hydration(entries, hidden, &suppressed);
            for diag in diagnostics {
                warn!("{}", diag);
            }
            for (key, entry) in to_insert {
                hidden.insert(key, entry);
            }
        });
    });
    count
}

/// Outcome of [`decide_hide_action`]: should the caller (un)write the
/// hide entry, or treat the click as a no-op?
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HideAction {
    /// The incoming `hidden_at_ts` is strictly newer than what's
    /// already recorded (or no entry exists). The caller must insert
    /// the new entry AND queue a delegate save. This is the "re-hide
    /// after revival" path: see the round-trip test
    /// `hide_unhide_rehide_round_trip` for the canonical example.
    Insert(HiddenDmThreadEntry),
    /// The existing entry's `hidden_at_ts` is already at or above the
    /// incoming value. No state change; no delegate save needed.
    NoOp,
}

/// Pure helper: decide whether a `hide_dm_thread` click should write a
/// new entry into the hidden-threads map.
///
/// Rules (issue freenet/river#261):
/// - No existing entry → `Insert`.
/// - Existing `hidden_at_ts` strictly less than incoming → `Insert`
///   with the *new* cutoff. This is the load-bearing "re-hide after
///   revival" branch: a thread that was hidden at ts=1000, revived by
///   a message at ts=1500, and then hidden again must end up with
///   cutoff = 1500. Without this branch the second click would no-op,
///   leaving the cutoff at 1000 — and the very next render would
///   still see the message at 1500 cross the threshold and surface
///   the thread again.
/// - Existing `hidden_at_ts >= incoming` → `NoOp` (don't churn the
///   delegate blob for an unchanged value).
///
/// Pinned by `decide_hide_action_*` and `hide_unhide_rehide_round_trip`
/// in this module's `tests`.
pub(crate) fn decide_hide_action(
    existing: Option<&HiddenDmThreadEntry>,
    incoming: HiddenDmThreadEntry,
) -> HideAction {
    match existing {
        Some(e) if e.hidden_at_ts >= incoming.hidden_at_ts => HideAction::NoOp,
        _ => HideAction::Insert(incoming),
    }
}

/// Hide the DM thread for `(room, peer)` from the left rail (issue
/// freenet/river#261).
///
/// `hidden_at_ts` is the most-recent message timestamp in the thread
/// at the moment the user clicked "Hide thread"; the filter rule uses
/// `<=` so any message strictly later revives the thread.
///
/// Delegates to the pure helper [`decide_hide_action`] so the
/// "advance vs no-op" decision can be unit-tested without the Dioxus
/// runtime.
pub fn hide_dm_thread(
    room_owner_vk: ed25519_dalek::VerifyingKey,
    peer: MemberId,
    hidden_at_ts: u64,
) {
    use crate::components::direct_messages::HIDDEN_DM_THREADS;

    let entry = HiddenDmThreadEntry {
        room_owner_vk: room_owner_vk.to_bytes(),
        peer,
        hidden_at_ts,
    };
    crate::util::defer(move || {
        let mut changed = false;
        HIDDEN_DM_THREADS.with_mut(|hidden| {
            let key = (room_owner_vk, peer);
            match decide_hide_action(hidden.get(&key), entry) {
                HideAction::Insert(new_entry) => {
                    hidden.insert(key, new_entry);
                    changed = true;
                }
                HideAction::NoOp => {}
            }
        });
        if changed {
            crate::util::safe_spawn_local(async {
                if let Err(e) = save_outbound_dms_to_delegate().await {
                    warn!("Failed to persist hide-DM-thread update: {}", e);
                }
            });
        }
    });
}

/// Un-hide the DM thread for `(room, peer)` — drops the local hide
/// entry. Called by [`crate::components::direct_messages::dm_thread_modal`]
/// after a successful outbound DM to guarantee revival even when the
/// outbound message's `unix_now()` second matches the hide cutoff
/// (Codex P1 finding on #261). Idempotent: a no-op when no entry
/// exists for the pair. Also the API a future "Hidden conversations"
/// admin path would use.
pub fn unhide_dm_thread(room_owner_vk: ed25519_dalek::VerifyingKey, peer: MemberId) {
    use crate::components::direct_messages::HIDDEN_DM_THREADS;

    // Record the tombstone BEFORE deferring the signal mutation, so a
    // delegate GetResponse landing between this call and the deferred
    // signal write still sees the suppression (Codex P3 #261).
    // Unconditional insert: even if the in-memory signal has no entry
    // right now (because hydration hasn't completed yet), we want
    // future hydrations to skip any matching entry.
    if let Ok(mut s) = RECENTLY_UNHIDDEN.lock() {
        s.insert((room_owner_vk, peer));
    }

    crate::util::defer(move || {
        HIDDEN_DM_THREADS.with_mut(|hidden| {
            hidden.remove(&(room_owner_vk, peer));
        });
        // Always queue a save (even if no in-memory entry existed at
        // the moment of removal): the recently-unhidden tombstone has
        // been set, and persisting the now-current `hidden_threads`
        // slice (without this pair) is what makes the unhide survive
        // a session restart. Without an unconditional save, a hidden
        // entry still sitting in the delegate from a prior session
        // would resurrect on the next reload — the in-memory
        // tombstone is session-only by design.
        crate::util::safe_spawn_local(async {
            if let Err(e) = save_outbound_dms_to_delegate().await {
                warn!("Failed to persist unhide-DM-thread update: {}", e);
            }
        });
    });
}

/// Coalescing state for [`save_outbound_dms_to_delegate`]. Single-flight
/// gate: without serialization, two `safe_spawn_local(save…)` calls
/// would race and whichever's StoreRequest landed at the delegate LAST
/// would silently overwrite earlier snapshots (skeptical-review
/// IMPORTANT lost-update finding on PR #259). The TOCTOU rationale
/// that drove the move from `IN_FLIGHT`/`DIRTY` atomics to this shape
/// lives on [`CoalesceState::mutex`]. Paired with [`ROOMS_SAVE_STATE`]
/// (freenet/river#246 extracted the shared primitive).
static OUTBOUND_DMS_SAVE_STATE: CoalesceState = CoalesceState::new();

/// Serialize the current [`OUTBOUND_DMS`] cache and persist it via the
/// chat delegate. Caller is responsible for having already mutated the
/// cache before invoking this. Fire-and-forget at most call sites —
/// the StoreResponse comes back through the normal message loop and is
/// logged but not awaited on the hot path.
///
/// Coalesced via [`coalesce_save`] (was a hand-rolled copy of the same
/// pattern until freenet/river#246 extracted the primitive): a chain
/// of N rapid mutations produces at most 2 delegate writes, and a
/// queued caller whose own loop runs zero iterations still returns the
/// authoritative result of the catch-up save that covered its
/// mutation.
pub async fn save_outbound_dms_to_delegate() -> Result<(), String> {
    coalesce_save(
        &OUTBOUND_DMS_SAVE_STATE,
        "Outbound-DMs",
        do_save_outbound_dms_to_delegate,
    )
    .await
}

async fn do_save_outbound_dms_to_delegate() -> Result<(), String> {
    use crate::components::direct_messages::{HIDDEN_DM_THREADS, OUTBOUND_DMS};

    let store = {
        let cache = OUTBOUND_DMS.read();
        let mut entries: Vec<OutboundDmEntry> = cache.by_token.values().cloned().collect();
        // Stable order keeps the saved blob byte-identical across runs
        // when no entries changed, so a save that's a no-op on disk
        // doesn't churn the delegate's "modified" bookkeeping.
        entries.sort_by(|a, b| {
            a.room_owner_vk
                .cmp(&b.room_owner_vk)
                .then_with(|| a.recipient.cmp(&b.recipient))
                .then_with(|| a.purge_token.0.cmp(&b.purge_token.0))
        });
        drop(cache);

        // Snapshot the hide-list (#261) under its own guard, then sort
        // for the same byte-identity rationale.
        let mut hidden_threads = {
            let hidden = HIDDEN_DM_THREADS.read();
            hidden.values().cloned().collect::<Vec<_>>()
        };
        hidden_threads.sort_by(|a, b| {
            a.room_owner_vk
                .cmp(&b.room_owner_vk)
                .then_with(|| a.peer.cmp(&b.peer))
        });

        OutboundDmStore {
            entries,
            hidden_threads,
        }
    };

    let mut buffer = Vec::new();
    ciborium::ser::into_writer(&store, &mut buffer)
        .map_err(|e| format!("Failed to serialize outbound DMs: {}", e))?;

    let request = ChatDelegateRequestMsg::StoreRequest {
        key: ChatDelegateKey::new(OUTBOUND_DMS_STORAGE_KEY.to_vec()),
        value: buffer,
    };

    match send_delegate_request(request).await {
        Ok(ChatDelegateResponseMsg::StoreResponse { result, .. }) => result,
        Ok(other) => Err(format!("Unexpected response: {:?}", other)),
        Err(e) => Err(e),
    }
}

/// Insert a freshly-sent outbound DM into the in-memory cache and
/// queue a delegate write. Enforces the same per-pair cap the contract
/// applies: if `(room, recipient)` already has
/// `MAX_DM_MESSAGES_PER_PAIR` entries cached, the oldest is evicted so
/// the cache stays bounded.
///
/// All signal mutation goes through [`crate::util::defer`] per the
/// WASM signal-safety rules.
pub fn save_outbound_dm(
    room_owner_vk: ed25519_dalek::VerifyingKey,
    sender: MemberId,
    recipient: MemberId,
    purge_token: PurgeToken,
    timestamp: u64,
    plaintext: String,
) {
    use crate::components::direct_messages::OUTBOUND_DMS;

    let entry = OutboundDmEntry {
        room_owner_vk: room_owner_vk.to_bytes(),
        sender,
        recipient,
        purge_token,
        timestamp,
        plaintext,
    };

    crate::util::defer(move || {
        OUTBOUND_DMS.with_mut(|cache| {
            cache.by_token.insert(
                (room_owner_vk, entry.recipient, entry.purge_token),
                entry.clone(),
            );

            // Per-pair cap eviction: drop the oldest entries for this
            // (room, sender, recipient) tuple until we are back under
            // the contract's cap.
            let room_bytes = room_owner_vk.to_bytes();
            let mut pair_entries: Vec<_> = cache
                .by_token
                .iter()
                .filter(|(_, e)| {
                    e.room_owner_vk == room_bytes
                        && e.sender == entry.sender
                        && e.recipient == entry.recipient
                })
                .map(|(k, e)| (*k, e.timestamp))
                .collect();
            if pair_entries.len() > MAX_DM_MESSAGES_PER_PAIR {
                pair_entries.sort_by_key(|(_, ts)| *ts);
                let drop_count = pair_entries.len() - MAX_DM_MESSAGES_PER_PAIR;
                for (key, _) in pair_entries.into_iter().take(drop_count) {
                    cache.by_token.remove(&key);
                }
            }
        });

        crate::util::safe_spawn_local(async {
            if let Err(e) = save_outbound_dms_to_delegate().await {
                warn!("Failed to persist outbound DM cache: {}", e);
            }
        });
    });
}

/// Drop cached outbound-DM entries whose token appears in some
/// recipient's purge envelope on a loaded room. This is the explicit
/// tombstone signal — the recipient signed an `AuthorizedRecipientPurges`
/// listing this token, so the contract has already dropped the
/// matching ciphertext via `post_apply_cleanup` and we should drop the
/// matching plaintext in lockstep.
///
/// Called from a `use_effect` in `App()` that subscribes to ROOMS, so
/// it fires after every room-state mutation. Persists the trimmed
/// cache if anything was actually removed.
///
/// **Why we ONLY act on purge envelopes (not on "ciphertext missing
/// from `direct_messages.messages`")** — original PR-review BLOCKING
/// finding: prune-clobbers-cache on cold start. If we pruned on the
/// negative "no longer in `messages`" signal, then a cold-start
/// sequence where `OUTBOUND_DMS` hydrates BEFORE the room contract
/// state finishes loading (which can happen any time `rooms_data` and
/// `outbound_dms` were last saved in different orders, or when the
/// network-backed `direct_messages` state lags behind the delegate's
/// `outbound_dms` blob) would wipe every cached entry and persist the
/// empty result — destroying the user's outbound plaintext history.
///
/// Purge envelopes don't have that race: a recipient envelope is
/// monotonically versioned and never disappears, so its presence is
/// always a true tombstone signal regardless of how fresh the rest of
/// the room state is.
pub fn prune_outbound_dms_for_purges() {
    use crate::components::direct_messages::OUTBOUND_DMS;

    // Collect the tombstoned `(room, recipient, token)` triples from
    // every loaded room's purge envelopes. `try_read` so we cooperate
    // with any in-flight write.
    let purged_keys: std::collections::HashSet<(
        ed25519_dalek::VerifyingKey,
        MemberId,
        PurgeToken,
    )> = {
        let Ok(rooms) = ROOMS.try_read() else {
            return;
        };
        let mut keys = std::collections::HashSet::new();
        for (owner_vk, room_data) in &rooms.map {
            for envelope in &room_data.room_state.direct_messages.purges {
                for token in &envelope.state.purged {
                    keys.insert((*owner_vk, envelope.recipient_id, *token));
                }
            }
        }
        keys
    };
    if purged_keys.is_empty() {
        return;
    }

    // Compute the intersection via a read-only borrow so we can skip
    // the entire defer/with_mut path when nothing needs removal.
    // Skipping the `with_mut` is required for the App() use_effect
    // not to loop on its own writes — see fn doc.
    let to_remove: Vec<(ed25519_dalek::VerifyingKey, MemberId, PurgeToken)> = {
        let Ok(cache) = OUTBOUND_DMS.try_read() else {
            return;
        };
        cache
            .by_token
            .keys()
            .filter(|k| purged_keys.contains(k))
            .copied()
            .collect()
    };
    if to_remove.is_empty() {
        return;
    }

    let removed = to_remove.len();
    crate::util::defer(move || {
        OUTBOUND_DMS.with_mut(|cache| {
            for key in &to_remove {
                cache.by_token.remove(key);
            }
        });
        info!(
            "Pruned {} outbound-DM cache entries against purge envelopes",
            removed
        );
        crate::util::safe_spawn_local(async {
            if let Err(e) = save_outbound_dms_to_delegate().await {
                warn!("Failed to persist pruned outbound DM cache: {}", e);
            }
        });
    });
}

fn create_chat_delegate_container() -> DelegateContainer {
    let delegate_bytes = include_bytes!("../../../public/contracts/chat_delegate.wasm");
    let delegate_code = DelegateCode::from(delegate_bytes.to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    DelegateContainer::Wasm(DelegateWasmAPIVersion::V1(delegate))
}

/// Extract the key from a request message for tracking purposes.
/// Pending-request correlation keys for the compare-and-swap storage ops
/// (freenet/river#345). These deliberately differ from the plain storage-key
/// bytes so an in-flight CAS save's response slot cannot be stolen by a
/// concurrent plain `GetResponse`/`StoreResponse` for the SAME storage key —
/// e.g. the cold-start rooms load (a fire-and-forget `GetRequest{rooms_data}`)
/// landing while a CAS save for `rooms_data` is awaiting. The delegate echoes
/// the original `key` in its response, so the response handler rebuilds the
/// same correlation key via these helpers.
pub(crate) fn cas_store_correlation_key(key: &ChatDelegateKey) -> Vec<u8> {
    let mut k = b"__cas_store__:".to_vec();
    k.extend_from_slice(key.as_bytes());
    k
}

pub(crate) fn get_versioned_correlation_key(key: &ChatDelegateKey) -> Vec<u8> {
    let mut k = b"__get_versioned__:".to_vec();
    k.extend_from_slice(key.as_bytes());
    k
}

fn get_request_key(request: &ChatDelegateRequestMsg) -> Vec<u8> {
    match request {
        // Key-value storage operations
        ChatDelegateRequestMsg::StoreRequest { key, .. } => key.as_bytes().to_vec(),
        ChatDelegateRequestMsg::GetRequest { key } => key.as_bytes().to_vec(),
        ChatDelegateRequestMsg::DeleteRequest { key } => key.as_bytes().to_vec(),
        ChatDelegateRequestMsg::ListRequest => b"__list_request__".to_vec(),
        // CAS storage ops use DISTINCT correlation keys (freenet/river#345)
        // so a concurrent plain Get/Store response for the same storage key
        // can't steal the awaiting save's slot. See the helper doc-comments.
        ChatDelegateRequestMsg::GetVersionedRequest { key } => get_versioned_correlation_key(key),
        ChatDelegateRequestMsg::CasStoreRequest { key, .. } => cas_store_correlation_key(key),

        // Signing key management
        ChatDelegateRequestMsg::StoreSigningKey { room_key, .. } => {
            let mut key = SIGNING_KEY_PREFIX.to_vec();
            key.extend_from_slice(room_key);
            key
        }
        ChatDelegateRequestMsg::GetPublicKey { room_key } => {
            let mut key = PUBLIC_KEY_PREFIX.to_vec();
            key.extend_from_slice(room_key);
            key
        }

        // Signing operations - use prefix + room_key + request_id for uniqueness
        ChatDelegateRequestMsg::SignMessage {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignMember {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignBan {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignConfig {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignMemberInfo {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignSecretVersion {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignEncryptedSecret {
            room_key,
            request_id,
            ..
        }
        | ChatDelegateRequestMsg::SignUpgrade {
            room_key,
            request_id,
            ..
        } => {
            let mut key = SIGN_PREFIX.to_vec();
            key.extend_from_slice(room_key);
            key.extend_from_slice(&request_id.to_le_bytes());
            key
        }

        // EnsureRoomSubscription: callers now `await` the response so the
        // delegate can refuse with "no signing key on file" and the UI can
        // clear its per-session dedup for a retry. The response is routed via
        // `complete_pending_room_subscription_request` from the response
        // handler. The key includes `request_id` so concurrent or sequential
        // calls for the same `room_owner_vk` can't collide on the same
        // pending-request slot (PR #276 review feedback).
        ChatDelegateRequestMsg::EnsureRoomSubscription {
            room_owner_vk,
            request_id,
            ..
        } => {
            let mut key = ROOM_SUBSCRIPTION_PREFIX.to_vec();
            key.extend_from_slice(room_owner_vk);
            key.extend_from_slice(&request_id.to_le_bytes());
            key
        }
    }
}

pub async fn send_delegate_request(
    request: ChatDelegateRequestMsg,
) -> Result<ChatDelegateResponseMsg, String> {
    debug!("Sending delegate request: {:?}", request);

    // Get the key bytes for tracking this request
    let key_bytes = get_request_key(&request);

    // Create a oneshot channel to receive the response
    let (sender, receiver) = oneshot::channel();

    // Register the pending request
    {
        let mut pending = PENDING_REQUESTS
            .lock()
            .map_err(|e| format!("Failed to lock pending requests: {}", e))?;
        pending.insert(key_bytes.clone(), sender);
    }

    // Serialize the request
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&request, &mut payload)
        .map_err(|e| format!("Failed to serialize request: {}", e))?;

    info!("Serialized request payload size: {} bytes", payload.len());

    // Reuse the session-cached delegate key (freenet/river#246) — re-hashing
    // the 710 KB delegate WASM here on every call was a per-request fixed
    // cost that, multiplied across the cold-open burst, contributed
    // measurably to the memory peak. The delegate code is invariant for
    // the session, so the key only needs to be computed once.
    let delegate_key = CHAT_DELEGATE_KEY.clone(); // Get the delegate key for targeting the delegate request

    let app_msg = freenet_stdlib::prelude::ApplicationMessage::new(payload);

    // Prepare the delegate request, targeting the delegate using its key
    let delegate_request = DelegateOp(DelegateRequest::ApplicationMessages {
        key: delegate_key, // Target the delegate instance
        params: Parameters::from(Vec::<u8>::new()),
        inbound: vec![freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(app_msg)],
    });

    // Get the API and send the request, releasing the lock before awaiting
    let api_result = {
        let mut web_api = WEB_API.write();
        if let Some(api) = web_api.as_mut() {
            // Send the request while holding the lock
            api.send(delegate_request).await
        } else {
            Err(freenet_stdlib::client_api::Error::ConnectionClosed)
        }
    };

    // Handle send errors - remove the pending request if send failed
    if let Err(e) = api_result {
        if let Ok(mut pending) = PENDING_REQUESTS.lock() {
            pending.remove(&key_bytes);
        }
        return Err(format!("Failed to send delegate request: {}", e));
    }

    info!("Request sent, waiting for response...");

    // Wait for the response with a timeout
    // Use WASM-compatible sleep from util module
    // Delegate runs locally on the same node, so responses should be near-instant.
    // However, first-time WASM compilation on slow mobile devices can take several
    // seconds. 10s balances responsiveness with device compatibility.
    let timeout = Box::pin(crate::util::sleep(std::time::Duration::from_secs(10)));

    match select(receiver, timeout).await {
        Either::Left((response, _)) => match response {
            Ok(resp) => {
                debug!("Received delegate response: {:?}", resp);
                Ok(resp)
            }
            Err(_) => Err("Response channel was cancelled".to_string()),
        },
        Either::Right((_, _)) => {
            // Timeout occurred - remove the pending request
            if let Ok(mut pending) = PENDING_REQUESTS.lock() {
                pending.remove(&key_bytes);
            }
            Err("Timeout waiting for delegate response".to_string())
        }
    }
}

// =============================================================================
// LEGACY DELEGATE MIGRATION IMPLEMENTATION
// =============================================================================

/// The action to take after the **current** delegate's `GetResponse` for
/// `rooms_data` has been observed. See `decide_legacy_migration_action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegacyMigrationAction {
    /// Current delegate has authoritative state (rooms or tombstones). Mark
    /// legacy migration done so it never fires for this user.
    MarkDone,
    /// Current delegate is empty (no record, or a placeholder serialization
    /// with no rooms and no tombstones). Fire the legacy probes — there is
    /// nothing in current to clobber.
    FireMigration,
}

/// Decide what to do with legacy migration based on the **current** delegate's
/// `GetResponse` for `rooms_data`.
///
/// This encodes the gating invariant from freenet/river#253:
/// - If the current delegate holds rooms or tombstones, it is the source of
///   truth and legacy migration must be blocked permanently — a stale legacy
///   snapshot would otherwise overwrite this newer state.
/// - If the current delegate is empty (no record, or just a placeholder
///   serialization with no rooms and no tombstones), legacy migration is
///   safe to fire because there is nothing to clobber.
///
/// `current_has_rooms` is `true` if the current delegate returned at least
/// one entry in `Rooms::map`. `current_has_tombstones` is `true` if at least
/// one entry exists in `Rooms::removed_rooms`. `value_present` is `true` if
/// the `GetResponse` carried `Some(_)` (regardless of whether the bytes
/// deserialize to a non-empty `Rooms`).
pub(crate) fn decide_legacy_migration_action(
    value_present: bool,
    current_has_rooms: bool,
    current_has_tombstones: bool,
) -> LegacyMigrationAction {
    if value_present && (current_has_rooms || current_has_tombstones) {
        LegacyMigrationAction::MarkDone
    } else {
        LegacyMigrationAction::FireMigration
    }
}

/// localStorage key PREFIX to track whether legacy migration has been
/// attempted. The actual key is suffixed with the fingerprint of the
/// current [`LEGACY_DELEGATES`] set so that adding a new legacy entry
/// invalidates the old flag automatically — Codex P1 finding on PR
/// #259: without per-set scoping, every delegate WASM bump (which
/// adds a new entry to `legacy_delegates.toml`) is silently blocked
/// for any user who already migrated under a previous set, making
/// their delegate-backed rooms appear lost on every upgrade.
#[allow(dead_code)]
const LEGACY_MIGRATION_FLAG_PREFIX: &str = "river_legacy_migration_done:";

/// BLAKE3 fingerprint (first 16 hex chars) of the current
/// `LEGACY_DELEGATES` set. Used as the localStorage key suffix so the
/// migration-done flag is per-set, not global.
#[allow(dead_code)]
fn legacy_set_fingerprint() -> String {
    let mut hasher = blake3::Hasher::new();
    for (key_bytes, code_hash) in LEGACY_DELEGATES {
        hasher.update(key_bytes);
        hasher.update(code_hash);
    }
    let hash = hasher.finalize();
    let hex = hash.to_hex();
    hex[..16].to_string()
}

#[allow(dead_code)]
fn legacy_migration_flag_key() -> String {
    format!(
        "{}{}",
        LEGACY_MIGRATION_FLAG_PREFIX,
        legacy_set_fingerprint()
    )
}

/// Check if legacy migration has already been done for the CURRENT
/// legacy-delegate set (via localStorage). Per-set scoping means
/// adding a new legacy entry on every WASM bump automatically
/// invalidates the previous flag and re-enables migration.
fn is_legacy_migration_done() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            if let Ok(Some(storage)) = window.local_storage() {
                let key = legacy_migration_flag_key();
                return storage.get_item(&key).ok().flatten().is_some();
            }
        }
        false
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // Non-WASM: assume migration not needed (testing environment)
        true
    }
}

/// Mark legacy migration as done for the current set in localStorage.
pub fn mark_legacy_migration_done() {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            if let Ok(Some(storage)) = window.local_storage() {
                let key = legacy_migration_flag_key();
                if let Err(e) = storage.set_item(&key, "true") {
                    warn!("Failed to set legacy migration flag: {:?}", e);
                } else {
                    info!("Legacy migration marked as done (key={})", key);
                }
            }
        }
    }
}

/// Guard to prevent repeated legacy migration attempts within the same session.
/// The migration is fire-and-forget: if the legacy delegates don't exist on the node,
/// the node returns "delegate not found" errors that never reach the response handler's
/// success path, so `mark_legacy_migration_done()` is never called. Without this guard,
/// every WebSocket reconnect would re-fire all 3 legacy delegate requests, creating a
/// tight error spam loop (~9 errors every 3 seconds).
static LEGACY_MIGRATION_ATTEMPTED: AtomicBool = AtomicBool::new(false);

/// Fire requests to load rooms from all known legacy delegates (fire and forget).
/// If any legacy delegate has room data, the response handler will migrate it.
///
/// **Must only be called once the current delegate has confirmed it has no
/// rooms_data** (see freenet/river#253). Firing this while the current delegate
/// may have data is unsafe because a legacy response can trigger a save that
/// overwrites the current delegate's storage before its GET response arrives,
/// destroying rooms the user created after the last legacy snapshot.
pub(crate) async fn fire_legacy_migration_request() {
    // Check if migration has already been done (persistent across sessions)
    if is_legacy_migration_done() {
        info!("Legacy migration already done, skipping");
        return;
    }

    // Only attempt migration once per session to avoid error spam on reconnect.
    // If the legacy delegates aren't installed, retrying won't help.
    if LEGACY_MIGRATION_ATTEMPTED.swap(true, Ordering::Relaxed) {
        info!("Legacy migration already attempted this session, skipping");
        return;
    }

    info!(
        "Attempting to migrate data from {} legacy delegate(s)",
        LEGACY_DELEGATES.len()
    );

    for (i, (key_bytes, code_hash_bytes)) in LEGACY_DELEGATES.iter().enumerate() {
        let legacy_code_hash = CodeHash::new(*code_hash_bytes);
        let legacy_delegate_key = DelegateKey::new(*key_bytes, legacy_code_hash);

        // Send a GetRequest for each storage key we want to migrate.
        // rooms_data is the original and gates the migration; outbound_dms
        // (#256) is mirrored so a delegate rebuild doesn't orphan the
        // sender's own DM plaintext.
        let storage_keys: [&[u8]; 2] = [ROOMS_STORAGE_KEY, OUTBOUND_DMS_STORAGE_KEY];
        for storage_key in storage_keys {
            let request = ChatDelegateRequestMsg::GetRequest {
                key: ChatDelegateKey::new(storage_key.to_vec()),
            };

            let mut payload = Vec::new();
            if let Err(e) = ciborium::ser::into_writer(&request, &mut payload) {
                error!(
                    "Failed to serialize legacy migration request #{} for key {:?}: {}",
                    i,
                    String::from_utf8_lossy(storage_key),
                    e
                );
                continue;
            }

            let app_msg = freenet_stdlib::prelude::ApplicationMessage::new(payload);

            let delegate_request = DelegateOp(DelegateRequest::ApplicationMessages {
                key: legacy_delegate_key.clone(),
                params: Parameters::from(Vec::<u8>::new()),
                inbound: vec![
                    freenet_stdlib::prelude::InboundDelegateMsg::ApplicationMessage(app_msg),
                ],
            });

            let api_result = {
                let mut web_api = WEB_API.write();
                if let Some(api) = web_api.as_mut() {
                    api.send(delegate_request).await
                } else {
                    Err(freenet_stdlib::client_api::Error::ConnectionClosed)
                }
            };

            match api_result {
                Ok(_) => info!(
                    "Legacy migration request #{} sent for key {:?}",
                    i,
                    String::from_utf8_lossy(storage_key)
                ),
                Err(e) => {
                    info!(
                        "Could not send legacy migration request #{} for key {:?} \
                         (expected if delegate not present): {}",
                        i,
                        String::from_utf8_lossy(storage_key),
                        e
                    );
                }
            }
        }
    }
}
