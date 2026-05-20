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
//! * Neither current nor any legacy key has state → only then may the device's
//!   stale local snapshot be PUT to seed the current key.
//!
//! ## Routing
//!
//! A GET response for a *legacy* contract key cannot be resolved back to an
//! `owner_vk` via `SYNC_INFO` (keyed by the current contract id) or via
//! `RoomData::contract_key` (also the current id). [`BACKWARD_PROBES`] is the
//! side-table that maps a probe's legacy `ContractInstanceId` back to the
//! owner being recovered, plus the remaining legacy keys still to try.

use crate::util::owner_vk_to_legacy_contract_keys;
use dioxus::logger::tracing::{info, warn};
use dioxus::prelude::{Global, GlobalSignal, ReadableExt};
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
use freenet_stdlib::prelude::{ContractInstanceId, ContractKey};
use std::collections::HashMap;

/// Hard cap on how many legacy generations a single probe will walk before
/// giving up. `owner_vk_to_legacy_contract_keys` is already bounded by the
/// size of the legacy registry, but this is a defence-in-depth guard against
/// a runaway chain if the registry ever grows unexpectedly large.
const MAX_PROBE_HOPS: usize = 32;

/// State of an in-progress backward probe for one room.
#[derive(Clone)]
pub struct ProbeState {
    /// The owner whose stranded room we're recovering.
    pub owner_vk: VerifyingKey,
    /// Legacy contract keys not yet tried, ordered newest-first. The key
    /// currently outstanding (whose GET we're awaiting) is NOT in this list —
    /// it's tracked by its instance id being a `BACKWARD_PROBES` map key.
    pub remaining: Vec<ContractKey>,
    /// Hops taken so far, for the [`MAX_PROBE_HOPS`] cap.
    pub hops: usize,
}

/// Maps the `ContractInstanceId` of the currently-outstanding legacy GET to
/// the probe it belongs to. A GET response for a legacy key is resolved back
/// to its `owner_vk` by looking up `key.id()` here.
pub static BACKWARD_PROBES: GlobalSignal<HashMap<ContractInstanceId, ProbeState>> =
    Global::new(HashMap::new);

/// Whether `instance_id` is the contract id of an outstanding backward-probe
/// GET. Used by `handle_get_response` to route legacy-key responses.
///
/// On the (rare) event that `BACKWARD_PROBES` is concurrently borrowed,
/// returns `false` — the GET response then falls through to the normal
/// owner-resolution path rather than the probe handler. A legacy-key
/// response that misses the probe route is harmless (it won't resolve to
/// an owner and is dropped); the probe's outstanding entry stays put and
/// a later cycle can still drive it.
pub fn is_probe_instance(instance_id: &ContractInstanceId) -> bool {
    BACKWARD_PROBES
        .try_read()
        .map(|probes| probes.contains_key(instance_id))
        .unwrap_or(false)
}

/// Start a backward probe for `owner_vk`'s room: GET the newest legacy
/// contract key. Called when the current-key GET came back empty.
///
/// Returns `true` if a probe was started (there is at least one legacy
/// generation to try, or one is already running for this owner) — the
/// caller should then NOT seed the current key, the probe will recover
/// or exhaust. Returns `false` when there are no legacy generations at
/// all, in which case the caller must fall back to seeding the current
/// key with the local snapshot (there is nothing to recover).
///
/// Spawns the GET via `safe_spawn_local`; signal mutation of
/// `BACKWARD_PROBES` is wrapped in `defer`.
pub fn start_backward_probe(owner_vk: VerifyingKey) -> bool {
    let mut legacy_keys = owner_vk_to_legacy_contract_keys(&owner_vk);
    if legacy_keys.is_empty() {
        info!(
            "No legacy room-contract generations — skipping backward probe for {:?}",
            river_core::room_state::member::MemberId::from(owner_vk)
        );
        return false;
    }

    // Don't start a second probe for an owner that already has one running.
    // Return `true` so the caller still treats recovery as in-flight and
    // does not seed the current key behind the probe's back.
    if let Ok(probes) = BACKWARD_PROBES.try_read() {
        if probes.values().any(|p| p.owner_vk == owner_vk) {
            info!(
                "Backward probe already in progress for {:?}, not restarting",
                river_core::room_state::member::MemberId::from(owner_vk)
            );
            return true;
        }
    }

    // The newest legacy key becomes the outstanding GET; the rest stay in
    // `remaining` for subsequent hops.
    let first = legacy_keys.remove(0);
    info!(
        "Starting backward probe for {:?}: {} legacy generation(s) to try",
        river_core::room_state::member::MemberId::from(owner_vk),
        legacy_keys.len() + 1
    );
    fire_probe_get(owner_vk, first, legacy_keys, 1);
    true
}

