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
use river_common::room_state::member::AuthorizedMember;
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

/// Stores information about a contract being synchronized
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
        info!("Processing rooms");

        let rooms_to_sync = {
            // Collect keys of rooms that need synchronization
            ROOMS
                .read()
                .map
                .iter()
                .filter_map(|(key, data)| {
                    // Check for rooms that need synchronization:
                    // 1. Disconnected rooms
                    // 2. Newly created rooms
                    // 3. Subscribed rooms that have local changes
                    if matches!(data.sync_status, RoomSyncStatus::Disconnected)
                        || matches!(data.sync_status, RoomSyncStatus::NewlyCreated)
                        || (matches!(data.sync_status, RoomSyncStatus::Subscribed)
                            && data.needs_sync())
                    {
                        Some(*key)
                    } else {
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
            for key in &rooms_to_sync {
                if let Some(room_data) = ROOMS.read().map.get(key) {
                    info!(
                        "Room {:?} needs sync: status={:?}, needs_sync={}, is_new={}",
                        key,
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
                if let Some(room_data) = ROOMS.read().map.get(&room_key) {
                    matches!(room_data.sync_status, RoomSyncStatus::Disconnected)
                        || matches!(room_data.sync_status, RoomSyncStatus::NewlyCreated)
                        || (matches!(room_data.sync_status, RoomSyncStatus::Subscribed)
                            && room_data.needs_sync())
                } else {
                    false
                }
            };

            if should_sync {
                // Update status before processing
                {
                    ROOMS.with_mut(|rooms| {
                        if let Some(room_data) = rooms.map.get_mut(&room_key) {
                            room_data.sync_status = RoomSyncStatus::Putting;
                        }
                    });
                }

                // Now process the room
                if let Err(e) = self.put_and_subscribe(&room_key).await {
                    error!("Failed to put and subscribe room: {}", e);

                    // Reset status on error
                    ROOMS.with_mut(|rooms| {
                        if let Some(room_data) = rooms.map.get_mut(&room_key) {
                            room_data.sync_status = RoomSyncStatus::Disconnected;
                        }
                    });

                    return Err(e);
                }
            }
        }

        Ok(())
    }

    /// Put room state to Freenet and subscribe to updates
    pub async fn put_and_subscribe(
        &mut self,
        owner_vk: &VerifyingKey,
    ) -> Result<(), SynchronizerError> {
        info!("Putting room state for: {:?}", owner_vk);

        // Get room data under a limited scope to release the lock quickly
        let room_data = ROOMS.read()
            .map
            .get(owner_vk)
            .ok_or_else(|| SynchronizerError::RoomNotFound(format!("{:?}", owner_vk)))?;

        let contract_key: ContractKey = owner_vk_to_contract_key(owner_vk);
        let state_bytes = to_cbor_vec(&room_data.room_state);

        self.contract_sync_info.insert(
            *contract_key.id(),
            ContractSyncInfo {
                owner_vk: *owner_vk,
            },
        );

        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let parameters = ChatRoomParametersV1 { owner: *owner_vk };
        let params_bytes = to_cbor_vec(&parameters);
        let parameters = Parameters::from(params_bytes);

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
        self.send_api_request(client_request)
            .await
            .map_err(|e| SynchronizerError::PutContractError(e.to_string()))?;

        // Update status for newly created rooms
        if let Some(room_data) = ROOMS.write().map.get_mut(owner_vk) {
            if matches!(room_data.sync_status, RoomSyncStatus::NewlyCreated) {
                info!(
                    "Changing newly created room status to Putting: {:?}",
                    owner_vk
                );
                room_data.sync_status = RoomSyncStatus::Putting;
            }
        }

        // Will subscribe when response comes back from PUT
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
        
        // Create a separate function to handle the API call to avoid holding locks across await points
        self.send_api_request(client_request).await?;
        
        info!("Subscribe request sent successfully for: {}", contract_key);

        // Update room status if we have the owner info
        if let Some(info) = self.contract_sync_info.get(&contract_key.id()) {
            info!("Found contract info for: {}", contract_key);
            ROOMS.with_mut(|rooms| {
                if let Some(room_data) = rooms.map.get_mut(&info.owner_vk) {
                    info!(
                        "Updating room status from {:?} to Subscribing for: {:?}",
                        room_data.sync_status, info.owner_vk
                    );
                    room_data.sync_status = RoomSyncStatus::Subscribing;
                } else {
                    warn!("Room data not found for owner: {:?}", info.owner_vk);
                }
            });
        } else {
            warn!("Contract info not found for key: {}", contract_key);
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
                        info!("Successfully updated room state for owner: {:?}", owner_vk);
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
                    owner_vk
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
                            owner_vk
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
                    owner_vk
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
        
        // Now get a fresh lock and send the request
        if let Some(web_api) = &mut *WEB_API.write() {
            web_api
                .send(request)
                .await
                .map_err(|e| SynchronizerError::SubscribeError(e.to_string()))?;
            Ok(())
        } else {
            Err(SynchronizerError::ApiNotInitialized)
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
        info!("Creating room from invitation for owner: {:?}", owner_vk);

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
            sync_status: RoomSyncStatus::Disconnected,
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
