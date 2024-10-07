mod nickname_field;

use crate::components::app::{CurrentRoom, Rooms};
use crate::util::get_current_room_data;
use crate::global_context::UserInfoModals;
use common::state::member::MemberId;
use dioxus::prelude::*;

#[component]
pub fn MemberInfo(member_id: MemberId, is_active: Signal<bool>) -> Element {
    // Retrieve context signals
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_state = get_current_room_data(rooms, current_room);
    let mut user_info_modals = use_context::<Signal<UserInfoModals>>();

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
    let member = members_list.iter().find(|m| m.member.id() == member_id);

    // Determine if the member is the room owner
    let is_owner = member
        .as_ref()
        .map_or(false, |m| Some(m.member.id()) == owner_key.as_ref().map(MemberId::new));

    // Get the inviter's nickname and ID
    let (invited_by, inviter_id) = if let Some(m) = member {
        let inviter_id = m.member.invited_by;
        let inviter_nickname = member_info_list
            .iter()
            .find(|mi| mi.member_info.member_id == inviter_id)
            .map(|mi| mi.member_info.preferred_nickname.clone())
            .unwrap_or_else(|| "Unknown".to_string());
        (inviter_nickname, Some(inviter_id))
    } else {
        ("Unknown".to_string(), None)
    };

    // Get the list of members invited by this member
    let invited_members: Vec<(MemberId, String)> = members_list
        .iter()
        .filter(|m| m.member.invited_by == member_id)
        .map(|m| {
            let nickname = member_info_list
                .iter()
                .find(|mi| mi.member_info.member_id == m.member.id())
                .map(|mi| mi.member_info.preferred_nickname.clone())
                .unwrap_or_else(|| "Unknown".to_string());
            (m.member.id(), nickname)
        })
        .collect();

    // Function to open a member's modal
    let open_member_modal = move |member_id: MemberId| {
        is_active.set(false);
        user_info_modals.with_mut(|modals| {
            if let Some(member_modal) = modals.modals.get_mut(&member_id) {
                member_modal.set(true);
            }
        });
    };

    // Get the member ID string to display
    let member_id_str = member_id.to_string();

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
                    h1 { class: "title is-4 mb-3", "Member Info" }

                    if is_owner {
                        div {
                            class: "tag is-primary mb-3",
                            "Room Owner"
                        }
                    }

                    {
                        if let Some(member) = member {
                            if !is_owner {
                                rsx! {
                                    NicknameField {
                                        member: member.clone(),
                                        member_info: member_info.clone()
                                    }
                                }
                            } else {
                            rsx! {}
                        }
                        } else {
                            rsx! { 
                                div { 
                                    class: "notification is-warning",
                                    "Member information not available" 
                                } 
                            }
                        }
                    }

                    div {
                        class: "field",
                        label { class: "label is-medium", "Member ID" }
                        div {
                            class: "control",
                            input {
                                class: "input",
                                value: member_id_str,
                                readonly: true
                            }
                        }
                    }
                    if !is_owner {
                        rsx! {
                            div {
                                class: "field",
                                label { class: "label is-medium", "Invited by" }
                                div {
                                    class: "control",
                                    if let Some(inviter_id) = inviter_id {
                                        a {
                                            class: "input",
                                            style: "cursor: pointer; color: #3273dc; text-decoration: underline;",
                                            onclick: move |_| open_member_modal(inviter_id),
                                            "{invited_by}"
                                        }
                                    } else {
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

                    div {
                        class: "field",
                        label { class: "label is-medium", "Invited" }
                        div {
                            class: "control",
                            if invited_members.is_empty() {
                                input {
                                    class: "input",
                                    value: "None",
                                    readonly: true
                                }
                            } else {
                                div {
                                    class: "tags are-medium",
                                    invited_members.iter().map(|(id, nickname)| {
                                        rsx! {
                                            a {
                                                class: "tag is-link",
                                                style: "cursor: pointer;",
                                                onclick: move |_| open_member_modal(*id),
                                                "{nickname}"
                                            }
                                        }
                                    })
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
