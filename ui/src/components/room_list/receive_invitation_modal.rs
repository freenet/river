use crate::components::app::freenet_api::{
    freenet_synchronizer::SynchronizerMessage, FreenetSynchronizer,
};
use crate::components::app::{PENDING_INVITES, ROOMS, SYNCHRONIZER, WEB_API};
use crate::components::members::Invitation;
use crate::invites::{PendingInvites, PendingRoomJoin, PendingRoomStatus};
use crate::room_data::Rooms;
use dioxus::logger::tracing::{error, info};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use wasm_bindgen::JsCast;

/// Main component for the invitation modal
#[component]
pub fn ReceiveInvitationModal(invitation: Signal<Option<Invitation>>) -> Element {
    // Extract the room key from the invitation if it exists
    let room_key = invitation.read().as_ref().map(|inv| inv.room.clone());

    // Listen for custom events from the FreenetSynchronizer
    use_effect(move || {
        let window = web_sys::window().expect("No window found");
        let closure = wasm_bindgen::closure::Closure::wrap(Box::new(
            move |event: web_sys::CustomEvent| {
                let detail = event.detail();
                if let Some(key_hex) = detail.as_string() {
                    info!("Received invitation accepted event with key: {}", key_hex);

                    // Convert hex string back to bytes
                    let mut bytes = Vec::new();
                    for i in 0..(key_hex.len() / 2) {
                        let byte_str = &key_hex[i * 2..(i + 1) * 2];
                        if let Ok(byte) = u8::from_str_radix(byte_str, 16) {
                            bytes.push(byte);
                        }
                    }

                    // Try to convert bytes to VerifyingKey
                    if bytes.len() == 32 {
                        let mut array = [0u8; 32];
                        array.copy_from_slice(&bytes);

                        if let Ok(key) = VerifyingKey::from_bytes(&array) {
                            // Use with_mut for atomic update
                            PENDING_INVITES.with_mut(|pending| {
                                if let Some(join) = pending.map.get_mut(&key) {
                                    join.status = PendingRoomStatus::Retrieved;
                                    info!(
                                        "Updated pending invitation status to Retrieved for key: {:?}",
                                        key
                                    );
                                }
                            });

                            // Check if this is the current invitation
                            let should_close = {
                                if let Some(inv) = invitation.read().as_ref() {
                                    inv.room == key
                                } else {
                                    false
                                }
                            };

                            // If it is, close the modal
                            if should_close {
                                invitation.set(None);
                                info!("Closed invitation modal for key: {:?}", key);
                            }
                        }
                    }
                }
            },
        )
            as Box<dyn FnMut(web_sys::CustomEvent)>);

        window
            .add_event_listener_with_callback(
                "river-invitation-accepted",
                closure.as_ref().unchecked_ref(),
            )
            .expect("Failed to add event listener");

        // Also check for already retrieved invitations
        if let Some(key) = room_key {
            if let Some(join) = PENDING_INVITES.read().map.get(&key) {
                if matches!(join.status, PendingRoomStatus::Retrieved) {
                    // Remove from pending invites
                    PENDING_INVITES.write().map.remove(&key);

                    // Clear the invitation
                    invitation.set(None);
                }
            }
        }

        // Return cleanup function to remove event listener
        (move || {
            window
                .remove_event_listener_with_callback(
                    "river-invitation-accepted",
                    closure.as_ref().unchecked_ref(),
                )
                .expect("Failed to remove event listener");
        })()
    });

    rsx! {
        div {
            class: if invitation.read().is_some() { "modal is-active" } else { "modal" },
            div {
                class: "modal-background",
                onclick: move |_| invitation.set(None)
            }
            div {
                class: "modal-content",
                div {
                    class: "box",
                    h1 { class: "title", "Invitation Received" }
                    {
                        let inv_data = invitation.read().as_ref().cloned();
                        match inv_data {
                            Some(inv) => {
                                // Clone the Signal itself, not just a temporary borrow
                                render_invitation_content(inv, invitation.clone())
                            },
                            None => rsx! { p { "No invitation data available" } }
                        }
                    }
                }
            }
            button {
                class: "modal-close is-large",
                onclick: move |_| invitation.set(None)
            }
        }
    }
}

/// Renders the content of the invitation modal based on the invitation data
fn render_invitation_content(inv: Invitation, invitation: Signal<Option<Invitation>>) -> Element {
    let pending_invites = PENDING_INVITES.read();
    let pending_status = pending_invites
        .map
        .get(&inv.room)
        .map(|join| &join.status);

    match pending_status {
        Some(PendingRoomStatus::Retrieving) => render_retrieving_state(),
        Some(PendingRoomStatus::Error(e)) => render_error_state(e, &inv.room, invitation),
        Some(PendingRoomStatus::Retrieved) => {
            // Room retrieved successfully, close modal
            render_retrieved_state(&inv.room, invitation)
        }
        None => render_invitation_options(inv, invitation),
    }
}

/// Renders the loading state when retrieving room data
fn render_retrieving_state() -> Element {
    rsx! {
        div {
            class: "has-text-centered p-4",
            p { class: "mb-4", "Retrieving room data..." }
            progress {
                class: "progress is-info",
                max: "100"
            }
        }
    }
}

/// Renders the error state when room retrieval fails
fn render_error_state(
    error: &str,
    room_key: &VerifyingKey,
    mut invitation: Signal<Option<Invitation>>,
) -> Element {
    let room_key = room_key.clone(); // Clone to avoid borrowing issues

    rsx! {
        div {
            class: "notification is-danger",
            p { class: "mb-4", "Failed to retrieve room: {error}" }
            button {
                class: "button",
                onclick: move |_| {
                    PENDING_INVITES.write().map.remove(&room_key);
                    invitation.set(None);
                },
                "Close"
            }
        }
    }
}

