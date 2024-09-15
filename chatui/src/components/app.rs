use dioxus::prelude::*;
use super::{chat_rooms::ChatRooms, main_chat::MainChat, user_list::UserList, modal::Modal};
use crate::models::ChatState;
use crate::models::ChatState;

pub fn App(cx: Scope) -> Element {
    let chat_state = use_state(cx, ChatState::default);

    cx.render(rsx! {
        div { class: "chat-container",
            ChatRooms { chat_state: chat_state }
            MainChat { chat_state: chat_state }
            UserList { chat_state: chat_state }
            Modal { chat_state: chat_state, show: false }
        }
    })
}
