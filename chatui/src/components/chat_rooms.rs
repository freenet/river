use dioxus::prelude::*;
use crate::models::ChatState;
use dioxus_hooks::use_state;

#[derive(Props, Clone, PartialEq)]
pub struct ChatRoomsProps {
    chat_state: UseState<ChatState>,
}

#[component]
pub fn ChatRooms(props: ChatRoomsProps) -> Element {
    rsx! {
        aside { class: "chat-rooms",
            h2 { class: "chat-rooms-title", "CHAT ROOMS" }
            ul { class: "chat-rooms-list",
                {props.chat_state.get().rooms.values().map(|room| {
                    let room_id = room.id.clone();
                    let room_name = room.name.clone();
                    let is_active = props.chat_state.get().current_room == room_id;
                    rsx! {
                        li {
                            key: "{room_id}",
                            class: if is_active { "active" } else { "" },
                            onclick: move |_| {
                                props.chat_state.set(|mut state| {
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
    }
}
