use super::{chat_rooms::ChatRooms, main_chat::MainChat, user_list::MemberList};
use crate::example_data::create_example_room;
use common::ChatRoomStateV1;
use dioxus::prelude::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use std::collections::HashMap;

pub fn App() -> Element {
    let mut map = HashMap::new();
    let example_room_data = create_example_room();
    map.insert(example_room_data.0, example_room_data.1);

    use_context_provider(|| Signal::new(Rooms { map }));
    use_context_provider(|| Signal::new(CurrentRoom { owner_key: None }));

    rsx! {
        div { class: "chat-container",
            ChatRooms {}
            MainChat {}
            MemberList {}
        }
    }
}

#[derive(Clone)]
pub struct RoomData {
    pub room_state: ChatRoomStateV1,
    pub user_signing_key: Option<SigningKey>,
}

impl PartialEq for RoomData {
    fn eq(&self, other: &Self) -> bool {
        self.room_state == other.room_state
    }
}

pub struct CurrentRoom {
    pub owner_key: Option<VerifyingKey>,
}

impl PartialEq for CurrentRoom {
    fn eq(&self, other: &Self) -> bool {
        self.owner_key == other.owner_key
    }
}

#[derive(Clone)]
pub struct Rooms {
    pub map: HashMap<VerifyingKey, RoomData>,
}

impl PartialEq for Rooms {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
    }
}
