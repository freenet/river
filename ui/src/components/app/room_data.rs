use common::room_state::member::MemberId;
use super::*;

#[derive(Debug, PartialEq)]
pub enum SendMessageError {
    UserSigningKeyNotSet,
    UserNotMember,
    UserBanned,
}

#[derive(Clone)]
pub struct RoomData {
    pub room_state: ChatRoomStateV1,
    pub user_signing_key: Option<SigningKey>,
}

impl RoomData {
    pub fn can_send_message(&self) -> Result<(), SendMessageError> {
        // Must have a user signing key to send a message
        match &self.user_signing_key {
            Some(signing_key) => {
                let verifying_key = signing_key.verifying_key();
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
            None => Err(SendMessageError::UserSigningKeyNotSet),
        }
    }
}

impl PartialEq for RoomData {
    fn eq(&self, other: &Self) -> bool {
        self.room_state == other.room_state
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
