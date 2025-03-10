use super::error::SynchronizerError;
use crate::components::app::{ROOMS, WEB_API};
use crate::constants::ROOM_CONTRACT_WASM;
use crate::room_data::{RoomData, RoomSyncStatus, Rooms};
use crate::util::{owner_vk_to_contract_key, to_cbor_vec};
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest, WebApi},
    prelude::{
        ContractCode, ContractContainer, ContractInstanceId, ContractKey, ContractWasmAPIVersion,
        Parameters, RelatedContracts, WrappedContract, WrappedState,
    },
};
use river_common::room_state::member::{AuthorizedMember, MemberId};
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

/// Stores information about a contract being synchronized
#[derive(Clone)]
pub struct ContractSyncInfo {
    pub owner_vk: VerifyingKey,
}

/// Manages synchronization of room state with Freenet
pub struct RoomSynchronizer {
    contract_sync_info: HashMap<ContractInstanceId, ContractSyncInfo>,
}

impl RoomSynchronizer {
    pub fn new() -> Self {
        Self {
            contract_sync_info: HashMap::new(),
        }
    }

    /// Process rooms that need synchronization
    pub async fn process_rooms(&mut self) -> Result<(), SynchronizerError> {
        info!("Processing rooms - starting");

        // First, collect all rooms that need synchronization
        let rooms_to_sync = {
            info!("About to read ROOMS signal to collect rooms needing sync");
            let rooms_read = ROOMS.read();
            info!("Successfully read ROOMS signal for collection");
            
            // Collect keys of rooms that need synchronization
            rooms_read
                .map
                .iter()
                .filter_map(|(key, data)| {
                    // Log detailed information about each room's sync status
                    let is_disconnected = matches!(data.sync_status, RoomSyncStatus::Disconnected);
                    let is_new = matches!(data.sync_status, RoomSyncStatus::NewlyCreated);
                    let is_subscribed = matches!(data.sync_status, RoomSyncStatus::Subscribed);
                    let needs_sync = data.needs_sync();
                    
                    info!(
                        "Room sync evaluation: key={:?}, status={:?}, is_disconnected={}, is_new={}, is_subscribed={}, needs_sync={}",
                        MemberId::from(key), data.sync_status, is_disconnected, is_new, is_subscribed, needs_sync
                    );
                    
                    // Log if room is in subscribing state
                    let is_subscribing = matches!(data.sync_status, RoomSyncStatus::Subscribing);
                    
                    info!(
                        "Room sync evaluation: key={:?}, status={:?}, is_disconnected={}, is_new={}, is_subscribing={}, is_subscribed={}, needs_sync={}",
                        MemberId::from(key), data.sync_status, is_disconnected, is_new, is_subscribing, is_subscribed, needs_sync
                    );
                    
                    // Check for rooms that need synchronization:
                    // 1. Disconnected rooms
                    // 2. Newly created rooms
                    // 3. Rooms that need to be subscribed
                    // 4. Subscribed rooms that have local changes
                    if is_disconnected || is_new || is_subscribing || (is_subscribed && needs_sync) {
                        info!("Room {:?} selected for synchronization", MemberId::from(key));
                        Some(*key)
                    } else {
                        info!("Room {:?} does NOT need synchronization", MemberId::from(key));
                        None
                    }
                })
                .collect::<Vec<VerifyingKey>>()
        };

        info!(
            "Found {} rooms that need synchronization",
            rooms_to_sync.len()
        );

        // Log details about each room that needs sync
        {
            info!("About to read ROOMS signal for logging details");
            let rooms_read = ROOMS.read();
            info!("Successfully read ROOMS signal for logging");
            
            for key in &rooms_to_sync {
                if let Some(room_data) = rooms_read.map.get(key) {
                    info!(
                        "Room {:?} needs sync: status={:?}, needs_sync={}, is_new={}",
                        MemberId::from(key),
                        room_data.sync_status,
                        room_data.needs_sync(),
                        matches!(room_data.sync_status, RoomSyncStatus::NewlyCreated)
                    );
                }
            }
        }

        // Process each room that needs synchronization
        for room_key in rooms_to_sync {
            // Check status again before processing
            let should_sync = {
                info!("About to read ROOMS signal to check if room {:?} should sync", MemberId::from(room_key));
                let rooms_read = ROOMS.read();
                info!("Successfully read ROOMS signal for checking sync status");
                
                if let Some(room_data) = rooms_read.map.get(&room_key) {
                    matches!(room_data.sync_status, RoomSyncStatus::Disconnected)
                        || matches!(room_data.sync_status, RoomSyncStatus::NewlyCreated)
                        || matches!(room_data.sync_status, RoomSyncStatus::Subscribing)
                        || (matches!(room_data.sync_status, RoomSyncStatus::Subscribed)
                            && room_data.needs_sync())
                } else {
                    false
                }
            };

            if should_sync {
                // Get the current status to determine what action to take
                let current_status = {
                    let rooms_read = ROOMS.read();
                    if let Some(room_data) = rooms_read.map.get(&room_key) {
                        room_data.sync_status.clone()
                    } else {
                        continue; // Skip if room no longer exists
                    }
                };

                match current_status {
                    RoomSyncStatus::Subscribing => {
                        // For rooms that need to be subscribed, directly call subscribe_to_contract
                        let contract_key = owner_vk_to_contract_key(&room_key);
                        info!("About to subscribe to contract for room {:?}", MemberId::from(room_key));
                        if let Err(e) = self.subscribe_to_contract(contract_key).await {
                            error!("Failed to subscribe to room: {}", e);

                            // Reset status on error
                            info!("About to modify ROOMS signal to reset status on error");
                            ROOMS.with_mut(|rooms| {
                                if let Some(room_data) = rooms.map.get_mut(&room_key) {
                                    room_data.sync_status = RoomSyncStatus::Disconnected;
                                    info!("Reset room status to Disconnected due to error");
                                }
                            });
                            info!("Successfully modified ROOMS signal for error reset");

                            return Err(e);
                        }
                    },
                    RoomSyncStatus::Disconnected | RoomSyncStatus::NewlyCreated => {
                        // Update status before processing
                        {
                            info!("About to modify ROOMS signal to update status for room {:?}", MemberId::from(room_key));
                            ROOMS.with_mut(|rooms| {
                                info!("Inside with_mut closure for updating status");
                                if let Some(room_data) = rooms.map.get_mut(&room_key) {
                                    room_data.sync_status = RoomSyncStatus::Putting;
                                    info!("Updated room status to Putting for room {:?}", MemberId::from(room_key));
                                }
                            });
                            info!("Successfully modified ROOMS signal for status update");
                        }

                        // Now process the room
                        info!("About to call put_and_subscribe for room {:?}", MemberId::from(room_key));
                        if let Err(e) = self.put_and_subscribe(&room_key).await {
                            error!("Failed to put and subscribe room: {}", e);

                            // Reset status on error
                            info!("About to modify ROOMS signal to reset status on error");
                            ROOMS.with_mut(|rooms| {
                                if let Some(room_data) = rooms.map.get_mut(&room_key) {
                                    room_data.sync_status = RoomSyncStatus::Disconnected;
                                    info!("Reset room status to Disconnected due to error");
                                }
                            });
                            info!("Successfully modified ROOMS signal for error reset");

                            return Err(e);
                        }
                    },
                    RoomSyncStatus::Subscribed if ROOMS.read().map.get(&room_key).map_or(false, |data| data.needs_sync()) => {
                        // For subscribed rooms with changes, mark them as synced
                        // In a real implementation, you might want to send updates to the network here
                        info!("Marking room as synced: {:?}", MemberId::from(room_key));
                        ROOMS.with_mut(|rooms| {
                            if let Some(room_data) = rooms.map.get_mut(&room_key) {
                                room_data.mark_synced();
                                info!("Marked room as synced: {:?}", MemberId::from(room_key));
                            }
                        });
                    },
                    _ => {
                        // Other states don't need processing
                        info!("Room {:?} in state {:?} doesn't need specific processing", 
                              MemberId::from(room_key), current_status);
                    }
                }
                
                info!("Successfully processed room {:?}", MemberId::from(room_key));
            }
        }

        info!("Finished processing all rooms");
        Ok(())
    }

