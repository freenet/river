use dioxus::prelude::*;
use dioxus_free_icons::Icon;
use dioxus_free_icons::icons::fa_solid_icons::FaHouse;
use std::collections::HashMap;
use ed25519_dalek::{SigningKey, VerifyingKey};
use common::ChatRoomStateV1;

#[component]
pub fn ChatRooms(
    rooms: Signal<HashMap<VerifyingKey, (ChatRoomStateV1, Option<SigningKey>)>>,
    current_room: Signal<Option<VerifyingKey>>
) -> Element {
    rsx! {
        aside { class: "chat-rooms",
            h2 { class: "chat-rooms-title", "CHAT ROOMS" }
            ul { class: "chat-rooms-list",
                {rooms.read().iter().map(|(room_key, (room_state, _))| {
                    let room_key = *room_key;
                    let room_name = room_state.configuration.configuration.name.clone();
                    let is_current = current_room.read().map_or(false, |cr| cr == room_key);
                    rsx! {
                        li { 
                            key: "{room_key:?}",
                            class: if is_current { "chat-room-item active" } else { "chat-room-item" },
                            button { 
                                onclick: move |_| {
                                    current_room.set(Some(room_key));
                                },
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
