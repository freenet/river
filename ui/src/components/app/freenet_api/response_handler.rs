use super::error::SynchronizerError;
use super::room_synchronizer::RoomSynchronizer;
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{CURRENT_ROOM, PENDING_INVITES, ROOMS};
use crate::invites::PendingRoomStatus;
use crate::room_data::RoomData;
use crate::util::{from_cbor_slice, owner_vk_to_contract_key};
use dioxus::logger::tracing::{error, info, warn};
use dioxus::signals::Readable;
use freenet_scaffold::ComposableState;
use freenet_stdlib::{
    client_api::{ContractResponse, HostResponse},
    prelude::UpdateData,
};
use river_common::room_state::member::MemberId;
use river_common::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_common::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta, ChatRoomParametersV1};

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
        match response {
            HostResponse::Ok => {
                info!("Received OK response from API");
            }
            HostResponse::ContractResponse(contract_response) => {
                match contract_response {
                    ContractResponse::GetResponse {
                        key,
                        contract: _contract,
                        state,
                    } => {
                        info!("Received get response for key {key}");

                        // Check if this is for a pending invitation
                        let owner_vk = SYNC_INFO.read().get_owner_vk_for_instance_id(&key.id());
                        if let Some(owner_vk) = owner_vk {
                            if PENDING_INVITES.read().map.contains_key(&owner_vk) {
                                info!(
                                    "This is a subscription for a pending invitation, adding state"
                                );
                                let retrieved_state: ChatRoomStateV1 =
                                    from_cbor_slice::<ChatRoomStateV1>(&*state);
                                
                                // Get the pending invite data once to avoid multiple reads
                                let (self_sk, authorized_member, preferred_nickname) = {
                                    let pending_invites = PENDING_INVITES.read();
                                    let invite = &pending_invites.map[&owner_vk];
                                    (
                                        invite.invitee_signing_key.clone(),
                                        invite.authorized_member.clone(),
                                        invite.preferred_nickname.clone(),
                                    )
                                };

                                // Prepare the member ID for checking
                                let member_id: MemberId = authorized_member.member.member_vk.into();
                                
                                // Get a clone of the room state to update sync info later
                                let room_state_for_sync = {
                                    // Use entry API to either get existing room or create a new one
                                    ROOMS.with_mut(|rooms| {
                                        let room_data = rooms.map.entry(owner_vk).or_insert_with(|| {
                                            // Create new room data if it doesn't exist
                                            RoomData {
                                                owner_vk,
                                                room_state: retrieved_state.clone(),
                                                self_sk: self_sk.clone(),
                                                contract_key: key.clone(),
                                            }
                                        });
                                        
                                        // If the room already existed, merge the retrieved state
                                        if rooms.map.contains_key(&owner_vk) {
                                            // Create parameters for merge
                                            let params = ChatRoomParametersV1 {
                                                owner: owner_vk,
                                            };
                                            
                                            // Clone current state to avoid borrow issues during merge
                                            let current_state = room_data.room_state.clone();
                                            
                                            // Merge the retrieved state into the existing state
                                            room_data.room_state.merge(
                                                &current_state,
                                                &params,
                                                &retrieved_state,
                                            ).expect("Failed to merge room states");
                                        }
                                        
                                        // Check if the authorized member is already in the room
                                        let already_in_room = room_data.room_state.members.members.iter()
                                            .any(|m| MemberId::from(m.member.member_vk) == member_id);
                                        
                                        // Only add the member if they're not already in the room
                                        if !already_in_room {
                                            // Add the authorized member to the room state
                                            room_data.room_state.members.members.push(authorized_member.clone());
                                            
                                            // Set the member's nickname in member_info
                                            let member_info = MemberInfo {
                                                member_id,
                                                version: 0,
                                                preferred_nickname: preferred_nickname.clone(),
                                            };
                                            
                                            // Create authorized member info and add it to the room state
                                            let authorized_member_info =
                                                AuthorizedMemberInfo::new_with_member_key(
                                                    member_info,
                                                    &room_data.self_sk,
                                                );
                                            room_data
                                                .room_state
                                                .member_info
                                                .member_info
                                                .push(authorized_member_info);
                                        }
                                        
                                        // Return a clone of the room state for use outside this closure
                                        room_data.room_state.clone()
                                    })
                                };
                                
                                // Update the sync info with the room state we just got
                                SYNC_INFO.with_mut(|sync_info| {
                                    sync_info.register_new_room(owner_vk);
                                    sync_info
                                        .update_last_synced_state(&owner_vk, &room_state_for_sync);
                                    sync_info
                                        .update_sync_status(&owner_vk, RoomSyncStatus::Subscribed);
                                });
                                // Now subscribe to the contract
                                let subscribe_result =
                                    self.room_synchronizer.subscribe_to_contract(&key).await;

                                if let Err(e) = subscribe_result {
                                    error!("Failed to subscribe to contract after GET: {}", e);
                                    // Update the sync status to error
                                    SYNC_INFO.write().update_sync_status(
                                        &owner_vk,
                                        RoomSyncStatus::Error(e.to_string()),
                                    );
                                } else {
                                    // Mark the invitation as subscribed and retrieved
                                    PENDING_INVITES.with_mut(|pending_invites| {
                                        if let Some(join) = pending_invites.map.get_mut(&owner_vk) {
                                            join.status = PendingRoomStatus::Subscribed;
                                        }
                                    });
                                }
                                // Dispatch an event to notify the UI
                                if let Some(window) = web_sys::window() {
                                    let key_hex = owner_vk
                                        .as_bytes()
                                        .iter()
                                        .map(|b| format!("{:02x}", b))
                                        .collect::<String>();
                                    let event =
                                        web_sys::CustomEvent::new("river-invitation-accepted")
                                            .unwrap();

                                    // Set the detail property
                                    js_sys::Reflect::set(
                                        &event,
                                        &wasm_bindgen::JsValue::from_str("detail"),
                                        &wasm_bindgen::JsValue::from_str(&key_hex),
                                    )
                                    .unwrap();

                                    window.dispatch_event(&event).unwrap();

                                    // Set the current room to the newly accepted room
                                    CURRENT_ROOM.with_mut(|current_room| {
                                        current_room.owner_key = Some(owner_vk);
                                    });
                                }
                            }
                        }
                    }
                    ContractResponse::PutResponse { key } => {
                        let contract_id = key.id();
                        info!("Received PutResponse for contract ID: {}", contract_id);

                        // Get the owner VK first, then release the read lock
                        let owner_vk_opt = {
                            let sync_info = SYNC_INFO.read();
                            sync_info.get_owner_vk_for_instance_id(&contract_id)
                        };

                        match owner_vk_opt {
                            Some(owner_vk) => {
                                info!(
                                    "Found owner VK for contract ID {}: {:?}",
                                    contract_id,
                                    MemberId::from(owner_vk)
                                );

                                // Now subscribe to the contract
                                let subscribe_result =
                                    self.room_synchronizer.subscribe_to_contract(&key).await;

                                if let Err(e) = subscribe_result {
                                    error!("Failed to subscribe to contract after PUT: {}", e);
                                    // Update the sync status to error
                                    SYNC_INFO.write().update_sync_status(
                                        &owner_vk,
                                        RoomSyncStatus::Error(e.to_string()),
                                    );
                                } else {
                                    // Update sync status in a separate block to avoid nested borrows
                                    SYNC_INFO
                                        .write()
                                        .update_sync_status(&owner_vk, RoomSyncStatus::Subscribing);
                                }

                                // Log the current state of all rooms after successful PUT
                                let rooms_count = {
                                    let rooms = ROOMS.read();
                                    let count = rooms.map.len();
                                    count
                                };
                                info!("Current rooms count after PutResponse: {}", rooms_count);

                                // Get room information in a separate block
                                let room_info: Vec<(MemberId, String)> = {
                                    let rooms = ROOMS.read();
                                    rooms
                                        .map
                                        .iter()
                                        .map(|(room_key, _)| {
                                            let contract_key = owner_vk_to_contract_key(room_key);
                                            let room_contract_id = contract_key.id();
                                            (
                                                MemberId::from(*room_key),
                                                room_contract_id.to_string(),
                                            )
                                        })
                                        .collect()
                                };

                                // Log room information
                                for (member_id, contract_id) in room_info {
                                    info!(
                                        "Room in map: {:?}, contract ID: {}",
                                        member_id, contract_id
                                    );
                                }
                            }
                            None => {
                                info!(
                                    "Warning: Could ntot find owner VK for contract ID: {}",
                                    contract_id
                                );
                            }
                        }
                    }
                    ContractResponse::UpdateNotification { key, update } => {
                        info!("Received update notification for key: {key}");
                        // Get contract info, log warning and return early if not found
                        // Get contract info, return early if not found
                        let room_owner_vk =
                            match SYNC_INFO.read().get_owner_vk_for_instance_id(&key.id()) {
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
                                    from_cbor_slice::<ChatRoomStateV1>(&*state);

                                // Regular state update
                                info!("Received new state in UpdateNotification: {:?}", new_state);
                                self.room_synchronizer
                                    .update_room_state(&room_owner_vk, &new_state);
                            }
                            UpdateData::Delta(delta) => {
                                let new_delta: ChatRoomStateV1Delta =
                                    from_cbor_slice::<ChatRoomStateV1Delta>(&*delta);
                                info!("Received new delta in UpdateNotification: {:?}", new_delta);
                                self.room_synchronizer
                                    .apply_delta(&room_owner_vk, new_delta);
                            }
                            UpdateData::StateAndDelta { state, delta } => {
                                info!("Received state and delta in UpdateNotification state: {:?} delta: {:?}", state, delta);
                                let new_state: ChatRoomStateV1 =
                                    from_cbor_slice::<ChatRoomStateV1>(&*state);
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
                        info!(
                            "Received subscribe response for key {key}, subscribed: {subscribed}"
                        );

                        // Get the owner VK for this contract first, then release the read lock
                        let owner_vk_opt = {
                            let sync_info = SYNC_INFO.read();
                            sync_info
                                .get_owner_vk_for_instance_id(&key.id())
                                .map(|vk| vk)
                        };

                        if let Some(owner_vk) = owner_vk_opt {
                            if subscribed {
                                info!(
                                    "Successfully subscribed to contract for room: {:?}",
                                    MemberId::from(owner_vk)
                                );

                                // Update the sync status to subscribed in a separate block
                                SYNC_INFO
                                    .write()
                                    .update_sync_status(&owner_vk, RoomSyncStatus::Subscribed);
                            } else {
                                warn!("Failed to subscribe to contract: {}", key.id());
                                // Update the sync status to error
                                SYNC_INFO.write().update_sync_status(
                                    &owner_vk,
                                    RoomSyncStatus::Error("Subscription failed".to_string()),
                                );
                            }
                        } else {
                            warn!("Could not find owner VK for contract ID: {}", key.id());
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
