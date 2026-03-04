use crate::components::app::freenet_api::error::SynchronizerError;
use crate::components::app::freenet_api::room_synchronizer::RoomSynchronizer;
use crate::components::app::sync_info::SYNC_INFO;
use crate::components::app::WEB_API;
use crate::util::{from_cbor_slice, owner_vk_to_contract_key};
use dioxus::logger::tracing::{info, warn};
use dioxus::prelude::ReadableExt;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
use freenet_stdlib::prelude::{ContractKey, UpdateData};
use river_core::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};

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

            // Check if this state contains an upgrade pointer
            if let Some(ref authorized_upgrade) = new_state.upgrade.0 {
                let new_address = authorized_upgrade.upgrade.new_chatroom_address;
                info!(
                    "Received upgrade pointer for room {:?}, new address: {}",
                    river_core::room_state::member::MemberId::from(room_owner_vk),
                    new_address
                );

                // Check if we're already at the new key
                let current_key = owner_vk_to_contract_key(&room_owner_vk);
                let current_id_bytes = current_key.id().as_bytes();
                let mut current_hash = [0u8; 32];
                current_hash.copy_from_slice(current_id_bytes);

                if blake3::Hash::from(current_hash) != new_address {
                    // We're on the old key — follow the upgrade pointer
                    info!(
                        "Following upgrade pointer: subscribing to new contract for room {:?}",
                        river_core::room_state::member::MemberId::from(room_owner_vk)
                    );

                    // Send GET to the new contract address to fetch its state
                    let new_contract_id =
                        freenet_stdlib::prelude::ContractInstanceId::new(*new_address.as_bytes());
                    wasm_bindgen_futures::spawn_local(async move {
                        let get_request = ContractRequest::Get {
                            key: new_contract_id,
                            return_contract_code: false,
                            subscribe: true,
                            blocking_subscribe: false,
                        };
                        if let Some(web_api) = WEB_API.write().as_mut() {
                            if let Err(e) =
                                web_api.send(ClientRequest::ContractOp(get_request)).await
                            {
                                warn!("Failed to follow upgrade pointer: {}", e);
                            }
                        }
                    });
                }
            }

            // Regular state update (also process normally for merge)
            info!("Received new state in UpdateNotification: {:?}", new_state);
            room_synchronizer.update_room_state(&room_owner_vk, &new_state);
        }
        UpdateData::Delta(delta) => {
            let new_delta: ChatRoomStateV1Delta = from_cbor_slice::<ChatRoomStateV1Delta>(&delta);
            info!("Received new delta in UpdateNotification: {:?}", new_delta);
            room_synchronizer.apply_delta(&room_owner_vk, new_delta);
        }
        UpdateData::StateAndDelta { state, delta } => {
            info!(
                "Received state and delta in UpdateNotification state: {:?} delta: {:?}",
                state, delta
            );
            let new_state: ChatRoomStateV1 = from_cbor_slice::<ChatRoomStateV1>(&state);

            // Check for upgrade pointer in state+delta updates too
            if let Some(ref authorized_upgrade) = new_state.upgrade.0 {
                let new_address = authorized_upgrade.upgrade.new_chatroom_address;
                info!(
                    "Received upgrade pointer in StateAndDelta for room {:?}",
                    river_core::room_state::member::MemberId::from(room_owner_vk)
                );

                let current_key = owner_vk_to_contract_key(&room_owner_vk);
                let current_id_bytes = current_key.id().as_bytes();
                let mut current_hash = [0u8; 32];
                current_hash.copy_from_slice(current_id_bytes);

                if blake3::Hash::from(current_hash) != new_address {
                    let new_contract_id =
                        freenet_stdlib::prelude::ContractInstanceId::new(*new_address.as_bytes());
                    wasm_bindgen_futures::spawn_local(async move {
                        let get_request = ContractRequest::Get {
                            key: new_contract_id,
                            return_contract_code: false,
                            subscribe: true,
                            blocking_subscribe: false,
                        };
                        if let Some(web_api) = WEB_API.write().as_mut() {
                            if let Err(e) =
                                web_api.send(ClientRequest::ContractOp(get_request)).await
                            {
                                warn!("Failed to follow upgrade pointer: {}", e);
                            }
                        }
                    });
                }
            }

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
    }

    Ok(())
}
