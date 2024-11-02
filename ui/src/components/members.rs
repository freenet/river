use crate::util::use_current_room_data;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{FaUserPlus, FaUsers};
use dioxus_free_icons::Icon;
use crate::components::app::MemberInfoModalSignal;
use crate::room_data::{CurrentRoom, Rooms};

mod invite_member_modal;
pub mod member_info_modal;

use self::invite_member_modal::InviteMemberModal;

#[component]
pub fn MemberList() -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let room_owner = current_room.read().owner_key?.clone();
    let current_room_state = use_current_room_data(rooms, current_room);
    let members = use_memo(move || {
        current_room_state
            .read()
            .as_ref()
            .map(|room_state| (room_state.room_state.member_info.clone(), room_state.room_state.members.clone()))
    });

    let mut member_info_modal_signal = use_context::<Signal<MemberInfoModalSignal>>();

    // Convert members to Vector of (nickname, member_id)
    let members = match members() {
        Some((member_info, members)) => {
            let mut all_members = Vec::new();
            
            // Add owner first if they have member info
            if let Some(owner_info) = member_info.member_info.iter().find(|mi| mi.member_info.member_id == room_owner.into()) {
                all_members.push((owner_info.member_info.preferred_nickname.clone(), owner_info.member_info.member_id, true));
            }
            
            // Add regular members
            all_members.extend(members.members.iter().map(|member| {
                let nickname = member_info
                    .member_info
                    .iter()
                    .find(|mi| mi.member_info.member_id == member.member.id())
                    .map(|mi| mi.member_info.preferred_nickname.clone())
                    .unwrap_or_else(|| "Unknown".to_string());
                (nickname, member.member.id(), false)
            }));
            
            all_members
        },
        None => Vec::new(),
    };

    let mut invite_modal_active = use_signal(|| false);

    rsx! {
        aside { class: "member-list",
            h2 { class: "sidebar-header",
                Icon { icon: FaUsers, width: 20, height: 20 }
                span { "Members" }
            }
            div { class: "invite-member-button",
                button {
                    class: "button is-small custom-button",
                    onclick: move |_| invite_modal_active.set(true),
                    Icon { class: "icon-margin-right", icon: FaUserPlus, width: 14, height: 14 }
                    span { "Invite" }
                }
            }
            ul { class: "member-list-list",
                for (nickname, member_id, is_owner) in members {
                    {
                        rsx! {
                            li {
                                key: "{member_id}",
                                class: "member-list-item",
                                a {
                                    href: "#",
                                    onclick: move |_| {
                                        member_info_modal_signal.write().member = Some(member_id);
                                    },
                                    format!("{}{}", if is_owner { "👑 " } else { "" }, nickname),
                                }
                            }
                        }
                    }
                }
            }
        }
        InviteMemberModal {
            is_active: invite_modal_active
        }
    }
}
