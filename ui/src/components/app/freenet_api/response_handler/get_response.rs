use crate::components::app::freenet_api::error::SynchronizerError;
use crate::components::app::freenet_api::room_synchronizer::RoomSynchronizer;
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{CURRENT_ROOM, PENDING_INVITES, ROOMS};
use crate::invites::PendingRoomStatus;
use crate::room_data::RoomData;
use crate::util::from_cbor_slice;
use dioxus::logger::tracing::{error, info};
use dioxus::prelude::Readable;
use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::ContractKey;
use river_common::room_state::member::MemberId;
use river_common::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1};

pub async fn handle_get_response(
    room_synchronizer: &mut RoomSynchronizer,
    key: ContractKey,
    _contract: Vec<u8>,
    state: Vec<u8>,
) -> Result<(), SynchronizerError> {
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
            
            // Update the room data
            ROOMS.with_mut(|rooms| {
                    // Get the entry for this room
                    let entry = rooms.map.entry(owner_vk);
                    
                    // Check if this is a new entry before inserting
                    let is_new_entry = matches!(entry, std::collections::hash_map::Entry::Vacant(_));
                    
                    // Insert or get the existing room data
                    let room_data = entry.or_insert_with(|| {
                        // Create new room data if it doesn't exist
                        RoomData {
                            owner_vk,
                            room_state: retrieved_state.clone(),
                            self_sk: self_sk.clone(),
                            contract_key: key.clone(),
                        }
                    });
                    
                    // If the room already existed, merge the retrieved state
                    if !is_new_entry {
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
                        
                        // This should be outside the if block because in theory the AuthorizedMember could be a member
                        // but have no corresponding MemberInfo AI!
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
            });
            
            // Update the sync info with the latest room state
            SYNC_INFO.with_mut(|sync_info| {
                sync_info.register_new_room(owner_vk);
                
                // Get the latest room state directly from ROOMS
                // Create a binding to extend the lifetime of the read lock
                let rooms_read = ROOMS.read();
                let latest_room_state = rooms_read.map.get(&owner_vk)
                    .map(|room_data| &room_data.room_state)
                    .expect("Room data should exist after insertion");
                
                sync_info.update_last_synced_state(&owner_vk, latest_room_state);
                sync_info.update_sync_status(&owner_vk, RoomSyncStatus::Subscribed);
            });
            // Now subscribe to the contract
            let subscribe_result =
                room_synchronizer.subscribe_to_contract(&key).await;

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
    
    Ok(())
}
