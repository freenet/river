use std::collections::HashMap;
use dioxus::prelude::Signal;
use common::{ChatRoomStateV1, ChatRoomParametersV1, state::{configuration::*, member::*, member_info::*, message::*}};
use ed25519_dalek::{VerifyingKey, SigningKey, Signer};
use rand::rngs::OsRng;

pub fn create_example_room() -> (VerifyingKey, ChatRoomStateV1) {
    let mut csprng = OsRng;
    let owner_key = SigningKey::generate(&mut csprng);
    let owner_vk = owner_key.verifying_key();
    let owner_id = MemberId::new(&owner_vk);

    let member_key = SigningKey::generate(&mut csprng);
    let member_vk = member_key.verifying_key();
    let member_id = MemberId::new(&member_vk);

    let mut room_state = ChatRoomStateV1::default();
    let parameters = ChatRoomParametersV1 { owner: owner_vk };

    // Set configuration
    let config = Configuration::default();
    room_state.configuration = AuthorizedConfigurationV1::new(config, &owner_key);

    // Add members
    let mut members = MembersV1::default();
    members.add_member(owner_id, None, &owner_key).unwrap();
    members.add_member(member_id, Some(owner_id), &owner_key).unwrap();
    room_state.members = members;

    // Add member info
    let mut member_info = MemberInfoV1::default();
    member_info.set_member_info(MemberInfo { nickname: "Owner".to_string() }, &owner_key).unwrap();
    member_info.set_member_info(MemberInfo { nickname: "Member".to_string() }, &member_key).unwrap();
    room_state.member_info = member_info;

    // Add messages
    let mut messages = MessagesV1::default();
    messages.add_message(Message { content: "Hello, welcome to the chat!".to_string() }, &owner_key, &room_state).unwrap();
    messages.add_message(Message { content: "Thanks for having me!".to_string() }, &member_key, &room_state).unwrap();
    messages.add_message(Message { content: "Let's start chatting!".to_string() }, &owner_key, &room_state).unwrap();
    room_state.recent_messages = messages;

    (owner_vk, room_state)
}
