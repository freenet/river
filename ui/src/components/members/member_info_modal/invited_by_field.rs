use crate::components::app::MEMBER_INFO_MODAL;
use dioxus::prelude::*;
use river_core::room_state::member::MemberId;

#[component]
pub fn InvitedByField(invited_by: String, inviter_id: Option<MemberId>) -> Element {
    rsx! {
        div {
            class: "mb-4",
            label { class: "block text-sm font-medium text-text-muted mb-2", "Invited by" }
            div {
                class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text flex items-center min-h-[2.5rem]",
                {
                    if inviter_id.is_some() {
                        rsx! {
                            span {
                                class: "text-accent hover:text-accent-hover cursor-pointer transition-colors",
                                onclick: move |_event| {
                                    MEMBER_INFO_MODAL.with_mut(|uim| {
                                        uim.member = inviter_id;
                                    })
                                },
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
