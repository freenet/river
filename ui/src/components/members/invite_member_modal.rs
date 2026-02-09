use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::components::members::Invitation;
use crate::room_data::RoomData;
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{FaArrowsRotate, FaCopy, FaXmark};
use dioxus_free_icons::Icon;
use ed25519_dalek::SigningKey;
use river_core::room_state::member::{AuthorizedMember, Member};
use std::rc::Rc;
use wasm_bindgen::JsCast;

/// Fallback URL for non-browser environments or when window.location is unavailable
const FALLBACK_BASE_URL: &str =
    "http://127.0.0.1:7509/v1/contract/web/raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv/";

/// Get the base URL for invitation links.
/// Derives from the current window.location so invitations work on any host/port.
fn get_invitation_base_url() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            // Get the current URL's origin (protocol + host + port) and pathname
            let location = window.location();
            let href = location.href().unwrap_or_default();
            // Remove any query string or fragment, keep the base path
            if let Some(pos) = href.find('?') {
                href[..pos].to_string()
            } else if let Some(pos) = href.find('#') {
                href[..pos].to_string()
            } else {
                href
            }
        } else {
            FALLBACK_BASE_URL.to_string()
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        FALLBACK_BASE_URL.to_string()
    }
}

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

                // Serialize member to CBOR for signing
                let mut member_bytes = Vec::new();
                ciborium::ser::into_writer(&member, &mut member_bytes)
                    .map_err(|e| format!("Failed to serialize member: {}", e))?;

                // Sign using delegate with fallback to local signing
                let signature = crate::signing::sign_member_with_fallback(
                    room_data.room_key(),
                    member_bytes,
                    &room_data.self_sk,
                )
                .await;

                // Create authorized member with pre-computed signature
                let authorized_member = AuthorizedMember::with_signature(member, signature);

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

    if !*is_active.read() {
        return rsx! {};
    }

    rsx! {
        // Backdrop
        div {
            class: "fixed inset-0 bg-black/50 z-40",
            onclick: move |_| is_active.set(false)
        }

        // Modal
        div { class: "fixed inset-0 z-50 flex items-center justify-center p-4",
            div {
                class: "bg-panel rounded-xl shadow-xl max-w-lg w-full max-h-[90vh] overflow-y-auto",
                onclick: move |e| e.stop_propagation(),

                // Header
                div { class: "px-6 py-4 border-b border-border flex items-center justify-between",
                    h2 { class: "text-lg font-semibold text-text", "Invite Member" }
                    button {
                        class: "p-1 text-text-muted hover:text-text transition-colors",
                        onclick: move |_| is_active.set(false),
                        Icon { icon: FaXmark, width: 14, height: 14 }
                    }
                }

                // Body
                div { class: "px-6 py-4",
                    match &*invitation_future.read_unchecked() {
                        Some(Ok(invitation)) => {
                            let room_name = current_room_data_signal()
                                .map(|r| {
                                    let sealed_name = &r.room_state.configuration.configuration.display.name;
                                    match unseal_bytes_with_secrets(sealed_name, &r.secrets) {
                                        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                                        Err(_) => sealed_name.to_string_lossy(),
                                    }
                                })
                                .unwrap_or_else(|| "this chat room".to_string());

                            let invite_code = invitation.to_encoded_string();
                            let base_url = get_invitation_base_url();
                            let invite_url = format!("{}?invitation={}", base_url, invite_code);

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
                                // Warning
                                div { class: "mb-4 p-3 bg-warning-bg border-l-4 border-yellow-500 rounded-r-lg",
                                    p { class: "text-sm text-text",
                                        span { class: "font-medium", "One invitation = one person. " }
                                        "Generate a new invitation for each person you invite."
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
                                div { class: "text-center py-8",
                                    p { class: "text-red-500 mb-4", "{err}" }
                                    button {
                                        class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text rounded-lg transition-colors",
                                        onclick: move |_| {
                                            is_active.set(false);
                                            is_active.set(true);
                                        },
                                        "Try Again"
                                    }
                                }
                            }
                        },
                        None => {
                            rsx! {
                                div { class: "text-center py-8",
                                    div { class: "w-8 h-8 border-2 border-accent border-t-transparent rounded-full animate-spin mx-auto mb-4" }
                                    p { class: "text-text-muted", "Generating invitation..." }
                                }
                            }
                        }
                    }
                }
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
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text mb-1", "Invitation link:" }
            div { class: "flex gap-2",
                input {
                    class: "flex-1 px-3 py-2 bg-surface border border-border rounded-lg text-sm text-text font-mono truncate",
                    r#type: "text",
                    value: invitation_url,
                    readonly: true
                }
                button {
                    class: "px-3 py-2 bg-accent hover:bg-accent-hover text-white text-sm rounded-lg transition-colors flex items-center gap-2",
                    onclick: copy_link_to_clipboard,
                    Icon { icon: FaCopy, width: 14, height: 14 }
                    span { "{copy_link_text}" }
                }
            }
        }

        // Full message section
        div { class: "mb-4",
            label { class: "block text-sm font-medium text-text mb-1", "Full invitation message:" }
            div {
                class: "p-3 bg-surface rounded-lg text-xs text-text font-mono whitespace-pre-wrap max-h-40 overflow-y-auto",
                "{invitation_text}"
            }
        }

        // Action buttons
        div { class: "flex flex-wrap gap-2",
            button {
                class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white text-sm font-medium rounded-lg transition-colors flex items-center gap-2",
                onclick: copy_message_to_clipboard,
                Icon { icon: FaCopy, width: 14, height: 14 }
                span { "{copy_msg_text}" }
            }
            button {
                class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text text-sm rounded-lg transition-colors flex items-center gap-2",
                onclick: move |_| {
                    copy_msg_text.set("Copy Message".to_string());
                    copy_link_text.set("Copy Link".to_string());
                    let current_value = *regenerate_trigger.read();
                    regenerate_trigger.set(current_value + 1);
                },
                Icon { icon: FaArrowsRotate, width: 14, height: 14 }
                span { "New Invitation" }
            }
            button {
                class: "px-4 py-2 text-text-muted hover:text-text text-sm rounded-lg transition-colors",
                onclick: move |_| is_active.set(false),
                "Close"
            }
        }
    }
}
