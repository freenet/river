//! Backward-probe recovery for rooms stranded under an older room-contract
//! generation (freenet/river#292).
//!
//! The room contract key is `BLAKE3(room_contract.wasm, params)`. Every
//! room-contract WASM upgrade moves the key for every owner, so a room that
//! was dormant across one or more upgrades has its live state stranded under
//! an older-generation contract key.
//!
//! ## Design principle (the same gating the chat delegate uses — #253)
//!
//! **The current contract key is always authoritative.** The backward probe
//! fires ONLY when the current key has no usable state:
//!
//! * Current key has state → adopt it. Never probe backward.
//! * Current key empty/absent → probe legacy keys newest-to-oldest. The first
//!   that returns real state is the room's last-active state; adopt it AND PUT
//!   it forward onto the current key so the room is no longer stranded.
//! * Neither current nor any legacy key has state → only then is the device's
//!   local snapshot PUT to seed the current key.
//!
//! ## Routing & bookkeeping
//!
//! A GET response for a *legacy* contract key cannot be resolved back to an
//! `owner_vk` via `SYNC_INFO` (keyed by the current contract id) or via
//! `RoomData::contract_key` (also the current id). [`BACKWARD_PROBES`] is the
//! side-table that maps a probe's legacy `ContractInstanceId` back to the probe
//! it belongs to.
//!
//! It is a plain `Mutex`-guarded map, NOT a Dioxus signal: it is internal
//! bookkeeping with zero UI reactivity (no component or memo ever reads it),
//! so it must not carry signal semantics. This is the same shape as
//! `chat_delegate::ENSURE_SUBSCRIPTION_SENT`. Using a plain `Mutex` means
//! mutations need no `defer()` and cannot cause a re-entrant signal borrow.
//!
//! ## Liveness — every probe GET has a watchdog
//!
//! A legacy generation whose contract has been garbage-collected from the
//! network may never produce a `GetResponse`. Every probe GET is therefore
//! paired with a [`PROBE_GET_TIMEOUT`] watchdog: if the response has not
//! arrived by then, the watchdog synthesizes an empty response so the probe
//! advances to the next generation (and ultimately seeds the current key)
//! rather than stalling forever. Without this a dormant room — exactly what
//! this feature targets — could be left permanently stuck.

use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::util::{owner_vk_to_legacy_contract_keys, safe_spawn_local, sleep, to_cbor_vec};
use dioxus::logger::tracing::{info, warn};
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
use freenet_stdlib::prelude::{ContractInstanceId, ContractKey};
use river_core::room_state::ChatRoomStateV1;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;

/// Hard cap on how many legacy generations a single probe will walk before
/// giving up. `owner_vk_to_legacy_contract_keys` is already bounded by the
/// size of the legacy registry; this is defence-in-depth against a runaway
/// chain if the registry ever grows unexpectedly large.
const MAX_PROBE_HOPS: usize = 64;

/// How long to wait for a probe GET response before treating the legacy
/// generation as absent and advancing. An existing contract responds well
/// inside this; only a garbage-collected one runs the timeout down.
const PROBE_GET_TIMEOUT: Duration = Duration::from_secs(12);

/// State of an in-progress backward probe for one room.
#[derive(Clone)]
pub struct ProbeState {
    /// The owner whose stranded room we're recovering.
    pub owner_vk: VerifyingKey,
    /// Legacy contract keys not yet tried, ordered newest-first. The key
    /// currently outstanding (whose GET we're awaiting) is NOT in this list —
    /// it is the `BACKWARD_PROBES` map key.
    pub remaining: Vec<ContractKey>,
    /// Hops taken so far, for the [`MAX_PROBE_HOPS`] cap.
    pub hops: usize,
    /// The device's local room snapshot, captured when the probe started.
    /// Used to (a) CRDT-merge with any recovered state before PUTting it
    /// forward, so unsynced local edits are not dropped, and (b) seed the
    /// current key as the genuine last resort if every generation is empty.
    /// Captured up front so the seed path never depends on a fallible signal
    /// read at probe-completion time.
    pub local_snapshot: ChatRoomStateV1,
    /// Unique token for this outstanding GET. The timeout watchdog captures it
    /// and only fires if the entry still carries the SAME epoch — so a stale
    /// watchdog from a completed probe cannot consume a later probe's entry
    /// that happens to re-use the same legacy contract id.
    pub epoch: u64,
}

