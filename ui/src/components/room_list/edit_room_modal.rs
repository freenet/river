use super::room_name_field::RoomNameField;
use crate::components::app::chat_delegate::save_rooms_to_delegate;
use crate::components::app::{CURRENT_ROOM, EDIT_ROOM_MODAL, ROOMS};
use dioxus::logger::tracing::{error, info};
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
        editing_room.read().as_ref().is_some_and(|room_data| {
            let user_vk = room_data.self_sk.verifying_key();
            let room_vk = EDIT_ROOM_MODAL.read().room.unwrap();
            user_vk == room_vk
        })
    });

    // Render the modal if room configuration is available
    if let Some(config) = room_config.clone().read().deref() {
        rsx! {
            // Modal backdrop
            div {
                class: "fixed inset-0 z-50 flex items-center justify-center",
                // Overlay
                div {
                    class: "absolute inset-0 bg-black/50",
                    onclick: move |_| {
                        EDIT_ROOM_MODAL.write().room = None;
                    }
                }
                // Modal content
                div {
                    class: "relative z-10 w-full max-w-md mx-4 bg-panel rounded-xl shadow-xl border border-border",
                    div {
                        class: "p-6",
                        h1 { class: "text-xl font-semibold text-text mb-4", "Room Details" }

                        RoomNameField {
                            config: config.clone(),
                            is_owner: *user_is_owner.read()
                        }

                        // Read-only room info
                        if let Some(room_data) = editing_room.read().as_ref() {
                            // Room Public Key
                            div {
                                class: "mt-4",
                                label {
                                    class: "block text-sm font-medium text-text-muted mb-1",
                                    title: "Ed25519 public key (Curve25519 elliptic curve)",
                                    "Room Public Key"
                                }
                                input {
                                    r#type: "text",
                                    readonly: true,
                                    title: "Ed25519 public key (Curve25519 elliptic curve)",
                                    class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text-muted text-sm font-mono cursor-text select-all",
                                    value: "{bs58::encode(room_data.owner_vk.as_bytes()).into_string()}"
                                }
                            }
                            // Contract ID
                            div {
                                class: "mt-4",
                                label {
                                    class: "block text-sm font-medium text-text-muted mb-1",
                                    "Contract ID"
                                }
                                input {
                                    r#type: "text",
                                    readonly: true,
                                    class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text-muted text-sm font-mono cursor-text select-all",
                                    value: "{room_data.contract_key.id()}"
                                }
                            }
                        }

                        // Leave Room Section
                        if *show_leave_confirmation.read() {
                            div {
                                class: "bg-yellow-500/10 border border-yellow-500/20 rounded-lg p-4 mt-4",
                                p {
                                    class: "text-yellow-400 mb-3",
                                    if *user_is_owner.read() {
                                        "Warning: You are the owner of this room. Leaving will permanently delete it for you. Other members might retain access if they have the contract key, but coordination will be lost."
                                    } else {
                                        "Are you sure you want to leave this room? This action cannot be undone."
                                    }
                                }
                                div {
                                    class: "flex gap-3",
                                    button {
                                        class: "px-4 py-2 bg-red-500 hover:bg-red-600 text-white font-medium rounded-lg transition-colors",
                                        onclick: move |_| {
                                            // Read the room_vk first and drop the read borrow
                                            let room_vk_to_remove = EDIT_ROOM_MODAL.read().room;

                                            if let Some(room_vk) = room_vk_to_remove {
                                                // Perform writes *after* the read borrow is dropped
                                                ROOMS.write().map.remove(&room_vk);

                                                // Check and potentially clear CURRENT_ROOM
                                                if CURRENT_ROOM.read().owner_key == Some(room_vk) {
                                                    CURRENT_ROOM.write().owner_key = None;
                                                }

                                                // Close the modal *last*
                                                EDIT_ROOM_MODAL.write().room = None;

                                                // Save updated rooms to delegate storage
                                                info!("Room removed, saving to delegate");
                                                spawn(async move {
                                                    if let Err(e) = save_rooms_to_delegate().await {
                                                        error!("Failed to save rooms after removal: {}", e);
                                                    }
                                                });
                                            }
                                            // Reset confirmation state regardless
                                            show_leave_confirmation.set(false);
                                        },
                                        "Confirm Leave"
                                    }
                                    button {
                                        class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text rounded-lg transition-colors",
                                        onclick: move |_| show_leave_confirmation.set(false),
                                        "Cancel"
                                    }
                                }
                            }
                        } else {
                             // Only show Leave button if not confirming
                            div {
                                class: "mt-4",
                                button {
                                    class: "px-4 py-2 border border-red-500 text-red-500 hover:bg-red-500/10 rounded-lg transition-colors",
                                    onclick: move |_| show_leave_confirmation.set(true),
                                    "Leave Room"
                                }
                            }
                        }
                    }
                    // Close button
                    button {
                        class: "absolute top-3 right-3 p-1 text-text-muted hover:text-text transition-colors",
                        onclick: move |_| {
                            EDIT_ROOM_MODAL.write().room = None;
                        },
                        "✕"
                    }
                }
            }
        }
    } else {
        rsx! {}
    }
}
