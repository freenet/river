use super::error::SynchronizerError;
use crate::room_data::{Rooms, RoomSyncStatus};
use crate::util::{from_cbor_slice, owner_vk_to_contract_key, to_cbor_vec};
use dioxus::prelude::*;
use dioxus::logger::tracing::{info, error, warn};
use ed25519_dalek::VerifyingKey;
use std::collections::HashMap;
use std::sync::Arc;
use freenet_scaffold::ComposableState;
use freenet_stdlib::{
    client_api::{ClientRequest, ContractRequest, ContractResponse, WebApi},
    prelude::{ContractCode, ContractContainer, ContractInstanceId, ContractKey, ContractWasmAPIVersion, Parameters, RelatedContracts, UpdateData, WrappedContract, WrappedState},
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

    /// Process rooms that need synchronization
    pub async fn process_rooms(&mut self, web_api: &mut WebApi) -> Result<(), SynchronizerError> {
        // Get mutable access to rooms
        let rooms = self.rooms.write();
        
        // Collect rooms that need synchronization
        let rooms_to_sync: Vec<(VerifyingKey, RoomSyncStatus)> = rooms.map.iter()
            .filter_map(|(key, data)| {
                if matches!(data.sync_status, RoomSyncStatus::Disconnected) {
                    Some((*key, data.sync_status.clone()))
                } else {
                    None
                }
            })
            .collect();
        
        // Release the lock before processing
        drop(rooms);
        
        // Process each room that needs synchronization
        for (room_key, _) in rooms_to_sync {
            let mut rooms = self.rooms.write();
            if let Some(room_data) = rooms.map.get_mut(&room_key) {
                if matches!(room_data.sync_status, RoomSyncStatus::Disconnected) {
                    drop(rooms); // Release lock before async call
                    self.put_and_subscribe(&room_key, web_api).await?;
                }
            }
        }
        
        Ok(())
    }

    /// Put room state to Freenet and subscribe to updates
    pub async fn put_and_subscribe(&mut self, owner_vk: &VerifyingKey, web_api: &mut WebApi) -> Result<(), SynchronizerError> {
        info!("Putting room state for: {:?}", owner_vk);

        // Get room data under a limited scope to release the lock quickly
        let (contract_key, state_bytes, _room_state) = {
            let mut rooms = self.rooms.write();
            let room_data = rooms.map.get_mut(owner_vk)
                .ok_or_else(|| SynchronizerError::RoomNotFound(format!("{:?}", owner_vk)))?;
            
            let contract_key: ContractKey = owner_vk_to_contract_key(owner_vk);
            let state_bytes = to_cbor_vec(&room_data.room_state);
            let room_state = room_data.room_state.clone();
            
            // Update status while we have the lock
            room_data.sync_status = RoomSyncStatus::Putting;
            
            (contract_key, state_bytes, room_state)
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
        web_api.send(client_request)
            .map_err(|e| SynchronizerError::PutContractError(e.to_string()))
            .await?;

        // Will subscribe when response comes back from PUT
        Ok(())
    }

    /// Subscribe to a contract after a successful PUT
    pub async fn subscribe_to_contract(&self, contract_key: ContractKey, web_api: &mut WebApi) -> Result<(), SynchronizerError> {
        let client_request = ClientRequest::ContractOp(ContractRequest::Subscribe {
            key: contract_key.clone(),
            summary: None,
        });
        
        web_api.send(client_request)
            .map_err(|e| SynchronizerError::SubscribeError(e.to_string()))
            .await?;
        
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
            room_data.room_state.merge(&parent_state, &parameters, new_state)
                .map_err(|e| SynchronizerError::StateMergeError(e.to_string()))?;
            room_data.mark_synced();
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
            room_data.room_state.apply_delta(&parent_state, &parameters, &Some(delta.clone()))
                .map_err(|e| SynchronizerError::DeltaApplyError(e.to_string()))?;
            room_data.mark_synced();
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
    pub fn mark_room_subscribed(&self, owner_vk: &VerifyingKey) {
        let mut rooms = self.rooms.write();
        if let Some(room_data) = rooms.map.get_mut(owner_vk) {
            room_data.sync_status = RoomSyncStatus::Subscribed;
        }
    }
}
