use crate::components::app::{CURRENT_ROOM, ROOMS, WEB_API};
use dioxus::logger::tracing::{debug, error, info, warn};
use dioxus::prelude::*;
use freenet_stdlib::client_api::ClientRequest::DelegateOp;
use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::prelude::{
    CodeHash, Delegate, DelegateCode, DelegateContainer, DelegateKey, DelegateWasmAPIVersion,
    Parameters,
};
use futures::channel::oneshot;
use futures::future::{select, Either};
use river_core::chat_delegate::{
    ChatDelegateKey, ChatDelegateRequestMsg, ChatDelegateResponseMsg, HiddenDmThreadEntry,
    OutboundDmEntry, OutboundDmStore, RequestId, RoomKey,
};
use river_core::room_state::direct_messages::{PurgeToken, MAX_DM_MESSAGES_PER_PAIR};
use river_core::room_state::member::MemberId;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

// Constant for the rooms storage key
pub const ROOMS_STORAGE_KEY: &[u8] = b"rooms_data";

// Re-export so other UI modules don't have to reach into `river_core` for the key.
pub use river_core::chat_delegate::OUTBOUND_DMS_STORAGE_KEY;

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
            fire_load_rooms_request().await;
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

/// Fire a request to load rooms from delegate storage without waiting for response.
/// The response will be handled by the response_handler through the message loop.
/// This avoids deadlock when called from inside the message loop.
async fn fire_load_rooms_request() {
    info!("Firing request to load rooms from delegate storage");

    let request = ChatDelegateRequestMsg::GetRequest {
        key: ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
    };

    // Serialize and send the request without waiting for response
    let mut payload = Vec::new();
    if let Err(e) = ciborium::ser::into_writer(&request, &mut payload) {
        error!("Failed to serialize load rooms request: {}", e);
        return;
    }

    let delegate_code =
        DelegateCode::from(include_bytes!("../../../public/contracts/chat_delegate.wasm").to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    let delegate_key = delegate.key().clone();

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
        error!("Failed to send load rooms request: {}", e);
    } else {
        info!("Load rooms request sent, response will be handled by message loop");
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

/// Save rooms to the delegate storage
pub async fn save_rooms_to_delegate() -> Result<(), String> {
    info!("Saving rooms to delegate storage");

    // Get the current rooms data - clone the data to avoid holding the read lock
    let rooms_data = {
        let mut rooms_clone = ROOMS.read().clone();
        // Include the current room selection
        rooms_clone.current_room_key = CURRENT_ROOM.read().owner_key;
        let mut buffer = Vec::new();
        ciborium::ser::into_writer(&rooms_clone, &mut buffer)
            .map_err(|e| format!("Failed to serialize rooms: {}", e))?;
        buffer
    };

    // Create a store request for the rooms data
    let request = ChatDelegateRequestMsg::StoreRequest {
        key: ChatDelegateKey::new(ROOMS_STORAGE_KEY.to_vec()),
        value: rooms_data,
    };

    // Send the request to the delegate
    match send_delegate_request(request).await {
        Ok(ChatDelegateResponseMsg::StoreResponse { result, .. }) => result,
        Ok(other) => Err(format!("Unexpected response: {:?}", other)),
        Err(e) => Err(e),
    }
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
/// response. Mirrors [`fire_load_rooms_request`] — the response is
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

    let delegate_code =
        DelegateCode::from(include_bytes!("../../../public/contracts/chat_delegate.wasm").to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    let delegate_key = delegate.key().clone();

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

/// Single-flight gate for [`save_outbound_dms_to_delegate`]. Without
/// serialization, two `safe_spawn_local(save…)` calls can race: each
/// snapshots the cache before its async `send_delegate_request().await`,
/// and whichever's StoreRequest lands at the delegate LAST wins —
/// silently losing entries that only made it into the earlier snapshot
/// (skeptical-review IMPORTANT lost-update finding on PR #259).
///
/// Implementation: a global `futures::lock::Mutex` serializes the
/// critical section, plus a dirty flag for coalescing. Pattern:
///   1. Caller sets `DIRTY = true`.
///   2. Caller awaits the mutex.
///   3. Inside the critical section, loop: clear DIRTY, snapshot
///      cache, send to delegate, await result. If DIRTY got set
///      again during the round-trip, loop. Else release.
///
/// This pattern (vs. the earlier `AtomicBool` IN_FLIGHT + DIRTY
/// pair) avoids the TOCTOU window between "swap DIRTY -> false"
/// and "store IN_FLIGHT -> false" that Codex flagged as a P2
/// race on PR #259's first re-review: in that window a concurrent
/// caller could see IN_FLIGHT == true, set DIRTY = true, and
/// return, while the in-flight save proceeded to release
/// IN_FLIGHT without re-checking DIRTY — stranding the dirty
/// update with no save running. The mutex+dirty-flag pattern
/// holds the mutex across the entire dirty-check so the race
/// window does not exist.
static OUTBOUND_DMS_SAVE_MUTEX: futures::lock::Mutex<()> = futures::lock::Mutex::new(());
static OUTBOUND_DMS_SAVE_DIRTY: AtomicBool = AtomicBool::new(false);

/// Serialize the current [`OUTBOUND_DMS`] cache and persist it via the
/// chat delegate. Caller is responsible for having already mutated the
/// cache before invoking this. Fire-and-forget at most call sites —
/// the StoreResponse comes back through the normal message loop and is
/// logged but not awaited on the hot path.
///
/// Concurrency: every caller marks the cache "dirty" and then queues
/// behind the mutex. The first caller through the mutex drains all
/// queued dirty work via the inner loop, so a chain of N rapid
/// mutations produces at most 2 delegate writes (the first one,
/// plus one final catch-up that observes dirty-set after the round
/// trip).
pub async fn save_outbound_dms_to_delegate() -> Result<(), String> {
    OUTBOUND_DMS_SAVE_DIRTY.store(true, Ordering::Release);
    let _guard = OUTBOUND_DMS_SAVE_MUTEX.lock().await;

    let mut last_result: Result<(), String> = Ok(());
    while OUTBOUND_DMS_SAVE_DIRTY.swap(false, Ordering::AcqRel) {
        let result = do_save_outbound_dms_to_delegate().await;
        if let Err(e) = &result {
            warn!(
                "Outbound-DMs save failed mid-coalesce, will retry latest snapshot: {}",
                e
            );
        }
        last_result = result;
    }
    last_result
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
fn get_request_key(request: &ChatDelegateRequestMsg) -> Vec<u8> {
    match request {
        // Key-value storage operations
        ChatDelegateRequestMsg::StoreRequest { key, .. } => key.as_bytes().to_vec(),
        ChatDelegateRequestMsg::GetRequest { key } => key.as_bytes().to_vec(),
        ChatDelegateRequestMsg::DeleteRequest { key } => key.as_bytes().to_vec(),
        ChatDelegateRequestMsg::ListRequest => b"__list_request__".to_vec(),

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

    let delegate_code =
        DelegateCode::from(include_bytes!("../../../public/contracts/chat_delegate.wasm").to_vec());
    let params = Parameters::from(Vec::<u8>::new());
    let delegate = Delegate::from((&delegate_code, &params));
    let delegate_key = delegate.key().clone(); // Get the delegate key for targeting the delegate request

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
