use super::{chat_rooms::ChatRooms, main_chat::MainChat, member_list::MemberList};
use crate::example_data::create_example_room;
use crate::global_context::UserInfoModals;
use common::ChatRoomStateV1;
use dioxus::prelude::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use std::collections::HashMap;

pub fn App() -> Element {
    use_context_provider(|| {
        let mut map = HashMap::new();
        let (owner_key, room_data) = create_example_room();
        map.insert(owner_key, room_data);
        Signal::new(Rooms { map })
    });
    use_context_provider(|| Signal::new(CurrentRoom { owner_key: None }));
    use_context_provider(|| Signal::new(UserInfoModals { modals: HashMap::new() }));

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