    /// Put room state to Freenet and subscribe to updates
    pub async fn put_and_subscribe(
        &mut self,
        owner_vk: &VerifyingKey,
    ) -> Result<(), SynchronizerError> {
        info!("Putting room state for: {:?}", MemberId::from(owner_vk));

        // First, get all the data we need under a limited scope
        let (contract_key, state_bytes, parameters_bytes) = {
            info!("About to read ROOMS signal in put_and_subscribe");
            let rooms_read = ROOMS.read();
            info!("Successfully read ROOMS signal in put_and_subscribe");
            
            let room_data = rooms_read
                .map
                .get(owner_vk)
                .ok_or_else(|| SynchronizerError::RoomNotFound(format!("{:?}", MemberId::from(owner_vk))))?;

            let contract_key: ContractKey = owner_vk_to_contract_key(owner_vk);
            let state_bytes = to_cbor_vec(&room_data.room_state);
            let parameters = ChatRoomParametersV1 { owner: *owner_vk };
            let params_bytes = to_cbor_vec(&parameters);
            
            info!("Extracted necessary data from ROOMS signal");
            (contract_key, state_bytes, params_bytes)
        };

        // Store contract sync info
        info!("Storing contract sync info");
        self.contract_sync_info.insert(
            *contract_key.id(),
            ContractSyncInfo {
                owner_vk: *owner_vk,
            },
        );

        // Prepare the contract request
        info!("Preparing contract request");
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let parameters = Parameters::from(parameters_bytes);

        let contract_container = ContractContainer::from(ContractWasmAPIVersion::V1(
            WrappedContract::new(Arc::new(contract_code), parameters),
        ));

        let wrapped_state = WrappedState::new(state_bytes.clone());

        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: wrapped_state,
            related_contracts: RelatedContracts::default(),
        };

