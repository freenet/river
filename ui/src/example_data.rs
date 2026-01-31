use crate::util::random_full_name;
use crate::{
    constants::ROOM_CONTRACT_WASM,
    room_data::{RoomData, Rooms},
    util::to_cbor_vec,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::{ContractCode, ContractInstanceId, ContractKey, Parameters};
use lipsum::lipsum;
use rand::rngs::OsRng;
use river_core::room_state::ChatRoomParametersV1;
use river_core::{
    room_state::{
        configuration::*,
        member::*,
        member_info::*,
        message::*,
        privacy::{RoomDisplayMetadata, SealedBytes},
    },
    ChatRoomStateV1,
};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn create_example_rooms() -> Rooms {
    let mut map = HashMap::new();

    // Room where you're just an observer (not a member)
    let room1 = create_room(&"Public Discussion Room".to_string(), SelfIs::Observer);
    map.insert(room1.owner_vk, room1.room_data);

    // Room where you're a member
    let room2 = create_room(&"Team Chat Room".to_string(), SelfIs::Member);
    map.insert(room2.owner_vk, room2.room_data);

    // Room where you're the owner
    let room3 = create_room(&"Your Private Room".to_string(), SelfIs::Owner);
    map.insert(room3.owner_vk, room3.room_data);

    Rooms {
        map,
        current_room_key: None,
    }
}

struct CreatedRoom {
    owner_vk: VerifyingKey,
    room_data: RoomData,
}

#[derive(Debug, PartialEq)]
enum SelfIs {
    Observer,
    Member,
    Owner,
}

// Function to create a room with an owner and members, self_is determines whether
// the user of the UI is the owner, a member, or an observer (not an owner or member)
fn create_room(room_name: &String, self_is: SelfIs) -> CreatedRoom {
    let mut csprng = OsRng;

    // Create self - the user actually using the app
    let self_sk = SigningKey::generate(&mut csprng);
    let self_vk = self_sk.verifying_key();
    let self_id = self_vk.into();

    // Create owner of the room
    let owner_sk = if self_is == SelfIs::Owner {
        &self_sk
    } else {
        &SigningKey::generate(&mut csprng)
    };
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    let mut room_state = ChatRoomStateV1::default();

    // Set configuration
    let mut config = Configuration::default();
    config.display = RoomDisplayMetadata {
        name: SealedBytes::public(room_name.clone().into_bytes()),
        description: None,
    };
    config.owner_member_id = owner_id;
    room_state.configuration = AuthorizedConfigurationV1::new(config, owner_sk);

    // Initialize member lists
    let mut members = MembersV1::default();
    let mut member_info = MemberInfoV1::default();

    // Always add owner to member_info
    member_info
        .member_info
        .push(AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: owner_id,
                version: 0,
                preferred_nickname: SealedBytes::public(
                    (random_full_name() + " (Owner)").into_bytes(),
                ),
            },
            owner_sk,
        ));

    // If self is a member but not the owner, add self
    if self_is == SelfIs::Member {
        members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: self_vk.clone(),
            },
            owner_sk,
        ));

        member_info
            .member_info
            .push(AuthorizedMemberInfo::new_with_member_key(
                MemberInfo {
                    member_id: self_id,
                    version: 0,
                    preferred_nickname: SealedBytes::public(
                        (random_full_name() + " (You)").into_bytes(),
                    ),
                },
                &self_sk,
            ));
    }

    // Always add another member to ensure the room has at least one member
    let other_member_sk = SigningKey::generate(&mut csprng);
    let other_member_vk = other_member_sk.verifying_key();
    let other_member_id = MemberId::from(&other_member_vk);

    // In rooms where self is owner, other member should be invited by self
    // In other rooms, other member should be invited by owner
    let inviter_id = if self_is == SelfIs::Owner {
        self_id
    } else {
        owner_id
    };
    let inviter_sk = if self_is == SelfIs::Owner {
        &self_sk
    } else {
        owner_sk
    };

    members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: inviter_id,
            member_vk: other_member_vk,
        },
        inviter_sk,
    ));

    member_info
        .member_info
        .push(AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: other_member_id,
                version: 0,
                preferred_nickname: SealedBytes::public(
                    (random_full_name() + " (Member)").into_bytes(),
                ),
            },
            &other_member_sk,
        ));

    // Add members to the room
    room_state.members = members.clone();
    room_state.member_info = member_info.clone();

    // Create a map of member IDs to their signing keys for message creation
    let mut member_keys = HashMap::new();
    if self_is == SelfIs::Member {
        member_keys.insert(self_id, self_sk.clone());
    }
    member_keys.insert(other_member_id, other_member_sk);

    // Add example messages
    add_example_messages(&mut room_state, &owner_id, owner_sk, &member_keys);

    let verification_result = room_state.verify(
        &room_state,
        &ChatRoomParametersV1 {
            owner: owner_vk.clone(),
        },
    );
    if !verification_result.is_ok() {
        panic!(
            "Failed to verify room state: {:?}",
            verification_result.err()
        );
    }

    // Generate contract key for the room
    let parameters = ChatRoomParametersV1 { owner: owner_vk };
    let params_bytes = to_cbor_vec(&parameters);
    let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
    // Use the full ContractKey constructor that includes the code hash
    let contract_key =
        ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code);

    CreatedRoom {
        owner_vk,
        room_data: RoomData {
            owner_vk: owner_vk.clone(),
            room_state,
            self_sk: self_sk.clone(),
            contract_key,
            last_read_message_id: None,
            secrets: std::collections::HashMap::new(),
            current_secret_version: None,
            last_secret_rotation: None,
            key_migrated_to_delegate: true, // Example data doesn't need migration
        },
    }
}

