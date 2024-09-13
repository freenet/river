use dioxus::prelude::*;
use crate::models::{ChatState, Room};

#[derive(Props)]
pub struct ChatRoomsProps {
    chat_state: UseState<ChatState>,
}

#[component]
pub fn ChatRooms(cx: Scope, props: ChatRoomsProps) -> Element {
    cx.render(rsx! {
        aside { class: "chat-rooms",
            h2 { class: "chat-rooms-title", "CHAT ROOMS" }
            ul { class: "chat-rooms-list",
                {chat_state.get().rooms.values().map(|room| {
                    let room_id = room.id.clone();
                    let room_name = room.name.clone();
                    let is_active = chat_state.get().current_room == room_id;
                    rsx! {
                        li {
                            key: "{room_id}",
                            class: if is_active { "active" } else { "" },
                            onclick: move |_| {
                                chat_state.set(|mut state| {
                                    state.current_room = room_id.clone();
                                    state
                                });
                            },
                            "{room_name}"
                        }
                    }
                })}
            }
        }
    })
}
