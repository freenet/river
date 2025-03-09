use super::error::SynchronizerError;
use super::room_synchronizer::RoomSynchronizer;
use crate::util::from_cbor_slice;
use dioxus::logger::tracing::{error, info, warn};
use freenet_stdlib::client_api::WebApi;
use freenet_stdlib::{
    client_api::{ContractResponse, HostResponse},
    prelude::UpdateData,
};
use river_common::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};

/// Handles responses from the Freenet API
pub struct ResponseHandler {
    room_synchronizer: RoomSynchronizer,
}

impl ResponseHandler {
    pub fn new(room_synchronizer: RoomSynchronizer) -> Self {
        Self { room_synchronizer }
    }

    // Create a new ResponseHandler that shares the same RoomSynchronizer
    pub fn new_with_shared_synchronizer(synchronizer: &RoomSynchronizer) -> Self {
        // Create a new RoomSynchronizer with the same rooms signal
        Self {
            room_synchronizer: RoomSynchronizer::new(),
        }
    }

    /// Handles individual API responses
    pub async fn handle_api_response(
        &mut self,
        response: HostResponse,
        web_api: &mut WebApi,
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
                        let contract_info = self.room_synchronizer.get_contract_info(&key.id());
                        // Subscribe to the contract after PUT
                        if let Some(_info) = contract_info {
                            info!("Attempting to subscribe to contract after PUT: {}", key);
                            if let Err(e) = self
                                .room_synchronizer
                                .subscribe_to_contract(key.clone(), web_api)
                                .await
                            {
                                error!("Failed to subscribe after PUT: {}", e);
                                return Err(e);
                            }
                            info!("Successfully sent subscribe request after PUT for: {}", key);
                        } else {
                            warn!("Received PUT response for unknown contract: {:?}", key);
                        }
                    }
                    ContractResponse::UpdateNotification { key, update } => {
                        info!("Received update notification for key: {key}");
                        // Get contract info, log warning and return early if not found
                        let contract_info =
                            match self.room_synchronizer.get_contract_info(&key.id()) {
                                Some(info) => info,
                                None => {
                                    warn!("Received update for unknown contract: {key}");
                                    return Ok(());
                                }
                            };

                        // Handle update notification
                        match update {
                            UpdateData::State(state) => {
                                let new_state: ChatRoomStateV1 =
                                    from_cbor_slice::<ChatRoomStateV1>(&state.into_bytes());
                                let owner_vk = contract_info.owner_vk;
                                self.room_synchronizer
                                    .update_room_state(&owner_vk, &new_state)?;
                            }
                            UpdateData::Delta(delta) => {
                                let new_delta: ChatRoomStateV1Delta =
                                    from_cbor_slice::<ChatRoomStateV1Delta>(&delta.into_bytes());
                                let owner_vk = contract_info.owner_vk;
                                self.room_synchronizer.apply_delta(&owner_vk, &new_delta)?;
                            }
                            UpdateData::StateAndDelta {
                                state,
                                delta: _delta,
                            } => {
                                let new_state: ChatRoomStateV1 =
                                    from_cbor_slice::<ChatRoomStateV1>(&state.into_bytes());
                                let owner_vk = contract_info.owner_vk;
                                self.room_synchronizer
                                    .update_room_state(&owner_vk, &new_state)?;
                            }
                            UpdateData::RelatedState { .. } => {
                                warn!("Received related state update, currently ignored");
                            }
                            UpdateData::RelatedDelta { .. } => {
                                warn!("Received related delta update, currently ignored");
                            }
                            UpdateData::RelatedStateAndDelta { .. } => {
                                warn!("Received related state and delta update, currently ignored");
                            }
                        }
                    }
                    ContractResponse::UpdateResponse { key, summary: _ } => {
                        info!("Received update response for key {key}");
                    }
                    ContractResponse::SubscribeResponse { key, subscribed: _ } => {
                        info!("Received subscribe response for key {key}");
                        // Get owner_vk first, then call mark_room_subscribed to avoid borrow conflict
                        let owner_vk = if let Some(info) =
                            self.room_synchronizer.get_contract_info(&key.id())
                        {
                            Some(info.owner_vk)
                        } else {
                            None
                        };

                        if let Some(vk) = owner_vk {
                            self.room_synchronizer.mark_room_subscribed(&vk);
                        }
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
