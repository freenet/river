use dioxus::prelude::*;
use super::{chat_rooms::ChatRooms, main_chat::MainChat, user_list::UserList, modal::Modal};

#[component]
pub fn App() -> Element {
    let show_modal = use_signal(|| false);
    let modal_type = use_signal(|| String::new());
    let modal_name = use_signal(|| String::new());

    rsx! {
        div { class: "chat-container",
            ChatRooms {}
            MainChat {}
            UserList {}
            Modal {
                show: show_modal,
                modal_type: modal_type,
                modal_name: modal_name
            }
        }
    }
}
