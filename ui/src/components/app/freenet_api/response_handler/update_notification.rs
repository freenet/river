use crate::components::app::freenet_api::error::SynchronizerError;
use crate::components::app::freenet_api::room_synchronizer::RoomSynchronizer;
use crate::components::app::sync_info::SYNC_INFO;
use crate::util::from_cbor_slice;
use dioxus::logger::tracing::{info, warn};
use dioxus::prelude::Readable;
use freenet_stdlib::prelude::{ContractKey, UpdateData};
use river_common::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};

pub fn handle_update_notification(
    room_synchronizer: &mut RoomSynchronizer,
    key: ContractKey,
    update: UpdateData,
) -> Result<(), SynchronizerError> {
    info!("Received update notification for key: {key}");
    // Get contract info, log warning and return early if not found
    // Get contract info, return early if not found
    let room_owner_vk =
        match SYNC_INFO.read().get_owner_vk_for_instance_id(&key.id()) {
            Some(vk) => vk,
            None => {
                warn!("Contract key not found in SYNC_INFO: {}", key.id());
                return Ok(());
            }
        };

    // Handle update notification
    match update {
        UpdateData::State(state) => {
            let new_state: ChatRoomStateV1 =
                from_cbor_slice::<ChatRoomStateV1>(&*state);

            // Regular state update
            info!("Received new state in UpdateNotification: {:?}", new_state);
            room_synchronizer
                .update_room_state(&room_owner_vk, &new_state);
        }
        UpdateData::Delta(delta) => {
            let new_delta: ChatRoomStateV1Delta =
                from_cbor_slice::<ChatRoomStateV1Delta>(&*delta);
            info!("Received new delta in UpdateNotification: {:?}", new_delta);
            room_synchronizer
                .apply_delta(&room_owner_vk, new_delta);
        }
        UpdateData::StateAndDelta { state, delta } => {
            info!("Received state and delta in UpdateNotification state: {:?} delta: {:?}", state, delta);
            let new_state: ChatRoomStateV1 =
                from_cbor_slice::<ChatRoomStateV1>(&*state);
            room_synchronizer
                .update_room_state(&room_owner_vk, &new_state);
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
    }
    
    Ok(())
}
