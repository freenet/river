use dioxus::prelude::*;

#[component]
pub fn NotMemberNotification() -> Element {
    rsx! {
        div { class: "notification is-info",
            "You are not a member of this room. Join the room to send messages."
        }
    }
}
