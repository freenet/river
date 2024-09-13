use dioxus::prelude::*;
use crate::models::ChatState;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::fa_solid_icons::FaHome;
use dioxus::hooks::UseState;

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
                        li { class: classes!("chat-room-item", if is_active { "active" }),
                            button { onclick: move |_| props.chat_state.set(|state| {
                                let mut new_state = state.clone();
                                new_state.current_room = room_id.clone();
                                new_state
                            }),
                                { room_name }
                            }
                        }
                    }
                })}
            }
        }
    }
}
