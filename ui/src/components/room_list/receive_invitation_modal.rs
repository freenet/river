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
            class: "modal",
            "Test Modal"
        }
    }
}
