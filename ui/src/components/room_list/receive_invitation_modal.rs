use dioxus::logger::tracing::{debug, error, info};
use crate::components::members::Invitation;
use crate::room_data::Rooms;
use crate::components::app::freenet_api::FreenetApiSynchronizer;
use crate::invites::{PendingInvites, PendingRoomJoin, PendingRoomStatus};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;

/// Main component for the invitation modal
#[component]
pub fn ReceiveInvitationModal(invitation: Signal<Option<Invitation>>) -> Element {
    let rooms = use_context::<Signal<Rooms>>();

    // We'll use this context directly when needed
    let _freenet_api_ctx = use_context::<Signal<FreenetApiSynchronizer>>();
    
    // We don't need to hold a mutable reference for the entire function
    // Just use the signal directly when needed

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
                                render_invitation_content(inv, invitation.clone(), rooms.clone())
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
fn render_invitation_content(
    inv: Invitation, 
    invitation: Signal<Option<Invitation>>, 
    rooms: Signal<Rooms>,
) -> Element {
    // Check if this room is in pending invites
    let pending_invites = use_context::<Signal<PendingInvites>>();
    let pending_read = pending_invites.read();
    let pending_status = pending_read
        .map.get(&inv.room)
        .map(|join| &join.status);
    
    match pending_status {
        Some(PendingRoomStatus::Retrieving) => render_retrieving_state(),
        Some(PendingRoomStatus::Error(e)) => render_error_state(e, &inv.room, invitation),
        Some(PendingRoomStatus::Retrieved) => {
            // Room retrieved successfully, close modal
            render_retrieved_state(&inv.room, invitation)
        },
        None => render_invitation_options(inv, invitation, rooms)
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
fn render_error_state(error: &str, room_key: &VerifyingKey, mut invitation: Signal<Option<Invitation>>) -> Element {
    let room_key = room_key.clone(); // Clone to avoid borrowing issues
    let mut pending = use_context::<Signal<PendingInvites>>();
    
    rsx! {
        div {
            class: "notification is-danger",
            p { class: "mb-4", "Failed to retrieve room: {error}" }
            button {
                class: "button",
                onclick: move |_| {
                    pending.write().map.remove(&room_key);
                    invitation.set(None);
                },
                "Close"
            }
        }
    }
}

/// Renders the state when room is successfully retrieved
fn render_retrieved_state(room_key: &VerifyingKey, mut invitation: Signal<Option<Invitation>>) -> Element {
    // Clone the key to avoid borrowing issues
    let key_to_remove = room_key.clone();
    
    // Schedule the cleanup to happen after rendering
    use_effect(move || {
        let mut pending = use_context::<Signal<PendingInvites>>();
        
        // Remove from pending invites
        if let Ok(mut pending_write) = pending.try_write() {
            pending_write.map.remove(&key_to_remove);
        }
        
        // Clear the invitation
        invitation.set(None);
        
        || {}
    });
    
    // Return empty element
    rsx! { "" }
}

/// Renders the invitation options based on the user's membership status
fn render_invitation_options(
    inv: Invitation, 
    invitation: Signal<Option<Invitation>>, 
    rooms: Signal<Rooms>,
) -> Element {
    let current_rooms = rooms.read();
    let (current_key_is_member, invited_member_exists) = check_membership_status(&inv, &current_rooms);

    if current_key_is_member {
        render_already_member(invitation)
    } else if invited_member_exists {
        render_restore_access_option(inv, invitation, rooms)
    } else {
        render_new_invitation(inv, invitation)
    }
}

/// Checks the membership status of the user in the room
fn check_membership_status(inv: &Invitation, current_rooms: &Rooms) -> (bool, bool) {
    if let Some(room_data) = current_rooms.map.get(&inv.room) {
        let user_vk = inv.invitee_signing_key.verifying_key();
        let current_key_is_member = user_vk == room_data.owner_vk ||
            room_data.room_state.members.members.iter().any(|m| m.member.member_vk == user_vk);
        let invited_member_exists = room_data.room_state.members.members.iter()
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
    rooms: Signal<Rooms>
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
                    let mut rooms = rooms.clone();
                    let mut invitation = invitation.clone();

                    move |_| {
                        let mut rooms = rooms.write();
                        if let Some(room_data) = rooms.map.get_mut(&room) {
                            room_data.restore_member_access(
                                member_vk,
                                inv.invitee.clone()
                            );
                        }
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
                    // Store invitation in a global state and trigger processing
                    let mut pending = use_context::<Signal<PendingInvites>>();
                    let mut api = use_context::<Signal<FreenetApiSynchronizer>>();
                    accept_invitation(inv_for_accept.clone(), &mut pending, &mut api);
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
fn accept_invitation(inv: Invitation, pending: &mut Signal<PendingInvites>, api: &mut Signal<FreenetApiSynchronizer>) {
    let room_owner = inv.room.clone();
    let authorized_member = inv.invitee.clone();
    let invitee_signing_key = inv.invitee_signing_key.clone();

    // Generate a nickname from the member's key
    let encoded = bs58::encode(authorized_member.member.member_vk.as_bytes()).into_string();
    let shortened = encoded.chars().take(6).collect::<String>();
    let nickname = format!("User-{}", shortened);

    info!("Adding room to pending invites: {:?}", room_owner);
    
    // Add to pending invites
    pending.write().map.insert(room_owner.clone(), PendingRoomJoin {
        authorized_member: authorized_member.clone(),
        invitee_signing_key: invitee_signing_key,
        preferred_nickname: nickname,
        status: PendingRoomStatus::Retrieving,
    });

    // Use a clone of the Signal itself, not the inner value
    let mut api_signal = api.clone();
    let mut pending_signal = pending.clone();
    let owner_key = room_owner.clone();
    
    info!("Spawning task to request room state");
    wasm_bindgen_futures::spawn_local(async move {
        info!("Requesting room state for invitation with owner key: {:?}", owner_key);
        
        // Add a small delay to ensure the WebSocket connection is fully established
        // This helps when accepting invitations immediately after startup
        crate::util::sleep(std::time::Duration::from_millis(500)).await;
        info!("Delay complete, proceeding with room state request");
        
        // Get a fresh mutable reference to the API inside the async task
        let result = {
            debug!("Getting fresh API reference from signal");
            let mut api_write = api_signal.write();
            debug!("Calling request_room_state");
            api_write.request_room_state(&owner_key).await
        };
        
        match result {
            Ok(_) => {
                info!("Successfully requested room state for invitation");
            },
            Err(e) => {
                // Log detailed error information
                error!("Failed to request room state for invitation: {}", e);
                error!("Error details: invitation for room with owner key: {:?}", owner_key);
                
                // Update pending invites to show error
                let mut pending = pending_signal.write();
                if let Some(pending_join) = pending.map.get_mut(&owner_key) {
                    pending_join.status = PendingRoomStatus::Error(format!("Failed to request room: {}", e));
                    debug!("Updated pending invitation status to Error");
                } else {
                    error!("Could not find pending invitation for room: {:?}", owner_key);
                }
            }
        }
    });
}
