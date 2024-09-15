use dioxus::prelude::*;
use super::{chat_rooms::ChatRooms, main_chat::MainChat, user_list::UserList, modal::Modal};
use crate::models::ChatState;

#[component]
pub fn App(cx: Scope) -> Element {
    let chat_state = use_state(cx, ChatState::default);
    let show_modal = use_state(cx, || false);
    let modal_type = use_state(cx, String::new);
    let modal_name = use_state(cx, String::new);

    cx.render(rsx! {
        div { class: "chat-container",
            ChatRooms { chat_state: chat_state.clone() }
            MainChat { chat_state: chat_state.clone() }
            UserList { chat_state: chat_state.clone() }
            Modal {
                show: show_modal.clone(),
                modal_type: modal_type.clone(),
                modal_name: modal_name.clone()
            }
        }
    })
}
