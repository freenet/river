use crate::{constants::ROOM_CONTRACT_WASM, util::to_cbor_vec};
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_stdlib::prelude::{ContractCode, ContractInstanceId, ContractKey, Parameters};
use river_common::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_common::room_state::member::AuthorizedMember;
use river_common::room_state::member::MemberId;
use river_common::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_common::room_state::ChatRoomParametersV1;
use river_common::ChatRoomStateV1;
use std::collections::HashMap;

#[derive(Debug, PartialEq)]
pub enum SendMessageError {
    UserNotMember,
    UserBanned,
}

#[derive(Clone, PartialEq, Debug)]
pub enum RoomSyncStatus {
    /// Room needs to be PUT to Freenet first
    NeedsPut,
    /// Room is being PUT to Freenet
    Putting,
    /// Room needs to be subscribed to
    Unsubscribed,
    /// Room subscription is in progress
    Subscribing,
    /// Room is successfully subscribed
    Subscribed,
    /// Error occurred during synchronization
    Error(String),
}

#[derive(Clone, PartialEq)]
pub struct RoomData {
    pub owner_vk: VerifyingKey,
    pub room_state: ChatRoomStateV1,
    pub self_sk: SigningKey,
    pub contract_key: ContractKey,
    pub sync_status: RoomSyncStatus,
}

impl RoomData {
    /// Check if the user can send a message in the room
    pub fn can_send_message(&self) -> Result<(), SendMessageError> {
        let verifying_key = self.self_sk.verifying_key();
        // Must be owner or a member of the room to send a message
        if verifying_key == self.owner_vk
            || self
                .room_state
                .members
                .members
                .iter()
                .any(|m| m.member.member_vk == verifying_key)
        {
            // Must not be banned from the room to send a message
            if self
                .room_state
                .bans
                .0
                .iter()
                .any(|b| b.ban.banned_user == verifying_key.into())
            {
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

    /// Replace an existing member entry with a new authorized member
    /// Returns true if the member was found and updated
    pub fn restore_member_access(
        &mut self,
        old_member_vk: VerifyingKey,
        new_authorized_member: AuthorizedMember,
    ) -> bool {
        // Find and replace the member entry
        if let Some(member) = self
            .room_state
            .members
            .members
            .iter_mut()
            .find(|m| m.member.member_vk == old_member_vk)
        {
            *member = new_authorized_member;
            true
        } else {
            false
        }
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

    pub fn owner_key(&self) -> Option<&VerifyingKey> {
        self.owner_key.as_ref()
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
    pub fn create_new_room_with_name(
        &mut self,
        self_sk: SigningKey,
        name: String,
        nickname: String,
    ) -> VerifyingKey {
        let owner_vk = self_sk.verifying_key();
        let mut room_state = ChatRoomStateV1::default();

        // Set initial configuration
        let mut config = Configuration::default();
        config.name = name;
        config.owner_member_id = owner_vk.into();
        room_state.configuration = AuthorizedConfigurationV1::new(config, &self_sk);

        // Add owner to member_info
        let owner_info = MemberInfo {
            member_id: owner_vk.into(),
            version: 0,
            preferred_nickname: nickname,
        };
        let authorized_owner_info = AuthorizedMemberInfo::new(owner_info, &self_sk);
        room_state
            .member_info
            .member_info
            .push(authorized_owner_info);

        // Generate contract key for the room
        let parameters = ChatRoomParametersV1 { owner: owner_vk };
        let params_bytes = to_cbor_vec(&parameters);
        let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
        let instance_id =
            ContractInstanceId::from_params_and_code(Parameters::from(params_bytes), contract_code);
        let contract_key = ContractKey::from(instance_id);

        let room_data = RoomData {
            owner_vk,
            room_state,
            self_sk,
            contract_key,
            sync_status: RoomSyncStatus::NeedsPut,
        };

        self.map.insert(owner_vk, room_data);
        owner_vk
    }
}
