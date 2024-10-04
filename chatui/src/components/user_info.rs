use dioxus::prelude::*;
use common::state::member::MemberId;

#[component]
pub fn UserInfo(member_id: MemberId, is_active: Signal<bool>) -> Element {
    rsx! {
        div {
            class: if *is_active.read() { "modal is-active" } else { "modal" },
            div { class: "modal-background",
                    onclick: move |_| {
                    is_active.set(false);
                }
            },
            div { class: "modal-content",
                div { class: "box",
                    p { "Member ID: {member_id}" }
                }
            },
            button { class: "modal-close is-large",
                onclick: move |_| {
                    is_active.set(false);
                }
            }
        }
    }
}