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
use common::room_state::ChatRoomParametersV1;
use freenet_scaffold::ComposableState;

pub fn create_example_rooms() -> Rooms {
    let mut map = HashMap::new();
    let mut csprng = OsRng;

    // Room where you're just an observer (not a member)
    let (owner_vk_1, _, room_data_1) = create_room(
        &mut csprng,
        "PublicRoomOwner",
        vec![],
        &"Public Discussion Room".to_string()
    );
    map.insert(owner_vk_1, room_data_1);

    // Room where you're a member
    let (owner_vk_2, _, room_data_2) = create_room(
        &mut csprng,
        "TeamLead",
        vec!["You", "Colleague1", "Colleague2"],
        &"Team Chat Room".to_string()
    );
    map.insert(owner_vk_2, room_data_2);

    // Room where you're the owner
    let (owner_vk_3, _, room_data_3) = create_room(
        &mut csprng,
        "You",
        vec!["Member1", "Member2"],
        &"Your Private Room".to_string()
    );
    map.insert(owner_vk_3, room_data_3);

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
    let mut your_member_key = None;

    // Add owner to member_info only
    member_info.member_info.push(AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: owner_id,
            version: 0,
            preferred_nickname: owner_name.to_string(),
        },
        &owner_key,
    ));
    if owner_name == "You" {
        your_member_key = Some(owner_key.clone());
    }

    // Add other members
    for &name in &member_names {
        let member_signing_key = SigningKey::generate(csprng);
        let member_vk_temp = member_signing_key.verifying_key();
        let member_id = MemberId::new(&member_vk_temp);
        info!("{}'s member ID: {}", name, member_id);

        if name == "You" {
            your_member_key = Some(member_signing_key.clone());
        }

        add_member(&mut members, &mut member_info, name, &owner_key, &member_id, &member_signing_key);
        member_vk = Some(member_vk_temp);
    }

    room_state.members = members;
    room_state.member_info = member_info;

    // Add messages if both Alice and Bob are involved
    if owner_name == "Alice" && member_names.contains(&"Bob") {
        add_example_messages(&mut room_state, &owner_id, &member_vk.as_ref().unwrap());
    }

    let user_signing_key = if owner_name == "You" {
        // If you're the owner, use the owner key
        owner_key
    } else if let Some(key) = your_member_key {
        // If you're a member, use your member key
        key
    } else {
        // Otherwise generate a new key for an observer
        SigningKey::generate(csprng)
    };

    let verification_result = room_state.verify(&room_state, &ChatRoomParametersV1 { owner: owner_vk });
    if !verification_result.is_ok() {
        panic!("Failed to verify room state: {:?}", verification_result.err());
    }

    (
        owner_vk,
        member_vk,
        RoomData {
            room_state,
            user_signing_key,
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
    let owner_member_id = MemberId::new(&owner_key.verifying_key());
    
    // For the owner, set invited_by to their own ID
    // For other members, set invited_by to the owner's ID
    let invited_by = if member_id == &owner_member_id {
        owner_member_id  // Owner invites themselves
    } else {
        owner_member_id  // Owner invites other members
    };

    members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id,
            invited_by,
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
