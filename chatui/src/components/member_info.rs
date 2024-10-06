mod nickname_field;

use crate::components::app::{CurrentRoom, Rooms};
use crate::util::get_current_room_data;
use common::state::member::MemberId;
use dioxus::prelude::*;
use nickname_field::NicknameField;

#[component]
pub fn MemberInfo(member_id: MemberId, is_active: Signal<bool>) -> Element {
    // Retrieve context signals
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_state = get_current_room_data(rooms, current_room);

    // Read the current room state
    let current_room_state_read = current_room_state.read();
    let room_state = match current_room_state_read.as_ref() {
        Some(state) => state,
        None => {
            return rsx! { div { "Room state not available" } };
        }
    };

    // Extract member info and members list
    let member_info_list = &room_state.room_state.member_info.member_info;
    let members_list = &room_state.room_state.members.members;
    let owner_key = current_room.read().owner_key;

    // Find the AuthorizedMemberInfo for the given member_id
    let member_info = match member_info_list.iter().find(|mi| mi.member_info.member_id == member_id) {
        Some(mi) => mi,
        None => {
            return rsx! { div { "Member not found (this shouldn't happen)" } };
        }
    };

    // Try to find the AuthorizedMember for the given member_id
    let member = members_list.iter().find(|m| m.member.owner_member_id == member_id);

    // Determine if the member is the room owner
    let is_owner = member
        .as_ref()
        .map_or(false, |m| m.member.owner_member_id == m.member.invited_by);

    // Get the inviter's nickname
    let invited_by = if let Some(m) = member {
        if m.member.owner_member_id == m.member.invited_by {
            "N/A (Room Owner)".to_string()
        } else {
            let inviter_id = m.member.invited_by;
            member_info_list
                .iter()
                .find(|mi| mi.member_info.member_id == inviter_id)
                .map(|mi| mi.member_info.preferred_nickname.clone())
                .unwrap_or_else(|| "Unknown".to_string())
        }
    } else {
        "Unknown".to_string()
    };

    // Get the member ID string to display
    let member_id_str = member
        .map(|m| m.member.owner_member_id.to_string())
        .unwrap_or_else(|| "Unknown".to_string());

    rsx! {
        div {
            class: if *is_active.read() { "modal is-active" } else { "modal" },
            div {
                class: "modal-background",
                onclick: move |_| {
                    is_active.set(false);
                }
            },
            div {
                class: "modal-content",
                div {
                    class: "box",
                    h1 { "Member Info" }

                    if let Some(member) = member {
                        if !is_owner {
                            rsx! {
                                NicknameField {
                                    member: member.clone(),
                                    member_info: member_info.clone()
                                }
                            }
                        } else {
                            rsx! { div { "Room Owner" } }
                        }
                    } else {
                        rsx! { div { "Member information not available" } }
                    }

                    div {
                        class: "field",
                        label { class: "label", "Member ID" }
                        div {
                            class: "control",
                            input {
                                class: "input",
                                value: member_id_str,
                                readonly: true
                            }
                        }
                    }
                    div {
                        class: "field",
                        label { class: "label", "Invited by" }
                        div {
                            class: "control",
                            input {
                                class: "input",
                                value: invited_by,
                                readonly: true
                            }
                        }
                    }
                    if is_owner {
                        div {
                            class: "field",
                            label { class: "label", "Role" }
                            div {
                                class: "control",
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
            button {
                class: "modal-close is-large",
                onclick: move |_| {
                    is_active.set(false);
                }
            }
        }
    }
}
