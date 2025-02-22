use crate::components::members::Invitation;
use crate::room_data::{CurrentRoom, RoomData, Rooms};
use dioxus::prelude::*;
use ed25519_dalek::SigningKey;
use river_common::room_state::member::{AuthorizedMember, Member};

#[component]
pub fn InviteMemberModal(is_active: Signal<bool>) -> Element {
    let rooms_signal = use_context::<Signal<Rooms>>();
    let current_room_signal = use_context::<Signal<CurrentRoom>>();
    let current_room_data_signal: Memo<Option<RoomData>> = use_memo(move || {
        let rooms = rooms_signal.read();
        let current_room = current_room_signal.read();
        current_room
            .owner_key
            .as_ref()
            .and_then(|key| rooms.map.get(key).cloned())
    });

    let mut invitation_future = use_resource(move || async move {
        let room_data = current_room_data_signal();
        if let Some(room_data) = room_data {
            // Generate new signing key for invitee
            let invitee_signing_key = SigningKey::generate(&mut rand::thread_rng());
            let invitee_verifying_key = invitee_signing_key.verifying_key();

            // Create member struct
            let member = Member {
                owner_member_id: room_data.owner_vk.into(),
                invited_by: room_data.self_sk.verifying_key().into(),
                member_vk: invitee_verifying_key,
            };

            // Create authorized member with signature
            let authorized_member = AuthorizedMember::new(member, &room_data.self_sk);

            // Create invitation
            let invitation = Invitation {
                room: room_data.owner_vk,
                invitee_signing_key,
                invitee: authorized_member,
            };

            Ok::<Invitation, String>(invitation)
        } else {
            Err("No room selected".to_string())
        }
    });

    rsx! {
        div {
            class: if *is_active.read() { "modal is-active" } else { "modal" },
            div {
                class: "modal-background",
                onclick: move |_| {
                    is_active.set(false);
                }
            }
            div { class: "modal-content",
                div { class: "box",
                    match &*invitation_future.read_unchecked() {
                        Some(Ok(invitation)) => rsx! {
                            h3 { class: "title is-4", "Invitation Generated" }
                            div { class: "field",
                                label { class: "label", "Share this invitation code:" }
                                div { class: "control",
                                    input {
                                        class: "input",
                                        readonly: true,
                                        value: "{invitation.to_encoded_string()}"
                                    }
                                }
                            }
                            div { class: "field is-grouped",
                                div { class: "control",
                                    button {
                                        class: "button is-primary",
                                        onclick: move |_| {
                                            invitation_future.restart();
                                        },
                                        "Generate New"
                                    }
                                }
                                div { class: "control",
                                    button {
                                        class: "button",
                                        onclick: move |_| is_active.set(false),
                                        "Close"
                                    }
                                }
                            }
                        },
                        Some(Err(err)) => rsx! {
                            h3 { class: "title is-4", "Error" }
                            p { class: "has-text-danger", "{err}" }
                            button {
                                class: "button",
                                onclick: move |_| invitation_future.restart(),
                                "Try Again"
                            }
                        },
                        None => rsx! {
                            h3 { class: "title is-4", "Generating Invitation..." }
                            progress { class: "progress is-small is-primary" }
                        }
                    }
                }
            }
            button {
                class: "modal-close is-large",
                onclick: move |_| is_active.set(false)
            }
        }
    }
}
