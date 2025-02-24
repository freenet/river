use crate::components::members::Invitation;
use crate::room_data::Rooms;
use crate::components::app::freenet_api::FreenetApiSynchronizer;
use crate::invites::{PENDING_INVITES, PendingRoomJoin, PendingRoomStatus};
use dioxus::prelude::*;

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
                            Some(inv) => {
                                // Check if this room is in pending invites
                                let pending_status = PENDING_INVITES.read()
                                    .map.get(&inv.room)
                                    .map(|join| &join.status);

                                match pending_status {
                                    Some(PendingRoomStatus::Retrieving) => rsx! {
                                        div {
                                            class: "has-text-centered p-4",
                                            p { class: "mb-4", "Retrieving room data..." }
                                            progress {
                                                class: "progress is-info",
                                                max: "100"
                                            }
                                        }
                                    },
                                    Some(PendingRoomStatus::Error(e)) => rsx! {
                                        div {
                                            class: "notification is-danger",
                                            p { class: "mb-4", "Failed to retrieve room: {e}" }
                                            button {
                                                class: "button",
                                                onclick: move |_| {
                                                    let mut pending = PENDING_INVITES.write();
                                                    pending.map.remove(&inv.room);
                                                    invitation.set(None);
                                                },
                                                "Close"
                                            }
                                        }
                                    },
                                    Some(PendingRoomStatus::Retrieved) => {
                                        // Room retrieved successfully, close modal
                                        let mut pending = PENDING_INVITES.write();
                                        pending.map.remove(&inv.room);
                                        invitation.set(None);
                                        rsx! { "" }
                                    },
                                    None => {
                                        let current_rooms = rooms.read();
                                        let (current_key_is_member, invited_member_exists) = if let Some(room_data) = current_rooms.map.get(&inv.room) {
                                            let user_vk = inv.invitee_signing_key.verifying_key();
                                            let current_key_is_member = user_vk == room_data.owner_vk ||
                                                room_data.room_state.members.members.iter().any(|m| m.member.member_vk == user_vk);
                                            let invited_member_exists = room_data.room_state.members.members.iter()
                                                .any(|m| m.member.member_vk == inv.invitee.member.member_vk);
                                            (current_key_is_member, invited_member_exists)
                                        } else {
                                            (false, false)
                                        };

                                        if current_key_is_member {
                                            rsx! {
                                                p { "You are already a member of this room with your current key." }
                                                button {
                                                    class: "button",
                                                    onclick: move |_| invitation.set(None),
                                                    "Close"
                                                }
                                            };
                                        } else if invited_member_exists {
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
                                            };
                                        } else {
                                            rsx! {
                                                p { "You have been invited to join a new room." }
                                                p { "Would you like to accept the invitation?" }
                                                div {
                                                    class: "buttons",
                                                    button {
                                                        class: "button is-primary",
                                                        onclick: {
                                                            let room_owner = inv.room.clone();
                                                            let authorized_member = inv.invitee.clone();
                                                            let freenet_api = use_context::<FreenetApiSynchronizer>();

                                                            move |_| {
                                                                let mut pending = PENDING_INVITES.write();
                                                                pending.map.insert(room_owner, PendingRoomJoin {
                                                                    authorized_member: authorized_member.clone(),
                                                                    preferred_nickname: "New Member".to_string(),
                                                                    status: PendingRoomStatus::Retrieving,
                                                                });

                                                                // Request room state from API
                                                                wasm_bindgen_futures::spawn_local(async move {
                                                                    freenet_api.request_room_state(&room_owner).await;
                                                                });
                                                            }
                                                        },
                                                        "Accept"
                                                    }
                                                    button {
                                                        class: "button",
                                                        onclick: move |_| invitation.set(None),
                                                        "Decline"
                                                    }
                                                }
                                            };
                                        }
                                    }
                                }
                            },
                            None => rsx! {
                                p { "No invitation data available" }
                            }
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
