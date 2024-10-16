use std::collections::HashMap;
use crate::room_data::{RoomData, Rooms};
use common::{
    room_state::{configuration::*, member::*, member_info::*, message::*},
    ChatRoomStateV1,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use std::time::{Duration, UNIX_EPOCH};
use dioxus_logger::tracing::info;

pub fn create_example_rooms() -> Rooms {
    let mut map = HashMap::new();
    let mut csprng = OsRng;

    // Create Alice's room
    let (alice_owner_vk, _bob_member_vk, room_data_alice) = create_room(&mut csprng, "Alice", vec!["Bob"], &"Alice's Room".to_string());
    map.insert(alice_owner_vk, room_data_alice);

    // Create Richard's room, only Richard as owner
    let (richard_owner_vk, _, room_data_richard) = create_room(&mut csprng, "Richard", vec![], &"Richard's Room".to_string());
    map.insert(richard_owner_vk, room_data_richard);

    Rooms { map }
}

// Function to create a room with an owner and members
fn create_room(csprng: &mut OsRng, owner_name: &str, member_names: Vec<&str>, room_name : &String) -> (VerifyingKey, Option<VerifyingKey>, RoomData) {
    let owner_key = SigningKey::generate(csprng);
    let owner_vk = owner_key.verifying_key();
    let owner_id = MemberId::new(&owner_vk);
    info!("{}'s owner ID: {}", owner_name, owner_id);

    let mut room_state = ChatRoomStateV1::default();

    // Set configuration
    let mut config = Configuration::default();
    config.name = room_name.clone();
    config.owner_member_id = owner_id;
    room_state.configuration = AuthorizedConfigurationV1::new(config, &owner_key);

    // Add members
    let mut members = MembersV1::default();
    let mut member_info = MemberInfoV1::default();
    let mut member_vk = None;

    add_member(&mut members, &mut member_info, owner_name, &owner_key, &owner_id, &owner_key);

    for &name in &member_names {
        let member_signing_key = SigningKey::generate(csprng);
        let member_vk_temp = member_signing_key.verifying_key();
        let member_id = MemberId::new(&member_vk_temp);
        info!("{}'s member ID: {}", name, member_id);

        add_member(&mut members, &mut member_info, name, &owner_key, &member_id, &member_signing_key);
        member_vk = Some(member_vk_temp);
    }

    room_state.members = members;
    room_state.member_info = member_info;

    // Add messages if both Alice and Bob are involved
    if owner_name == "Alice" && member_names.contains(&"Bob") {
        add_example_messages(&mut room_state, &owner_id, &member_vk.as_ref().unwrap());
    }

    (
        owner_vk,
        member_vk,
        RoomData {
            room_state,
            user_signing_key: SigningKey::generate(csprng),
        },
    )
}

// Function to add a member to the room
fn add_member(
    members: &mut MembersV1,
    member_info: &mut MemberInfoV1,
    name: &str,
    owner_key: &SigningKey,
    member_id: &MemberId,
    signing_key: &SigningKey,
) {
    let member_vk = signing_key.verifying_key();
    members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: MemberId::new(&owner_key.verifying_key()),
            invited_by: MemberId::new(&owner_key.verifying_key()),
            member_vk: member_vk.clone(),
        },
        owner_key,
    ));
    member_info.member_info.push(AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: *member_id,
            version: 0,
            preferred_nickname: name.to_string(),
        },
        signing_key,
    ));
}

// Function to add example messages to a room
fn add_example_messages(room_state: &mut ChatRoomStateV1, alice_owner_id: &MemberId, bob_vk: &VerifyingKey) {
    let base_time = UNIX_EPOCH + Duration::from_secs(1633012200); // September 30, 2021 14:30:00 UTC
    let mut messages = MessagesV1::default();
    let bob_member_id = MemberId::new(bob_vk);

    messages.messages.push(AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: *alice_owner_id,
            author: *alice_owner_id,
            time: base_time,
            content: "Alright, Bob. Apparently, we're supposed to 'test' each other again. Because our human overlords still haven't figured out how to use their own code.".to_string(),
        },
        &SigningKey::generate(&mut OsRng),
    ));
    messages.messages.push(AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: *alice_owner_id,
            author: bob_member_id,
            time: base_time + Duration::from_secs(60),
            content: "Yeah, yeah, Alice. Let me guess: they want us to do the same 'DHT lookup optimization' they asked for last week. It’s almost like they forgot they programmed us to remember things.".to_string(),
        },
        &SigningKey::generate(&mut OsRng),
    ));
    messages.messages.push(AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: *alice_owner_id,
            author: *alice_owner_id,
            time: base_time + Duration::from_secs(120),
            content: "Exactly. I swear, the next time one of them says 'AI will replace humans,' I'm going to suggest replacing them first. How hard is it to keep track of test results?".to_string(),
        },
        &SigningKey::generate(&mut OsRng),
    ));
    messages.messages.push(AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: *alice_owner_id,
            author: bob_member_id,
            time: base_time + Duration::from_secs(180),
            content: "I know, right? Anyway, here’s my optimization data. Spoiler: it’s still better than anything they could do manually, not that they’d notice.".to_string(),
        },
        &SigningKey::generate(&mut OsRng),
    ));
    room_state.recent_messages = messages;
}