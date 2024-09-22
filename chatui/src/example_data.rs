use common::{ChatRoomStateV1, state::{configuration::*, member::*, member_info::*, message::*}};
use ed25519_dalek::{VerifyingKey, SigningKey};
use rand::rngs::OsRng;
use std::time::SystemTime;

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
    members.members.push(AuthorizedMember::new(Member { owner_member_id: owner_id, invited_by: owner_id, member_vk: owner_vk.clone(), nickname: "Owner".to_string() }, &owner_key));
    members.members.push(AuthorizedMember::new(Member { owner_member_id: member_id, invited_by: owner_id, member_vk: member_vk.clone(), nickname: "Member".to_string() }, &owner_key));
    room_state.members = members;

    // Add member info
    let mut member_info = MemberInfoV1::default();
    member_info.member_info.push(AuthorizedMemberInfo::new(MemberInfo { member_id: owner_id, version: 0, preferred_nickname: "Owner".to_string() }, &owner_key));
    member_info.member_info.push(AuthorizedMemberInfo::new(MemberInfo { member_id: member_id, version: 0, preferred_nickname: "Member".to_string() }, &member_key));
    room_state.member_info = member_info;

    // Add messages
    let mut messages = MessagesV1::default();
    messages.messages.push(AuthorizedMessageV1::new(MessageV1 { owner_member_id: owner_id, author: owner_id, time: SystemTime::now(), content: "Hello, welcome to the chat!".to_string() }, &owner_key));
    messages.messages.push(AuthorizedMessageV1::new(MessageV1 { owner_member_id: owner_id, author: member_id, time: SystemTime::now(), content: "Thanks for having me!".to_string() }, &member_key));
    messages.messages.push(AuthorizedMessageV1::new(MessageV1 { owner_member_id: owner_id, author: owner_id, time: SystemTime::now(), content: "Let's start chatting!".to_string() }, &owner_key));
    room_state.recent_messages = messages;

    (owner_vk, room_state)
}
