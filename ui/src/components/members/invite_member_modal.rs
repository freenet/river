use crate::components::members::Invitation;
use crate::room_data::{CurrentRoom, RoomData, Rooms};
use dioxus::prelude::*;
use ed25519_dalek::SigningKey;
use river_common::room_state::member::{AuthorizedMember, Member};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{self, ClipboardEvent};

const BASE_URL: &str = "http://127.0.0.1:50509/v1/contract/web/C8tm2U616vC2dBo8ffWoc8YL9yJGyKJ5C4Y2Nfm2YAn5";

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

    let invitation_future = use_resource(
        move || async move {
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
                            
                            let invite_code = invitation.to_encoded_string();
                            let invite_url = format!("{}?invitation={}", BASE_URL, invite_code);
                            
                            let html_msg = use_signal(|| {
                                format!(
                                    "To join <b>{}</b>, install <a href=\"https://freenet.org/\">Freenet</a> and click <a href=\"{}\">this link</a>",
                                    room_name, invite_url
                                )
                            });

                            let plain_msg = use_signal(|| {
                                format!(
                                    "To join {}, install Freenet (https://freenet.org/) and open this link:\n{}",
                                    room_name, invite_url
                                )
                            });

                            let mut copy_text = use_signal(|| "Copy Invitation".to_string());
                            
                            let copy_to_clipboard = {
                                let html_msg = html_msg.clone();
                                let plain_msg = plain_msg.clone();
                                move |_| {
                                    if let Some(window) = web_sys::window() {
                                        if let Some(document) = window.document() {
                                            // Create a temporary textarea for the plain text
                                            let temp_input = document.create_element("textarea").unwrap();
                                            temp_input.set_text_content(Some(&plain_msg.read()));
                                            let body = document.body().unwrap();
                                            body.append_child(&temp_input).unwrap();
                                            
                                            if let Ok(input_element) = temp_input.dyn_into::<web_sys::HtmlTextAreaElement>() {
                                                input_element.select();
                                                
                                                // Create and dispatch copy event
                                                let event = ClipboardEvent::new_with_options(
                                                    "copy",
                                                    web_sys::ClipboardEventInit::new()
                                                        .data_type("text/html")
                                                        .data(&html_msg.read())
                                                ).unwrap();
                                                
                                                input_element.dispatch_event(&event).unwrap();
                                                document.exec_command("copy").unwrap();
                                                
                                                // Cleanup
                                                body.remove_child(&temp_input).unwrap();
                                                copy_text.set("Copied!".to_string());
                                            }
                                        }
                                    }
                                }
                            };

                            rsx! {
                                h3 { class: "title is-4", "Invitation Generated" }
                                
                                div { class: "message is-info",
                                    div { class: "message-body",
                                        "Important: Keep this invitation link private. Anyone who gets this link can join the room pretending to be the invited person."
                                    }
                                }

                                div { class: "field",
                                    label { class: "label", "Preview of invitation message:" }
                                    div { 
                                        class: "box content",
                                        dangerous_inner_html: "{html_msg}"
                                    }
                                }

                                div { class: "field is-grouped",
                                    div { class: "control",
                                        button {
                                            class: "button is-primary",
                                            onclick: copy_to_clipboard,
                                            span { class: "icon", i { class: "fas fa-copy" } }
                                            span { "{copy_text}" }
                                        }
                                    }
                                    div { class: "control",
                                        button {
                                            class: "button",
                                            onclick: move |_| {
                                                is_active.set(false);
                                                is_active.set(true);
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
                            }
                        }
                        Some(Err(err)) => rsx! {
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
