use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerMessage;
use crate::components::app::{PENDING_INVITES, ROOMS, SYNCHRONIZER};
use crate::components::members::Invitation;
use crate::invites::{PendingRoomJoin, PendingRoomStatus};
use crate::room_data::Rooms;
use dioxus::logger::tracing::{error, info};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_common::room_state::member::MemberId;
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
                            // First check if this is the current invitation
                            let should_close = {
                                if let Some(inv) = invitation.read().as_ref() {
                                    inv.room == key
                                } else {
                                    false
                                }
                            };

                            // Use with_mut for atomic update
                            PENDING_INVITES.with_mut(|pending| {
                                if let Some(join) = pending.map.get_mut(&key) {
                                    join.status = PendingRoomStatus::Subscribed;
                                    info!(
                                        "Updated pending invitation status to Subscribed for key: {:?}",
                                        key
                                    );
                                }
                            });

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

        // Also check for already subscribed invitations
        if let Some(key) = room_key {
            let should_remove = {
                let pending_invites = PENDING_INVITES.read();
                pending_invites
                    .map
                    .get(&key)
                    .map(|join| matches!(join.status, PendingRoomStatus::Subscribed))
                    .unwrap_or(false)
            };

            if should_remove {
                // Remove from pending invites
                PENDING_INVITES.with_mut(|pending_invites| {
                    pending_invites.map.remove(&key);
                });

                // Clear the invitation
                invitation.set(None);
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
    let pending_status = pending_invites.map.get(&inv.room).map(|join| &join.status);

    match pending_status {
        Some(PendingRoomStatus::PendingSubscription) => render_pending_subscription_state(),
        Some(PendingRoomStatus::Subscribing) => render_subscribing_state(),
        Some(PendingRoomStatus::Error(e)) => render_error_state(e, &inv.room, invitation),
        Some(PendingRoomStatus::Subscribed) => {
            // Room subscribed and retrieved successfully, close modal
            render_subscribed_state(&inv.room, invitation)
        }
        None => render_invitation_options(inv, invitation),
    }
}

/// Renders the state when waiting to subscribe to room data
fn render_pending_subscription_state() -> Element {
    rsx! {
        div {
            class: "has-text-centered p-4",
            p { class: "mb-4", "Preparing to subscribe to room..." }
            progress {
                class: "progress is-primary",
                max: "100"
            }
        }
    }
}

/// Renders the loading state when subscribing to room data
fn render_subscribing_state() -> Element {
    rsx! {
        div {
            class: "has-text-centered p-4",
            p { class: "mb-4", "Subscribing to room..." }
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

/// Renders the state when room is successfully subscribed and retrieved
fn render_subscribed_state(
    room_key: &VerifyingKey,
    mut invitation: Signal<Option<Invitation>>,
) -> Element {
    // Get the room data to display confirmation
    let room_name = ROOMS.read()
        .map.get(room_key)
        .map(|r| r.room_state.configuration.configuration.name.clone())
        .unwrap_or_else(|| "the room".to_string());
    
    // Trigger a synchronization to ensure the new member is propagated
    let _ = SYNCHRONIZER
        .write()
        .get_message_sender()
        .unbounded_send(SynchronizerMessage::ProcessRooms);
    
    // Close the modal after a short delay
    use_effect(move || {
        let room_key = room_key.clone();
        wasm_bindgen_futures::spawn_local(async move {
            // Wait a moment to show the success message
            futures_timer::Delay::new(std::time::Duration::from_millis(1500)).await;
            invitation.set(None);
            
            // Remove from pending invites after successful join
            PENDING_INVITES.with_mut(|pending| {
                pending.map.remove(&room_key);
            });
            
            info!("Successfully joined room: {}", room_name);
        });
        || {} // Return a cleanup function
    });
    
    rsx! {
        div {
            class: "has-text-centered p-4",
            p { class: "mb-4 has-text-success is-size-5", 
                i { class: "fas fa-check-circle mr-2" }
                "Successfully joined \"{room_name}\"!"
            }
            p { "You'll be redirected to the room shortly..." }
        }
    }
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

    // Generate a default nickname from the member's key
    let encoded = bs58::encode(inv.invitee.member.member_vk.as_bytes()).into_string();
    let shortened = encoded.chars().take(6).collect::<String>();
    let default_nickname = format!("User-{}", shortened);

    // Create a signal for the nickname
    let mut nickname = use_signal(|| default_nickname);

    rsx! {
        p { "You have been invited to join a new room." }
        p { "Choose a nickname to use in this room:" }

        div { class: "field",
            div { class: "control",
                input {
                    class: "input",
                    r#type: "text",
                    value: "{nickname}",
                    oninput: move |evt| nickname.set(evt.value().clone()),
                    placeholder: "Your preferred nickname"
                }
            }
        }

        p { "Would you like to accept the invitation?" }
        div {
            class: "buttons",
            button {
                class: "button is-primary",
                disabled: nickname.read().trim().is_empty(),
                onclick: move |_| {
                    accept_invitation(inv_for_accept.clone(), nickname.read().clone());
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
fn accept_invitation(inv: Invitation, nickname: String) {
    let room_owner = inv.room.clone();
    let authorized_member = inv.invitee.clone();
    let invitee_signing_key = inv.invitee_signing_key.clone();

    // Use the user-provided nickname
    let nickname = if nickname.trim().is_empty() {
        // Fallback to generated nickname if somehow empty
        let encoded = bs58::encode(authorized_member.member.member_vk.as_bytes()).into_string();
        let shortened = encoded.chars().take(6).collect::<String>();
        format!("User-{}", shortened)
    } else {
        nickname
    };

    info!(
        "Adding room to pending invites: {:?}",
        MemberId::from(room_owner)
    );

    // Add to pending invites
    PENDING_INVITES.with_mut(|pending_invites| {
        pending_invites.map.insert(
            room_owner.clone(),
            PendingRoomJoin {
                authorized_member: authorized_member.clone(),
                invitee_signing_key: invitee_signing_key.clone(),
                preferred_nickname: nickname.clone(),
                status: PendingRoomStatus::PendingSubscription,
            },
        );
    });

    info!("Requesting room state for invitation");

    // First, check if we already have this room in our ROOMS
    let room_exists = ROOMS.read().map.contains_key(&room_owner);
    
    if room_exists {
        info!("Room already exists, adding member directly");
        
        // If the room already exists, add the member directly to the room
        ROOMS.with_mut(|rooms| {
            if let Some(room_data) = rooms.map.get_mut(&room_owner) {
                // Add the member to the room
                room_data.room_state.members.members.push(authorized_member.clone());
                
                // Add member info with the nickname
                let member_id = authorized_member.member.id();
                let member_info = river_common::room_state::member_info::MemberInfo {
                    member_id,
                    version: 0,
                    preferred_nickname: nickname.clone(),
                };
                let authorized_member_info = river_common::room_state::member_info::AuthorizedMemberInfo::new(
                    member_info,
                    &invitee_signing_key,
                );
                room_data.room_state.member_info.member_info.push(authorized_member_info);
                
                info!("Added member {:?} to room {:?}", member_id, MemberId::from(room_owner));
            }
        });
    }

    // Send the AcceptInvitation message to synchronize with the network
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
                MemberId::from(room_owner)
            );
        }
    }
}
