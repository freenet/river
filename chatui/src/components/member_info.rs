use dioxus::prelude::*;
use common::state::member::{AuthorizedMember, MemberId, MembersV1};
use common::state::member_info::{AuthorizedMemberInfo, MemberInfoV1};
use crate::components::app::{CurrentRoom, Rooms};
use crate::global_context::UserInfoModals;
use crate::util::get_current_room_state;

#[component]
fn NicknameField(nickname: String) -> Element {
    rsx! {
        h1 { "Member Info" }
        div { class: "field",
            label { class: "label", "Nickname" }
            div { class: "control",
                input {
                    class: "input",
                    value: nickname,
                    readonly: true
                }
            }
        }
    }
}

#[component]
pub fn MemberInfo(member_id: MemberId, is_active: Signal<bool>) -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_state = get_current_room_state(rooms, current_room);
    let members: Memo<Option<(MemberInfoV1, MembersV1)>> = use_memo(move || {
        current_room_state
            .read()
            .as_ref()
            .map(|room_state| (room_state.room_state.member_info.clone(), room_state.room_state.members.clone()))
    });
    let member: Memo<Option<(AuthorizedMember, AuthorizedMemberInfo)>> = use_memo(move || {
        if let Some((member_info, members)) = members.read().as_ref() {
            if let Some(member) = members.members.iter().find(|member| member.member.owner_member_id == member_id).cloned() {
                if let Some(member_info) = member_info.member_info.iter().find(|mi| mi.member_info.member_id == member_id).cloned() {
                    return Some((member, member_info));
                }
            }
        }
        None
    });

    let member_read = member.read();
    if member_read.as_ref().is_none() {
        return rsx! {
            div { "Member not found (this shouldn't happen)" }
        };
    }

    let (member, member_info) = member_read.as_ref().unwrap();

    let invited_by = member.member.invited_by;
    let members_read = members.read();
    let invited_by = members_read.as_ref().and_then(|(member_info, _)| {
        member_info.member_info.iter().find(|mi| mi.member_info.member_id == invited_by).map(|mi| mi.member_info.preferred_nickname.clone())
    }).unwrap_or_else(|| "Unknown".to_string());

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
                    // Show the member id, the member's nickname, and the member's public key - using bulma form elements
                    NicknameField { nickname: member_info.member_info.preferred_nickname.clone() }
                    div { class: "field",
                        label { class: "label", "Member ID" }
                        div { class: "control",
                            input {
                                class: "input",
                                value: member.member.owner_member_id.to_string(),
                                readonly: true
                            }
                        }
                    }
                    div { class: "field",
                        label { class: "label", "Invited by" }
                        div { class: "control",
                            input {
                                class: "input",
                                value: invited_by,
                                readonly: true
                            }
                        }
                    }
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
