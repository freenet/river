use crate::components::members::Invitation;
use crate::room_data::Rooms;
use dioxus::prelude::*;

#[component]
pub fn ReceiveInvitationModal(invitation: Signal<Option<Invitation>>) -> Element {
    let mut rooms = use_context::<Signal<Rooms>>();

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
                        let inv_data = invitation.read().clone();
                        match inv_data.as_ref() {
                            Some(inv) => {
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
                                }
                            } else if invited_member_exists {
                                rsx! {
                                    p { "This invitation is for a member that already exists in the room." }
                                    p { "If you lost access to your previous key, you can use this invitation to restore access with your current key." }
                                    div {
                                        class: "buttons",
                                        button {
                                            class: "button is-warning",
                                            onclick: move |_| {
                                                let room = inv.room;
                                                let member_vk = inv.invitee.member.member_vk;
                                                let signing_key = inv.invitee_signing_key.clone();
                                                
                                                let mut rooms = rooms.write();
                                                if let Some(room_data) = rooms.map.get_mut(&room) {
                                                    // Replace the old key with the new one
                                                    room_data.restore_member_access(
                                                        member_vk,
                                                        &signing_key
                                                    );
                                                }
                                                invitation.set(None); 
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
                            } else {
                                rsx! {
                                    p { "You have been invited to join a new room." }
                                    p { "Would you like to accept the invitation?" }
                                    div {
                                        class: "buttons",
                                        button {
                                            class: "button is-primary",
                                            onclick: move |_| {
                                                // Handle accepting the invitation
                                                // This is where you would add the logic to accept the invitation
                                                invitation.set(None);
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
