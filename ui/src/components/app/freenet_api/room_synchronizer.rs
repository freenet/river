use super::error::SynchronizerError;
use crate::room_data::{Rooms, RoomSyncStatus};
use crate::util::{owner_vk_to_contract_key, to_cbor_vec};
use dioxus::prelude::*;
use dioxus::logger::tracing::{info, warn, error};
use ed25519_dalek::VerifyingKey;
use std::collections::HashMap;
use std::sync::Arc;
use freenet_scaffold::ComposableState;
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest, WebApi},
    prelude::{ContractCode, ContractContainer, ContractInstanceId, ContractKey, ContractWasmAPIVersion, Parameters, RelatedContracts, WrappedContract, WrappedState},
};
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use crate::constants::ROOM_CONTRACT_WASM;

/// Stores information about a contract being synchronized
pub struct ContractSyncInfo {
    pub owner_vk: VerifyingKey,
}

/// Manages synchronization of room state with Freenet
pub struct RoomSynchronizer {
    rooms: Signal<Rooms>,
    contract_sync_info: HashMap<ContractInstanceId, ContractSyncInfo>,
}

impl RoomSynchronizer {
    pub fn new(rooms: Signal<Rooms>) -> Self {
        Self {
            rooms,
            contract_sync_info: HashMap::new(),
        }
    }
    
    // Get a reference to the rooms signal
    pub fn get_rooms_signal(&self) -> &Signal<Rooms> {
        &self.rooms
    }

    /// Process rooms that need synchronization
    pub async fn process_rooms(&mut self, web_api: &mut WebApi) -> Result<(), SynchronizerError> {
        info!("Checking for rooms that need synchronization");
        
        // No need to check if WebAPI is connected - if we have a reference to it, it's initialized
        // Just proceed with the synchronization
        
        // Use a more cautious approach with signals
        let rooms_to_sync = {
            // Get read-only access to rooms first to identify which ones need sync
            let rooms_read = self.rooms.read();
            
            // Collect keys of rooms that need synchronization
            rooms_read.map.iter()
                .filter_map(|(key, data)| {
                    if matches!(data.sync_status, RoomSyncStatus::Disconnected) {
                        Some(*key)
                    } else {
                        None
                    }
                })
                .collect::<Vec<VerifyingKey>>()
        };
        
        info!("Found {} rooms that need synchronization", rooms_to_sync.len());
        
        // Process each room that needs synchronization
        for room_key in rooms_to_sync {
            // Check status again before processing
            let should_sync = {
                let rooms_read = self.rooms.read();
                if let Some(room_data) = rooms_read.map.get(&room_key) {
                    matches!(room_data.sync_status, RoomSyncStatus::Disconnected)
                } else {
                    false
                }
            };
            
            if should_sync {
                // Update status before processing
                {
                    let mut rooms_write = self.rooms.write();
                    if let Some(room_data) = rooms_write.map.get_mut(&room_key) {
                        room_data.sync_status = RoomSyncStatus::Putting;
                    }
                }
                
                // Now process the room
                if let Err(e) = self.put_and_subscribe(&room_key, web_api).await {
                    error!("Failed to put and subscribe room: {}", e);
                    
                    // Reset status on error
                    let mut rooms_write = self.rooms.write();
                    if let Some(room_data) = rooms_write.map.get_mut(&room_key) {
                        room_data.sync_status = RoomSyncStatus::Disconnected;
                    }
                    
                    return Err(e);
                }
            }
        }
        
        Ok(())
    }

