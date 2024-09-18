use dioxus::prelude::*;
use common::ChatRoomStateV1;
use crate::models::ChatState;
use super::{chat_rooms::ChatRooms, main_chat::MainChat, user_list::MemberList, modal::Modal};

pub fn App() -> Element {
    let chat_state = use_signal(ChatState::default);
    
    rsx! {
        div { class: "chat-container",
            ChatRooms { chat_state: chat_state.clone() }
            MainChat { chat_state: chat_state.clone() }
            MemberList { chat_state: chat_state.clone() }
            Modal { chat_state: chat_state.clone() }
        }
    }
}