/// Monotonic source of [`ProbeState::epoch`] values.
static PROBE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Maps the `ContractInstanceId` of the currently-outstanding legacy GET to
/// the probe it belongs to. Plain `Mutex` map — see the module docs.
static BACKWARD_PROBES: LazyLock<Mutex<HashMap<ContractInstanceId, ProbeState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Lock the probe table, recovering from a poisoned mutex (a panic while the
/// lock was held must not wedge all future recovery).
fn probes() -> MutexGuard<'static, HashMap<ContractInstanceId, ProbeState>> {
    BACKWARD_PROBES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether `instance_id` is the contract id of an outstanding backward-probe
/// GET. Used by `handle_get_response` to route legacy-key responses.
pub fn is_probe_instance(instance_id: &ContractInstanceId) -> bool {
    probes().contains_key(instance_id)
}

/// Whether the outstanding probe for `instance_id` still carries `epoch`. The
/// timeout watchdog uses this so it acts only on the exact probe GET it was
/// armed for — never on a later probe that re-used the same contract id.
fn probe_has_epoch(instance_id: &ContractInstanceId, epoch: u64) -> bool {
    probes().get(instance_id).map(|p| p.epoch) == Some(epoch)
}

/// Take (remove) the probe entry for `instance_id`, if any. The caller has
/// just received (or synthesized, on timeout) the GET response for that
/// legacy key and is now responsible for either recovering the state or
/// advancing to the next hop.
pub fn take_probe(instance_id: &ContractInstanceId) -> Option<ProbeState> {
    probes().remove(instance_id)
}

/// Start a backward probe for `owner_vk`'s room: GET the newest legacy
/// contract key. Called when the current-key GET came back empty.
///
/// `local_snapshot` is the device's current room state, captured by the
/// caller; it rides along in [`ProbeState`] for the merge / last-resort seed.
///
/// Returns `true` if a probe is in flight (one was started, or one was
/// already running for this owner) — the caller must then NOT seed the
/// current key; the probe owns recovery-or-seed. Returns `false` only when
/// there are no legacy generations at all, in which case the caller falls
/// back to seeding the current key itself.
pub fn start_backward_probe(owner_vk: VerifyingKey, local_snapshot: ChatRoomStateV1) -> bool {
    let mut legacy_keys = owner_vk_to_legacy_contract_keys(&owner_vk);
    if legacy_keys.is_empty() {
        info!(
            "No legacy room-contract generations — skipping backward probe for {:?}",
            river_core::room_state::member::MemberId::from(owner_vk)
        );
        return false;
    }

    // Don't start a second probe for an owner that already has one running.
    // Return `true` so the caller still treats recovery as in-flight.
    //
    // There is no gap to race here: a probe always has exactly one
    // `BACKWARD_PROBES` entry while in flight. `handle_probe_get_response`
    // runs `take_probe` → `advance_backward_probe` → `fire_probe_get`
    // (which re-inserts the next hop) with NO `.await` in between, so on
    // single-threaded WASM no other task — including this guard — can
    // observe the table mid-advance.
    if probes().values().any(|p| p.owner_vk == owner_vk) {
        info!(
            "Backward probe already in progress for {:?}, not restarting",
            river_core::room_state::member::MemberId::from(owner_vk)
        );
        return true;
    }

    // The newest legacy key becomes the outstanding GET; the rest stay in
    // `remaining` for subsequent hops.
    let first = legacy_keys.remove(0);
    info!(
        "Starting backward probe for {:?}: {} legacy generation(s) to try",
        river_core::room_state::member::MemberId::from(owner_vk),
        legacy_keys.len() + 1
    );
    fire_probe_get(owner_vk, first, legacy_keys, 1, local_snapshot);
    true
}

/// Advance an in-progress probe to its next legacy key. Called when a legacy
/// GET came back empty (or its watchdog fired).
///
/// Returns `true` if the probe advanced (a GET for the next legacy key was
/// fired). Returns `false` when the probe is exhausted — every legacy
/// generation has been tried and none held real state. On `false` the caller
/// performs the last-resort seed of the current contract key with the
/// device's local snapshot.
pub fn advance_backward_probe(state: ProbeState) -> bool {
    let ProbeState {
        owner_vk,
        mut remaining,
        hops,
        local_snapshot,
        epoch: _, // each hop's GET gets a fresh epoch from `fire_probe_get`
    } = state;

    if remaining.is_empty() {
        info!(
            "Backward probe for {:?} exhausted all legacy generations — \
             no stranded state found",
            river_core::room_state::member::MemberId::from(owner_vk)
        );
        return false;
    }
    if hops >= MAX_PROBE_HOPS {
        warn!(
            "Backward probe for {:?} hit MAX_PROBE_HOPS ({}) — aborting",
            river_core::room_state::member::MemberId::from(owner_vk),
            MAX_PROBE_HOPS
        );
        return false;
    }

    let next = remaining.remove(0);
    fire_probe_get(owner_vk, next, remaining, hops + 1, local_snapshot);
    true
}

/// Register `key` as the outstanding probe GET for `owner_vk`, send the GET,
/// and arm a watchdog so an absent legacy generation cannot stall the probe.
fn fire_probe_get(
    owner_vk: VerifyingKey,
    key: ContractKey,
    remaining: Vec<ContractKey>,
    hops: usize,
    local_snapshot: ChatRoomStateV1,
) {
    let instance_id = *key.id();
    let epoch = PROBE_EPOCH.fetch_add(1, Ordering::Relaxed);

    // Register the probe BEFORE sending the GET so the response handler can
    // resolve the legacy key when the reply arrives. Plain mutex — synchronous,
    // no defer, no signal re-entrancy.
    probes().insert(
        instance_id,
        ProbeState {
            owner_vk,
            remaining,
            hops,
            local_snapshot,
            epoch,
        },
    );

    // Keep the room out of the subscription-timeout sweep: a probe in flight
    // IS active subscription progress. Each hop refreshes `subscribing_since`
    // (hops are <= PROBE_GET_TIMEOUT apart, well under REPUT_DELAY_MS), so
    // `rooms_awaiting_subscription` does not reclaim the room mid-probe and
    // re-issue redundant current-key GETs / a duplicate probe.
    crate::util::defer(move || {
        SYNC_INFO.with_mut(|sync_info| {
            sync_info.update_sync_status(&owner_vk, RoomSyncStatus::Subscribing);
        });
    });

    info!(
        "Backward probe hop {} for {:?}: GET legacy contract {}",
        hops,
        river_core::room_state::member::MemberId::from(owner_vk),
        instance_id
    );

    safe_spawn_local(async move {
        let get_request = ContractRequest::Get {
            key: instance_id,
            // Legacy generations need their own WASM cached for the GET to
            // resolve on a node that has never seen that contract.
            return_contract_code: true,
            subscribe: false,
            blocking_subscribe: false,
        };
        let send_result = if let Some(web_api) = crate::components::app::WEB_API.write().as_mut() {
            web_api.send(ClientRequest::ContractOp(get_request)).await
        } else {
            Ok(()) // WebAPI gone — let the watchdog drive the probe forward.
        };
        if let Err(e) = send_result {
            warn!("Failed to send backward-probe GET for {instance_id}: {e}");
            // Leave the entry in place: the watchdog below treats the missing
            // response as empty and advances the probe, so a transient send
            // failure still walks to the next generation rather than silently
            // abandoning recovery.
        }
    });

    // Watchdog: if no real GET response consumes THIS probe entry (matched by
    // epoch, so a stale watchdog from a finished probe can't fire against a
    // later probe that re-used the same contract id) within PROBE_GET_TIMEOUT,
    // synthesize an empty response so the probe advances.
    safe_spawn_local(async move {
        sleep(PROBE_GET_TIMEOUT).await;
        if probe_has_epoch(&instance_id, epoch) {
            warn!(
                "Backward-probe GET for {instance_id} timed out after {}s — \
                 treating the legacy generation as absent and advancing",
                PROBE_GET_TIMEOUT.as_secs()
            );
            // An empty/default state routes through the same handler as a real
            // empty response: it advances the probe, or seeds on exhaustion.
            let empty = to_cbor_vec(&ChatRoomStateV1::default());
            crate::components::app::freenet_api::response_handler::get_response::handle_probe_get_response(
                key, empty,
            )
            .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_owner(seed: u8) -> VerifyingKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32]).verifying_key()
    }

    /// `advance_backward_probe` with an empty `remaining` must report
    /// exhaustion (`false`) — the probe ends, no panic, no further GET. The
    /// caller uses the `false` return to seed the current contract key.
    #[test]
    fn advance_with_empty_remaining_reports_exhausted() {
        let advanced = advance_backward_probe(ProbeState {
            owner_vk: test_owner(3),
            remaining: Vec::new(),
            hops: 1,
            local_snapshot: ChatRoomStateV1::default(),
            epoch: 0,
        });
        assert!(
            !advanced,
            "an exhausted probe must report false so the caller seeds the current key"
        );
    }

    /// A probe that has hit `MAX_PROBE_HOPS` must also report exhaustion
    /// (`false`) even when `remaining` is non-empty — the runaway guard must
    /// not silently swallow the need to seed.
    #[test]
    fn advance_at_hop_cap_reports_exhausted() {
        let dummy_code = freenet_stdlib::prelude::ContractCode::from(b"dummy".to_vec());
        let dummy_params = freenet_stdlib::prelude::Parameters::from(b"dummy".to_vec());
        let dummy_key =
            freenet_stdlib::prelude::ContractKey::from_params_and_code(dummy_params, &dummy_code);
        let advanced = advance_backward_probe(ProbeState {
            owner_vk: test_owner(4),
            remaining: vec![dummy_key],
            hops: MAX_PROBE_HOPS,
            local_snapshot: ChatRoomStateV1::default(),
            epoch: 0,
        });
        assert!(
            !advanced,
            "a probe at the hop cap must report false so the caller seeds the current key"
        );
    }

    /// Probe-table semantics: `take_probe` consumes the entry exactly once, so
    /// a real GET response and the timeout watchdog cannot both advance the
    /// same hop; and `probe_has_epoch` rejects a stale-epoch watchdog.
    #[test]
    fn take_probe_is_single_shot_and_epoch_guarded() {
        let id = ContractInstanceId::new([200u8; 32]);
        probes().insert(
            id,
            ProbeState {
                owner_vk: test_owner(5),
                remaining: Vec::new(),
                hops: 1,
                local_snapshot: ChatRoomStateV1::default(),
                epoch: 42,
            },
        );
        assert!(is_probe_instance(&id));
        assert!(probe_has_epoch(&id, 42), "the armed epoch must match");
        assert!(
            !probe_has_epoch(&id, 99),
            "a watchdog from a different probe epoch must NOT match"
        );

        assert!(
            take_probe(&id).is_some(),
            "take_probe returns the entry once"
        );
        assert!(
            !is_probe_instance(&id),
            "after take_probe the entry is gone — the watchdog/real-response loser no-ops"
        );
        assert!(
            take_probe(&id).is_none(),
            "a second take_probe returns None"
        );
    }
}
