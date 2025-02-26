use crate::components::members::Invitation;
use crate::room_data::Rooms;
use crate::components::app::freenet_api::FreenetApiSynchronizer;
use crate::invites::{PendingInvites, PendingRoomJoin, PendingRoomStatus};
use dioxus::prelude::*;

/// Main component for the invitation modal
#[component]
pub fn ReceiveInvitationModal(invitation: Signal<Option<Invitation>>) -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    
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
                            Some(inv) => render_invitation_content(inv, invitation.clone(), rooms.clone()),
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
    rooms: Signal<Rooms>
) -> Element {
    // Check if this room is in pending invites
    let pending_invites = use_context::<Signal<PendingInvites>>();
    let pending_read = pending_invites.read();
    let pending_status = pending_read
        .map.get(&inv.room)
        .map(|join| &join.status);

    match pending_status {
        Some(PendingRoomStatus::Retrieving) => render_retrieving_state(),
        Some(PendingRoomStatus::Error(e)) => {
            let room_key = inv.room.clone();
            let close_modal = move |_| {
                let mut pending = use_context::<Signal<PendingInvites>>();
                pending.write().map.remove(&room_key);
                invitation.set(None);
            };
            render_error_state(e, close_modal)
        },
        Some(PendingRoomStatus::Retrieved) => {
            // Room retrieved successfully, close modal
            let mut pending = use_context::<Signal<PendingInvites>>();
            pending.write().map.remove(&inv.room);
            invitation.set(None);
            rsx! { "" }
        },
        None => render_invitation_options(inv, rooms, invitation)
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
fn render_error_state(error: &str, on_close: impl FnMut(Event<MouseData>) + 'static) -> Element {
    rsx! {
        div {
            class: "notification is-danger",
            p { class: "mb-4", "Failed to retrieve room: {error}" }
            button {
                class: "button",
                onclick: on_close,
                "Close"
            }
        }
    }
}

/// Renders the invitation options based on the user's membership status
fn render_invitation_options(
    inv: Invitation, 
    rooms: Signal<Rooms>,
    invitation: Signal<Option<Invitation>>
) -> Element {
    let current_rooms = rooms.read();
    let (current_key_is_member, invited_member_exists) = check_membership_status(&inv, &current_rooms);

    let close_modal = move |_| invitation.set(None);

    if current_key_is_member {
        render_already_member(close_modal)
    } else if invited_member_exists {
        render_restore_access_option(inv, rooms, close_modal)
    } else {
        render_new_invitation(inv, close_modal)
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
fn render_already_member(on_close: impl FnMut(Event<MouseData>) + 'static) -> Element {
    rsx! {
        p { "You are already a member of this room with your current key." }
        button {
            class: "button",
            onclick: on_close,
            "Close"
        }
    }
}

/// Renders the UI for restoring access to an existing member
fn render_restore_access_option(
    inv: Invitation, 
    rooms: Signal<Rooms>,
    on_close: impl FnMut(Event<MouseData>) + 'static + Clone
) -> Element {
    let on_restore = {
        let room = inv.room.clone();
        let member_vk = inv.invitee.member.member_vk.clone();
        let authorized_member = inv.invitee.clone();
        let on_close = on_close.clone();
        
        move |_| {
            let mut rooms = rooms.write();
            if let Some(room_data) = rooms.map.get_mut(&room) {
                room_data.restore_member_access(
                    member_vk.clone(),
                    authorized_member.clone()
                );
            }
            on_close(Event::new(MouseData::default()));
        }
    };

    rsx! {
        p { "This invitation is for a member that already exists in the room." }
        p { "If you lost access to your previous key, you can use this invitation to restore access with your current key." }
        div {
            class: "buttons",
            button {
                class: "button is-warning",
                onclick: on_restore,
                "Restore Access"
            }
            button {
                class: "button",
                onclick: on_close,
                "Cancel"
            }
        }
    }
}

/// Renders the UI for a new invitation
fn render_new_invitation(
    inv: Invitation, 
    on_close: impl FnMut(Event<MouseData>) + 'static
) -> Element {
    let on_accept = {
        let inv = inv.clone();
        move |_| {
            accept_invitation(inv.clone());
        }
    };

    rsx! {
        p { "You have been invited to join a new room." }
        p { "Would you like to accept the invitation?" }
        div {
            class: "buttons",
            button {
                class: "button is-primary",
                onclick: on_accept,
                "Accept"
            }
            button {
                class: "button",
                onclick: on_close,
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
    let freenet_api = use_context::<FreenetApiSynchronizer>();

    // Generate a nickname from the member's key
    let encoded = bs58::encode(authorized_member.member.member_vk.as_bytes()).into_string();
    let shortened = encoded.chars().take(6).collect::<String>();
    let nickname = format!("User-{}", shortened);

    // Add to pending invites
    let mut pending = use_context::<Signal<PendingInvites>>();
    pending.write().map.insert(room_owner.clone(), PendingRoomJoin {
        authorized_member: authorized_member.clone(),
        invitee_signing_key: invitee_signing_key,
        preferred_nickname: nickname,
        status: PendingRoomStatus::Retrieving,
    });

    // Request room state from API
    let mut api = freenet_api.clone();
    let owner_key = room_owner.clone();
    wasm_bindgen_futures::spawn_local(async move {
        api.request_room_state(&owner_key).await;
    });
}
