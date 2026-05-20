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

/// If `state` carries an upgrade pointer to a *different* contract, send a
/// GET+subscribe to the new address.
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

        // Fresh-walk reset: a response whose delivering contract was not
        // itself a followed hop is the root of a new walk. Resetting here
        // discards any stale set a previous walk left behind if its GET
        // never returned (so the set cannot leak unboundedly).
        match delivered_from {
            Some(from) if set.contains(&from) => {}
            _ => set.clear(),
        }

        // Cycle guard 2: a pointer back to ANY contract already visited in
        // this walk (not just the immediately-preceding one) is a cycle.
        if set.contains(&new_contract_id) {
            warn!(
                "Upgrade pointer for room {:?} targets already-visited contract \
                 {} — refusing to follow (cycle guard)",
                river_core::room_state::member::MemberId::from(*room_owner_vk),
                new_contract_id
            );
            visited.remove(room_owner_vk);
            return;
        }

        // Runaway guard: cap total hops across the chain walk.
        if set.len() >= MAX_UPGRADE_HOPS {
            warn!(
                "Upgrade chain for room {:?} hit MAX_UPGRADE_HOPS ({}) — aborting",
                river_core::room_state::member::MemberId::from(*room_owner_vk),
                MAX_UPGRADE_HOPS
            );
            visited.remove(room_owner_vk);
            return;
        }

        set.insert(new_contract_id);
        info!(
            "Following upgrade pointer (hop {}): subscribing to new contract for room {:?}",
            set.len(),
            river_core::room_state::member::MemberId::from(*room_owner_vk)
        );
    }

    // Register the target so `handle_get_response` can resolve the owner when
    // the GET for this pointer hop comes back.
    upgrade_targets().insert(new_contract_id, *room_owner_vk);

    crate::util::safe_spawn_local(async move {
        let get_request = ContractRequest::Get {
            key: new_contract_id,
            return_contract_code: true,
            subscribe: true,
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
