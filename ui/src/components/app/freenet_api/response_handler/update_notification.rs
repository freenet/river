use crate::components::app::freenet_api::error::SynchronizerError;
use crate::components::app::freenet_api::room_synchronizer::RoomSynchronizer;
use crate::components::app::sync_info::SYNC_INFO;
use crate::components::app::WEB_API;
use crate::util::{from_cbor_slice, owner_vk_to_contract_key};
use dioxus::logger::tracing::{debug, info, warn};
use dioxus::prelude::{Global, GlobalSignal, ReadableExt};
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
use freenet_stdlib::prelude::{ContractInstanceId, ContractKey, UpdateData};
use river_core::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};
use std::collections::HashMap;

/// Hard cap on the number of upgrade-pointer hops a single chain walk
/// will follow before giving up. A correctly-formed upgrade chain has at
/// most one hop per WASM generation; this is a defence-in-depth guard
/// against a runaway pointer→pointer→pointer chain (a malicious or
/// corrupt `OptionalUpgradeV1`).
const MAX_UPGRADE_HOPS: usize = 32;

/// Per-room count of upgrade-pointer hops already followed in the
/// current chain walk. Keyed by `room_owner_vk`. Incremented each time
/// `follow_upgrade_pointer_if_needed` follows a pointer; the entry is
/// removed once the chain terminates (the GET response carries no
/// further pointer, or it points at the current key).
static UPGRADE_HOP_COUNTS: GlobalSignal<HashMap<VerifyingKey, usize>> = Global::new(HashMap::new);

/// Maps the `ContractInstanceId` of an upgrade-pointer target whose
/// GET we have outstanding back to the `room_owner_vk` it belongs to.
///
/// A GET response for an upgrade-pointer target arrives keyed by a
/// contract id that is NOT the room's current contract id, so it
/// cannot be resolved via `SYNC_INFO` or `RoomData::contract_key`. This
/// side-table lets `handle_get_response` recover the owner so it can
/// (a) continue walking a multi-hop chain and (b) merge the recovered
/// state into the right room.
static UPGRADE_TARGETS: GlobalSignal<HashMap<ContractInstanceId, VerifyingKey>> =
    Global::new(HashMap::new);

/// The `room_owner_vk` an outstanding upgrade-pointer GET for
/// `instance_id` belongs to, if any. Used by `handle_get_response` as an
/// owner-resolution fallback for pointer-target contracts.
pub(crate) fn upgrade_target_owner(instance_id: &ContractInstanceId) -> Option<VerifyingKey> {
    UPGRADE_TARGETS.read().get(instance_id).copied()
}

/// Remove the upgrade-target side-table entry for `instance_id` — its
/// GET response has been consumed.
pub(crate) fn clear_upgrade_target(instance_id: &ContractInstanceId) {
    let instance_id = *instance_id;
    crate::util::defer(move || {
        UPGRADE_TARGETS.with_mut(|targets| {
            targets.remove(&instance_id);
        });
    });
}

/// If `state` carries an upgrade pointer to a *different* contract,
/// send a GET+subscribe to the new address.
///
/// Multi-hop (freenet/river#292): the GET response for the contract
/// reached by following a pointer is itself routed back through
/// `handle_get_response`, which calls this function again — so a
/// pointer→pointer→pointer chain is walked all the way to the end.
///
/// `delivered_from` is the `ContractInstanceId` of the contract whose
/// GET response (or update notification) carried `state`. It is used as
/// a cycle guard: a pointer that targets the very contract that just
/// delivered it is ignored rather than re-fetched forever. The
/// [`MAX_UPGRADE_HOPS`] counter is a second, total-length guard.
pub(crate) fn follow_upgrade_pointer_if_needed(
    state: &ChatRoomStateV1,
    room_owner_vk: &VerifyingKey,
    delivered_from: Option<ContractInstanceId>,
) {
    let Some(ref authorized_upgrade) = state.upgrade.0 else {
        // Chain terminates here — drop any hop counter for this room so a
        // future, unrelated chain walk starts fresh.
        clear_upgrade_hops(room_owner_vk);
        return;
    };

    let new_address = authorized_upgrade.upgrade.new_chatroom_address;
    let new_contract_id = ContractInstanceId::new(*new_address.as_bytes());

    info!(
        "Received upgrade pointer for room {:?}, new address: {}",
        river_core::room_state::member::MemberId::from(*room_owner_vk),
        new_address
    );

    // Cycle guard 1: the pointer targets the current bundled contract —
    // we already know that key, no need to chase it.
    let current_key = owner_vk_to_contract_key(room_owner_vk);
    let current_id_bytes = current_key.id().as_bytes();
    let mut current_hash = [0u8; 32];
    current_hash.copy_from_slice(current_id_bytes);
    if blake3::Hash::from(current_hash) == new_address {
        clear_upgrade_hops(room_owner_vk);
        return;
    }

    // Cycle guard 2: the pointer targets the contract that just
    // delivered this state. Following it would loop forever.
    if delivered_from == Some(new_contract_id) {
        warn!(
            "Upgrade pointer for room {:?} targets the delivering contract \
             {} — refusing to follow (cycle guard)",
            river_core::room_state::member::MemberId::from(*room_owner_vk),
            new_contract_id
        );
        clear_upgrade_hops(room_owner_vk);
        return;
    }

    // Runaway guard: cap total hops across the chain walk.
    let hops = UPGRADE_HOP_COUNTS
        .read()
        .get(room_owner_vk)
        .copied()
        .unwrap_or(0);
    if hops >= MAX_UPGRADE_HOPS {
        warn!(
            "Upgrade chain for room {:?} hit MAX_UPGRADE_HOPS ({}) — aborting",
            river_core::room_state::member::MemberId::from(*room_owner_vk),
            MAX_UPGRADE_HOPS
        );
        clear_upgrade_hops(room_owner_vk);
        return;
    }

    info!(
        "Following upgrade pointer (hop {}): subscribing to new contract for room {:?}",
        hops + 1,
        river_core::room_state::member::MemberId::from(*room_owner_vk)
    );

    let room_owner_vk_copy = *room_owner_vk;
    crate::util::defer(move || {
        UPGRADE_HOP_COUNTS.with_mut(|counts| {
            *counts.entry(room_owner_vk_copy).or_insert(0) += 1;
        });
        // Register the target so `handle_get_response` can resolve the
        // owner when the GET for this pointer hop comes back.
        UPGRADE_TARGETS.with_mut(|targets| {
            targets.insert(new_contract_id, room_owner_vk_copy);
        });
    });

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

/// Drop the hop counter for `room_owner_vk` — the chain walk has ended.
fn clear_upgrade_hops(room_owner_vk: &VerifyingKey) {
    let room_owner_vk = *room_owner_vk;
    crate::util::defer(move || {
        UPGRADE_HOP_COUNTS.with_mut(|counts| {
            counts.remove(&room_owner_vk);
        });
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
