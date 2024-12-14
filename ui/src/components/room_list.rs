pub(crate) mod create_room_modal;
pub(crate) mod edit_room_modal;
pub(crate) mod room_name_field;

use crate::components::app::CreateRoomModalSignal;
use crate::room_data::{CurrentRoom, Rooms};
use create_room_modal::CreateRoomModal;
use dioxus::prelude::*;
use dioxus_free_icons::{
    icons::fa_solid_icons::{FaComments, FaLink, FaPlus},
    Icon,
};

#[component]
pub fn RoomList() -> Element {
    let mut rooms = use_context::<Signal<Rooms>>();
    let mut current_room = use_context::<Signal<CurrentRoom>>();

    rsx! {
        aside { class: "room-list",
            div { class: "logo-container",
                img {
                    class: "logo",
                    src: asset!("/assets/freenet_logo.svg"),
                    alt: "Freenet Logo"
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
            div { class: "room-actions",
                {
                    let mut create_room_signal = use_context::<Signal<CreateRoomModalSignal>>();
                    rsx! {
                        button {
                            class: "create",
                            onclick: move |_| {
                                create_room_signal.write().show = true;
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
        }
    }
}
