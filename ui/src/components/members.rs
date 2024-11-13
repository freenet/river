use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{FaUserPlus, FaUsers};
use dioxus_free_icons::Icon;
use common::room_state::member::MemberId;
use common::room_state::ChatRoomParametersV1;
use std::collections::HashSet;
use ed25519_dalek::VerifyingKey;
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
        let rooms = rooms.read();
        let room_data = rooms.map.get(&room_owner)?;
        let room_state = room_data.room_state.clone();
        let self_member_id : MemberId = room_data.self_sk.verifying_key().into();
        
        let member_info = &room_state.member_info;
        let members = &room_state.members;
        
        fn get_member_labels(
            member_id: MemberId, 
            room_owner: &VerifyingKey, 
            self_member_id: MemberId,
            members: &common::room_state::member::MembersV1,
            params: &common::room_state::ChatRoomParametersV1
        ) -> HashSet<(&'static str, &'static str)> {
            let mut labels = HashSet::new();
            if member_id == room_owner.into() {
                labels.insert(("👑 ", "Room Owner")); // Owner label with space and tooltip
            }
            if member_id == self_member_id {
                labels.insert(("⭐", "You")); // Self label with tooltip
            }

            // Skip relationship labels for self and owner
            if member_id != self_member_id && member_id != room_owner.into() {
                // Find the member in the members list
                if let Some(member) = members.members.iter().find(|m| m.member.id() == member_id) {
                    // Check if this member was invited by the current user
                    if member.member.invited_by == self_member_id {
                        labels.insert(("🔑", "You invited this member"));
                    }
                    // Check if the current user was invited by this member
                    if let Some(self_member) = members.members.iter().find(|m| m.member.id() == self_member_id) {
                        if self_member.member.invited_by == member.member.id() {
                            labels.insert(("🎪", "Invited you to the room"));
                        }
                    }
                }
            }
            labels
        }

        let mut all_members = Vec::new();
        
        // Process owner first
        let owner_id: MemberId = room_owner.into();
        let params = ChatRoomParametersV1 { owner: room_owner.clone() };
        let owner_labels = get_member_labels(owner_id, &room_owner, self_member_id, &members, &params);
        let owner_nickname = member_info
            .member_info
            .iter()
            .find(|mi| mi.member_info.member_id == owner_id)
            .map(|mi| mi.member_info.preferred_nickname.clone())
            .unwrap_or_else(|| "Unknown".to_string());

        let owner_display_name = if !owner_labels.is_empty() {
            let label_spans = owner_labels.into_iter()
                .map(|(emoji, tooltip)| format!(r#"<span title="{}">{}</span>"#, tooltip, emoji))
                .collect::<Vec<_>>()
                .join(" ");
            format!("{} {}", owner_nickname, label_spans)
        } else {
            owner_nickname
        };
        all_members.push((owner_display_name, owner_id));

        // Process other members
        for member in members.members.iter() {
            let member_id = member.member.id();
            // Skip owner since we already processed them
            if member_id == owner_id {
                continue;
            }
            
            let labels = get_member_labels(member_id, &room_owner, self_member_id, &members, &params);
            let nickname = member_info
                .member_info
                .iter()
                .find(|mi| mi.member_info.member_id == member_id)
                .map(|mi| mi.member_info.preferred_nickname.clone())
                .unwrap_or_else(|| "Unknown".to_string());

            let display_name = if !labels.is_empty() {
                let label_spans = labels.into_iter()
                    .map(|(emoji, tooltip)| format!(r#"<span title="{}">{}</span>"#, tooltip, emoji))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{} {}", nickname, label_spans)
            } else {
                nickname
            };
            
            all_members.push((display_name, member_id));
        }
        
        Some(all_members)
    })().unwrap_or_default();

    let mut handle_member_click = move |member_id| {
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
            ul { class: "member-list-list",
                for (display_name, member_id) in members {
                    li {
                        key: "{member_id}",
                        class: "member-list-item",
                        a {
                            href: "#",
                            onclick: move |_| handle_member_click(member_id),
                            dangerous_inner_html: "{display_name}"
                        }
                    }
                }
            }
            div { class: "member-actions",
                button {
                    class: "invite",
                    onclick: move |_| invite_modal_active.set(true),
                    Icon { icon: FaUserPlus, width: 16, height: 16 }
                    span { "Invite Member" }
                }
            }
        }
        InviteMemberModal {
            is_active: invite_modal_active
        }
    }
}
