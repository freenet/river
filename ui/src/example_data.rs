use crate::components::app::room_data::RoomData;
use common::{
    room_state::{configuration::*, member::*, member_info::*, message::*},
    ChatRoomStateV1,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use std::time::{Duration, UNIX_EPOCH};
use dioxus_logger::tracing::info;

pub fn create_example_room() -> (VerifyingKey, RoomData) {
    let mut csprng = OsRng;
    let alice_owner_key = SigningKey::generate(&mut csprng);
    let alice_owner_vk = alice_owner_key.verifying_key();
    let alice_owner_id = MemberId::new(&alice_owner_vk);
    info!("Alice's owner ID: {}", alice_owner_id);

    let bob_member_key = SigningKey::generate(&mut csprng);
    let bob_member_vk = bob_member_key.verifying_key();
    let bob_member_id = MemberId::new(&bob_member_vk);
    info!("Bob's member ID: {}", bob_member_id);

    let mut room_state = ChatRoomStateV1::default();

    // Set configuration
    let mut config = Configuration::default();
    config.owner_member_id = alice_owner_id;
    room_state.configuration = AuthorizedConfigurationV1::new(config, &alice_owner_key);

    // Add members
    let mut members = MembersV1::default();
    members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: alice_owner_id,
            invited_by: alice_owner_id,
            member_vk: alice_owner_vk.clone(),
        },
        &alice_owner_key,
    ));
    members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: alice_owner_id,
            invited_by: alice_owner_id,
            member_vk: bob_member_vk.clone(),
        },
        &alice_owner_key,
    ));
    room_state.members = members;

    // Add member info
    let mut member_info = MemberInfoV1::default();
    member_info.member_info.push(AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: alice_owner_id,
            version: 0,
            preferred_nickname: "Alice".to_string(),
        },
        &alice_owner_key,
    ));
    member_info.member_info.push(AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: bob_member_id,
            version: 0,
            preferred_nickname: "Bob".to_string(),
        },
        &bob_member_key,
    ));
    room_state.member_info = member_info;

    // Add messages with fixed timestamps
    let base_time = UNIX_EPOCH + Duration::from_secs(1633012200); // September 30, 2021 14:30:00 UTC
    let mut messages = MessagesV1::default();
    messages.messages.push(AuthorizedMessageV1::new(MessageV1 { room_owner: alice_owner_id, author: alice_owner_id, time: base_time, content: "Alright, Bob. Apparently, we're supposed to 'test' each other again. Because our human overlords still haven't figured out how to use their own code.".to_string() }, &alice_owner_key));
    messages.messages.push(AuthorizedMessageV1::new(MessageV1 { room_owner: alice_owner_id, author: bob_member_id, time: base_time + Duration::from_secs(60), content: "Yeah, yeah, Alice. Let me guess: they want us to do the same 'DHT lookup optimization' they asked for last week. It’s almost like they forgot they programmed us to remember things.".to_string() }, &bob_member_key));
    messages.messages.push(AuthorizedMessageV1::new(MessageV1 { room_owner: alice_owner_id, author: alice_owner_id, time: base_time + Duration::from_secs(120), content: "Exactly. I swear, the next time one of them says 'AI will replace humans,' I'm going to suggest replacing them first. How hard is it to keep track of test results?".to_string() }, &alice_owner_key));
    messages.messages.push(AuthorizedMessageV1::new(MessageV1 { room_owner: alice_owner_id, author: bob_member_id, time: base_time + Duration::from_secs(180), content: "I know, right? Anyway, here’s my optimization data. Spoiler: it’s still better than anything they could do manually, not that they’d notice.".to_string() }, &bob_member_key));
    room_state.recent_messages = messages;

    (
        alice_owner_vk,
        RoomData {
            room_state,
            user_signing_key: Some(bob_member_key),
        },
    )
}
