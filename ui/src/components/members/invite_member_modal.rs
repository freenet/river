use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use bs58;
use common::room_state::ChatRoomParametersV1;
use common::room_state::member::{Member, AuthorizedMember, MembersDelta};
use crate::room_data::{CurrentRoom, Rooms};
use freenet_scaffold::ComposableState;

#[component]
pub fn InviteMemberModal(is_active: Signal<bool>) -> Element {
    let mut rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let mut user_key = use_signal(String::new);
    let mut error_message = use_signal(String::new);

    let invite_member = move |_| {
        error_message.set(String::new());
        
        // Validate key format
        let key = user_key.read().clone();
        if !key.starts_with("river:user:vk:") {
            error_message.set("Invalid key format".to_string());
            return;
        }

        // Extract and decode the key
        let encoded_key = key.trim_start_matches("river:user:vk:");
        let decoded_key = match bs58::decode(encoded_key).into_vec() {
            Ok(bytes) => bytes,
            Err(_) => {
                error_message.set("Invalid key encoding".to_string());
                return;
            }
        };

        // Convert to VerifyingKey
        let member_vk = match VerifyingKey::from_bytes(decoded_key.as_slice().try_into().unwrap_or(&[0; 32])) {
            Ok(vk) => vk,
            Err(_) => {
                error_message.set("Invalid verification key".to_string());
                return;
            }
        };

        // Get current room data
        let current = current_room.read();
        let owner_key = match &current.owner_key {
            Some(key) => key,
            None => {
                error_message.set("No room selected".to_string());
                return;
            }
        };

        let mut rooms_write = rooms.write();
        let room_data = match rooms_write.map.get_mut(owner_key) {
            Some(data) => data,
            None => {
                error_message.set("Room not found".to_string());
                return;
            }
        };

        // Create new member
        let member = Member {
            owner_member_id: owner_key.into(),
            invited_by: room_data.self_sk.verifying_key().into(),
            member_vk,
        };

        // Create authorized member
        let authorized_member = AuthorizedMember::new(member, &room_data.self_sk);
        
        // Create and apply delta
        let delta = vec![authorized_member];
        
        // Clone the state to avoid borrow checker issues
        let room_state = room_data.room_state.clone();
        let parameters = ChatRoomParametersV1 { owner: room_data.owner_vk };
        if let Err(e) = room_data.room_state.members.apply_delta(
            &room_state,
            &parameters,
            &Some(delta)
        ) {
            error_message.set(format!("Failed to add member: {}", e));
            return;
        }

        // Reset and close modal
        user_key.set(String::new());
        is_active.set(false);
    };

    rsx! {
        div {
            class: if *is_active.read() { "modal is-active" } else { "modal" },
            div {
                class: "modal-background",
                onclick: move |_| {
                    is_active.set(false);
                }
            }
            div {
                class: "modal-content",
                div {
                    class: "box",
                    h1 { class: "title is-4 mb-3", "Invite Member" }
                    
                    div {
                        class: "field",
                        label { class: "label", "Member Verification Key" }
                        div {
                            class: "control",
                            input {
                                class: "input",
                                r#type: "text",
                                placeholder: "Enter river:user:vk: key",
                                value: "{user_key}",
                                onchange: move |evt| user_key.set(evt.value().to_string())
                            }
                        }
                    }

                    {
                        (!error_message.read().is_empty()).then(|| rsx!(
                            div {
                                class: "notification is-danger",
                                "{error_message}"
                            }
                        ))
                    }

                    div {
                        class: "field",
                        div {
                            class: "control",
                            button {
                                class: "button is-primary",
                                onclick: invite_member,
                                "Add Member"
                            }
                        }
                    }
                }
            }
            button {
                class: "modal-close is-large",
                onclick: move |_| {
                    is_active.set(false);
                }
            }
        }
    }
}
