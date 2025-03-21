mod get_response;
mod put_response;
mod subscribe_response;
mod update_notification;
mod update_response;

use super::error::SynchronizerError;
use super::room_synchronizer::RoomSynchronizer;
use dioxus::logger::tracing::{info, warn};
use freenet_stdlib::client_api::{ContractResponse, HostResponse};
use freenet_stdlib::prelude::OutboundDelegateMsg;
use river_common::chat_delegate::ChatDelegateResponseMsg;
pub use get_response::handle_get_response;
pub use put_response::handle_put_response;
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

    /// Handles individual API responses
    pub async fn handle_api_response(
        &mut self,
        response: HostResponse,
    ) -> Result<(), SynchronizerError> {
        match response {
            HostResponse::Ok => {
                info!("Received OK response from API");
            }
            HostResponse::ContractResponse(contract_response) => match contract_response {
                ContractResponse::GetResponse {
                    key,
                    contract,
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
                    handle_subscribe_response(key, subscribed);
                }
                _ => {
                    info!("Unhandled contract response: {:?}", contract_response);
                }
            },
            HostResponse::DelegateResponse { key: _, values } => {
                info!("Received delegate response from API");
                for v in values {
                    match v {
                        OutboundDelegateMsg::ApplicationMessage(app_msg) => {
                            if let Ok(response) = ciborium::de::from_reader::<ChatDelegateResponseMsg, _>(app_msg.payload.as_slice()) {
                                info!("Received chat delegate response: {:?}", response);
                                // Process the response based on its type
                                match response {
                                    ChatDelegateResponseMsg::GetResponse { key, value } => {
                                        info!("Got value for key: {:?}, value present: {}", 
                                            String::from_utf8_lossy(&key), 
                                            value.is_some());
                                    },
                                    ChatDelegateResponseMsg::ListResponse { keys } => {
                                        info!("Listed {} keys", keys.len());
                                    },
                                    ChatDelegateResponseMsg::StoreResponse { key, result } => {
                                        match result {
                                            Ok(_) => info!("Successfully stored key: {:?}", String::from_utf8_lossy(&key)),
                                            Err(e) => warn!("Failed to store key: {:?}, error: {}", String::from_utf8_lossy(&key), e),
                                        }
                                    },
                                    ChatDelegateResponseMsg::DeleteResponse { key, result } => {
                                        match result {
                                            Ok(_) => info!("Successfully deleted key: {:?}", String::from_utf8_lossy(&key)),
                                            Err(e) => warn!("Failed to delete key: {:?}, error: {}", String::from_utf8_lossy(&key), e),
                                        }
                                    },
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