/// Advance an in-progress probe to its next legacy key. Called when a legacy
/// GET came back empty.
///
/// Returns `true` if the probe advanced (a GET for the next legacy key was
/// fired). Returns `false` when the probe is exhausted — every legacy
/// generation has been tried and none held real state. On `false` the
/// caller must perform the last-resort seed of the current contract key
/// with the device's local snapshot.
pub fn advance_backward_probe(state: ProbeState) -> bool {
    let ProbeState {
        owner_vk,
        mut remaining,
        hops,
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
    fire_probe_get(owner_vk, next, remaining, hops + 1);
    true
}

/// Register `key` as the outstanding probe GET for `owner_vk` and send the
/// GET request. `remaining` is the not-yet-tried tail of legacy keys.
fn fire_probe_get(
    owner_vk: VerifyingKey,
    key: ContractKey,
    remaining: Vec<ContractKey>,
    hops: usize,
) {
    let instance_id = *key.id();
    let probe = ProbeState {
        owner_vk,
        remaining,
        hops,
    };

    // Register the probe BEFORE sending the GET so the response handler can
    // resolve the legacy key when the reply arrives. Deferred per the Dioxus
    // signal-safety rules (this can run inside a response-handler task).
    crate::util::defer(move || {
        BACKWARD_PROBES.with_mut(|probes| {
            probes.insert(instance_id, probe);
        });
    });

    info!(
        "Backward probe hop {} for {:?}: GET legacy contract {}",
        hops,
        river_core::room_state::member::MemberId::from(owner_vk),
        instance_id
    );

    crate::util::safe_spawn_local(async move {
        let get_request = ContractRequest::Get {
            key: instance_id,
            // Legacy generations need their own WASM cached for the GET to
            // resolve on a node that has never seen that contract.
            return_contract_code: true,
            subscribe: false,
            blocking_subscribe: false,
        };
        if let Some(web_api) = crate::components::app::WEB_API.write().as_mut() {
            if let Err(e) = web_api.send(ClientRequest::ContractOp(get_request)).await {
                warn!("Failed to send backward-probe GET: {}", e);
                // Drop the probe entry so a future probe for this owner can
                // start fresh rather than silently colliding with a dead one.
                crate::util::defer(move || {
                    BACKWARD_PROBES.with_mut(|probes| {
                        probes.remove(&instance_id);
                    });
                });
            }
        }
    });
}

/// Take (remove) the probe entry for `instance_id`, if any. The caller has
/// just received the GET response for that legacy key and is now responsible
/// for either recovering the state or advancing to the next hop.
pub fn take_probe(instance_id: &ContractInstanceId) -> Option<ProbeState> {
    let mut taken = None;
    BACKWARD_PROBES.with_mut(|probes| {
        taken = probes.remove(instance_id);
    });
    taken
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `advance_backward_probe` with an empty `remaining` must report
    /// exhaustion (`false`) — the probe ends, no panic, no further GET.
    /// The caller uses the `false` return to trigger the last-resort
    /// seed of the current contract key.
    #[test]
    fn advance_with_empty_remaining_reports_exhausted() {
        let owner = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]).verifying_key();
        // Should not panic and should not require a Dioxus runtime — the
        // exhausted branch returns before touching any signal.
        let advanced = advance_backward_probe(ProbeState {
            owner_vk: owner,
            remaining: Vec::new(),
            hops: 1,
        });
        assert!(
            !advanced,
            "an exhausted probe must report false so the caller seeds the current key"
        );
    }

    /// A probe that has hit `MAX_PROBE_HOPS` must also report exhaustion
    /// (`false`) even when `remaining` is non-empty — the runaway guard
    /// must not silently swallow the need to seed.
    #[test]
    fn advance_at_hop_cap_reports_exhausted() {
        let owner = ed25519_dalek::SigningKey::from_bytes(&[4u8; 32]).verifying_key();
        let dummy_code = freenet_stdlib::prelude::ContractCode::from(b"dummy".to_vec());
        let dummy_params = freenet_stdlib::prelude::Parameters::from(b"dummy".to_vec());
        let dummy_key = ContractKey::from_params_and_code(dummy_params, &dummy_code);
        let advanced = advance_backward_probe(ProbeState {
            owner_vk: owner,
            remaining: vec![dummy_key],
            hops: MAX_PROBE_HOPS,
        });
        assert!(
            !advanced,
            "a probe at the hop cap must report false so the caller seeds the current key"
        );
    }
}
