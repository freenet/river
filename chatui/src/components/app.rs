use std::thread::Scope;
use dioxus::prelude::*;
use common::ChatRoomStateV1;
use crate::models::ChatState;
use super::{chat_rooms::ChatRooms, main_chat::MainChat, user_list::MemberList, modal::Modal};

pub fn App() -> Element {
    // TODO: 
    let chat_state = ChatState::default();
    use_context_provider(|| chat_state);
    rsx! {
        div { class: "chat-container",
            ChatRooms { }
            MainChat { }
            MemberList { }
            Modal {  }
        }
    }
}
