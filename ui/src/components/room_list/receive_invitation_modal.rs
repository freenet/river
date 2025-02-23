use crate::room_data::Rooms;
use dioxus::prelude::*;
use crate::components::members::Invitation;

#[component]
pub fn ReceiveInvitationModal(invitation: Signal<Option<Invitation>>) -> Element {
    let rooms = use_context::<Signal<Rooms>>();

    rsx! {
        div {
            class: if invitation.read().is_some() { "modal is-active" } else { "modal" },
            div { class: "modal-background" }
            div { class: "modal-content",
                div { class: "box",
                    if let Some(inv) = invitation.read().as_ref() {
                        rsx! {
                            h3 { class: "title is-4", "Received Invitation" }
                            p { "Would you like to join this chat room?" }
                            div { class: "field is-grouped",
                                div { class: "control",
                                    button {
                                        class: "button is-primary",
                                        onclick: move |_| {
                                            // TODO: Handle accepting invitation
                                            invitation.set(None);
                                        },
                                        "Accept"
                                    }
                                }
                                div { class: "control",
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
            button {
                class: "modal-close is-large",
                onclick: move |_| invitation.set(None)
            }
        }
    }
}
