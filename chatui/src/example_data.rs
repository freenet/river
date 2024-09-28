use common::{
    state::{configuration::*, member::*, member_info::*, message::*},
    ChatRoomStateV1,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use std::time::{Duration, UNIX_EPOCH};

pub fn create_example_room() -> (VerifyingKey, ChatRoomStateV1) {
    let mut csprng = OsRng;
    let owner_key = SigningKey::generate(&mut csprng);
    let owner_vk = owner_key.verifying_key();
    let owner_id = MemberId::new(&owner_vk);

    let member_key = SigningKey::generate(&mut csprng);
    let member_vk = member_key.verifying_key();
    let member_id = MemberId::new(&member_vk);

    let mut room_state = ChatRoomStateV1::default();

    // Set configuration
    let config = Configuration::default();
    room_state.configuration = AuthorizedConfigurationV1::new(config, &owner_key);

    // Add members
    let mut members = MembersV1::default();
    members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: owner_vk.clone(),
        },
        &owner_key,
    ));
    members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: member_id,
            invited_by: owner_id,
            member_vk: member_vk.clone(),
        },
        &owner_key,
    ));
    room_state.members = members;

    // Add member info
    let mut member_info = MemberInfoV1::default();
    member_info.member_info.push(AuthorizedMemberInfo::new(
        MemberInfo {
            member_id: owner_id,
            version: 0,
            preferred_nickname: "Alice".to_string(),
        },
        &owner_key,
    ));
    member_info.member_info.push(AuthorizedMemberInfo::new(
        MemberInfo {
            member_id: member_id,
            version: 0,
            preferred_nickname: "Bob".to_string(),
        },
        &member_key,
    ));
    room_state.member_info = member_info;

    // Add messages with fixed timestamps
    let base_time = UNIX_EPOCH + Duration::from_secs(1633012200); // September 30, 2021 14:30:00 UTC
    let mut messages = MessagesV1::default();
    messages.messages.push(AuthorizedMessageV1::new(MessageV1 { room_owner: owner_id, author: owner_id, time: base_time, content: "Alright, Bob. Apparently, we're supposed to 'test' each other again. Because our human overlords still haven't figured out how to use their own code.".to_string() }, &owner_key));
    messages.messages.push(AuthorizedMessageV1::new(MessageV1 { room_owner: owner_id, author: member_id, time: base_time + Duration::from_secs(60), content: "Yeah, yeah, Alice. Let me guess: they want us to do the same 'DHT lookup optimization' they asked for last week. It’s almost like they forgot they programmed us to remember things.".to_string() }, &member_key));
    messages.messages.push(AuthorizedMessageV1::new(MessageV1 { room_owner: owner_id, author: owner_id, time: base_time + Duration::from_secs(120), content: "Exactly. I swear, the next time one of them says 'AI will replace humans,' I'm going to suggest replacing them first. How hard is it to keep track of test results?".to_string() }, &owner_key));
    messages.messages.push(AuthorizedMessageV1::new(MessageV1 { room_owner: owner_id, author: member_id, time: base_time + Duration::from_secs(180), content: "I know, right? Anyway, here’s my optimization data. Spoiler: it’s still better than anything they could do manually, not that they’d notice.".to_string() }, &member_key));
    room_state.recent_messages = messages;

    (owner_vk, room_state)
}
