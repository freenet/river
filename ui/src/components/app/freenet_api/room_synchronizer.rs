use super::error::SynchronizerError;
use crate::components::app::{ROOMS, WEB_API, PENDING_INVITES};
use crate::constants::ROOM_CONTRACT_WASM;
use crate::util::{owner_vk_to_contract_key, to_cbor_vec, sleep};
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use freenet_scaffold::ComposableState;
use std::time::Duration;
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest},
    prelude::{
        ContractCode, ContractContainer, ContractInstanceId, ContractWasmAPIVersion,
        Parameters, WrappedContract, WrappedState, UpdateData,
    },
};
use river_common::room_state::member::MemberId;
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::collections::HashMap;
use std::sync::Arc;
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::invites::PendingRoomStatus;

/// Identifies contracts that have changed in order to send state updates to Freenet
pub struct RoomSynchronizer {
    contract_sync_info: HashMap<ContractInstanceId, ContractSyncInfo>,
}

impl RoomSynchronizer {
    pub(crate) fn apply_delta(&self, owner_vk: &VerifyingKey, delta: ChatRoomStateV1Delta) {
        ROOMS.with_mut(|rooms| {
            if let Some(room_data) = rooms.map.get_mut(owner_vk) {
                let params = ChatRoomParametersV1 {
                    owner: *owner_vk,
                };
                // Apply the delta to the room state
                // Clone the state to avoid borrowing issues
                let state_clone = room_data.room_state.clone();
                room_data.room_state.apply_delta(&state_clone, &params, &Some(delta)).expect("Failed to apply delta");
                // Update the last synced state
                SYNC_INFO.write().update_last_synced_state(owner_vk, &room_data.room_state);
            } else {
                warn!("Room not found in rooms map");
            }
        });
    }
}

impl RoomSynchronizer {
    pub fn new() -> Self {
        Self {
            contract_sync_info: HashMap::new(),
        }
    }

