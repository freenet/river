mod nickname_field;
mod invited_by_field;

pub use crate::room_data::{CurrentRoom, Rooms};
use crate::util::get_current_room_data;
use common::room_state::member::MemberId;
use dioxus::prelude::*;
use crate::components::app::MemberInfoModalSignal;
use crate::components::members::member_info_modal::nickname_field::NicknameField;
use crate::components::members::member_info_modal::invited_by_field::InvitedByField;

#[component]
pub fn MemberInfoModal() -> Element {
    // Retrieve context signals
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_state = get_current_room_data(rooms, current_room);
    let mut member_info_modal_signal = use_context::<Signal<MemberInfoModalSignal>>();

    // Read the current room room_state
    let current_room_state_read = current_room_state.read();
    let room_state = match current_room_state_read.as_ref() {
        Some(state) => state,
        None => {
            return rsx! { div { "Room room_state not available" } };
        }
    };

    // Extract member info and members list
    let member_info_list = &room_state.room_state.member_info.member_info;
    let members_list = &room_state.room_state.members.members;
    let owner_key = current_room.read().owner_key;

    if let Some(member_id) = member_info_modal_signal.clone().read().member {

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
            if is_owner {
                ("N/A (Room Owner)".to_string(), None)
            } else {
                let inviter_id = m.member.invited_by;
                let inviter_nickname = member_info_list
                    .iter()
                    .find(|mi| mi.member_info.member_id == inviter_id)
                    .map(|mi| mi.member_info.preferred_nickname.clone())
                    .unwrap_or_else(|| "Unknown".to_string());
                (inviter_nickname, Some(inviter_id))
            }
        } else {
            ("Unknown".to_string(), None)
        };

        // Get the member ID string to display
        let member_id_str = member_id.to_string();

        rsx! {
            div {
                class: "modal is-active",
                div {
                    class: "modal-background",
                    onclick: move |_| {
                        member_info_modal_signal.with_mut(|s| {
                            s.member = None;
                        });
                    }
                }
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

                        if let Some(member) = member {
                            NicknameField {
                                member: member.clone(),
                                member_info: member_info.clone()
                            }
                        } else {
                            div {
                                class: "notification is-warning",
                                "Member information not available"
                            }
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
                        }
                    }
                }
                button {
                    class: "modal-close is-large",
                    onclick: move |_| {
                        member_info_modal_signal.with_mut(|mim| {
                            mim.member = None;
                        });                    }
                }
            }
        }
    } else {
        rsx! {}
    }
}
