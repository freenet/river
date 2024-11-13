use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{FaUserPlus, FaUsers};
use dioxus_free_icons::Icon;
use common::room_state::member::MemberId;
use common::room_state::member::MembersV1;
use common::room_state::ChatRoomParametersV1;
use ed25519_dalek::VerifyingKey;
use crate::components::app::MemberInfoModalSignal;
use crate::room_data::{CurrentRoom, Rooms};

mod invite_member_modal;
pub mod member_info_modal;

use self::invite_member_modal::InviteMemberModal;

// Helper struct to store member display info
struct MemberDisplay {
    nickname: String,
    member_id: MemberId,
    is_owner: bool,
    is_self: bool,
    invited_you: bool,
    invited_by_you: bool,
}

// Helper functions to check member relationships
fn is_member_owner(member_id: MemberId, owner_id: MemberId) -> bool {
    member_id == owner_id
}

fn is_member_self(member_id: MemberId, self_id: MemberId) -> bool {
    member_id == self_id
}

fn did_member_invite_you(member_id: MemberId, members: &MembersV1, self_id: MemberId, params: &ChatRoomParametersV1) -> bool {
    members.is_inviter_of(member_id, self_id, params)
}

fn did_you_invite_member(member_id: MemberId, members: &MembersV1, self_id: MemberId) -> bool {
    members.members.iter()
        .find(|m| m.member.id() == member_id)
        .map(|m| m.member.invited_by == self_id)
        .unwrap_or(false)
}

// Function to format member display name with tags
fn format_member_display(member: &MemberDisplay) -> String {
    let mut tags = Vec::new();
    
    if member.is_owner {
        tags.push("👑");
    }
    if member.is_self {
        tags.push("⭐");
    }
    if member.invited_by_you {
        tags.push("🔑");
    }
    if member.invited_you {
        tags.push("🎪");
    }

    if tags.is_empty() {
        member.nickname.clone()
    } else {
        format!("{} {}", member.nickname, tags.join(" "))
    }
}

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
        let self_member_id: MemberId = room_data.self_sk.verifying_key().into();
        let owner_id: MemberId = room_owner.clone().into();
        
        let member_info = &room_state.member_info;
        let members = &room_state.members;
        
        let mut all_members = Vec::new();
        
        // Process owner first
        let owner_nickname = member_info
            .member_info
            .iter()
            .find(|mi| mi.member_info.member_id == owner_id)
            .map(|mi| mi.member_info.preferred_nickname.clone())
            .unwrap_or_else(|| "Unknown".to_string());

        let owner_display = MemberDisplay {
            nickname: owner_nickname,
            member_id: owner_id,
            is_owner: true,
            is_self: owner_id == self_member_id,
            invited_you: did_member_invite_you(owner_id, members, self_member_id, &ChatRoomParametersV1 { owner: room_owner.clone() }),
            invited_by_you: false, // Owner can't be invited
        };
        
        all_members.push((format_member_display(&owner_display), owner_id));

        // Process other members
        for member in members.members.iter() {
            let member_id = member.member.id();
            if member_id == owner_id {
                continue;
            }
            
            let nickname = member_info
                .member_info
                .iter()
                .find(|mi| mi.member_info.member_id == member_id)
                .map(|mi| mi.member_info.preferred_nickname.clone())
                .unwrap_or_else(|| "Unknown".to_string());

            let member_display = MemberDisplay {
                nickname,
                member_id,
                is_owner: false,
                is_self: member_id == self_member_id,
                invited_you: did_member_invite_you(member_id, members, self_member_id, &ChatRoomParametersV1 { owner: room_owner.clone() }),
                invited_by_you: did_you_invite_member(member_id, members, self_member_id),
            };
            
            all_members.push((format_member_display(&member_display), member_id));
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
