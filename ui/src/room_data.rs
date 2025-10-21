#![allow(dead_code)]

use crate::{constants::ROOM_CONTRACT_WASM, util::to_cbor_vec};
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::{ContractCode, ContractInstanceId, ContractKey, Parameters};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::AuthorizedMember;
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::privacy::{RoomDisplayMetadata, SealedBytes};
use river_core::room_state::ChatRoomParametersV1;
use river_core::ChatRoomStateV1;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, PartialEq)]
pub enum SendMessageError {
    UserNotMember,
    UserBanned,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomData {
    pub owner_vk: VerifyingKey,
    pub room_state: ChatRoomStateV1,
    pub self_sk: SigningKey,
    pub contract_key: ContractKey,
    /// The current room secret for encryption/decryption (if room is private)
    #[serde(skip)]
    pub current_secret: Option<[u8; 32]>,
    /// The version of the current secret
    #[serde(skip)]
    pub current_secret_version: Option<u32>,
}

impl RoomData {
    /// Check if the room is in private mode
    pub fn is_private(&self) -> bool {
        matches!(
            self.room_state.configuration.configuration.privacy_mode,
            river_core::room_state::privacy::PrivacyMode::Private
        )
    }

    /// Get the current secret for encryption/decryption
    pub fn get_secret(&self) -> Option<(&[u8; 32], u32)> {
        match (self.current_secret.as_ref(), self.current_secret_version) {
            (Some(secret), Some(version)) => Some((secret, version)),
            _ => None,
        }
    }

    /// Set the current room secret
    pub fn set_secret(&mut self, secret: [u8; 32], version: u32) {
        self.current_secret = Some(secret);
        self.current_secret_version = Some(version);
    }

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

#[derive(Clone, Serialize, Deserialize)]
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
        let config = Configuration {
            owner_member_id: owner_vk.into(),
            display: RoomDisplayMetadata {
                name: SealedBytes::public(name.into_bytes()),
                description: None,
            },
            ..Configuration::default()
        };
        room_state.configuration = AuthorizedConfigurationV1::new(config, &self_sk);

        // Add owner to member_info
        let owner_info = MemberInfo {
            member_id: owner_vk.into(),
            version: 0,
            preferred_nickname: SealedBytes::public(nickname.into_bytes()),
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
            current_secret: None,
            current_secret_version: None,
        };

        self.map.insert(owner_vk, room_data);
        owner_vk
    }

    /// Merge the other Rooms into this Rooms (eg. when Rooms are loaded from storage)
    pub fn merge(&mut self, other: Rooms) -> Result<(), String> {
        for (vk, room_data) in other.map {
            // If not already in the map, add the room
            if let std::collections::hash_map::Entry::Vacant(e) = self.map.entry(vk) {
                e.insert(room_data);
            } else {
                // If the room is already in the map, merge in the new data
                let self_room_data = self.map.get_mut(&vk).unwrap();
                if self_room_data.self_sk != room_data.self_sk {
                    return Err("self_sk is different".to_string());
                }
                self_room_data.room_state.merge(
                    &self_room_data.room_state.clone(),
                    &ChatRoomParametersV1 { owner: vk },
                    &room_data.room_state,
                )?;
            }
        }
        Ok(())
    }
}
