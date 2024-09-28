use super::{chat_rooms::ChatRooms, main_chat::MainChat, user_list::MemberList};
use crate::components::chat_room_modal::ChatRoomModal;
use crate::example_data::create_example_room;
use common::ChatRoomStateV1;
use dioxus::prelude::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use std::collections::HashMap;
use std::ops::Deref;

pub fn App() -> Element {
    let rooms: Signal<HashMap<VerifyingKey, (ChatRoomStateV1, Option<SigningKey>)>> =
        use_signal(|| {
            let mut map = HashMap::new();
            let (verifying_key, room_state) = create_example_room();
            map.insert(verifying_key, (room_state, None));
            map
        });
    let current_room: Signal<Option<VerifyingKey>> = use_signal(|| None);
    let current_room_state: Memo<Option<ChatRoomStateV1>> = use_memo(move || {
        current_room().and_then(|current_room_key| {
            rooms
                .read()
                .deref()
                .get(&current_room_key)
                .map(|(room_state, _)| room_state.clone())
        })
    });

    rsx! {
        div { class: "chat-container",
            ChatRooms {
                rooms: rooms,
                current_room: current_room
            }
            MainChat {
                current_room: current_room,
                current_room_state: current_room_state
            }
            MemberList {
                current_room: current_room,
                current_room_state: current_room_state
            }
            ChatRoomModal {
                current_room: current_room,
                current_room_state: current_room_state
            }
        }
    }
}
