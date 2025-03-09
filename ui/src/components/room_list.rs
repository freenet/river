pub(crate) mod create_room_modal;
pub(crate) mod edit_room_modal;
pub(crate) mod receive_invitation_modal;
pub(crate) mod room_name_field;

use crate::components::app::{CreateRoomModalSignal, CREATE_ROOM_MODAL, CURRENT_ROOM, ROOMS};
use crate::room_data::{CurrentRoom, Rooms};
use create_room_modal::CreateRoomModal;
use dioxus::prelude::*;
use dioxus_free_icons::{
    icons::fa_solid_icons::{FaComments, FaLink, FaPlus},
    Icon,
};

#[component]
pub fn RoomList() -> Element {

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
        }
    }
}
