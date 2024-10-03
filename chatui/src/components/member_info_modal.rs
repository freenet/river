use common::state::member::MemberId;
use dioxus::prelude::*;

#[component]
pub fn MemberInfoModal(
    member_id: UseState<Option<MemberId>>,
    on_close: EventHandler<()>,
) -> Element {
    let is_active = member_id.get().is_some();

    rsx! {
        div { class: "modal {if is_active { \"is-active\" } else { \"\" }}",
            div { class: "modal-background", onclick: move |_| on_close.call(()) }
            div { class: "modal-card",
                header { class: "modal-card-head",
                    p { class: "modal-card-title", "Member Information" }
                    button { class: "delete", "aria-label": "close", onclick: move |_| on_close.call(()) }
                }
                section { class: "modal-card-body",
                    // TODO: Add member information and edit fields here
                    p { "Member ID: {member_id.get().map_or_else(|| \"None\".to_string(), |id| id.to_string())}" }
                }
                footer { class: "modal-card-foot",
                    button { class: "button is-success", "Save changes" }
                    button { class: "button", onclick: move |_| on_close.call(()), "Cancel" }
                }
            }
        }
    }
}
