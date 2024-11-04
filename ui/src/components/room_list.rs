pub(crate) mod edit_room_modal;
pub(crate) mod room_name_field;

use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaComments;
use dioxus_free_icons::Icon;
use crate::room_data::{CurrentRoom, Rooms};

#[component]
pub fn RoomList() -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();

    rsx! {
        aside { class: "room-list",
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
            ul { class: "room-list-list",
                button {
                    class: "button is-primary is-fullwidth mb-4",
                    onclick: move |_| {
                        let mut rooms_write = rooms.write();
                        let new_room_key = rooms_write.create_new_room(rooms_write.map.values().next().unwrap().self_sk.clone());
                        current_room.set(CurrentRoom { owner_key: Some(new_room_key) });
                    },
                    "New Room"
                }
                {rooms.read().map.iter().map(|(room_key, room_data)| {
                    let room_key = *room_key;
                    let room_name = room_data.room_state.configuration.configuration.name.clone();
                    let is_current = current_room.read().owner_key == Some(room_key);
                    let mut current_room_clone = current_room.clone(); // Clone the Signal
                    rsx! {
                        li {
                            key: "{room_key:?}",
                            class: if is_current { "chat-room-item active" } else { "chat-room-item" },
                            div {
                                class: "room-name-button",
                                onclick: move |_| {
                                    current_room_clone.set(CurrentRoom { owner_key : Some(room_key)});
                                },
                                div {
                                    class: "room-name-container",
                                    span {
                                        class: "room-name-text",
                                        "{room_name}"
                                    }
                                }
                            }
                        }
                    }
                }).collect::<Vec<_>>().into_iter()}
            }
        }
    }
}
