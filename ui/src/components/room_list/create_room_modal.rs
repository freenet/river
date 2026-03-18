use crate::components::app::{CREATE_ROOM_MODAL, CURRENT_ROOM, ROOMS};
use dioxus::prelude::*;
use ed25519_dalek::SigningKey;

#[component]
pub fn CreateRoomModal() -> Element {
    let mut room_name = use_signal(String::new);
    let mut nickname = use_signal(String::new);
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
        // Keep a clone of the signing key for delegate storage (self_sk is moved into room creation)
        let sk_clone = self_sk.clone();
        let nick = nickname.read().clone();
        let private = false; // Private rooms temporarily disabled
        info!(
            "🔵 Creating {} room with nickname: {}",
            if private { "private" } else { "public" },
            nick
        );

        // Defer all signal mutations to a clean execution context to
        // prevent RefCell re-entrant borrow panics.
        crate::util::defer(move || {
            // Create room and get the key
            info!("🔵 About to call create_new_room_with_name...");
            let new_room_key = ROOMS
                .with_mut(|rooms| rooms.create_new_room_with_name(self_sk, name, nick, private));
            info!("🔵 Room created with key: {:?}", new_room_key);

            // Store signing key in delegate so it can sign messages on behalf of this room
            let room_key_bytes = new_room_key.to_bytes();
            crate::util::safe_spawn_local(async move {
                use crate::signing::store_signing_key;
                use dioxus::logger::tracing::{error, info};
                info!("Storing signing key in delegate for new room");
                if let Err(e) = store_signing_key(room_key_bytes, &sk_clone).await {
                    error!("Failed to store signing key in delegate: {}", e);
                } else {
                    info!("Signing key stored in delegate for new room");
                }
            });

            // Update current room
            info!("🔵 Updating CURRENT_ROOM...");
            CURRENT_ROOM.with_mut(|current_room| {
                current_room.owner_key = Some(new_room_key);
            });
            info!("🔵 CURRENT_ROOM updated");

            // Mark room as needing sync (this will trigger use_effect in app.rs)
            info!("🔵 Marking room for synchronization...");
            crate::components::app::mark_needs_sync(new_room_key);
            info!("🔵 Room marked for sync");

            // Reset and close modal
            info!("🔵 Resetting form fields...");
            room_name.set(String::new());
            nickname.set(String::new());
            info!("🔵 Closing modal...");
            CREATE_ROOM_MODAL.with_mut(|modal| {
                modal.show = false;
            });
            info!("🔵 Modal closed");
            info!("🔵 Create room handler completed successfully");
        });
    };

    let is_open = CREATE_ROOM_MODAL.read().show;

    if !is_open {
        return rsx! {};
    }

    rsx! {
        // Backdrop
        div {
            class: "fixed inset-0 bg-black/50 z-40",
            onclick: move |_| {
                CREATE_ROOM_MODAL.with_mut(|modal| {
                    modal.show = false;
                });
            }
        }

        // Modal
        div { class: "fixed inset-0 z-50 flex items-center justify-center p-4",
            div {
                class: "bg-panel rounded-xl shadow-xl max-w-md w-full",
                onclick: move |e| e.stop_propagation(),

                // Header
                div { class: "px-6 py-4 border-b border-border",
                    h2 { class: "text-lg font-semibold text-text", "Create New Room" }
                }

                // Body
                div { class: "px-6 py-4 space-y-4",
                    div {
                        label { class: "block text-sm font-medium text-text mb-1", "Room Name" }
                        input {
                            class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent/50 focus:border-accent",
                            value: "{room_name}",
                            placeholder: "Enter room name",
                            onchange: move |evt| room_name.set(evt.value().to_string())
                        }
                    }

                    div {
                        label { class: "block text-sm font-medium text-text mb-1", "Your Nickname" }
                        input {
                            class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text placeholder-text-muted focus:outline-none focus:ring-2 focus:ring-accent/50 focus:border-accent",
                            value: "{nickname}",
                            placeholder: "Enter your nickname",
                            onchange: move |evt| nickname.set(evt.value().to_string())
                        }
                    }

                    label { class: "flex items-center gap-3 cursor-not-allowed opacity-50",
                        input {
                            r#type: "checkbox",
                            class: "w-4 h-4 rounded border-border text-accent focus:ring-accent/50",
                            checked: false,
                            disabled: true,
                        }
                        span { class: "text-sm text-text-muted",
                            "Private rooms temporarily disabled"
                        }
                    }
                }

                // Footer
                div { class: "px-6 py-4 border-t border-border flex justify-end gap-3",
                    button {
                        class: "px-4 py-2 text-sm text-text-muted hover:text-text hover:bg-surface rounded-lg transition-colors",
                        onclick: move |_| {
                            CREATE_ROOM_MODAL.with_mut(|modal| {
                                modal.show = false;
                            });
                        },
                        "Cancel"
                    }
                    button {
                        class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white text-sm font-medium rounded-lg transition-colors",
                        onclick: create_room,
                        "Create Room"
                    }
                }
            }
        }
    }
}
