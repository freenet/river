use std::collections::HashMap;
use std::ops::Deref;
use dioxus::prelude::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use common::ChatRoomStateV1;
use crate::example_data::create_example_room;
use super::{chat_rooms::ChatRooms, main_chat::MainChat, user_list::MemberList};
use crate::components::chat_room_modal::ChatRoomModal;
use log::info;

pub fn App() -> Element {
    use_effect(|| {
        wasm_logger::init(wasm_logger::Config::default());
        info!("Logger initialized");
    });
    let rooms: Signal<HashMap<VerifyingKey, (ChatRoomStateV1, Option<SigningKey>)>> = use_signal(|| {
        let mut map = HashMap::new();
        let (verifying_key, room_state) = create_example_room();
        map.insert(verifying_key, (room_state, None));
        map
    }
    );
    let current_room: Signal<Option<VerifyingKey>> = use_signal(|| {
        let first_room = rooms.read().keys().next().cloned();
        info!("Initial current_room: {:?}", first_room);
        first_room
    });
    let current_room_state: Memo<Option<ChatRoomStateV1>> = use_memo(move || {
        let state = current_room().and_then(|current_room_key| {
            rooms.read().deref().get(&current_room_key).map(|(room_state, _)| room_state.clone())
        });
        info!("Current room state updated: {:?}", state.is_some());
        state
    });

    use_effect(move || {
        info!("Current room: {:?}", current_room.get());
        info!("Current room state: {:?}", current_room_state.get().is_some());
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