    /// Send updates to the network for any room that has changed locally
    /// Should be called after modification detected to Signal<Rooms>
    pub async fn process_rooms(&mut self) -> Result<(), SynchronizerError> {
        info!("Processing rooms");

        // First, check for pending invitations that need subscription
        // Collect keys that need subscription without holding the read lock
        let invites_to_subscribe: Vec<VerifyingKey> = {
            let pending_invites = PENDING_INVITES.read();
            pending_invites.map.iter()
                .filter(|(_, join)| matches!(join.status, PendingRoomStatus::PendingSubscription))
                .map(|(key, _)| *key)
                .collect()
        };
        
        if !invites_to_subscribe.is_empty() {
            info!("Found {} pending invitations to subscribe to", invites_to_subscribe.len());
            
            for owner_vk in invites_to_subscribe {
                info!("Subscribing to room for invitation: {:?}", MemberId::from(owner_vk));
                
                let contract_key = owner_vk_to_contract_key(&owner_vk);
                
                // Create a subscribe request instead of a put request
                let subscribe_request = ContractRequest::Subscribe {
                    key: contract_key,
                    summary: None,
                };
                
                let client_request = ClientRequest::ContractOp(subscribe_request);
                
                if let Some(web_api) = WEB_API.write().as_mut() {
                    match web_api.send(client_request).await {
                        Ok(_) => {
                            info!("Sent SubscribeRequest for room {:?}", MemberId::from(owner_vk));
                            // Register the room in SYNC_INFO
                            SYNC_INFO.write().register_new_room(owner_vk);
                            // Update the sync status to subscribing
                            SYNC_INFO.write().update_sync_status(&owner_vk, RoomSyncStatus::Subscribing);
                            
                            // Update the pending invite status to Subscribing
                            PENDING_INVITES.with_mut(|pending| {
                                if let Some(join) = pending.map.get_mut(&owner_vk) {
                                    join.status = PendingRoomStatus::Subscribing;
                                }
                            });
                        },
                        Err(e) => {
                            error!("Error sending SubscribeRequest to room {:?}: {}", MemberId::from(owner_vk), e);
                            // Update pending invite status to error
                            PENDING_INVITES.with_mut(|pending| {
                                if let Some(join) = pending.map.get_mut(&owner_vk) {
                                    join.status = PendingRoomStatus::Error(e.to_string());
                                }
                            });
                        }
                    }
                } else {
                    warn!("WebAPI not available, skipping room subscription");
                }
            }
        }

        info!("Checking for rooms that need to be subscribed");

        let rooms_to_subscribe = SYNC_INFO.write().rooms_awaiting_subscription();

        if !rooms_to_subscribe.is_empty() {
            for (owner_vk, state) in &rooms_to_subscribe {
                info!("Subscribing to room: {:?}", MemberId::from(*owner_vk));

                let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
                let parameters = ChatRoomParametersV1 { owner: *owner_vk };
                let params_bytes = to_cbor_vec(&parameters);
                let parameters = Parameters::from(params_bytes);

                let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
                    WrappedContract::new(Arc::new(contract_code), parameters),
                ));

                let wrapped_state = WrappedState::new(to_cbor_vec(state).into());

                // Somewhat misleadingly, we now subscribe using a put request with subscribe: true
                let contract_key = owner_vk_to_contract_key(owner_vk);
                let contract_id = contract_key.id();
                info!("Preparing PutRequest for room {:?} with contract ID: {}", MemberId::from(*owner_vk), contract_id);

                let put_request = ContractRequest::Put {
                    contract: contract_container,
                    state: wrapped_state,
                    related_contracts: Default::default(),
                    subscribe: true,
                };

                let client_request = ClientRequest::ContractOp(put_request);

                info!("Sending PutRequest for room {:?} with contract ID: {}", MemberId::from(*owner_vk), contract_id);

                if let Some(web_api) = WEB_API.write().as_mut() {
                    match web_api.send(client_request).await {
                        Ok(_) => {
                            info!("Sent PutRequest for room {:?}", MemberId::from(*owner_vk));
                            // Update the sync status to subscribing
                            SYNC_INFO.write().update_sync_status(owner_vk, RoomSyncStatus::Subscribing);
                            
                            // Add a delay after PUT to allow the contract to be created
                            // This helps prevent the "contract not found" error
                            info!("Waiting {}ms for contract to be created...", super::constants::POST_PUT_DELAY_MS);
                            sleep(Duration::from_millis(super::constants::POST_PUT_DELAY_MS)).await;
                            info!("Finished waiting for contract creation");
                        },
                        Err(e) => {
                            // Don't fail the entire process if one room fails
                            error!("Error sending PutRequest to room {:?}: {}", MemberId::from(*owner_vk), e);
                            // Update sync status to error
                            SYNC_INFO.write().update_sync_status(owner_vk, RoomSyncStatus::Error(e.to_string()));
                        }
                    }
                } else {
                    warn!("WebAPI not available, skipping room subscription");
                }
            }
        }

        info!("Checking for rooms to update");

        let rooms_to_sync = SYNC_INFO.write().needs_to_send_update();

        info!(
            "Found {} rooms that need synchronization",
            rooms_to_sync.len()
        );

        for (room_vk, state) in &rooms_to_sync {
            info!("Processing room: {:?}", MemberId::from(*room_vk));

            let contract_key = owner_vk_to_contract_key(room_vk);

            let update_request = ContractRequest::Update {
                key: contract_key,
                data: UpdateData::State(to_cbor_vec(state).into()),
            };

            let client_request = ClientRequest::ContractOp(update_request);

            if let Some(web_api) = WEB_API.write().as_mut() {
                match web_api.send(client_request).await {
                    Ok(_) => {
                        info!("Successfully sent update for room: {:?}", MemberId::from(*room_vk));
                    },
                    Err(e) => {
                        // Don't fail the entire process if one room fails
                        error!("Failed to send update for room {:?}: {}", MemberId::from(*room_vk), e);
                    }
                }
            } else {
                warn!("WebAPI not available, skipping room update");
            }
        }

        info!("Finished processing all rooms");
        Ok(())
    }

    /// Updates the room state and last_sync_state, should be called after state update received from network
    pub(crate) fn update_room_state(&self, room_owner_vk: &VerifyingKey, state: &ChatRoomStateV1) {
        ROOMS.with_mut(|rooms| {
            if let Some(room_data) = rooms.map.get_mut(room_owner_vk) {
                // Update the room state by merging the new state with the existing one,
                // more robust than just replacing it
                room_data.room_state
                    .merge(&room_data.room_state.clone(), &ChatRoomParametersV1 { owner: *room_owner_vk }, state)
                    .expect("Failed to merge room state");
                
                // Make sure the room is registered in SYNC_INFO
                SYNC_INFO.with_mut(|sync_info| {
                    sync_info.register_new_room(*room_owner_vk);
                    // We use the post-merged state to avoid some edge cases
                    sync_info.update_last_synced_state(room_owner_vk, &room_data.room_state);
                });
            } else {
                warn!("Room not found in rooms map");
            }
        });

    }

    // The create_room_from_invitation method has been removed as we now handle
    // invitation acceptance through the process_rooms flow and response handler
}

/// Stores information about a contract being synchronized
#[derive(Clone)]
pub struct ContractSyncInfo {
    pub owner_vk: VerifyingKey,
}
