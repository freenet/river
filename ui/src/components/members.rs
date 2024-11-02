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
    let mut member_info_modal_signal = use_context::<Signal<MemberInfoModalSignal>>();
    let mut invite_modal_active = use_signal(|| false);

    let members = use_memo(move || {
        let current = current_room.read();
        let room_owner = current.owner_key.clone()?;
        let room_id = current.current_room_id?;
        let rooms = rooms.read();
        let room_state = rooms.map.get(&room_id)?.room_state.clone();
        
        let member_info = &room_state.member_info;
        let members = &room_state.members;
        
        let mut all_members = Vec::new();
        
        // Add owner first if they have member info
        if let Some(owner_info) = member_info.member_info.iter().find(|mi| mi.member_info.member_id == room_owner.into()) {
            let nickname = format!("{} 👑", owner_info.member_info.preferred_nickname);
            all_members.push((nickname, owner_info.member_info.member_id, true));
        }
        
        // Add regular members
        all_members.extend(members.members.iter().map(|member| {
            let nickname = member_info
                .member_info
                .iter()
                .find(|mi| mi.member_info.member_id == member.member.id())
                .map(|mi| mi.member_info.preferred_nickname.replace("👑", "💩"))
                .unwrap_or_else(|| "Unknown".to_string());
            (nickname, member.member.id(), false)
        }));
        
        Some(all_members)
    }).unwrap_or_default();

    let handle_member_click = move |member_id| {
        member_info_modal_signal.with_mut(|signal| {
            signal.member = Some(member_id);
        });
    };

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
                for (nickname, member_id, _is_owner) in members() {
                    li {
                        key: "{member_id}",
                        class: "member-list-item",
                        a {
                            href: "#",
                            onclick: move |_| handle_member_click(member_id),
                            "{nickname}"
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
