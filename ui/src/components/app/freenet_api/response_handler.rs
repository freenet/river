mod get_response;
mod put_response;
mod subscribe_response;
mod update_notification;
mod update_response;

use super::error::SynchronizerError;
use super::room_synchronizer::RoomSynchronizer;
use crate::components::app::chat_delegate::ROOMS_STORAGE_KEY;
use crate::components::app::notifications::mark_initial_sync_complete;
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::util::owner_vk_to_contract_key;
use crate::room_data::CurrentRoom;
use crate::room_data::Rooms;
use ciborium::de::from_reader;
use dioxus::logger::tracing::{error, info, warn};
use freenet_stdlib::client_api::{ContractResponse, HostResponse};
use freenet_stdlib::prelude::OutboundDelegateMsg;
pub use get_response::handle_get_response;
pub use put_response::handle_put_response;
use river_core::chat_delegate::{ChatDelegateRequestMsg, ChatDelegateResponseMsg};
pub use subscribe_response::handle_subscribe_response;
pub use update_notification::handle_update_notification;
pub use update_response::handle_update_response;

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
        // Clone the RoomSynchronizer to share the same state
        Self {
            room_synchronizer: synchronizer.clone(),
        }
    }

    /// Handles individual API responses.
    /// Returns `true` if a re-PUT should be scheduled (e.g., subscription failed but we have local state).
    pub async fn handle_api_response(
        &mut self,
        response: HostResponse,
    ) -> Result<bool, SynchronizerError> {
        let mut needs_reput = false;

        match response {
            HostResponse::Ok => {
                info!("Received OK response from API");
            }
            HostResponse::ContractResponse(contract_response) => match contract_response {
                ContractResponse::GetResponse {
                    key,
                    contract: _,
                    state,
                } => {
                    handle_get_response(
                        &mut self.room_synchronizer,
                        key,
                        Vec::new(),
                        state.to_vec(),
                    )
                    .await?;
                }
                ContractResponse::PutResponse { key } => {
                    handle_put_response(&mut self.room_synchronizer, key).await?;
                }
                ContractResponse::UpdateNotification { key, update } => {
                    handle_update_notification(&mut self.room_synchronizer, key, update)?;
                }
                ContractResponse::UpdateResponse { key, summary } => {
                    handle_update_response(key, summary.to_vec());
                }
                ContractResponse::SubscribeResponse { key, subscribed } => {
                    needs_reput = handle_subscribe_response(key, subscribed);
                }
                _ => {
                    info!("Unhandled contract response: {:?}", contract_response);
                }
            },
            HostResponse::DelegateResponse { key, values } => {
                info!(
                    "Received delegate response from API with key: {:?} containing {} values",
                    key,
                    values.len()
                );
                for (i, v) in values.iter().enumerate() {
                    info!("Processing delegate response value #{}", i);
                    match v {
                        OutboundDelegateMsg::ApplicationMessage(app_msg) => {
                            info!(
                                "Delegate response is an ApplicationMessage, processed flag: {}",
                                app_msg.processed
                            );

                            // Log the raw payload for debugging
                            let payload_str = if app_msg.payload.len() < 100 {
                                format!("{:?}", app_msg.payload)
                            } else {
                                format!("{:?}... (truncated)", &app_msg.payload[..100])
                            };
                            info!("ApplicationMessage payload: {}", payload_str);

                            // Try to deserialize as a response
                            let deserialization_result = from_reader::<ChatDelegateResponseMsg, _>(
                                app_msg.payload.as_slice(),
                            );

                            // Also try to deserialize as a request to see if that's what's happening
                            let request_deser_result = from_reader::<ChatDelegateRequestMsg, _>(
                                app_msg.payload.as_slice(),
                            );
                            info!(
                                "Deserialization as request result: {:?}",
                                request_deser_result.is_ok()
                            );

                            if let Ok(response) = deserialization_result {
                                info!(
                                    "Successfully deserialized as ChatDelegateResponseMsg: {:?}",
                                    response
                                );
                                // Process the response based on its type
                                match response {
                                    ChatDelegateResponseMsg::GetResponse { key, value } => {
                                        info!(
                                            "Got value for key: {:?}, value present: {}",
                                            String::from_utf8_lossy(key.as_bytes()),
                                            value.is_some()
                                        );

                                        // Check if this is the rooms data
                                        if key.as_bytes() == ROOMS_STORAGE_KEY {
                                            if let Some(rooms_data) = value {
                                                // Deserialize the rooms data
                                                match from_reader::<Rooms, _>(&rooms_data[..]) {
                                                    Ok(loaded_rooms) => {
                                                        info!("Successfully loaded rooms from delegate");

                                                        // Restore the current room selection if saved
                                                        if let Some(saved_room_key) = loaded_rooms.current_room_key {
                                                            info!("Restoring current room selection from delegate");
                                                            *CURRENT_ROOM.write() = CurrentRoom {
                                                                owner_key: Some(saved_room_key),
                                                            };
                                                        }

                                                        // Collect room keys before merge
                                                        let room_keys: Vec<_> = loaded_rooms.map.keys().copied().collect();

                                                        // Merge the loaded rooms with the current rooms
                                                        ROOMS.with_mut(|current_rooms| {
                                                            if let Err(e) = current_rooms.merge(loaded_rooms) {
                                                                error!("Failed to merge rooms: {}", e);
                                                            } else {
                                                                info!("Successfully merged rooms from delegate");
                                                            }
                                                        });

                                                        // Mark all loaded rooms as having completed initial sync
                                                        // and subscribe to receive updates
                                                        for room_key in &room_keys {
                                                            mark_initial_sync_complete(room_key);
                                                        }

                                                        // Subscribe to each loaded room's contract
                                                        info!(
                                                            "Subscribing to {} rooms loaded from delegate",
                                                            room_keys.len()
                                                        );
                                                        for room_key in room_keys {
                                                            // Register the room in SYNC_INFO
                                                            SYNC_INFO.write().register_new_room(room_key);
                                                            SYNC_INFO
                                                                .write()
                                                                .update_sync_status(&room_key, RoomSyncStatus::Subscribing);

                                                            // Get contract key and subscribe
                                                            let contract_key = owner_vk_to_contract_key(&room_key);
                                                            if let Err(e) = self
                                                                .room_synchronizer
                                                                .subscribe_to_contract(&contract_key)
                                                                .await
                                                            {
                                                                error!(
                                                                    "Failed to subscribe to loaded room {:?}: {}",
                                                                    contract_key.id(),
                                                                    e
                                                                );
                                                            } else {
                                                                info!(
                                                                    "Successfully sent subscribe request for loaded room {:?}",
                                                                    contract_key.id()
                                                                );
                                                            }
                                                        }
                                                    }
                                                    Err(e) => {
                                                        error!(
                                                            "Failed to deserialize rooms data: {}",
                                                            e
                                                        );
                                                    }
                                                }
                                            } else {
                                                info!("No rooms data found in delegate");
                                            }
                                        } else {
                                            warn!(
                                                "Unexpected key in GetResponse: {:?}",
                                                String::from_utf8_lossy(key.as_bytes())
                                            );
                                        }
                                    }
                                    ChatDelegateResponseMsg::ListResponse { keys } => {
                                        info!("Listed {} keys", keys.len());
                                    }
                                    ChatDelegateResponseMsg::StoreResponse {
                                        key,
                                        result,
                                        value_size: _,
                                    } => match result {
                                        Ok(_) => info!(
                                            "Successfully stored key: {:?}",
                                            String::from_utf8_lossy(key.as_bytes())
                                        ),
                                        Err(e) => warn!(
                                            "Failed to store key: {:?}, error: {}",
                                            String::from_utf8_lossy(key.as_bytes()),
                                            e
                                        ),
                                    },
                                    ChatDelegateResponseMsg::DeleteResponse { key, result } => {
                                        match result {
                                            Ok(_) => info!(
                                                "Successfully deleted key: {:?}",
                                                String::from_utf8_lossy(key.as_bytes())
                                            ),
                                            Err(e) => warn!(
                                                "Failed to delete key: {:?}, error: {}",
                                                String::from_utf8_lossy(key.as_bytes()),
                                                e
                                            ),
                                        }
                                    }
                                }
                            } else {
                                warn!("Failed to deserialize chat delegate response");
                            }
                        }
                        _ => {
                            warn!("Unhandled delegate response: {:?}", v);
                        }
                    }
                }
            }
            _ => {
                warn!("Unhandled API response: {:?}", response);
            }
        }
        Ok(needs_reput)
    }

    pub fn get_room_synchronizer_mut(&mut self) -> &mut RoomSynchronizer {
        &mut self.room_synchronizer
    }

    // Get a reference to the room synchronizer
    pub fn get_room_synchronizer(&self) -> &RoomSynchronizer {
        &self.room_synchronizer
    }
}
