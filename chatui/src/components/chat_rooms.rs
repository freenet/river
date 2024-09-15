use dioxus::prelude::*;
use crate::models::ChatState;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::fa_solid_icons::FaHouse;

use dioxus::prelude::*;
use crate::models::ChatState;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::fa_solid_icons::FaHouse;

#[component]
pub fn ChatRooms(cx: Scope, chat_state: UseState<ChatState>) -> Element {
    cx.render(rsx! {
        aside { class: "chat-rooms",
            h2 { class: "chat-rooms-title", "CHAT ROOMS" }
            ul { class: "chat-rooms-list",
                {cx.props.chat_state.read().rooms.values().map(|room| {
                    let room_id = room.id.clone();
                    let room_name = room.name.clone();
                    let is_active = cx.props.chat_state.read().current_room == room_id;
                    rsx! {
                        li { 
                            class: if is_active { "chat-room-item active" } else { "chat-room-item" },
                            button { 
                                onclick: move |_| cx.props.chat_state.write().current_room = room_id.clone(),
                                Icon { icon: FaHouse, width: 20, height: 20 }
                                span { "{room_name}" }
                            }
                        }
                    }
                })}
            }
        }
    })
}
