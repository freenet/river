use crate::components::app::freenet_api::error::SynchronizerError;
use crate::components::app::freenet_api::room_synchronizer::RoomSynchronizer;
use crate::components::app::sync_info::SYNC_INFO;
use crate::components::app::WEB_API;
use crate::util::{from_cbor_slice, owner_vk_to_contract_key};
use dioxus::logger::tracing::{debug, info, warn};
use dioxus::prelude::ReadableExt;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
use freenet_stdlib::prelude::{ContractInstanceId, ContractKey, UpdateData};
use river_core::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, MutexGuard};

/// Hard cap on the number of upgrade-pointer hops a single chain walk will
/// follow before giving up. A correctly-formed upgrade chain has at most one
/// hop per WASM generation; this is a defence-in-depth guard against a runaway
/// pointer→pointer→pointer chain (a malicious or corrupt `OptionalUpgradeV1`).
const MAX_UPGRADE_HOPS: usize = 32;

/// Per-room set of upgrade-pointer-target contract ids already visited in the
/// current chain walk, keyed by `room_owner_vk`. The set is BOTH the cycle
/// guard (a pointer back to any already-visited contract — not just the
/// immediately-preceding one — is a cycle) AND the hop cap (`len()`).
///
/// Plain `Mutex` map, NOT a Dioxus signal — internal bookkeeping with zero UI
/// reactivity, like `backward_probe::BACKWARD_PROBES`.
static UPGRADE_CHAIN_VISITED: LazyLock<Mutex<HashMap<VerifyingKey, HashSet<ContractInstanceId>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Maps the `ContractInstanceId` of an upgrade-pointer target whose GET we
/// have outstanding back to the `room_owner_vk` it belongs to.
///
/// A GET response for an upgrade-pointer target arrives keyed by a contract id
/// that is NOT the room's current contract id, so it cannot be resolved via
/// `SYNC_INFO` or `RoomData::contract_key`. This side-table lets
/// `handle_get_response` recover the owner so it can (a) continue walking a
/// multi-hop chain and (b) merge the recovered state into the right room.
///
/// Plain `Mutex` map, NOT a Dioxus signal — see `UPGRADE_CHAIN_VISITED`.
static UPGRADE_TARGETS: LazyLock<Mutex<HashMap<ContractInstanceId, VerifyingKey>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn upgrade_visited() -> MutexGuard<'static, HashMap<VerifyingKey, HashSet<ContractInstanceId>>> {
    UPGRADE_CHAIN_VISITED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn upgrade_targets() -> MutexGuard<'static, HashMap<ContractInstanceId, VerifyingKey>> {
    UPGRADE_TARGETS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The `room_owner_vk` an outstanding upgrade-pointer GET for `instance_id`
/// belongs to, if any. Used by `handle_get_response` as an owner-resolution
/// fallback for pointer-target contracts.
pub(crate) fn upgrade_target_owner(instance_id: &ContractInstanceId) -> Option<VerifyingKey> {
    upgrade_targets().get(instance_id).copied()
}

/// Remove the upgrade-target side-table entry for `instance_id` — its GET
/// response has been consumed. Synchronous (plain mutex), so a later GET
/// response for the same id cannot be mis-routed by a not-yet-applied
/// deferred removal.
pub(crate) fn clear_upgrade_target(instance_id: &ContractInstanceId) {
    upgrade_targets().remove(instance_id);
}

/// Drop the visited-set for `room_owner_vk` — the chain walk has ended.
fn clear_upgrade_visited(room_owner_vk: &VerifyingKey) {
    upgrade_visited().remove(room_owner_vk);
}

/// Outcome of classifying one upgrade-pointer hop against a walk's visited-set.
#[derive(Debug, PartialEq, Eq)]
enum UpgradeHopDecision {
    /// Follow the pointer — `new_id` has been recorded in the visited-set.
    Follow,
    /// The pointer loops back to a contract already visited in this walk.
    Cycle,
    /// The walk has already followed [`MAX_UPGRADE_HOPS`] hops.
    CapReached,
}

