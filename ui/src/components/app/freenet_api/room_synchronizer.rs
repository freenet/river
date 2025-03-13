use super::error::SynchronizerError;
use crate::components::app::{ROOMS, WEB_API};
use crate::constants::ROOM_CONTRACT_WASM;
use crate::room_data::{RoomData, Rooms};
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
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use futures::SinkExt;
use freenet_stdlib::prelude::UpdateData;
use river_common::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use crate::components::app::sync_info::SYNC_INFO;

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
        info!("Processing rooms - starting");

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
                web_api.send(client_request)
                    .await
                    .map_err(|e| SynchronizerError::PutContractError(e.to_string()))?;
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
                // We use the post-merged state to avoid some edge cases
                SYNC_INFO.write().update_last_synced_state(room_owner_vk, &room_data.room_state);
            } else {
                warn!("Room not found in rooms map");
            }
        });

    }

    /// Create a new room from an invitation, will also call process_rooms to sync new room
    pub async fn create_room_from_invitation(
        &mut self,
        owner_vk: VerifyingKey,
        authorized_member: AuthorizedMember,
        invitee_signing_key: SigningKey,
        nickname: String,
    ) -> Result<(), SynchronizerError> {
        info!("Creating room from invitation for owner: {:?}", MemberId::from(owner_vk));

        // Create a new empty room state
        let mut room_state = ChatRoomStateV1::default();

        room_state.members.members.push(authorized_member.clone());

        room_state.member_info.member_info.push(AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: MemberId::from(authorized_member.member.member_vk),
                version: 0,
                preferred_nickname: nickname,
            },
            &invitee_signing_key,
        ));

        // Create the contract key
        let contract_key = owner_vk_to_contract_key(&owner_vk);

        // Create a new room data entry
        let room_data = RoomData {
            owner_vk,
            room_state,
            self_sk: invitee_signing_key,
            contract_key,
        };

        // Add the room to our rooms map
        {
            ROOMS.with_mut(|rooms| {
                rooms.map.insert(owner_vk, room_data.clone());
            });
        }

        // Register the contract info
        self.update_room_state(&owner_vk, &room_data.room_state);

        // Now trigger a sync for this room
        self.process_rooms().await?;

        Ok(())
    }
}

/// Stores information about a contract being synchronized
#[derive(Clone)]
pub struct ContractSyncInfo {
    pub owner_vk: VerifyingKey,
}