fn add_example_messages(
    room_state: &mut ChatRoomStateV1,
    owner_id: &MemberId,
    owner_key: &SigningKey,
    member_keys: &HashMap<MemberId, SigningKey>,
) {
    // Use a timestamp 24 hours ago as base time for messages
    let now = crate::util::get_current_system_time()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let base_time = now - (24 * 60 * 60 * 1000); // 24 hours ago in milliseconds

    let mut messages = MessagesV1::default();
    let mut current_time_ms = base_time;

    // Verify owner exists in member_info but NOT in members list
    if !room_state
        .member_info
        .member_info
        .iter()
        .any(|m| m.member_info.member_id == *owner_id)
    {
        panic!("Owner ID not found in member_info: {}", owner_id);
    }
    if room_state
        .members
        .members
        .iter()
        .any(|m| m.member.id() == *owner_id)
    {
        panic!(
            "Owner ID found in members list when it should not be: {}",
            owner_id
        );
    }

    // Verify that all member_keys are valid and members exist
    for (member_id, signing_key) in member_keys.iter() {
        if MemberId::from(&signing_key.verifying_key()) != *member_id {
            panic!("Member ID does not match signing key");
        }

        // Verify they exist in members list (unless they're the owner)
        if *member_id != *owner_id
            && !room_state
                .members
                .members
                .iter()
                .any(|m| m.member.id() == *member_id)
        {
            panic!("Member ID not found in members list: {}", member_id);
        }
    }

    // Create a vec of possible authors (owner + members)
    let authors: Vec<(MemberId, &SigningKey)> = member_keys
        .iter()
        .map(|(id, key)| (*id, key))
        .chain(std::iter::once((*owner_id, owner_key)))
        .collect();

    let message_count = 6;

    for _ in 0..message_count {
        // Pick a random author
        let author_idx = rand::random::<usize>() % authors.len();
        let (author_id, signing_key) = authors[author_idx];

        // Generate message with random length (15-35 words)
        let word_count = rand::random::<u8>() % 21 + 15;
        let msg = AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: *owner_id,
                author: author_id,
                time: get_time_from_millis(current_time_ms),
                content: RoomMessageBody::public(lipsum(word_count as usize)),
            },
            signing_key,
        );

        messages.messages.push(msg);

        // Add a more natural time gap between messages (30 sec to 15 min)
        current_time_ms += (rand::random::<u64>() % 870 + 30) * 1000;
    }

    // Add reactions to messages from OTHER members (not owner)
    // Rule: One reaction per user per message
    // In "Your Private Room" the owner IS self, so this shows self reacting to others
    let non_owner_messages: Vec<_> = messages
        .messages
        .iter()
        .filter(|m| m.message.author != *owner_id)
        .collect();

    // Get a non-owner member ID for multi-user reaction demo
    let other_member_id = member_keys.keys().find(|id| *id != owner_id).cloned();

    if non_owner_messages.len() >= 1 {
        // First non-owner message: owner reacts with thumbs up, other member with heart
        let msg_id = non_owner_messages[0].id();
        let mut reactions = HashMap::new();
        reactions.insert("👍".to_string(), vec![*owner_id]);
        if let Some(other_id) = other_member_id {
            reactions.insert("❤️".to_string(), vec![other_id]);
        }
        messages.actions_state.reactions.insert(msg_id, reactions);
    }

    if non_owner_messages.len() >= 2 {
        // Second non-owner message: owner reacts with celebration
        let msg_id = non_owner_messages[1].id();
        let mut reactions = HashMap::new();
        reactions.insert("🎉".to_string(), vec![*owner_id]);
        messages.actions_state.reactions.insert(msg_id, reactions);
    }

    room_state.recent_messages = messages;
}

fn get_time_from_millis(ms: u64) -> SystemTime {
    // Use WASM-compatible time function
    crate::util::get_current_system_time()
        - Duration::from_millis(
            crate::util::get_current_system_time()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::from_secs(0))
                .as_millis() as u64
                - ms,
        )
}

// Test function to create the example data
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_example_rooms() {
        let rooms = create_example_rooms();
        assert_eq!(rooms.map.len(), 3);

        for (owner_vk, room_data) in rooms.map.iter() {
            // Verify the room state
            let verification_result = room_data.room_state.verify(
                &room_data.room_state,
                &ChatRoomParametersV1 {
                    owner: owner_vk.clone(),
                },
            );
            assert!(
                verification_result.is_ok(),
                "Room state failed to verify: {:?}",
                verification_result.err()
            );

            // Verify room has at least basic configuration
            assert!(
                room_data
                    .room_state
                    .configuration
                    .configuration
                    .display
                    .name
                    .declared_len()
                    > 0
            );

            // Verify members list exists
            assert!(!room_data.room_state.members.members.is_empty());

            // Verify member info exists
            assert!(!room_data.room_state.member_info.member_info.is_empty());
        }
    }
}
