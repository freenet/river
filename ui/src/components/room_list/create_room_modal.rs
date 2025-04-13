use crate::components::app::{CREATE_ROOM_MODAL, CURRENT_ROOM, ROOMS};
use dioxus::prelude::*;
use ed25519_dalek::SigningKey;

#[component]
pub fn CreateRoomModal() -> Element {
    let mut room_name = use_signal(String::new);
    let mut nickname = use_signal(String::new);

    let create_room = move |_| {
        let name = room_name.read().clone();
        if name.is_empty() {
            return;
        }

        // Generate key outside the borrow
        let self_sk = SigningKey::generate(&mut rand::thread_rng());
        let nick = nickname.read().clone();

        // Create room and get the key
        let new_room_key =
            ROOMS.with_mut(|rooms| rooms.create_new_room_with_name(self_sk, name, nick));

        // Update current room
        CURRENT_ROOM.with_mut(|current_room| {
            current_room.owner_key = Some(new_room_key);
        });

        // Reset and close modal
        room_name.set(String::new());
        nickname.set(String::new());
        CREATE_ROOM_MODAL.with_mut(|modal| {
            modal.show = false;
        });
    };

    rsx! {
        div {
            class: format_args!("modal {}", if CREATE_ROOM_MODAL.read().show { "is-active" } else { "" }),
            div {
                class: "modal-background",
                onclick: move |_| {
                    CREATE_ROOM_MODAL.with_mut(|modal| {
                        modal.show = false;
                    });
                }
            }
            div {
                class: "modal-content",
                div {
                    class: "box",
                    h1 { class: "title is-4 mb-3", "Create New Room" }

                    div { class: "field",
                        label { class: "label", "Room Name" }
                        div { class: "control",
                            input {
                                class: "input",
                                value: "{room_name}",
                                onchange: move |evt| room_name.set(evt.value().to_string())
                            }
                        }
                    }

                    div { class: "field",
                        label { class: "label", "Your Nickname" }
                        div { class: "control",
                            input {
                                class: "input",
                                value: "{nickname}",
                                onchange: move |evt| nickname.set(evt.value().to_string())
                            }
                        }
                    }

                    div { class: "field",
                        div { class: "control",
                            button {
                                class: "button is-primary",
                                onclick: create_room,
                                "Create Room"
                            }
                        }
                    }
                }
            }
            button {
                class: "modal-close is-large",
                onclick: move |_| {
                    CREATE_ROOM_MODAL.with_mut(|modal| {
                        modal.show = false;
                    });
                }
            }
        }
    }
}