/// Classify an upgrade-pointer hop and update the per-walk `visited` set.
///
/// If `delivered_from` is not itself a visited hop, this response is the root
/// of a fresh walk and `visited` is reset first — discarding any stale set an
/// earlier walk left behind if its GET never returned. The cycle guard then
/// rejects a pointer back to ANY visited contract, and the cap rejects a walk
/// longer than [`MAX_UPGRADE_HOPS`]. Pure (no I/O, no signals) so the guards
/// are unit-testable (freenet/river#292).
fn classify_upgrade_hop(
    visited: &mut HashSet<ContractInstanceId>,
    delivered_from: Option<ContractInstanceId>,
    new_id: ContractInstanceId,
) -> UpgradeHopDecision {
    match delivered_from {
        Some(from) if visited.contains(&from) => {}
        _ => visited.clear(),
    }
    if visited.contains(&new_id) {
        return UpgradeHopDecision::Cycle;
    }
    if visited.len() >= MAX_UPGRADE_HOPS {
        return UpgradeHopDecision::CapReached;
    }
    visited.insert(new_id);
    UpgradeHopDecision::Follow
}

/// If `state` carries an upgrade pointer to a *different* contract, send a
/// (discovery-only, non-subscribing) GET to the new address.
///
/// Multi-hop (freenet/river#292): the GET response for the contract reached by
/// following a pointer is itself routed back through `handle_get_response`,
/// which calls this function again — so a pointer→pointer→pointer chain is
/// walked all the way to the end.
///
/// `delivered_from` is the `ContractInstanceId` of the contract whose GET
/// response (or update notification) carried `state`. A response whose
/// `delivered_from` is not in the visited-set starts a fresh walk (the
/// visited-set is reset), which both bounds the set's lifetime and discards
/// any stale set left by an earlier walk that never terminated. The
/// per-walk visited-set then catches every cycle (not just an immediate
/// A→A), and its size is capped by [`MAX_UPGRADE_HOPS`].
pub(crate) fn follow_upgrade_pointer_if_needed(
    state: &ChatRoomStateV1,
    room_owner_vk: &VerifyingKey,
    delivered_from: Option<ContractInstanceId>,
) {
    let Some(ref authorized_upgrade) = state.upgrade.0 else {
        // Chain terminates here — drop the visited-set for this room so a
        // future, unrelated chain walk starts fresh.
        clear_upgrade_visited(room_owner_vk);
        return;
    };

    let new_address = authorized_upgrade.upgrade.new_chatroom_address;
    let new_contract_id = ContractInstanceId::new(*new_address.as_bytes());

    info!(
        "Received upgrade pointer for room {:?}, new address: {}",
        river_core::room_state::member::MemberId::from(*room_owner_vk),
        new_address
    );

    // Cycle guard 1: the pointer targets the current bundled contract — we
    // already know that key, no need to chase it.
    let current_key = owner_vk_to_contract_key(room_owner_vk);
    let current_id_bytes = current_key.id().as_bytes();
    let mut current_hash = [0u8; 32];
    current_hash.copy_from_slice(current_id_bytes);
    if blake3::Hash::from(current_hash) == new_address {
        clear_upgrade_visited(room_owner_vk);
        return;
    }

    // Visited-set bookkeeping + cycle / runaway guards, all under one lock.
    {
        let mut visited = upgrade_visited();
        let set = visited.entry(*room_owner_vk).or_default();
        match classify_upgrade_hop(set, delivered_from, new_contract_id) {
            UpgradeHopDecision::Follow => {
                info!(
                    "Following upgrade pointer (hop {}): discovery GET of new contract for room {:?}",
                    set.len(),
                    river_core::room_state::member::MemberId::from(*room_owner_vk)
                );
            }
            UpgradeHopDecision::Cycle => {
                warn!(
                    "Upgrade pointer for room {:?} targets already-visited contract \
                     {} — refusing to follow (cycle guard)",
                    river_core::room_state::member::MemberId::from(*room_owner_vk),
                    new_contract_id
                );
                visited.remove(room_owner_vk);
                return;
            }
            UpgradeHopDecision::CapReached => {
                warn!(
                    "Upgrade chain for room {:?} hit MAX_UPGRADE_HOPS ({}) — aborting",
                    river_core::room_state::member::MemberId::from(*room_owner_vk),
                    MAX_UPGRADE_HOPS
                );
                visited.remove(room_owner_vk);
                return;
            }
        }
    }

    // Register the target so `handle_get_response` can resolve the owner when
    // the GET for this pointer hop comes back. The entry is short-lived —
    // `clear_upgrade_target` removes it once that GET response is consumed. If
    // the GET never returns (a garbage-collected intermediate), the entry is a
    // bounded, harmless leak: at most one per distinct upgrade-pointer target
    // ever seen (≈ the number of contract generations), and a stale entry
    // still maps that target id to its correct owner.
    upgrade_targets().insert(new_contract_id, *room_owner_vk);

    crate::util::safe_spawn_local(async move {
        let get_request = ContractRequest::Get {
            key: new_contract_id,
            return_contract_code: true,
            // Discovery-only: the chain walk GETs each hop to find where the
            // room's state now lives, but does NOT subscribe to intermediate
            // (stale) generations. Subscribing to them would register a
            // contract id that `SYNC_INFO` cannot resolve, so their
            // `UpdateNotification`s would be silently dropped. Live updates
            // come from the room's CURRENT contract key, subscribed via the
            // normal sync path / the backward-probe PUT-forward.
            subscribe: false,
            blocking_subscribe: false,
        };
        if let Some(web_api) = WEB_API.write().as_mut() {
            if let Err(e) = web_api.send(ClientRequest::ContractOp(get_request)).await {
                warn!("Failed to follow upgrade pointer: {}", e);
            }
        }
    });
}

