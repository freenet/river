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
                            button {
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
                                    span {
                                        class: "room-edit-button",
                                        title: "Edit room",
                                        onclick: move |evt: Event<MouseData>| {
                                            evt.stop_propagation();
                                            info!("Editing room: {:?}", room_key);
                                            edit_room_modal_signal.write().room = Some(room_key);
                                        },
                                        Icon { icon: FaPencil, width: 12, height: 12 }
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
