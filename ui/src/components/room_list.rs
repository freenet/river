pub(crate) mod create_room_modal;
pub(crate) mod edit_room_modal;
pub(crate) mod receive_invitation_modal;
pub(crate) mod room_name_field;

use crate::components::app::chat_delegate::save_rooms_to_delegate;
use crate::components::app::{CREATE_ROOM_MODAL, CURRENT_ROOM, ROOMS};
use crate::room_data::CurrentRoom;
use dioxus::logger::tracing::error;
use dioxus::prelude::*;
use dioxus_free_icons::{
    icons::fa_solid_icons::{FaComments, FaLink, FaPlus},
    Icon,
};

// Access the build timestamp (ISO 8601 format) environment variable set by build.rs
const BUILD_TIMESTAMP_ISO: &str = env!("BUILD_TIMESTAMP_ISO", "Build timestamp not set");

/// Convert UTC ISO timestamp to local time string
fn format_build_time_local() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        use js_sys::Date;
        let date = Date::new(&wasm_bindgen::JsValue::from_str(BUILD_TIMESTAMP_ISO));
        if date.to_string().as_string().is_some() {
            // Format as "YYYY-MM-DD HH:MM" in local time
            let year = date.get_full_year();
            let month = date.get_month() + 1; // JS months are 0-indexed
            let day = date.get_date();
            let hours = date.get_hours();
            let minutes = date.get_minutes();
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                year, month, day, hours, minutes
            )
        } else {
            BUILD_TIMESTAMP_ISO.to_string()
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        BUILD_TIMESTAMP_ISO.to_string()
    }
}

#[component]
pub fn RoomList() -> Element {
    // Memoize the room list to avoid reading signals during render
    let room_items = use_memo(move || {
        let rooms = ROOMS.read();
        let current_room_key = CURRENT_ROOM.read().owner_key;

        rooms
            .map
            .iter()
            .map(|(room_key, room_data)| {
                let room_key = *room_key;
                let room_name = room_data
                    .room_state
                    .configuration
                    .configuration
                    .display
                    .name
                    .to_string_lossy();
                let is_current = current_room_key == Some(room_key);
                (room_key, room_name, is_current)
            })
            .collect::<Vec<_>>()
    });

    rsx! {
        aside { class: "w-64 flex-shrink-0 bg-panel border-r border-border flex flex-col overflow-y-auto",
            // Logo
            div { class: "p-4 flex justify-center",
                img {
                    class: "w-24 h-auto",
                    src: asset!("/assets/river_logo.svg"),
                    alt: "River Logo"
                }
            }

            // Rooms header with create button
            div { class: "px-4 py-2 flex items-center justify-between",
                h2 { class: "text-sm font-semibold text-text-muted uppercase tracking-wide flex items-center gap-2",
                    Icon { width: 16, height: 16, icon: FaComments }
                    span { "Rooms" }
                }
                button {
                    class: "p-1.5 rounded-md text-text-muted hover:text-accent hover:bg-surface transition-colors",
                    title: "Create Room",
                    onclick: move |_| {
                        CREATE_ROOM_MODAL.write().show = true;
                    },
                    Icon { width: 14, height: 14, icon: FaPlus }
                }
            }

            // Room list
            ul { class: "flex-1 px-2 py-1 space-y-0.5",
                {room_items.read().iter().map(|(room_key, room_name, is_current)| {
                    let room_key = *room_key;
                    let room_name = room_name.clone();
                    let is_current = *is_current;
                    rsx! {
                        li { key: "{room_key:?}",
                            button {
                                class: format!(
                                    "w-full text-left px-3 py-2 rounded-lg text-sm transition-colors {}",
                                    if is_current {
                                        "bg-accent/10 text-accent font-medium"
                                    } else {
                                        "text-text hover:bg-surface"
                                    }
                                ),
                                onclick: move |_| {
                                    *CURRENT_ROOM.write() = CurrentRoom { owner_key: Some(room_key) };
                                    spawn(async move {
                                        if let Err(e) = save_rooms_to_delegate().await {
                                            error!("Failed to save current room selection: {}", e);
                                        }
                                    });
                                },
                                span { class: "block truncate", "{room_name}" }
                            }
                        }
                    }
                }).collect::<Vec<_>>().into_iter()}
            }

            // Bottom actions - secondary only (Create Room is in header as icon)
            div { class: "p-3 border-t border-border",
                button {
                    class: "w-full flex items-center justify-center gap-2 px-3 py-2 rounded-lg text-sm text-text-muted bg-surface hover:bg-surface-hover transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
                    disabled: true,
                    Icon { width: 14, height: 14, icon: FaLink }
                    span { "Join Room" }
                }
            }

            // Build info (local time)
            div { class: "px-3 py-2 text-xs text-text-muted text-center",
                {"Built: "} {format_build_time_local()}
            }
        }
    }
}
