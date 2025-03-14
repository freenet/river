use super::error::SynchronizerError;
use super::room_synchronizer::RoomSynchronizer;
use crate::util::{from_cbor_slice, owner_vk_to_contract_key};
use dioxus::logger::tracing::{info, warn};
use dioxus::signals::Readable;
use freenet_stdlib::{
    client_api::{ContractResponse, HostResponse},
    prelude::UpdateData,
};
use river_common::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{PENDING_INVITES, ROOMS};
use crate::room_data::RoomData;
use river_common::room_state::member::MemberId;
use river_common::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use crate::invites::PendingRoomStatus;

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
                        info!("Received PUT response for room {}, marking as subscribed", key.id());
                        // Update the sync status to indicate that the room is subscribed
                        let owner_vk = SYNC_INFO.read().get_owner_vk_for_instance_id(&key.id()).expect(
                            "Failed to get owner VK for instance ID"
                        );
                        SYNC_INFO.write().update_sync_status(&owner_vk, RoomSyncStatus::Subscribed);
                    }
                    ContractResponse::UpdateNotification { key, update } => {
                        info!("Received update notification for key: {key}");
                        // Get contract info, log warning and return early if not found
                        // Get contract info, return early if not found
                        let room_owner_vk = match SYNC_INFO.read().get_owner_vk_for_instance_id(&key.id()) {
                            Some(vk) => vk,
                            None => {
                                warn!("Contract key not found in SYNC_INFO: {}", key.id());
                                return Ok(());
                            }
                        };

                        // Check if this is for a pending invitation
                        let pending_invite = PENDING_INVITES.read().map.get(&room_owner_vk).cloned();

                        // Handle update notification
                        match update {
                            UpdateData::State(state) => {
                                let new_state: ChatRoomStateV1 =
                                    from_cbor_slice::<ChatRoomStateV1>(&state.into_bytes());
                                
                                if let Some(pending) = pending_invite {
                                    // This is an update for a pending invitation
                                    info!("Received state for pending invitation: {:?}", MemberId::from(room_owner_vk));
                                    
                                    // Create a new room data with the received state and invitation data
                                    let mut room_state = new_state.clone();
                                    
                                    // Add the authorized member from the invitation if not already present
                                    let member_id = MemberId::from(pending.authorized_member.member.member_vk);
                                    if !room_state.members.members.iter().any(|m| MemberId::from(m.member.member_vk) == member_id) {
                                        room_state.members.members.push(pending.authorized_member.clone());
                                    }
                                    
                                    // Add member info with the preferred nickname
                                    let member_info = AuthorizedMemberInfo::new_with_member_key(
                                        MemberInfo {
                                            member_id,
                                            version: 0,
                                            preferred_nickname: pending.preferred_nickname,
                                        },
                                        &pending.invitee_signing_key,
                                    );
                                    
                                    if !room_state.member_info.member_info.iter().any(|mi| mi.member_info.member_id == member_id) {
                                        room_state.member_info.member_info.push(member_info);
                                    }
                                    
                                    // Create the contract key
                                    let contract_key = owner_vk_to_contract_key(&room_owner_vk);
                                    
                                    // Create a new room data entry
                                    let room_data = RoomData {
                                        owner_vk: room_owner_vk,
                                        room_state,
                                        self_sk: pending.invitee_signing_key,
                                        contract_key,
                                    };
                                    
                                    // Add the room to our rooms map
                                    ROOMS.with_mut(|rooms| {
                                        rooms.map.insert(room_owner_vk, room_data.clone());
                                    });
                                    
                                    // Update the sync info
                                    SYNC_INFO.with_mut(|sync_info| {
                                        sync_info.register_new_room(room_owner_vk);
                                        sync_info.update_last_synced_state(&room_owner_vk, &room_data.room_state);
                                        sync_info.update_sync_status(&room_owner_vk, RoomSyncStatus::Subscribed);
                                    });
                                    
                                    // Mark the invitation as subscribed and retrieved
                                    PENDING_INVITES.with_mut(|pending_invites| {
                                        if let Some(join) = pending_invites.map.get_mut(&room_owner_vk) {
                                            join.status = PendingRoomStatus::Subscribed;
                                        }
                                    });
                                    
                                    // Dispatch an event to notify the UI
                                    if let Some(window) = web_sys::window() {
                                        let key_hex = room_owner_vk.as_bytes().iter().map(|b| format!("{:02x}", b)).collect::<String>();
                                        let event = web_sys::CustomEvent::new(
                                            "river-invitation-accepted"
                                        ).unwrap();
                                        
                                        // Set the detail property
                                        js_sys::Reflect::set(
                                            &event,
                                            &wasm_bindgen::JsValue::from_str("detail"),
                                            &wasm_bindgen::JsValue::from_str(&key_hex)
                                        ).unwrap();
                                        
                                        window.dispatch_event(&event).unwrap();
                                    }
                                } else {
                                    // Regular state update
                                    info!("Received new state in UpdateNotification: {:?}", new_state);
                                    self.room_synchronizer
                                        .update_room_state(&room_owner_vk, &new_state);
                                }
                            }
                            UpdateData::Delta(delta) => {
                                let new_delta: ChatRoomStateV1Delta =
                                    from_cbor_slice::<ChatRoomStateV1Delta>(&delta.into_bytes());
                                info!("Received new delta in UpdateNotification: {:?}", new_delta);
                                self.room_synchronizer.apply_delta(&room_owner_vk, new_delta);
                            }
                            UpdateData::StateAndDelta {
                                state,
                                delta,
                            } => {
                                info!("Received state and delta in UpdateNotification state: {:?} delta: {:?}", state, delta);
                                let new_state: ChatRoomStateV1 =
                                    from_cbor_slice::<ChatRoomStateV1>(&state.into_bytes());
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
                        info!("Received subscribe response for key {key}, subscribed: {subscribed}");
                        
                        // Check if this is for a pending invitation
                        let owner_vk = SYNC_INFO.read().get_owner_vk_for_instance_id(&key.id());
                        
                        if let Some(owner_vk) = owner_vk {
                            if PENDING_INVITES.read().map.contains_key(&owner_vk) {
                                info!("This is a subscription for a pending invitation");
                                // The state will come in a separate UpdateNotification
                            }
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
