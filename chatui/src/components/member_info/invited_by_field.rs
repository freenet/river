use dioxus::prelude::*;
use common::state::member::MemberId;
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
                {
                    if inviter_id.is_some() {
                        rsx! {
                            a {
                                class: "input",
                                style: "cursor: pointer; color: #3273dc; text-decoration: underline;",
                                onclick: open_inviter_modal,
                                "{invited_by}"
                            }
                        }
                    } else {
                        rsx! {
                            input {
                                class: "input",
                                value: "{invited_by}",
                                readonly: true
                            }
                        }
                    }
                }
            }
        }
    }
}
