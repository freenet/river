mod nickname_field;

use crate::components::app::{CurrentRoom, Rooms};
use crate::util::get_current_room_data;
use common::state::member::{AuthorizedMember, MemberId, MembersV1};
use common::state::member_info::{AuthorizedMemberInfo, MemberInfoV1};
use dioxus::prelude::*;
use nickname_field::NicknameField;
use ed25519_dalek::VerifyingKey;

#[component]
pub fn MemberInfo(member_id: MemberId, is_active: Signal<bool>) -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_state = get_current_room_data(rooms, current_room);
    let members: Memo<Option<(MemberInfoV1, Option<MembersV1>)>> = use_memo(move || {
        current_room_state
            .read()
            .as_ref()
            .map(|room_state| (room_state.room_state.member_info.clone(), Some(room_state.room_state.members.clone())))
    });
    
    let member: Memo<Option<(Option<AuthorizedMember>, AuthorizedMemberInfo)>> = use_memo(move || {
        if let Some((member_info, members)) = members.read().as_ref() {
            if let Some(member_info) = member_info.member_info.iter().find(|mi| mi.member_info.member_id == member_id).cloned() {
                if let Some(members) = members {
                    if let Some(member) = members.members.iter().find(|member| member.member.owner_member_id == member_id).cloned() {
                        return Some((Some(member), member_info));
                    }
                }
                // If member is not found in MembersV1, it might be the room owner
                return Some((None, member_info));
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

    let is_owner = member.is_none();
    let current_room_read = current_room.read();
    let owner_key = current_room_read.owner_key;

    let invited_by = if is_owner {
        "N/A (Room Owner)".to_string()
    } else {
        let invited_by = member.as_ref().unwrap().member.invited_by;
        let members_read = members.read();
        members_read.as_ref().and_then(|(member_info, _)| {
            member_info.member_info.iter().find(|mi| mi.member_info.member_id == invited_by).map(|mi| mi.member_info.preferred_nickname.clone())
        }).unwrap_or_else(|| "Unknown".to_string())
    };

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
                    h1 { "Member Info" }
                    
                    NicknameField { member: member.clone(), member_info: member_info.clone() }
                    div { class: "field",
                        label { class: "label", "Member ID" }
                        div { class: "control",
                            input {
                                class: "input",
                                value: if is_owner {
                                    owner_key.map(|vk| MemberId::new(&vk).to_string()).unwrap_or_else(|| "Unknown".to_string())
                                } else {
                                    member.as_ref().unwrap().member.owner_member_id.to_string()
                                },
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
                    if is_owner {
                        div { class: "field",
                            label { class: "label", "Role" }
                            div { class: "control",
                                input {
                                    class: "input",
                                    value: "Room Owner",
                                    readonly: true
                                }
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
