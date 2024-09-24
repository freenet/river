use std::collections::HashMap;
use std::ops::Deref;
use dioxus::prelude::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use common::ChatRoomStateV1;
use super::{chat_rooms::ChatRooms, main_chat::MainChat, user_list::MemberList, modal::Modal};

pub fn App() -> Element {
    let rooms: Signal<HashMap<VerifyingKey, (ChatRoomStateV1, Option<SigningKey>)>> = use_signal(|| HashMap::new());
    let current_room: Signal<Option<VerifyingKey>> = use_signal(|| None);
    let current_room_state : Memo<Option<ChatRoomStateV1>> = use_memo(move || {
        match current_room() {
            Some(current_room_key) => {
                if let Some((room_state, _)) = rooms.read().deref().get(&current_room_key) {
                    Some(room_state.clone())
                } else {
                    None
                }
            }
            None => None,
        }
    });
    rsx! {
        div { class: "chat-container",
            ChatRooms {}
            MainChat {}
            MemberList {}
            Modal {}
        }
    }
}