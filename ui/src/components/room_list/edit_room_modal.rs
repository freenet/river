use std::ops::Deref;
use dioxus::prelude::*;
use crate::components::app::EditRoomModalSignal;
use crate::room_data::Rooms;
use super::room_name_field::RoomNameField;

#[component]
pub fn EditRoomModal() -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let mut edit_room_signal = use_context::<Signal<EditRoomModalSignal>>();

    // Memoize the room being edited
    let editing_room = use_memo(move || {
        edit_room_signal.read().room.and_then(|editing_room_vk| {
            rooms.read().map.iter().find_map(|(room_vk, room_data)| {
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
        editing_room
            .read()
            .as_ref()
            .map_or(false, |room_data| {
                let user_vk = room_data.user_signing_key.verifying_key();
                let room_vk = edit_room_signal.read().room.unwrap();
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
                        edit_room_signal.write().room = None;
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
                    }
                }
                button {
                    class: "modal-close is-large",
                    onclick: move |_| {
                        edit_room_signal.write().room = None;
                    }
                }
            }
        }
    } else {
        rsx! { }
    }
}
