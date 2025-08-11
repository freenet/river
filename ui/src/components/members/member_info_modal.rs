mod ban_button;
mod invited_by_field;
mod nickname_field;

use crate::components::app::{CURRENT_ROOM, MEMBER_INFO_MODAL, ROOMS};
use crate::components::members::member_info_modal::ban_button::BanButton;
use crate::components::members::member_info_modal::invited_by_field::InvitedByField;
use crate::components::members::member_info_modal::nickname_field::NicknameField;
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use river_core::room_state::member::MemberId;
use river_core::room_state::ChatRoomParametersV1;

#[component]
pub fn MemberInfoModal() -> Element {
    // Memos
    let current_room_data_signal = use_memo(move || {
        CURRENT_ROOM
            .read()
            .owner_key
            .as_ref()
            .and_then(|key| ROOMS.read().map.get(key).cloned())
    });
    let self_member_id: Memo<Option<MemberId>> = use_memo(move || {
        ROOMS
            .read()
            .map
            .get(&CURRENT_ROOM.read().owner_key?)
            .map(|r| MemberId::from(&r.self_sk.verifying_key()))
    });

    // Memoized values
    let owner_key_signal = use_memo(move || CURRENT_ROOM.read().owner_key);

    // Effect to handle closing the modal based on a specific condition

    // Event handlers
    let handle_close_modal = {
        move |_| {
            MEMBER_INFO_MODAL.with_mut(|signal| {
                signal.member = None;
            });
        }
    };

    // Room state - create a longer-lived binding
    let current_room_data = current_room_data_signal.read();
    let room_state = match current_room_data.as_ref() {
        Some(state) => state,
        None => {
            return rsx! { div { "Room state not available" } };
        }
    };

    // Extract member info and members list
    let member_info_list = &room_state.room_state.member_info.member_info;
    let members_list = &room_state.room_state.members.members;

    let modal_content = if let Some(member_id) = MEMBER_INFO_MODAL.read().member {
        // Find the AuthorizedMemberInfo for the given member_id
        let member_info = match member_info_list
            .iter()
            .find(|mi| mi.member_info.member_id == member_id)
        {
            Some(mi) => mi,
            None => {
                error!("Member info not found for member {member_id}");
                return rsx! {
                    div {
                        class: "notification is-danger",
                        "Member information is missing or corrupted"
                    }
                };
            }
        };

        // Try to find the AuthorizedMember for the given member_id
        let member = members_list.iter().find(|m| m.member.id() == member_id);

        // Determine if the member is the room owner
        let is_owner = owner_key_signal
            .as_ref()
            .is_some_and(|k| MemberId::from(&*k) == member_id);

        // Only show error if member isn't found AND isn't the owner
        if member.is_none() && !is_owner {
            error!("Member {member_id} not found in members list and is not owner");
            return rsx! {
                div {
                    class: "notification is-danger",
                    "Member not found in room members list"
                }
            };
        }

        // Determine if the member is downstream of the current user in the invite chain
        let is_downstream = member
            .and_then(|m| {
                owner_key_signal.as_ref().map(|owner| {
                    let params = ChatRoomParametersV1 {
                        owner: *owner,
                    };
                    // Get the invite chain for this member
                    let invite_chain = room_state.room_state.members.get_invite_chain(m, &params);

                    let self_member_id =
                        self_member_id().expect("Self member ID should be available");
                    // Member is downstream if:
                    // 1. They were invited by owner (empty chain) and current user is owner, or
                    // 2. Current user appears in their invite chain
                    invite_chain.is_ok_and(|chain| {
                        chain.is_empty()
                            && self_member_id == CURRENT_ROOM.read().owner_id().unwrap()
                            || chain.iter().any(|m| m.member.id() == self_member_id)
                    })
                })
            })
            .unwrap_or(false);

        info!(
            "Rendering MemberInfoModal for member_id: {:?} is_owner: {:?} is_downstream: {:?}",
            member_id, is_owner, is_downstream
        );

        // Get the inviter's nickname and ID
        let (invited_by, inviter_id) = match (member, is_owner) {
            (_, true) => ("N/A (Room Owner)".to_string(), None),
            (Some(m), false) => {
                let inviter_id = m.member.invited_by;
                let nickname = member_info_list
                    .iter()
                    .find(|mi| mi.member_info.member_id == inviter_id)
                    .map(|mi| mi.member_info.preferred_nickname.clone())
                    .unwrap_or_else(|| "Unknown".to_string());
                (nickname, Some(inviter_id))
            }
            _ => ("Unknown".to_string(), None),
        };

        // Get the member ID string to display
        let member_id_str = member_id.to_string();

        rsx! {
            div {
                class: "modal is-active",
                div {
                    class: "modal-background",
                    onclick: handle_close_modal
                }
                div {
                    class: "modal-content",
                    div {
                        class: "box",
                        h1 { class: "title is-4 mb-3", "Member Info" }

                        // Show tags for owner, self, and relationships
                        if is_owner {
                            div {
                                class: "tag is-link mb-3 mr-2",
                                span { class: "tag-emoji", "👑" } " " "Room Owner"
                            }
                        }
                        if member_id == self_member_id.unwrap() {
                            div {
                                class: "tag is-info mb-3 mr-2",
                                span { class: "tag-emoji", "⭐" } " " "You"
                            }
                        }
                        if is_downstream {
                            div {
                                class: "tag is-success mb-3 mr-2",
                                span { class: "tag-emoji", "🔑" } " " "Invited by You"
                            }
                        }
                        // Check if this member invited the current user
                        if let Some(self_member) = members_list.iter().find(|m| m.member.id() == self_member_id.unwrap()) {
                            if self_member.member.invited_by == member_id {
                                div {
                                    class: "tag is-warning mb-3",
                                    span { class: "tag-emoji", "🎪" } " " "Invited You"
                                }
                            }
                        }

                        NicknameField {
                            member_info: member_info.clone()
                        }

                        div {
                            class: "field",
                            label { class: "label is-medium", "Member ID" }
                            div {
                                class: "control",
                                input {
                                    class: "input",
                                    value: "{member_id_str}",
                                    readonly: true
                                }
                            }
                        }

                        if !is_owner {
                            InvitedByField {
                                invited_by: invited_by.clone(),
                                inviter_id: inviter_id,
                            }

                            // Check if member is downstream of current user
                            {
                    let _current_user_id = {
                                    current_room_data_signal.read().as_ref()
                                        .map(|r| r.self_sk.verifying_key())
                                        .map(|k| MemberId::from(&k))
                                };

                                rsx! {
                                    BanButton {
                                        member_to_ban: member_id,
                                        is_downstream: is_downstream,
                                        nickname: member_info.member_info.preferred_nickname.clone()
                                    }
                                    ""
                                }
                            }

                        }
                    }
                }
                button {
                    class: "modal-close is-large",
                    onclick: handle_close_modal
                }
            }
        }
    } else {
        rsx! {}
    };

    modal_content
}
