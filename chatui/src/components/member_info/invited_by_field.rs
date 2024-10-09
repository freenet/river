use dioxus::prelude::*;
use common::room_state::member::MemberId;
use crate::global_context::UserInfoModals;

#[component]
pub fn InvitedByField(
    invited_by: String,
    inviter_id: Option<MemberId>,
    is_active: Signal<bool>
) -> Element {
    let mut user_info_modals = use_context::<Signal<UserInfoModals>>();

    let open_inviter_modal = move |_| {
        if let Some(inviter_id) = inviter_id {
            is_active.set(false);
            user_info_modals.with_mut(|modals| {
                if let Some(inviter_modal) = modals.modals.get_mut(&inviter_id) {
                    inviter_modal.set(true);
                }
            });
        }
    };

    rsx! {
        div {
            class: "field",
            label { class: "label is-medium", "Invited by" }
            div {
                class: "control",
                div {
                    class: "input",
                    style: "display: flex; align-items: center; height: auto; min-height: 2.5em;",
                    {
                        if inviter_id.is_some() {
                            rsx! {
                                span {
                                    class: "clickable-username",
                                    style: "cursor: pointer; display: inline-block;",
                                    onclick: open_inviter_modal,
                                    "{invited_by}"
                                }
                            }
                        } else {
                            rsx! {
                                span {
                                    "{invited_by}"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
