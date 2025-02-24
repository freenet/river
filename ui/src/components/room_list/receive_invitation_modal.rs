use crate::components::members::Invitation;
use crate::room_data::Rooms;
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
                    if let Some(inv) = invitation.read().as_ref() {
                        {
                            let current_rooms = rooms.read();
                                    let is_member = if let Some(room_data) = current_rooms.map.get(&inv.room) {
                                        // Check if user is owner or member
                                        let user_vk = inv.invitee_signing_key.verifying_key();
                                        user_vk == room_data.owner_vk || room_data.room_state.members.members.iter().any(|m| m.member.member_vk == user_vk)
                                    } else {
                                        false
                                    };

                                    if is_member {
                                        rsx! {
                                            p { "You are already a member of this room." }
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
