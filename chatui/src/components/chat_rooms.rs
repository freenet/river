use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::fa_solid_icons::FaHouse;

#[component]
pub fn ChatRooms() -> Element {
    let mut chat_state = use_context::<ChatState>();
    
    rsx! {
        aside { class: "chat-rooms",
            h2 { class: "chat-rooms-title", "CHAT ROOMS" }
            ul { class: "chat-rooms-list",
                {chat_state.rooms.iter().map(|(room_key, room_state)| {
                    let room_key = *room_key;
                    let room_name = room_state.read().configuration.configuration.name.clone();
                    let is_active = chat_state.read() == Some(room_key);
                    rsx! {
                        li { 
                            key: "{room_key:?}",
                            class: if is_active { "chat-room-item active" } else { "chat-room-item" },
                            button { 
                                onclick: move |_| chat_state.current_room.set(Some(room_key)),
                                Icon { icon: FaHouse, width: 20, height: 20 }
                                span { "{room_name}" }
                            }
                        }
                    }
                })}
            }
        }
    }
}
