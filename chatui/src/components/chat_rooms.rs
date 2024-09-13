use dioxus::prelude::*;
use crate::components::{chat_rooms::ChatRooms, main_chat::MainChat};

pub fn App(cx: Scope) -> Element {
    cx.render(rsx! {
        div { class: "app",
            h1 { "Freenet Chat" }
            div { class: "chat-container",
                ChatRooms {}
                MainChat {}
            }
        }
    })
}
