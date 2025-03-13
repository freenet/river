use super::error::SynchronizerError;
use super::room_synchronizer::RoomSynchronizer;
use crate::util::from_cbor_slice;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::signals::Readable;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::WebApi;
use freenet_stdlib::{
    client_api::{ContractResponse, HostResponse},
    prelude::UpdateData,
};
use river_common::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};

/// Handles responses from the Freenet API
pub struct ResponseHandler {
    room_synchronizer: RoomSynchronizer,
}

impl ResponseHandler {
    pub fn new(room_synchronizer: RoomSynchronizer) -> Self {
        Self { room_synchronizer }
    }

    // Create a new ResponseHandler that shares the same RoomSynchronizer
    pub fn new_with_shared_synchronizer(_synchronizer: &RoomSynchronizer) -> Self {
        // Create a new RoomSynchronizer with the same rooms signal
        Self {
            room_synchronizer: RoomSynchronizer::new(),
        }
    }

    /// Handles individual API responses
    pub async fn handle_api_response(
        &mut self,
        response: HostResponse,
    ) -> Result<(), SynchronizerError> {
        info!("Handling API response: {:?}", response);
        match response {
            HostResponse::Ok => {
                info!("Received OK response from API");
            }
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::GetResponse {
                        key,
                        contract: _,
                        state: _,
                    } => {
                        warn!("GetResponse received for key {key} but not currently handled");
                    }
                    ContractResponse::PutResponse { key } => {
                        info!("Received PUT response for room {}, marking as subscribed", key.id());
                        // Update the sync status to indicate that the room is subscribed
                        let owner_vk = SYNC_INFO.read().get_owner_vk_for_instance_id(&key.id()).expect(
                            "Failed to get owner VK for instance ID"
                        );
                        SYNC_INFO.write().update_sync_status(&owner_vk, RoomSyncStatus::Subscribed);
                    }
                    ContractResponse::UpdateNotification { key, update } => {
                        info!("Received update notification for key: {key}");
                        // Get contract info, log warning and return early if not found
                        // Get contract info, return early if not found
                        let room_owner_vk = match SYNC_INFO.read().get_owner_vk_for_instance_id(&key.id()) {
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
                                    from_cbor_slice::<ChatRoomStateV1>(&state.into_bytes());
                                info!("Received new state in UpdateNotification: {:?}", new_state);
                                self.room_synchronizer
                                    .update_room_state(&room_owner_vk, &new_state);
                            }
                            UpdateData::Delta(delta) => {
                                let new_delta: ChatRoomStateV1Delta =
                                    from_cbor_slice::<ChatRoomStateV1Delta>(&delta.into_bytes());
                                info!("Received new delta in UpdateNotification: {:?}", new_delta);
                                self.room_synchronizer.apply_delta(&room_owner_vk, new_delta);
                            }
                            UpdateData::StateAndDelta {
                                state,
                                delta,
                            } => {
                                info!("Received state and delta in UpdateNotification state: {:?} delta: {:?}", state, delta);
                                let new_state: ChatRoomStateV1 =
                                    from_cbor_slice::<ChatRoomStateV1>(&state.into_bytes());
                                self.room_synchronizer
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
                    }
                    ContractResponse::UpdateResponse { key, summary } => {
                        let summary_len = summary.len();
                        info!("Received update response for key {key}, summary length {summary_len}, currently ignored");
                    }
                    ContractResponse::SubscribeResponse { key, subscribed } => {
                        info!("Received subscribe response for key {key}, currently ignored");
                    }
                    _ => {
                        info!("Unhandled contract response: {:?}", contract_response);
                    }
                }
            }
            _ => {
                warn!("Unhandled API response: {:?}", response);
            }
        }
        Ok(())
    }

    pub fn get_room_synchronizer_mut(&mut self) -> &mut RoomSynchronizer {
        &mut self.room_synchronizer
    }

    // Get a reference to the room synchronizer
    pub fn get_room_synchronizer(&self) -> &RoomSynchronizer {
        &self.room_synchronizer
    }
}