/// Renders the state when room is successfully retrieved
fn render_retrieved_state(
    _room_key: &VerifyingKey,
    _invitation: Signal<Option<Invitation>>,
) -> Element {
    // Just return an empty element - the cleanup is now handled in the main component
    rsx! { "" }
}

/// Renders the invitation options based on the user's membership status
fn render_invitation_options(inv: Invitation, invitation: Signal<Option<Invitation>>) -> Element {
    let (current_key_is_member, invited_member_exists) =
        check_membership_status(&inv, &ROOMS.read());

    if current_key_is_member {
        render_already_member(invitation)
    } else if invited_member_exists {
        render_restore_access_option(inv, invitation)
    } else {
        render_new_invitation(inv, invitation)
    }
}

/// Checks the membership status of the user in the room
fn check_membership_status(inv: &Invitation, current_rooms: &Rooms) -> (bool, bool) {
    if let Some(room_data) = current_rooms.map.get(&inv.room) {
        let user_vk = inv.invitee_signing_key.verifying_key();
        let current_key_is_member = user_vk == room_data.owner_vk
            || room_data
                .room_state
                .members
                .members
                .iter()
                .any(|m| m.member.member_vk == user_vk);
        let invited_member_exists = room_data
            .room_state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == inv.invitee.member.member_vk);
        (current_key_is_member, invited_member_exists)
    } else {
        (false, false)
    }
}

/// Renders the UI when the user is already a member of the room
fn render_already_member(mut invitation: Signal<Option<Invitation>>) -> Element {
    rsx! {
        p { "You are already a member of this room with your current key." }
        button {
            class: "button",
            onclick: move |_| invitation.set(None),
            "Close"
        }
    }
}

/// Renders the UI for restoring access to an existing member
fn render_restore_access_option(
    inv: Invitation,
    mut invitation: Signal<Option<Invitation>>,
) -> Element {
    rsx! {
        p { "This invitation is for a member that already exists in the room." }
        p { "If you lost access to your previous key, you can use this invitation to restore access with your current key." }
        div {
            class: "buttons",
            button {
                class: "button is-warning",
                onclick: {
                    let room = inv.room.clone();
                    let member_vk = inv.invitee.member.member_vk.clone();
                    let mut invitation = invitation.clone();

                    move |_| {
                        // Use with_mut for atomic update
                        ROOMS.with_mut(|rooms| {
                            if let Some(room_data) = rooms.map.get_mut(&room) {
                                room_data.restore_member_access(
                                    member_vk,
                                    inv.invitee.clone()
                                );
                            }
                        });
                        invitation.set(None);
                    }
                },
                "Restore Access"
            }
            button {
                class: "button",
                onclick: move |_| invitation.set(None),
                "Cancel"
            }
        }
    }
}

/// Renders the UI for a new invitation
fn render_new_invitation(inv: Invitation, mut invitation: Signal<Option<Invitation>>) -> Element {
    // Clone the invitation for the closure
    let inv_for_accept = inv.clone();

    rsx! {
        p { "You have been invited to join a new room." }
        p { "Would you like to accept the invitation?" }
        div {
            class: "buttons",
            button {
                class: "button is-primary",
                onclick: move |_| {
                    accept_invitation(inv_for_accept.clone());
                },
                "Accept"
            }
            button {
                class: "button",
                onclick: move |_| invitation.set(None),
                "Decline"
            }
        }
    }
}

/// Handles the invitation acceptance process
fn accept_invitation(inv: Invitation) {
    let room_owner = inv.room.clone();
    let authorized_member = inv.invitee.clone();
    let invitee_signing_key = inv.invitee_signing_key.clone();

    // Generate a nickname from the member's key
    let encoded = bs58::encode(authorized_member.member.member_vk.as_bytes()).into_string();
    let shortened = encoded.chars().take(6).collect::<String>();
    let nickname = format!("User-{}", shortened);

    info!("Adding room to pending invites: {:?}", room_owner);

    // Add to pending invites
    PENDING_INVITES.write().map.insert(
        room_owner.clone(),
        PendingRoomJoin {
            authorized_member: authorized_member.clone(),
            invitee_signing_key: invitee_signing_key.clone(),
            preferred_nickname: nickname.clone(),
            status: PendingRoomStatus::Retrieving,
        },
    );

    info!("Spawning task to request room state");
    wasm_bindgen_futures::spawn_local(async move {
        info!(
            "Requesting room state for invitation with owner key: {:?}",
            room_owner
        );

        // Add a small delay to ensure the WebSocket connection is fully established
        // This helps when accepting invitations immediately after startup
        crate::util::sleep(std::time::Duration::from_millis(500)).await;
        info!("Delay complete, proceeding with room state request");

        // Send the AcceptInvitation message
        let result = SYNCHRONIZER
            .write()
            .get_message_sender()
            .unbounded_send(SynchronizerMessage::AcceptInvitation {
                owner_vk: room_owner.clone(),
                authorized_member,
                invitee_signing_key,
                nickname,
            })
            .map_err(|e| format!("Failed to send message: {}", e));

        match result {
            Ok(_) => {
                info!("Successfully requested room state for invitation");
            }
            Err(e) => {
                // Log detailed error information
                error!("Failed to request room state for invitation: {}", e);
                error!(
                    "Error details: invitation for room with owner key: {:?}",
                    room_owner
                );
            }
        }
    });
}