        let client_request = ClientRequest::ContractOp(put_request);

        // Put the contract state using our helper method
        info!("Sending API request");
        self.send_api_request(client_request)
            .await
            .map_err(|e| SynchronizerError::PutContractError(e.to_string()))?;
        info!("API request sent successfully");

        // Update status for newly created rooms - do this in a separate scope
        // IMPORTANT: This is a separate write operation after an await point
        {
            info!("About to write to ROOMS signal to update status");
            ROOMS.with_mut(|rooms| {
                info!("Inside with_mut closure for updating newly created room status");
                if let Some(room_data) = rooms.map.get_mut(owner_vk) {
                    if matches!(room_data.sync_status, RoomSyncStatus::NewlyCreated) {
                        info!(
                            "Changing newly created room status to Putting: {:?}",
                            MemberId::from(owner_vk)
                        );
                        room_data.sync_status = RoomSyncStatus::Putting;
                    }
                }
            });
            info!("Successfully updated ROOMS signal for newly created room");
        }

        // Will subscribe when response comes back from PUT
        info!("put_and_subscribe completed successfully");
        Ok(())
    }

    /// Subscribe to a contract after a successful PUT
    pub async fn subscribe_to_contract(
        &mut self,
        contract_key: ContractKey,
    ) -> Result<(), SynchronizerError> {
        info!("Subscribing to contract: {}", contract_key);

        let client_request = ClientRequest::ContractOp(ContractRequest::Subscribe {
            key: contract_key.clone(),
            summary: None,
        });

        info!("Sending subscribe request for contract: {}", contract_key);
        
        // Send the request first before updating any state to avoid holding locks across await points
        self.send_api_request(client_request).await?;
        
        info!("Subscribe request sent successfully for: {}", contract_key);

        // Update room status if we have the owner info
        let owner_vk_option = {
            // Get the owner_vk in a separate scope to avoid holding the reference
            if let Some(info) = self.contract_sync_info.get(&contract_key.id()) {
                info!("Found contract info for: {}", contract_key);
                Some(info.owner_vk)
            } else {
                warn!("Contract info not found for key: {}", contract_key);
                None
            }
        };
        
        // Now update the room status if we have the owner_vk
        if let Some(owner_vk) = owner_vk_option {
            info!("About to update ROOMS signal for subscription status");
            ROOMS.with_mut(|rooms| {
                info!("Inside with_mut closure for updating subscription status");
                if let Some(room_data) = rooms.map.get_mut(&owner_vk) {
                    info!(
                        "Updating room status from {:?} to Subscribing for: {:?}",
                        room_data.sync_status, MemberId::from(owner_vk)
                    );
                    room_data.sync_status = RoomSyncStatus::Subscribing;
                } else {
                    warn!("Room data not found for owner: {:?}", MemberId::from(owner_vk));
                }
            });
            info!("Successfully updated ROOMS signal for subscription");
        }

        Ok(())
    }

    /// Helper method to update room state with new state data
    pub fn update_room_state(
        &mut self,
        owner_vk: &VerifyingKey,
        new_state: &ChatRoomStateV1,
    ) -> Result<(), SynchronizerError> {
        let result = ROOMS.with_mut(|rooms| {
            if let Some(room_data) = rooms.map.get_mut(owner_vk) {
                // Clone the state to avoid borrowing issues
                let parent_state = room_data.room_state.clone();
                let parameters = ChatRoomParametersV1 { owner: *owner_vk };

                match room_data
                    .room_state
                    .merge(&parent_state, &parameters, new_state)
                {
                    Ok(_) => {
                        room_data.mark_synced();
                        info!("Successfully updated room state for owner: {:?}", MemberId::from(owner_vk));
                        Ok(())
                    }
                    Err(e) => {
                        let error = SynchronizerError::StateMergeError(e.to_string());
                        error!("Failed to merge room state: {}", error);
                        Err(error)
                    }
                }
            } else {
                warn!(
                    "Received state update for unknown room with owner: {:?}",
                    MemberId::from(owner_vk)
                );
                Ok(())
            }
        });

        result?;
        Ok(())
    }

    /// Apply a delta update to a room's state
    pub fn apply_delta(
        &mut self,
        owner_vk: &VerifyingKey,
        delta: &ChatRoomStateV1Delta,
    ) -> Result<(), SynchronizerError> {
        let result = ROOMS.with_mut(|rooms| {
            if let Some(room_data) = rooms.map.get_mut(owner_vk) {
                // Clone the state to avoid borrowing issues
                let parent_state = room_data.room_state.clone();
                let parameters = ChatRoomParametersV1 { owner: *owner_vk };

                match room_data.room_state.apply_delta(
                    &parent_state,
                    &parameters,
                    &Some(delta.clone()),
                ) {
                    Ok(_) => {
                        room_data.mark_synced();
                        info!(
                            "Successfully applied delta to room state for owner: {:?}",
                            MemberId::from(owner_vk)
                        );
                        Ok(())
                    }
                    Err(e) => {
                        let error = SynchronizerError::DeltaApplyError(e.to_string());
                        error!("Failed to apply delta to room state: {}", error);
                        Err(error)
                    }
                }
            } else {
                warn!(
                    "Received delta update for unknown room with owner: {:?}",
                    MemberId::from(owner_vk)
                );
                Ok(())
            }
        });

        result?;
        Ok(())
    }

    /// Get contract sync info for a contract ID
    pub fn get_contract_info(&self, contract_id: &ContractInstanceId) -> Option<&ContractSyncInfo> {
        self.contract_sync_info.get(contract_id)
    }

    /// Mark a room as subscribed
    pub fn mark_room_subscribed(&mut self, owner_vk: &VerifyingKey) {
        ROOMS.with_mut(|rooms| {
            if let Some(room_data) = rooms.map.get_mut(owner_vk) {
                room_data.sync_status = RoomSyncStatus::Subscribed;
            }
        });
    }

    /// Helper method to safely send API requests without holding locks across await points
    async fn send_api_request(
        &self,
        request: ClientRequest<'static>,
    ) -> Result<(), SynchronizerError> {
        // First check if API is available without holding a write lock
        {
            if WEB_API.read().is_none() {
                return Err(SynchronizerError::ApiNotInitialized);
            }
        }
        
        // Get the API instance first, then release the lock before awaiting
        let api_result = {
            let mut web_api_guard = WEB_API.write();
            if let Some(_web_api) = &mut *web_api_guard {
                // Prepare the request without cloning
                let request_clone = request.clone();
                Ok(request_clone)
            } else {
                Err(SynchronizerError::ApiNotInitialized)
            }
        };
        
        // Now send the request without holding the lock
        match api_result {
            Ok(request_clone) => {
                // Get a fresh lock just for sending
                if let Some(web_api) = &mut *WEB_API.write() {
                    web_api.send(request_clone)
                        .await
                        .map_err(|e| SynchronizerError::SubscribeError(e.to_string()))?;
                    Ok(())
                } else {
                    Err(SynchronizerError::ApiNotInitialized)
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Create a new room from an invitation
    pub async fn create_room_from_invitation(
        &mut self,
        owner_vk: VerifyingKey,
        _authorized_member: AuthorizedMember,
        invitee_signing_key: SigningKey,
        _nickname: String,
    ) -> Result<(), SynchronizerError> {
        info!("Creating room from invitation for owner: {:?}", MemberId::from(owner_vk));

        // Create a new empty room state
        let room_state = ChatRoomStateV1::default();

        // Create the contract key
        let contract_key = owner_vk_to_contract_key(&owner_vk);

        // Create a new room data entry
        let room_data = RoomData {
            owner_vk,
            room_state,
            self_sk: invitee_signing_key,
            contract_key,
            sync_status: RoomSyncStatus::Subscribing, // Changed from Disconnected to Subscribing
            last_synced_state: None,
        };

        // Add the room to our rooms map
        {
            ROOMS.with_mut(|rooms| {
                rooms.map.insert(owner_vk, room_data);
            });
        }

        // Register the contract info
        self.contract_sync_info
            .insert(*contract_key.id(), ContractSyncInfo { owner_vk });

        // Now trigger a sync for this room
        self.process_rooms().await?;

        Ok(())
    }
}
