use dioxus::prelude::*;
use super::{chat_rooms::ChatRooms, main_chat::MainChat, user_list::UserList, modal::Modal};
use crate::models::ChatState;

pub fn App(cx: Scope) -> Element {
    let chat_state = use_state(cx, || ChatState::default());

    cx.render(rsx! {
        div { class: "chat-container",
            ChatRooms { chat_state: chat_state.clone() }
            MainChat { chat_state: chat_state.clone() }
            UserList { chat_state: chat_state.clone() }
            Modal { chat_state: chat_state.clone(), show: false }
        }
    })
}
