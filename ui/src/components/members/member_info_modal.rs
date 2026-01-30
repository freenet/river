mod ban_button;
mod invited_by_field;
mod nickname_field;

use crate::components::app::{CURRENT_ROOM, MEMBER_INFO_MODAL, ROOMS};
use crate::components::members::member_info_modal::ban_button::BanButton;
use crate::components::members::member_info_modal::invited_by_field::InvitedByField;
use crate::components::members::member_info_modal::nickname_field::NicknameField;
use crate::util::ecies::unseal_bytes_with_secrets;
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
                        class: "p-4 bg-red-500/10 border border-red-500/20 rounded-lg text-red-400",
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
                    class: "p-4 bg-red-500/10 border border-red-500/20 rounded-lg text-red-400",
                    "Member not found in room members list"
                }
            };
        }

        // Determine if the member is downstream of the current user in the invite chain
        let is_downstream = member
            .and_then(|m| {
                owner_key_signal.as_ref().map(|owner| {
                    let params = ChatRoomParametersV1 { owner: *owner };
                    // Get the invite chain for this member
                    let invite_chain = room_state.room_state.members.get_invite_chain(m, &params);

                    let self_member_id =
                        self_member_id().expect("Self member ID should be available");
                    // Member is downstream if:
                    // 1. Current user is owner (owner can ban anyone), or
                    // 2. Current user appears in their invite chain (upstream of target)
                    invite_chain.is_ok_and(|chain| {
                        self_member_id == CURRENT_ROOM.read().owner_id().unwrap()
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
                    .map(|mi| {
                        match unseal_bytes_with_secrets(&mi.member_info.preferred_nickname, &room_state.secrets) {
                            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                            Err(_) => mi.member_info.preferred_nickname.to_string_lossy(),
                        }
                    })
                    .unwrap_or_else(|| "Unknown".to_string());
                (nickname, Some(inviter_id))
            }
            _ => ("Unknown".to_string(), None),
        };

        // Get the member ID string to display
        let member_id_str = member_id.to_string();

        rsx! {
            // Modal backdrop
            div {
                class: "fixed inset-0 z-50 flex items-center justify-center",
                // Overlay
                div {
                    class: "absolute inset-0 bg-black/50",
                    onclick: handle_close_modal
                }
                // Modal content
                div {
                    class: "relative z-10 w-full max-w-md mx-4 bg-panel rounded-xl shadow-xl border border-border",
                    div {
                        class: "p-6",
                        h1 { class: "text-xl font-semibold text-text mb-4", "Member Info" }

                        // Show tags for owner, self, and relationships
                        div { class: "flex flex-wrap gap-2 mb-4",
                            if is_owner {
                                span {
                                    class: "inline-flex items-center px-2.5 py-0.5 rounded-full text-sm font-medium bg-blue-500/20 text-blue-400",
                                    "👑 Room Owner"
                                }
                            }
                            if member_id == self_member_id.unwrap() {
                                span {
                                    class: "inline-flex items-center px-2.5 py-0.5 rounded-full text-sm font-medium bg-cyan-500/20 text-cyan-400",
                                    "⭐ You"
                                }
                            }
                            if is_downstream {
                                span {
                                    class: "inline-flex items-center px-2.5 py-0.5 rounded-full text-sm font-medium bg-green-500/20 text-green-400",
                                    "🔑 Invited by You"
                                }
                            }
                            // Check if this member invited the current user
                            if let Some(self_member) = members_list.iter().find(|m| m.member.id() == self_member_id.unwrap()) {
                                if self_member.member.invited_by == member_id {
                                    span {
                                        class: "inline-flex items-center px-2.5 py-0.5 rounded-full text-sm font-medium bg-yellow-500/20 text-yellow-400",
                                        "🎪 Invited You"
                                    }
                                }
                            }
                        }

                        NicknameField {
                            member_info: member_info.clone()
                        }

                        div {
                            class: "mb-4",
                            label { class: "block text-sm font-medium text-text-muted mb-2", "Member ID" }
                            input {
                                class: "w-full px-3 py-2 bg-surface border border-border rounded-lg text-text font-mono text-sm",
                                value: "{member_id_str}",
                                readonly: true
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
                    // Close button
                    button {
                        class: "absolute top-3 right-3 p-1 text-text-muted hover:text-text transition-colors",
                        onclick: handle_close_modal,
                        "✕"
                    }
                }
            }
        }
    } else {
        rsx! {}
    };

    modal_content
}
