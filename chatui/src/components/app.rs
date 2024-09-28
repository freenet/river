use super::{chat_rooms::ChatRooms, main_chat::MainChat, user_list::MemberList};
use crate::components::chat_room_modal::ChatRoomModal;
use crate::example_data::create_example_room;
use common::ChatRoomStateV1;
use dioxus::prelude::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use std::collections::HashMap;
use std::ops::Deref;

pub fn App() -> Element {
    let rooms: Signal<HashMap<VerifyingKey, RoomData>> =
        use_signal(|| {
            let mut map = HashMap::new();
            let (verifying_key, room_state) = create_example_room();
            map.insert(verifying_key, RoomData { room_state, user_signing_key: None });
            map
        });
    let current_room: Signal<Option<VerifyingKey>> = use_signal(|| None);
    let current_room_state: Memo<Option<ChatRoomStateV1>> = use_memo(move || {
        current_room().and_then(|current_room_key| {
            rooms
                .read()
                .deref()
                .get(&current_room_key)
                .map(|room_data| room_data.room_state.clone())
        })
    });
    let show_modal = use_signal(|| false);
    
    rsx! {
        div { class: "chat-container",
            ChatRooms {
                rooms: rooms.clone(),
                current_room: current_room.clone()
            }
            MainChat {
                current_room: current_room.clone(),
                current_room_state: current_room_state.clone(),
                show_modal: show_modal.clone()
            }
            MemberList {
                current_room: current_room.clone(),
                current_room_state: current_room_state.clone()
            }
            ChatRoomModal {
                current_room: current_room,
                current_room_state: current_room_state,
                show: show_modal
            }
        }
    }
}

pub struct RoomData {
    pub room_state: ChatRoomStateV1,
    pub user_signing_key: Option<SigningKey>,
}
