use crate::components::app::{MemberInfoModalSignal, MEMBER_INFO_MODAL};
use dioxus::prelude::*;
use river_common::room_state::member::MemberId;

#[component]
pub fn InvitedByField(invited_by: String, inviter_id: Option<MemberId>) -> Element {
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
}
