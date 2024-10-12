use super::{chat_rooms::ChatRooms, main_chat::MainChat, member_list::MemberList};
use crate::example_data::create_example_rooms;
use crate::global_context::UserInfoModals;
use dioxus::prelude::*;
use std::collections::HashMap;
use crate::room_data::{CurrentRoom};

pub fn App() -> Element {
    use_context_provider(|| Signal::new(create_example_rooms()));
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