pub fn handle_update_notification(
    room_synchronizer: &mut RoomSynchronizer,
    key: ContractKey,
    update: UpdateData,
) -> Result<(), SynchronizerError> {
    info!("Received update notification for key: {key}");
    // Get contract info, return early if not found
    let room_owner_vk = match SYNC_INFO.read().get_owner_vk_for_instance_id(key.id()) {
        Some(vk) => vk,
        None => {
            warn!("Contract key not found in SYNC_INFO: {}", key.id());
            return Ok(());
        }
    };

    // Handle update notification
    match update {
        UpdateData::State(state) => {
            let new_state: ChatRoomStateV1 = from_cbor_slice::<ChatRoomStateV1>(&state);
            follow_upgrade_pointer_if_needed(&new_state, &room_owner_vk, Some(*key.id()));

            info!(
                "UpdateNotification: state ({} messages, {} members)",
                new_state.recent_messages.messages.len(),
                new_state.members.members.len()
            );
            debug!("Received new state in UpdateNotification: {:?}", new_state);
            room_synchronizer.update_room_state(&room_owner_vk, &new_state);
        }
        UpdateData::Delta(delta) => {
            let new_delta: ChatRoomStateV1Delta = from_cbor_slice::<ChatRoomStateV1Delta>(&delta);
            info!("UpdateNotification: delta received");
            debug!("Received new delta in UpdateNotification: {:?}", new_delta);
            room_synchronizer.apply_delta(&room_owner_vk, new_delta);
        }
        UpdateData::StateAndDelta { state, delta } => {
            let new_state: ChatRoomStateV1 = from_cbor_slice::<ChatRoomStateV1>(&state);
            info!(
                "UpdateNotification: state+delta ({} messages, {} members)",
                new_state.recent_messages.messages.len(),
                new_state.members.members.len()
            );
            debug!(
                "Received state and delta in UpdateNotification state: {:?} delta: {:?}",
                state, delta
            );
            follow_upgrade_pointer_if_needed(&new_state, &room_owner_vk, Some(*key.id()));

            room_synchronizer.update_room_state(&room_owner_vk, &new_state);
        }
        UpdateData::RelatedState { .. } => {
            warn!("Received related state update, ignored");
        }
        UpdateData::RelatedDelta { .. } => {
            warn!("Received related delta update, ignored");
        }
        UpdateData::RelatedStateAndDelta { .. } => {
            warn!("Received related state and delta update, ignored");
        }
        // `UpdateData` is `#[non_exhaustive]` since freenet-stdlib 0.6.0.
        // Future variants are ignored the same way as the existing
        // `Related*` arms.
        _ => {
            warn!("Received unknown UpdateData variant, ignored");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(seed: u8) -> ContractInstanceId {
        ContractInstanceId::new([seed; 32])
    }

    /// A response whose delivering contract is not in the visited-set is the
    /// root of a fresh walk: any stale set is reset and the hop is followed.
    #[test]
    fn classify_resets_and_follows_on_fresh_root() {
        let mut visited = HashSet::new();
        visited.insert(cid(99)); // stale leftover from an earlier walk
        let decision = classify_upgrade_hop(&mut visited, Some(cid(1)), cid(2));
        assert_eq!(decision, UpgradeHopDecision::Follow);
        assert!(visited.contains(&cid(2)), "followed hop must be recorded");
        assert!(
            !visited.contains(&cid(99)),
            "a fresh walk must discard the stale set"
        );
    }

    /// Mid-chain (the delivering contract IS a visited hop), an unvisited
    /// target is followed without resetting the set.
    #[test]
    fn classify_follows_mid_chain_without_reset() {
        let mut visited = HashSet::new();
        visited.insert(cid(1));
        let decision = classify_upgrade_hop(&mut visited, Some(cid(1)), cid(2));
        assert_eq!(decision, UpgradeHopDecision::Follow);
        assert!(visited.contains(&cid(1)) && visited.contains(&cid(2)));
    }

    /// A pointer back to any already-visited contract — not just the
    /// immediately-preceding one — is a cycle (the A→B→A case).
    #[test]
    fn classify_detects_multi_hop_cycle() {
        let mut visited = HashSet::new();
        visited.insert(cid(1)); // A
        visited.insert(cid(2)); // B, delivering this response
        let decision = classify_upgrade_hop(&mut visited, Some(cid(2)), cid(1));
        assert_eq!(
            decision,
            UpgradeHopDecision::Cycle,
            "B pointing back to already-visited A must be caught"
        );
    }

    /// `delivered_from == None` (a root response with no tracked delivering
    /// contract) is treated as a fresh walk: the visited-set is reset.
    #[test]
    fn classify_with_no_delivered_from_resets() {
        let mut visited = HashSet::new();
        visited.insert(cid(50)); // stale leftover
        let decision = classify_upgrade_hop(&mut visited, None, cid(7));
        assert_eq!(decision, UpgradeHopDecision::Follow);
        assert!(
            !visited.contains(&cid(50)),
            "None delivered_from must reset the set"
        );
        assert!(visited.contains(&cid(7)));
    }

    /// A walk that has already followed MAX_UPGRADE_HOPS contracts stops.
    #[test]
    fn classify_stops_at_hop_cap() {
        let mut visited: HashSet<ContractInstanceId> =
            (0..MAX_UPGRADE_HOPS as u16).map(|i| cid(i as u8)).collect();
        // Deliver from a visited contract so the set is not reset.
        let from = *visited.iter().next().unwrap();
        let decision = classify_upgrade_hop(&mut visited, Some(from), cid(250));
        assert_eq!(decision, UpgradeHopDecision::CapReached);
    }
}
