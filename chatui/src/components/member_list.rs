use crate::components::app::{CurrentRoom, Rooms};
use crate::components::member_info_modal::MemberInfoModal;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaUsers;
use dioxus_free_icons::Icon;
use common::state::member::MemberId;

#[component]
pub fn MemberList() -> Element {
    let rooms = use_context::<Signal<Rooms>>();
    let current_room = use_context::<Signal<CurrentRoom>>();
    let current_room_state = use_memo(move || match current_room.read().owner_key {
        Some(owner_key) => rooms
            .read()
            .map
            .get(&owner_key)
            .map(|rd| rd.room_state.clone()),
        None => None,
    });
    let members = use_memo(move || {
        current_room_state
            .read()
            .as_ref()
            .map(|room_state| (room_state.member_info.clone(), room_state.members.clone()))
    });

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

    let selected_member = use_state(|| None::<MemberId>);

    rsx! {
        aside { 
            class: "member-list",
            h2 { 
                class: "sidebar-header",
                Icon { icon: FaUsers, width: 20, height: 20 }
                span { "Members" }
            }
            ul { 
                class: "member-list-list",
                for (nickname, member_id) in members {
                    li {
                        key: "{member_id}",
                        class: "member-list-item",
                        onclick: move |_| selected_member.set(Some(member_id)),
                        "{nickname}"
                    }
                }
            }
            MemberInfoModal {
                member_id: selected_member.clone(),
                on_close: move |_| selected_member.set(None)
            }
        }
    }
}
