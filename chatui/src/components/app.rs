use dioxus::prelude::*;
use crate::models::ChatState;
use super::{chat_rooms::ChatRooms, main_chat::MainChat, user_list::MemberList, modal::Modal};

pub fn App() -> Element {
    let chat_state = use_context_provider(|| ChatState::new());
    
    rsx! {
        div { class: "chat-container",
            ChatRooms {}
            MainChat {}
            MemberList {}
            Modal {}
        }
    }
}
