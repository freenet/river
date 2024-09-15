use dioxus::prelude::*;
use super::{chat_rooms::ChatRooms, main_chat::MainChat, user_list::UserList, modal::Modal};
use crate::models::ChatState;

pub fn App() -> Element {
    rsx! {
        div { class: "chat-container",
            ChatRooms {}
            MainChat {}
            UserList {}
            Modal {}
        }
    }
}
