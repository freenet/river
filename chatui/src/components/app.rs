use dioxus::prelude::*;
use crate::example_data::create_example_room;
use std::collections::HashMap;
use crate::components::chat_rooms::ChatRooms;
use crate::components::main_chat::MainChat;
use crate::components::user_list::MemberList;
use crate::components::modal::Modal;

pub fn App() -> Element {
    let rooms = use_signal(|| {
        let mut rooms = HashMap::new();
        let (room_key, room_state) = create_example_room();
        rooms.insert(room_key, (room_state, None));
        rooms
    });

    let current_room = use_signal(|| None);
    let current_room_state = use_memo(|| current_room.read().and_then(|key| rooms.read().get(&key).map(|(state, _)| state.clone())));

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
            Modal {
                current_room: current_room,
                current_room_state: current_room_state
            }
        }
    }
}
