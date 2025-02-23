use crate::room_data::Rooms;
use dioxus::prelude::*;
use crate::components::members::Invitation;

#[component]
pub fn ReceiveInvitationModal(
    is_active: Signal<bool>,
    invitation: Signal<Option<Invitation>>,
) -> Element {
    let rooms = use_context::<Signal<Rooms>>();

    rsx! {
        div {
            class: if *is_active.read() { "modal is-active" } else { "modal" },
            div {
                class: "modal-background",
                onclick: move |_| {
                    is_active.set(false);
                }
            }
            div {
                class: "modal-content",
                div {
                    class: "box",
                    if let Some(invitation) = invitation.read().as_ref() {
                        rsx! {
                            h3 { class: "title is-4", "Room Invitation Received" }
                            
                            div { class: "message is-info",
                                div { class: "message-body",
                                    "You've received an invitation to join a chat room."
                                }
                            }

                            div { class: "field is-grouped",
                                div { class: "control",
                                    button {
                                        class: "button is-primary",
                                        onclick: move |_| {
                                            // TODO: Handle accepting invitation
                                            is_active.set(false);
                                        },
                                        "Accept Invitation"
                                    }
                                }
                                div { class: "control",
                                    button {
                                        class: "button",
                                        onclick: move |_| is_active.set(false),
                                        "Decline"
                                    }
                                }
                            }
                        }
                    } else {
                        rsx! {
                            h3 { class: "title is-4", "Invalid Invitation" }
                            p { class: "has-text-danger", "The invitation code could not be processed." }
                            button {
                                class: "button",
                                onclick: move |_| is_active.set(false),
                                "Close"
                            }
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