    /// Put room state to Freenet and subscribe to updates
    pub async fn put_and_subscribe(&mut self, owner_vk: &VerifyingKey, web_api: &mut WebApi) -> Result<(), SynchronizerError> {
        info!("Putting room state for: {:?}", owner_vk);

        // Get room data under a limited scope to release the lock quickly
        let (contract_key, state_bytes) = {
            let rooms_read = self.rooms.read();
            let room_data = rooms_read.map.get(owner_vk)
                .ok_or_else(|| SynchronizerError::RoomNotFound(format!("{:?}", owner_vk)))?;
            
            let contract_key: ContractKey = owner_vk_to_contract_key(owner_vk);
            let state_bytes = to_cbor_vec(&room_data.room_state);
            
            (contract_key, state_bytes)
        };

        self.contract_sync_info.insert(*contract_key.id(), ContractSyncInfo {
            owner_vk: *owner_vk,
        });

        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let parameters = ChatRoomParametersV1 { owner: *owner_vk };
        let params_bytes = to_cbor_vec(&parameters);
        let parameters = Parameters::from(params_bytes);

        let contract_container = ContractContainer::from(
            ContractWasmAPIVersion::V1(
                WrappedContract::new(
                    Arc::new(contract_code),
                    parameters,
                ),
            )
        );

        let wrapped_state = WrappedState::new(state_bytes.clone());

        let put_request = ContractRequest::Put {
            contract: contract_container,
            state: wrapped_state,
            related_contracts: RelatedContracts::default(),
        };
        
        let client_request = ClientRequest::ContractOp(put_request);

        // Put the contract state
        web_api.send(client_request).await
            .map_err(|e| SynchronizerError::PutContractError(e.to_string()))?;

        // Will subscribe when response comes back from PUT
        Ok(())
    }

    /// Subscribe to a contract after a successful PUT
    pub async fn subscribe_to_contract(&mut self, contract_key: ContractKey, web_api: &mut WebApi) -> Result<(), SynchronizerError> {
        let client_request = ClientRequest::ContractOp(ContractRequest::Subscribe {
            key: contract_key.clone(),
            summary: None,
        });
        
        web_api.send(client_request).await
            .map_err(|e| SynchronizerError::SubscribeError(e.to_string()))?;
        
        // Update room status if we have the owner info
        if let Some(info) = self.contract_sync_info.get(&contract_key.id()) {
            let mut rooms = self.rooms.write();
            if let Some(room_data) = rooms.map.get_mut(&info.owner_vk) {
                room_data.sync_status = RoomSyncStatus::Subscribing;
            }
        }
        
        Ok(())
    }

    /// Helper method to update room state with new state data
    pub fn update_room_state(&mut self, owner_vk: &VerifyingKey, new_state: &ChatRoomStateV1) -> Result<(), SynchronizerError> {
        let mut rooms = self.rooms.write();
        if let Some(room_data) = rooms.map.get_mut(owner_vk) {
            // Clone the state to avoid borrowing issues
            let parent_state = room_data.room_state.clone();
            let parameters = ChatRoomParametersV1 { owner: *owner_vk };
            
            match room_data.room_state.merge(&parent_state, &parameters, new_state) {
                Ok(_) => {
                    room_data.mark_synced();
                    info!("Successfully updated room state for owner: {:?}", owner_vk);
                },
                Err(e) => {
                    let error = SynchronizerError::StateMergeError(e.to_string());
                    error!("Failed to merge room state: {}", error);
                    return Err(error);
                }
            }
        } else {
            warn!("Received state update for unknown room with owner: {:?}", owner_vk);
        }
        Ok(())
    }

    /// Apply a delta update to a room's state
    pub fn apply_delta(&mut self, owner_vk: &VerifyingKey, delta: &ChatRoomStateV1Delta) -> Result<(), SynchronizerError> {
        let mut rooms = self.rooms.write();
        if let Some(room_data) = rooms.map.get_mut(owner_vk) {
            // Clone the state to avoid borrowing issues
            let parent_state = room_data.room_state.clone();
            let parameters = ChatRoomParametersV1 { owner: *owner_vk };
            
            match room_data.room_state.apply_delta(&parent_state, &parameters, &Some(delta.clone())) {
                Ok(_) => {
                    room_data.mark_synced();
                    info!("Successfully applied delta to room state for owner: {:?}", owner_vk);
                },
                Err(e) => {
                    let error = SynchronizerError::DeltaApplyError(e.to_string());
                    error!("Failed to apply delta to room state: {}", error);
                    return Err(error);
                }
            }
        } else {
            warn!("Received delta update for unknown room with owner: {:?}", owner_vk);
        }
        Ok(())
    }

    /// Get contract sync info for a contract ID
    pub fn get_contract_info(&self, contract_id: &ContractInstanceId) -> Option<&ContractSyncInfo> {
        self.contract_sync_info.get(contract_id)
    }

    /// Mark a room as subscribed
    pub fn mark_room_subscribed(&mut self, owner_vk: &VerifyingKey) {
        let mut rooms = self.rooms.write();
        if let Some(room_data) = rooms.map.get_mut(owner_vk) {
            room_data.sync_status = RoomSyncStatus::Subscribed;
        }
    }
}
