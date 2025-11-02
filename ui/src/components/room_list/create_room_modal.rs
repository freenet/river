use crate::components::app::{CREATE_ROOM_MODAL, CURRENT_ROOM, NEEDS_SYNC, ROOMS};
use dioxus::prelude::*;
use ed25519_dalek::SigningKey;

#[component]
pub fn CreateRoomModal() -> Element {
    let mut room_name = use_signal(String::new);
    let mut nickname = use_signal(String::new);
    let mut is_private = use_signal(|| false);

    let create_room = move |_| {
        use dioxus::logger::tracing::info;
        info!("🔵 Create room button clicked");

        let name = room_name.read().clone();
        if name.is_empty() {
            info!("🔴 Room name is empty, returning");
            return;
        }
        info!("🔵 Room name: {}", name);

        // Generate key outside the borrow
        info!("🔵 Generating signing key...");
        let self_sk = SigningKey::generate(&mut rand::thread_rng());
        let nick = nickname.read().clone();
        let private = is_private.read().clone();
        info!(
            "🔵 Creating {} room with nickname: {}",
            if private { "private" } else { "public" },
            nick
        );

        // Create room and get the key
        info!("🔵 About to call create_new_room_with_name...");
        let new_room_key =
            ROOMS.with_mut(|rooms| rooms.create_new_room_with_name(self_sk, name, nick, private));
        info!("🔵 Room created with key: {:?}", new_room_key);

        // Update current room
        info!("🔵 Updating CURRENT_ROOM...");
        CURRENT_ROOM.with_mut(|current_room| {
            current_room.owner_key = Some(new_room_key);
        });
        info!("🔵 CURRENT_ROOM updated");

        // Mark room as needing sync (this will trigger use_effect in app.rs)
        info!("🔵 Marking room for synchronization...");
        NEEDS_SYNC.write().insert(new_room_key);
        info!("🔵 Room marked for sync");

        // Reset and close modal
        info!("🔵 Resetting form fields...");
        room_name.set(String::new());
        nickname.set(String::new());
        is_private.set(false);
        info!("🔵 Closing modal...");
        CREATE_ROOM_MODAL.with_mut(|modal| {
            modal.show = false;
        });
        info!("🔵 Modal closed");
        info!("🔵 Create room handler completed successfully");
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
                        label { class: "checkbox",
                            input {
                                r#type: "checkbox",
                                class: "mr-2",
                                checked: "{is_private}",
                                onchange: move |evt| is_private.set(evt.value() == "true")
                            }
                            "Private room (messages and member nicknames will be encrypted)"
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
