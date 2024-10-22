use std::collections::HashMap;
use ed25519_dalek::{SigningKey, VerifyingKey};
use common::ChatRoomStateV1;
use common::room_state::member::MemberId;

#[derive(Debug, PartialEq)]
pub enum SendMessageError {
    UserNotMember,
    UserBanned,
}

#[derive(Clone, PartialEq)]
pub struct RoomData {
    pub room_state: ChatRoomStateV1,
    pub user_signing_key: SigningKey,
}

impl RoomData {
    /// Check if the user can send a message in the room
    pub fn can_send_message(&self) -> Result<(), SendMessageError> {
        let verifying_key = self.user_signing_key.verifying_key();
        // Must be a member of the room to send a message
        if self.room_state.members.members.iter().any(|m| m.member.member_vk == verifying_key) {
            // Must not be banned from the room to send a message
            if self.room_state.bans.0.iter().any(|b| b.ban.banned_user == MemberId::new(&verifying_key)) {
                Err(SendMessageError::UserBanned)
            } else {
                Ok(())
            }
        } else {
            Err(SendMessageError::UserNotMember)
        }
    }
}

pub struct CurrentRoom {
    pub owner_key: Option<VerifyingKey>,
}

impl PartialEq for CurrentRoom {
    fn eq(&self, other: &Self) -> bool {
        self.owner_key == other.owner_key
    }
}

#[derive(Clone)]
pub struct Rooms {
    pub map: HashMap<VerifyingKey, RoomData>,
}

impl PartialEq for Rooms {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
    }
}
