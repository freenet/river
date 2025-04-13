use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::components::members::Invitation;
use crate::room_data::{CurrentRoom, RoomData, Rooms};
use dioxus::prelude::*;
use ed25519_dalek::SigningKey;
use river_common::room_state::member::{AuthorizedMember, Member};
use std::rc::Rc;
use wasm_bindgen::JsCast;

const BASE_URL: &str =
    "http://127.0.0.1:50509/v1/contract/web/BcfxyjCH4snaknrBoCiqhYc9UFvmiJvhsp5d4L5DuvRa/";

#[component]
pub fn InviteMemberModal(is_active: Signal<bool>) -> Element {
    // Add a signal to track when a new invitation is generated
    let regenerate_trigger = use_signal(|| 0);

    let current_room_data_signal: Memo<Option<RoomData>> = use_memo(move || {
        CURRENT_ROOM
            .read()
            .owner_key
            .as_ref()
            .and_then(|key| ROOMS.read().map.get(key).cloned())
    });

    let invitation_future = use_resource(move || {
        let _trigger = *regenerate_trigger.read(); // Use underscore to indicate intentional unused variable
                                                   // Using trigger value to force re-execution when regenerate_trigger changes
        async move {
            if !*is_active.read() {
                return Err("Modal closed".to_string());
            }
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
                        Some(Ok(invitation)) => {
                            let room_name = current_room_data_signal()
                                .map(|r| r.room_state.configuration.configuration.name.clone())
                                .unwrap_or_else(|| "this chat room".to_string());

                            // Generate a fresh invite code and URL each time
                            let invite_code = invitation.to_encoded_string();
                            let invite_url = format!("{}?invitation={}", BASE_URL, invite_code);

                            let default_msg = format!(
                                "You've been invited to join the chat room \"{}\"!\n\n\
                                To join:\n\
                                1. Install Freenet from https://freenet.org\n\
                                2. Open this link:\n\
                                {}\n\n\
                                IMPORTANT: This invitation contains a unique identity key created just for you. \
                                Do not share it with others.",
                                room_name, invite_url
                            );

                            rsx! {
                                h3 { class: "title is-4", "Single-Use Invitation" }

                                div { class: "message is-warning",
                                    div { class: "message-header",
                                        "⚠️ One Invitation = One Person"
                                    }
                                    div { class: "message-body",
                                        "Each invitation link contains a unique identity key. Never share the same invitation with multiple people - generate a new invitation for each person you invite."
                                    }
                                }

                                InvitationContent {
                                    invitation_text: default_msg,
                                    invitation_url: invite_url.clone(),
                                    invitation: Rc::new(invitation.clone()),
                                    is_active: is_active,
                                    regenerate_trigger: regenerate_trigger
                                }
                            }
                        }
                        Some(Err(err)) => {
                            rsx! {
                                h3 { class: "title is-4", "Error" }
                                p { class: "has-text-danger", "{err}" }
                                button {
                                    class: "button",
                                    onclick: move |_| {
                                        is_active.set(false);
                                        is_active.set(true);
                                    },
                                    "Try Again"
                                }
                            }
                        },
                        None => {
                            rsx! {
                                h3 { class: "title is-4", "Generating Invitation..." }
                                progress { class: "progress is-small is-primary" }
                            }
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
#[component]
fn InvitationContent(
    invitation_text: String,
    invitation_url: String,
    invitation: Rc<Invitation>,
    is_active: Signal<bool>,
    regenerate_trigger: Signal<i32>,
) -> Element {
    let mut copy_msg_text = use_signal(|| "Copy Message".to_string());
    let mut copy_link_text = use_signal(|| "Copy Link".to_string());

    // Clone the texts for use in the closures
    let invitation_text_for_clipboard = invitation_text.clone();
    let invitation_url_for_clipboard = invitation_url.clone();

    let copy_message_to_clipboard = move |_| {
        if let Some(window) = web_sys::window() {
            if let Ok(navigator) = window.navigator().dyn_into::<web_sys::Navigator>() {
                let clipboard = navigator.clipboard();
                let _ = clipboard.write_text(&invitation_text_for_clipboard);
                copy_msg_text.set("Copied!".to_string());
                // Reset the other button
                copy_link_text.set("Copy Link".to_string());
            }
        }
    };

    let copy_link_to_clipboard = {
        move |_| {
            if let Some(window) = web_sys::window() {
                if let Ok(navigator) = window.navigator().dyn_into::<web_sys::Navigator>() {
                    let clipboard = navigator.clipboard();
                    let _ = clipboard.write_text(&invitation_url_for_clipboard);
                    copy_link_text.set("Copied!".to_string());
                    // Reset the other button
                    copy_msg_text.set("Copy Message".to_string());
                }
            }
        }
    };

    rsx! {
        // Link section
        div { class: "field",
            label { class: "label", "Invitation link:" }
            div { class: "field has-addons",
                div { class: "control is-expanded",
                    input {
                        class: "input",
                        r#type: "text",
                        value: invitation_url,
                        readonly: true
                    }
                }
                div { class: "control",
                    button {
                        class: "button is-info",
                        onclick: copy_link_to_clipboard,
                        span { class: "icon", i { class: "fas fa-copy" } }
                        span { "{copy_link_text}" }
                    }
                }
            }
        }

        // Full message section
        div { class: "field",
            label { class: "label", "Full invitation message:" }
            div {
                class: "box",
                style: "white-space: pre-wrap; font-family: monospace; max-height: 200px; overflow-y: auto;",
                "{invitation_text}"
            }
        }

        div { class: "field is-grouped",
            div { class: "control",
                button {
                    class: "button is-primary",
                    onclick: copy_message_to_clipboard,
                    span { class: "icon", i { class: "fas fa-copy" } }
                    span { "{copy_msg_text}" }
                }
            }
            div { class: "control",
                button {
                    class: "button is-info",
                    onclick: move |_| {
                        // Reset the copy button texts when generating a new invitation
                        copy_msg_text.set("Copy Message".to_string());
                        copy_link_text.set("Copy Link".to_string());

                        // Increment the regenerate trigger to force a new invitation
                        let current_value = *regenerate_trigger.read();
                        regenerate_trigger.set(current_value + 1);
                    },
                    span { class: "icon", i { class: "fas fa-key" } }
                    span { "Generate New Invitation" }
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
    }
}
