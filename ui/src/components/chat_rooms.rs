pub(crate) mod edit_room_modal;
pub(crate) mod room_name_field;

use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{FaPencil, FaComments};
use dioxus_free_icons::Icon;
use dioxus_logger::tracing::info;
use crate::components::app::EditRoomModalSignal;
use crate::room_data::{CurrentRoom, Rooms};

#[component]
pub fn ChatRooms() -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let mut edit_room_modal_signal = use_context::<Signal<EditRoomModalSignal>>();

    rsx! {
        style { {r#"
            .room-item-content {
                display: flex;
                justify-content: space-between;
                align-items: center;
                width: 100%;
                padding: 8px;
            }
            .room-name-button {
                flex-grow: 1;
                text-align: left;
                overflow: hidden;
                text-overflow: ellipsis;
                white-space: nowrap;
                padding-right: 8px;
                min-width: 0;
                max-width: calc(100% - 24px);
                font-size: 14px;
            }
            .room-edit-button {
                background: none;
                border: none;
                cursor: pointer;
                padding: 2px;
                flex-shrink: 0;
                display: flex;
                align-items: center;
                justify-content: center;
                width: 16px;
                height: 16px;
                color: #bbb;
                opacity: 0.6;
                transition: opacity 0.2s ease-in-out;
            }
            .room-edit-button:hover {
                opacity: 1;
            }
            .chat-room-item {
                margin-bottom: 4px;
            }
            .chat-room-item.active .room-name-button {
                font-weight: bold;
            }
        "#} }
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
                                class: "room-item-content",
                                button {
                                    class: "room-name-button",
                                    onclick: move |_| {
                                        current_room_clone.set(CurrentRoom { owner_key : Some(room_key)});
                                    },
                                    "{room_name}"
                                }
                                button {
                                    class: "room-edit-button",
                                    title: "Edit room",
                                    onclick: move |_| {
                                        info!("Editing room: {:?}", room_key);
                                        edit_room_modal_signal.write().room = Some(room_key);
                                    },
                                    Icon { icon: FaPencil, width: 8, height: 8 }
                                }
                            }
                        }
                    }
                }).collect::<Vec<_>>().into_iter()}
            }
        }
    }
}
