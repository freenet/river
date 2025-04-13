use super::room_name_field::RoomNameField;
use crate::components::app::{CURRENT_ROOM, EDIT_ROOM_MODAL, ROOMS};
use dioxus::prelude::*;
use std::ops::Deref;

#[component]
pub fn EditRoomModal() -> Element {
    // State for leave confirmation
    let mut show_leave_confirmation = use_signal(|| false);

    // Memoize the room being edited
    let editing_room = use_memo(move || {
        EDIT_ROOM_MODAL.read().room.and_then(|editing_room_vk| {
            ROOMS.read().map.iter().find_map(|(room_vk, room_data)| {
                if &editing_room_vk == room_vk {
                    Some(room_data.clone())
                } else {
                    None
                }
            })
        })
    });

    // Memoize the room configuration
    let room_config = use_memo(move || {
        editing_room
            .read()
            .as_ref()
            .map(|room_data| room_data.room_state.configuration.configuration.clone())
    });

    // Memoize if the current user is the owner of the room being edited
    let user_is_owner = use_memo(move || {
        editing_room.read().as_ref().map_or(false, |room_data| {
            let user_vk = room_data.self_sk.verifying_key();
            let room_vk = EDIT_ROOM_MODAL.read().room.unwrap();
            user_vk == room_vk
        })
    });

    // Render the modal if room configuration is available
    if let Some(config) = room_config.clone().read().deref() {
        rsx! {
            div {
                class: "modal is-active",
                div {
                    class: "modal-background",
                    onclick: move |_| {
                        EDIT_ROOM_MODAL.write().room = None;
                    }
                }
                div {
                    class: "modal-content",
                    div {
                        class: "box",
                        h1 { class: "title is-4 mb-3", "Room Configuration" }

                        RoomNameField {
                            config: config.clone(),
                            is_owner: *user_is_owner.read()
                        }

                        // Leave Room Section
                        if *show_leave_confirmation.read() {
                            div {
                                class: "notification is-warning mt-4",
                                p {
                                    if *user_is_owner.read() {
                                        "Warning: You are the owner of this room. Leaving will permanently delete it for you. Other members might retain access if they have the contract key, but coordination will be lost."
                                    } else {
                                        "Are you sure you want to leave this room? This action cannot be undone."
                                    }
                                }
                                div {
                                    class: "buttons mt-3",
                                    button {
                                        class: "button is-danger",
                                        onclick: move |_| {
                                            if let Some(room_vk) = EDIT_ROOM_MODAL.read().room {
                                                // Remove room from ROOMS
                                                ROOMS.write().map.remove(&room_vk);

                                                // If this was the current room, clear it
                                                if CURRENT_ROOM.read().owner_key == Some(room_vk) {
                                                    CURRENT_ROOM.write().owner_key = None;
                                                }

                                                // Close the modal
                                                EDIT_ROOM_MODAL.write().room = None;
                                            }
                                            show_leave_confirmation.set(false); // Reset confirmation state
                                        },
                                        "Confirm Leave"
                                    }
                                    button {
                                        class: "button",
                                        onclick: move |_| show_leave_confirmation.set(false),
                                        "Cancel"
                                    }
                                }
                            }
                        } else {
                             // Only show Leave button if not confirming
                            div {
                                class: "field mt-4",
                                div {
                                    class: "control",
                                    button {
                                        class: "button is-danger is-outlined",
                                        onclick: move |_| show_leave_confirmation.set(true),
                                        "Leave Room"
                                    }
                                }
                            }
                        }
                    }
                }
                button {
                    class: "modal-close is-large",
                    onclick: move |_| {
                        EDIT_ROOM_MODAL.write().room = None;
                    }
                }
            }
        }
    } else {
        rsx! {}
    }
}
