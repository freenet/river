use crate::components::members::Invitation;
use crate::room_data::Rooms;
use dioxus::prelude::*;

#[component]
pub fn ReceiveInvitationModal(invitation: Signal<Option<Invitation>>) -> Element {
    let rooms = use_context::<Signal<Rooms>>();

    rsx! {
            div {
                class: if invitation.read().is_some() { "modal is-active" } else { "modal" },
                div { class: "modal-background",
                    onclick: move |_| invitation.set(None)
                }
                div { class: "modal-content",
                    div { class: "box",
                    
                        h1 { class: "title", "Invitation Received" }
                        if rooms.read().map.contains_key(&invitation.read().as_ref().unwrap().room.into()) {
                            p { "You are already a member of this room." }
                        } else {
                            p { "You have been invited to join a new room." }
                            p { "Would you like to accept the invitation?" }
                            button {
                                class: "button is-primary",
                                onclick: move |_| {
                                    // Handle accepting the invitation
                                    // This is where you would add the logic to accept the invitation
                                    invitation.set(None);
                                },
                                "Accept"
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
}
