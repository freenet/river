use super::{chat_rooms::ChatRooms, main_chat::MainChat, member_list::MemberList};
use crate::example_data::{create_example_room, create_example_room_for_invite};
use crate::global_context::UserInfoModals;
use dioxus::prelude::*;
use std::collections::HashMap;
use crate::room_data::{CurrentRoom, Rooms};

pub fn App() -> Element {
    use_context_provider(|| {
        let mut map = HashMap::new();
        {
            let (owner_key, room_data) = create_example_room();
            map.insert(owner_key, room_data);
        }
        {
            let (owner_key, room_data) = create_example_room_for_invite();
            map.insert(owner_key, room_data);
        }
            
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


