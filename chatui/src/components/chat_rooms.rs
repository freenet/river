use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaComments;
use dioxus_free_icons::Icon;
use ed25519_dalek::VerifyingKey;
use std::collections::HashMap;
use crate::components::app::RoomData;

#[component]
pub fn ChatRooms(
    rooms: Signal<HashMap<VerifyingKey, RoomData>>,
    current_room: Signal<Option<VerifyingKey>>,
) -> Element {
    rsx! {
        aside { class: "chat-rooms",
            div { class: "logo-container",
                img {
                    class: "logo",
                    src: "/freenet_logo.svg",
                    alt: "Freenet Logo"
                }
            }
            div { class: "sidebar-header",
                div { class: "rooms-title",
                    h2 {
                        Icon { icon: FaComments, width: 20, height: 20 }
                        span { "Rooms" }
                    }
                }
            }
            ul { class: "chat-rooms-list",
                {rooms.read().iter().map(|(room_key, room_data)| {
                    let room_key = *room_key;
                    let room_name = room_data.room_state.configuration.configuration.name.clone();
                    let is_current = current_room.read().map_or(false, |cr| cr == room_key);
                    rsx! {
                        li {
                            key: "{room_key:?}",
                            class: if is_current { "chat-room-item active" } else { "chat-room-item" },
                            button {
                                class: "room-name-button",
                                onclick: move |_| {
                                    current_room.set(Some(room_key));
                                },
                                "{room_name}"
                            }
                        }
                    }
                })}
            }
        }
    }
}
