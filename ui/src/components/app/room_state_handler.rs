//! Handles room state responses from the Freenet API
//!
//! This module processes room state responses, particularly for handling
//! pending invitations and adding new members to rooms.

use dioxus::logger::tracing::info;
use crate::invites::{PendingInvites, PendingRoomStatus};
use crate::room_data::{RoomData, RoomSyncStatus, Rooms};
use ed25519_dalek::VerifyingKey;
use river_common::room_state::ChatRoomStateV1;
use river_common::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_common::room_state::member::MemberId;
use freenet_stdlib::prelude::ContractKey;

/// Process a room state response from the Freenet API
///
/// This function:
/// 1. Checks if the room is in pending invites
/// 2. If it is, adds the authorized member to the room state
/// 3. Sets the member's nickname in member_info
/// 4. Updates the room data and marks the invitation as processed
pub fn process_room_state_response(
    rooms: &mut Rooms,
    room_owner: &VerifyingKey,
    room_state: ChatRoomStateV1,
    contract_key: ContractKey,
    pending_invites: &mut PendingInvites
) -> bool {
    // Check if this room is in pending invites
    if let Some(pending_join) = pending_invites.map.get(room_owner) {
        info!("Processing pending invitation for room owned by {:?}", room_owner);
        
        // Use the signing key from the pending invitation
        let self_sk = pending_join.invitee_signing_key.clone();
        let mut room_data = RoomData {
            owner_vk: *room_owner,
            room_state,
            self_sk,
            contract_key,
            sync_status: RoomSyncStatus::Subscribed,
        };
        
        // Add the authorized member to the room state
        room_data.room_state.members.members.push(pending_join.authorized_member.clone());
        
        // Set the member's nickname in member_info
        let member_id: MemberId = pending_join.authorized_member.member.member_vk.into();
        let member_info = MemberInfo {
            member_id,
            version: 1,
            preferred_nickname: pending_join.preferred_nickname.clone(),
        };
        
        // Create authorized member info and add it to the room state
        let authorized_member_info = AuthorizedMemberInfo::new_with_member_key(
            member_info,
            &room_data.self_sk,
        );
        room_data.room_state.member_info.member_info.push(authorized_member_info);
        
        // Add the room to the rooms map
        rooms.map.insert(room_owner.clone(), room_data);
        
        // Update pending invite status to Retrieved
        if let Some(pending_join) = pending_invites.map.get_mut(room_owner) {
            pending_join.status = PendingRoomStatus::Retrieved;
        }
        
        true
    } else {
        // Not a pending invite, just a regular room state update
        false
    }
}

/// Updates the pending invites with an error
pub fn set_pending_invite_error(
    pending_invites: &mut PendingInvites,
    room_owner: &VerifyingKey, 
    error: String
) {
    if let Some(pending_join) = pending_invites.map.get_mut(room_owner) {
        pending_join.status = PendingRoomStatus::Error(error);
    }
}
