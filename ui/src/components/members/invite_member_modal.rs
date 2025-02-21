use river_common::crypto_values::CryptoValue;
use crate::room_data::{CurrentRoom, Rooms};
use river_common::room_state::member::{AuthorizedMember, Member, MembersDelta};
use river_common::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use crate::components::members::Invitation;
use ed25519_dalek::SigningKey;
use dioxus::prelude::*;
use freenet_scaffold::ComposableState;

#[component]
pub fn InviteMemberModal(is_active: Signal<bool>) -> Element {
    let mut rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let mut user_key = use_signal(String::new);
    let mut error_message = use_signal(String::new);
    let mut invitation_url = use_signal(String::new);

    let invite_member = move |_| {
        error_message.set(String::new());

        // Validate key format
        let key = user_key.read().clone();
        let crypto_value = match CryptoValue::from_encoded_string(&key) {
            Ok(CryptoValue::VerifyingKey(vk)) => vk,
            Ok(_) => {
                error_message.set("Invalid key type - expected verifying key".to_string());
                return;
            }
            Err(e) => {
                error_message.set(format!("Invalid key format: {}", e));
                return;
            }
        };

        let member_vk = crypto_value;

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

        let delta = ChatRoomStateV1Delta {
            members: Some(MembersDelta::new(vec![authorized_member])),
            ..Default::default()
        };

        // Apply changes
        if let Err(e) = room_data.room_state.apply_delta(
            &room_data.room_state.clone(),
            &ChatRoomParametersV1 {
                owner: owner_key.clone(),
            },
            &Some(delta),
        ) {
            error_message.set(format!("Failed to apply delta: {:?}", e));
            return;
        }

        // Generate invitation
        let invitee_signing_key = SigningKey::generate(&mut rand::thread_rng());
        let invitation = Invitation {
            room: owner_key.clone(),
            invitee_signing_key: invitee_signing_key.clone(),
            invitee: authorized_member,
        };

        // Create invitation URL
        let encoded = invitation.to_encoded_string();
        let url = format!("http://127.0.0.1:50509/v1/contract/web/{}/?invitation={}", 
            CryptoValue::VerifyingKey(*owner_key).to_encoded_string(), encoded);
        invitation_url.set(url);

        // Reset input but keep modal open to show URL
        user_key.set(String::new());
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
                                placeholder: "Enter river:v1:vk:... key",
                                value: "{user_key}",
                                onchange: move |evt| user_key.set(evt.value().to_string())
                            }
                        }}
                    }

                    {
                        (!error_message.read().is_empty()).then(|| rsx!(
                            div {
                                class: "notification is-danger",
                                "{error_message}"
                            }
                        ))
                    }

                    if invitation_url.read().is_empty() {
                        rsx! { div {
                            class: "field",
                            div {
                                class: "control",
                                button {
                                    class: "button is-primary",
                                    onclick: invite_member,
                                    "Generate Invitation"
                                }
                            }
                        }}
                    } else {
                        rsx! { div {
                                class: "field",
                                label { class: "label", "Invitation URL" }
                                div {
                                    class: "control",
                                    input {
                                        class: "input",
                                        r#type: "text",
                                        readonly: true,
                                        value: "{invitation_url}"
                                    }
                                }
                                p {
                                    class: "help",
                                    "Share this URL with the invited member"
                                }
                                div {
                                    class: "control mt-3",
                                    button {
                                        class: "button",
                                        onclick: move |_| {
                                            invitation_url.set(String::new());
                                            is_active.set(false);
                                        },
                                        "Close"
                                    }
                                }
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
