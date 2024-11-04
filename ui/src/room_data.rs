use std::collections::HashMap;
use ed25519_dalek::{SigningKey, VerifyingKey};
use common::ChatRoomStateV1;
use common::room_state::ChatRoomParametersV1;
use common::room_state::member::MemberId;

#[derive(Debug, PartialEq)]
pub enum SendMessageError {
    UserNotMember,
    UserBanned,
}

#[derive(Clone, PartialEq)]
pub struct RoomData {
    pub owner_vk: VerifyingKey,
    pub room_state: ChatRoomStateV1,
    pub self_sk: SigningKey,
}

impl RoomData {
    /// Check if the user can send a message in the room
    pub fn can_send_message(&self) -> Result<(), SendMessageError> {
        let verifying_key = self.self_sk.verifying_key();
        // Must be owner or a member of the room to send a message
        if verifying_key == self.owner_vk || self.room_state.members.members.iter().any(|m| m.member.member_vk == verifying_key) {
            // Must not be banned from the room to send a message
            if self.room_state.bans.0.iter().any(|b| b.ban.banned_user == verifying_key.into()) {
                Err(SendMessageError::UserBanned)
            } else {
                Ok(())
            }
        } else {
            Err(SendMessageError::UserNotMember)
        }
    }

    pub fn owner_id(&self) -> MemberId {
        self.owner_vk.into()
    }

    pub fn parameters(&self) -> ChatRoomParametersV1 {
        ChatRoomParametersV1 {
            owner: self.owner_vk,
        }
    }
}

pub struct CurrentRoom {
    pub owner_key: Option<VerifyingKey>,
}

impl CurrentRoom {
    pub fn owner_id(&self) -> Option<MemberId> {
        self.owner_key.map(|vk| vk.into())
    }
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

impl Rooms {
    pub fn create_new_room(&mut self, self_sk: SigningKey) -> VerifyingKey {
        let owner_vk = self_sk.verifying_key();
        let mut room_state = ChatRoomStateV1::default();
        
        // Set initial configuration
        let mut config = Configuration::default();
        config.name = "New Room".to_string();
        config.owner_member_id = owner_vk.into();
        room_state.configuration = AuthorizedConfigurationV1::new(config, &self_sk);

        let room_data = RoomData {
            owner_vk,
            room_state,
            self_sk,
        };

        self.map.insert(owner_vk, room_data);
        owner_vk
    }
}
