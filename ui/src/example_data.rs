use std::collections::HashMap;
use crate::room_data::{RoomData, Rooms};
use common::{
    room_state::{configuration::*, member::*, member_info::*, message::*},
    ChatRoomStateV1,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use std::time::{Duration, UNIX_EPOCH};
use common::room_state::ChatRoomParametersV1;
use freenet_scaffold::ComposableState;
use lipsum::lipsum;

pub fn create_example_rooms() -> Rooms {
    let mut map = HashMap::new();

    // Room where you're just an observer (not a member)
    let room1 = create_room(
        &"Public Discussion Room".to_string(),
        SelfIs::Observer,
    );
    map.insert(room1.owner_vk, room1.room_data);

    // Room where you're a member
    let room2 = create_room(
        &"Team Chat Room".to_string(),
        SelfIs::Member,
    );
    map.insert(room2.owner_vk, room2.room_data);

    // Room where you're the owner
    let room3 = create_room(
        &"Your Private Room".to_string(),
        SelfIs::Owner,
    );
    map.insert(room3.owner_vk, room3.room_data);

    Rooms { map }
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
fn create_room(
    room_name: &String,
    self_is: SelfIs,
) -> CreatedRoom {
    let mut csprng = OsRng;

    // Create self - the user actually using the app
    let self_sk = SigningKey::generate(&mut csprng);
    let self_vk = self_sk.verifying_key();
    let self_id = self_vk.into();

    // Create owner of the room
    let owner_sk = if self_is == SelfIs::Owner { &self_sk } else { &SigningKey::generate(&mut csprng) };
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    let mut room_state = ChatRoomStateV1::default();

    // Set configuration
    let mut config = Configuration::default();
    config.name = room_name.clone();
    config.owner_member_id = owner_id;
    room_state.configuration = AuthorizedConfigurationV1::new(config, owner_sk);

    // Initialize member lists
    let mut members = MembersV1::default();
    let mut member_info = MemberInfoV1::default();

    // Always add owner to member_info
    member_info.member_info.push(AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: owner_id,
            version: 0,
            preferred_nickname: lipsum(2),
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

        member_info.member_info.push(AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: self_id,
                version: 0,
                preferred_nickname: lipsum(2),
            },
            &self_sk,
        ));
    }

    // Add members to the room
    room_state.members = members.clone();
    room_state.member_info = member_info.clone();

    // Add some example members
    let other_member_sk = SigningKey::generate(&mut csprng);
    let other_member_vk = other_member_sk.verifying_key();
    let other_member_id = MemberId::from(&other_member_vk);

    // Add the other member, invited by owner
    members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: other_member_vk,
        },
        owner_sk,
    ));

    member_info.member_info.push(AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: other_member_id,
            version: 0,
            preferred_nickname: lipsum(2),
        },
        &other_member_sk,
    ));

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

    CreatedRoom {
        owner_vk,
        room_data: RoomData {
            room_state,
            self_sk: self_sk.clone(),
        },
    }
}

fn add_example_messages(
    room_state: &mut ChatRoomStateV1,
    owner_id: &MemberId,
    owner_key: &SigningKey,
    member_keys: &HashMap<MemberId, SigningKey>,
) {
    let base_time = UNIX_EPOCH + Duration::from_secs(1633012200); // September 30, 2021 14:30:00 UTC
    let mut messages = MessagesV1::default();
    let mut current_time = base_time;

    // Verify that all member_keys are valid and members exist
    for (member_id, signing_key) in member_keys.iter() {
        if MemberId::from(&signing_key.verifying_key()) != *member_id {
            panic!("Member ID does not match signing key");
        }

        // Verify they exist in members list
        if !room_state
            .members
            .members
            .iter()
            .any(|m| m.member.id() == *member_id)
        {
            panic!("Member ID not found in members list: {}", member_id);
        }
    }

    // Add messages from each member
    for (member_id, signing_key) in member_keys.iter() {
        // Add two messages for this member
        for i in 0..2 {
            messages.messages.push(AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: *owner_id,
                    author: *member_id,
                    time: current_time,
                    content: lipsum(if i == 0 { 20 } else { 15 }),
                },
                signing_key,
            ));
            current_time += Duration::from_secs(60);
        }
    }

    // Add messages from the owner
    messages.messages.push(AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: *owner_id,
            author: *owner_id,
            time: current_time,
            content: lipsum(25),
        },
        owner_key,
    ));
    current_time += Duration::from_secs(60);

    room_state.recent_messages = messages;
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
            assert!(!room_data.room_state.configuration.configuration.name.is_empty());
            
            // Verify members list exists
            assert!(!room_data.room_state.members.members.is_empty());
            
            // Verify member info exists
            assert!(!room_data.room_state.member_info.member_info.is_empty());
        }
    }
}
