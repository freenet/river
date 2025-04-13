pub(crate) mod create_room_modal;
pub(crate) mod edit_room_modal;
pub(crate) mod receive_invitation_modal;
pub(crate) mod room_name_field;

use crate::components::app::{CREATE_ROOM_MODAL, CURRENT_ROOM, ROOMS};
use crate::room_data::CurrentRoom;
use create_room_modal::CreateRoomModal;
use dioxus::prelude::*;
use dioxus_free_icons::{
    icons::fa_solid_icons::{FaComments, FaLink, FaPlus},
    Icon,
};
use wasm_bindgen_futures::spawn_local;

// Access the build timestamp (ISO 8601 format) environment variable set by build.rs
const BUILD_TIMESTAMP_ISO: &str = env!("BUILD_TIMESTAMP_ISO", "Build timestamp not set");

#[component]
pub fn RoomList() -> Element {
    // Signal to hold the locally formatted build time string
    let mut formatted_build_time = use_signal(|| "Loading build time...".to_string());

    // Use eval to run JavaScript for local time formatting
    let mut eval = document::eval(
        r#"
        const isoTimestamp = await dioxus.recv(); // Receive the ISO string
        if (!isoTimestamp || isoTimestamp === "Build timestamp not set") {
            return "Build time unavailable";
        }
        try {
            const date = new Date(isoTimestamp);
            // Format using locale defaults for date and time (verbose)
            const options = {
                year: 'numeric', month: 'short', day: 'numeric',
                hour: 'numeric', minute: '2-digit', // second: '2-digit', // Optionally add seconds
                timeZoneName: 'short' // Optionally add timezone name
            };
            // Use undefined locale to default to browser's locale
            return date.toLocaleString(undefined, options);
        } catch (e) {
            console.error("Error formatting build timestamp:", e);
            return "Invalid build time";
        }
        "#);

    // Run the JS formatting logic once on component mount
    use_effect(move || {
        spawn_local(async move {
            // Send the ISO timestamp to the JavaScript evaluator
            if let Err(e) = eval.send(BUILD_TIMESTAMP_ISO.into()) {
                log::error!("Failed to send timestamp to JS eval: {:?}", e);
                formatted_build_time.set("Eval error".to_string());
                return;
            }

            // Receive the formatted string back from JavaScript
            if let Ok(result) = eval.recv::<String>().await {
                if let Ok(time_str) = result.as_string() {
                    formatted_build_time.set(time_str);
                } else {
                    formatted_build_time.set("Format error".to_string());
                }
            } else {
                formatted_build_time.set("Receive error".to_string());
            }
        });
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
                CreateRoomModal {}
                {ROOMS.read().map.iter().map(|(room_key, room_data)| {
                    let room_key = *room_key;
                    let room_name = room_data.room_state.configuration.configuration.name.clone();
                    let is_current = CURRENT_ROOM.read().owner_key == Some(room_key);
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
                // Display the formatted local time from the signal
                {"Built: "} {formatted_build_time}
            }
            // --- End of build datetime information ---
        }
    }
}
