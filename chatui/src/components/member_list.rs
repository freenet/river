use crate::components::app::{CurrentRoom, Rooms};
use crate::util::get_current_room_state;
use crate::global_context::UserInfoModals;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaUsers;
use dioxus_free_icons::Icon;
use crate::components::member_info::MemberInfo;

#[component]
pub fn MemberList() -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_state = get_current_room_state(rooms, current_room);
    let members = use_memo(move || {
        current_room_state
            .read()
            .as_ref()
            .map(|room_state| (room_state.room_state.member_info.clone(), room_state.room_state.members.clone()))
    });

    let mut user_info_modals = use_context::<Signal<UserInfoModals>>();

    // Convert members to Vector of (nickname, member_id)
    let members = match members() {
        Some((member_info, members)) => members
            .members
            .iter()
            .map(|member| {
                let nickname = member_info
                    .member_info
                    .iter()
                    .find(|mi| mi.member_info.member_id == member.member.owner_member_id)
                    .map(|mi| mi.member_info.preferred_nickname.clone())
                    .unwrap_or_else(|| "Unknown".to_string());
                (nickname, member.member.owner_member_id)
            })
            .collect::<Vec<_>>(),
        None => Vec::new(),
    };

    rsx! {
        aside { class: "member-list",
            h2 { class: "sidebar-header",
                Icon { icon: FaUsers, width: 20, height: 20 }
                span { "Members" }
            }
            ul { class: "member-list-list",
                for (nickname, member_id) in members {
                    {
                    let mut is_active = user_info_modals.with_mut(|modals| {
                        modals.modals.entry(member_id).or_insert_with(|| use_signal(|| false)).clone()
                    });
                    rsx! {
                        MemberInfo {
                            member_id,
                            is_active: is_active.clone(),
                        }
                        li {
                            key: "{member_id}",
                            class: "member-list-item",
                            a {
                                href: "#",
                                onclick: move |_| {
                                    is_active.set(true);
                                },
                                "{nickname}"
                            }
                        }
                    }
                    }
                }
            }
        }
    }
}
