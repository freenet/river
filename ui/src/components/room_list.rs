pub(crate) mod create_room_modal;
pub(crate) mod edit_room_modal;
pub(crate) mod receive_invitation_modal;
pub(crate) mod room_name_field;

use crate::components::app::{CREATE_ROOM_MODAL, CURRENT_ROOM, ROOMS};
use crate::room_data::CurrentRoom;
use dioxus::prelude::*;
use dioxus_free_icons::{
    icons::fa_solid_icons::{FaComments, FaLink, FaPlus},
    Icon,
};
// use wasm_bindgen_futures::spawn_local;

// Access the build timestamp (ISO 8601 format) environment variable set by build.rs
const BUILD_TIMESTAMP_ISO: &str = env!("BUILD_TIMESTAMP_ISO", "Build timestamp not set");

#[component]
pub fn RoomList() -> Element {
    // Memoize the room list to avoid reading signals during render
    let room_items = use_memo(move || {
        let rooms = ROOMS.read();
        let current_room_key = CURRENT_ROOM.read().owner_key;

        rooms.map.iter().map(|(room_key, room_data)| {
            let room_key = *room_key;
            let room_name = room_data.room_state.configuration.configuration.display.name.to_string_lossy();
            let is_current = current_room_key == Some(room_key);
            (room_key, room_name, is_current)
        }).collect::<Vec<_>>()
    });

    rsx! {
        aside { class: "room-list",
            div { class: "logo-container",
                img {
                    class: "logo",
                    src: asset!("/assets/river_logo.svg"),
                    alt: "River Logo"
                }
            }
            div { class: "sidebar-header",
                div { class: "rooms-title",
                    h2 {
                        Icon {
                            width: 20,
                            height: 20,
                            icon: FaComments,
                        }
                        span { "Rooms" }
                    }
                }
            }
            ul { class: "room-list-list",
                {room_items.read().iter().map(|(room_key, room_name, is_current)| {
                    let room_key = *room_key;
                    let room_name = room_name.clone();
                    let is_current = *is_current;
                    rsx! {
                        li {
                            key: "{room_key:?}",
                            class: if is_current { "chat-room-item active" } else { "chat-room-item" },
                            div {
                                class: "room-name-button",
                                onclick: move |_| {
                                    *CURRENT_ROOM.write() = CurrentRoom { owner_key : Some(room_key)};
                                },
                                div {
                                    class: "room-name-container",
                                    style: "min-width: 0; word-wrap: break-word; white-space: normal;",
                                    span {
                                        class: "room-name-text",
                                        style: "word-break: break-word;",
                                        "{room_name}"
                                    }
                                }
                            }
                        }
                    }
                }).collect::<Vec<_>>().into_iter()}
            }
            div { class: "room-actions",
                {
                    rsx! {
                        button {
                            class: "create",
                            onclick: move |_| {
                                CREATE_ROOM_MODAL.write().show = true;
                            },
                            Icon {
                                width: 16,
                                height: 16,
                                icon: FaPlus,
                            }
                            span { "Create Room" }
                        }
                        button {
                            class: "add",
                            disabled: true,
                            Icon {
                                width: 16,
                                height: 16,
                                icon: FaLink,
                            }
                            span { "Add Room" }
                        }
                    }
                }
            }

            // --- Add the build datetime information here ---
            div {
                class: "build-info",
                // Display the UTC build time directly
                {"Built: "} {BUILD_TIMESTAMP_ISO} {" (UTC)"}
            }
            // --- End of build datetime information ---
        }
    }
}
