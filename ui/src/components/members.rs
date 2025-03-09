use crate::components::app::{MemberInfoModalSignal, CURRENT_ROOM, MEMBER_INFO_MODAL, ROOMS};
use crate::room_data::{CurrentRoom, Rooms};
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{FaUserPlus, FaUsers};
use dioxus_free_icons::Icon;
use ed25519_dalek::{SigningKey, VerifyingKey};
use river_common::room_state::member::MembersV1;
use river_common::room_state::member::{AuthorizedMember, MemberId};
use river_common::room_state::ChatRoomParametersV1;
use serde::{Deserialize, Serialize};

pub mod invite_member_modal;
pub mod member_info_modal;
use self::invite_member_modal::InviteMemberModal;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Invitation {
    pub room: VerifyingKey,
    pub invitee_signing_key: SigningKey,
    pub invitee: AuthorizedMember,
}

impl Invitation {
    /// Encode as base58 string
    pub fn to_encoded_string(&self) -> String {
        let mut data = Vec::new();
        ciborium::ser::into_writer(self, &mut data).expect("Serialization should not fail");
        bs58::encode(data).into_string()
    }

    /// Decode from base58 string
    pub fn from_encoded_string(s: &str) -> Result<Self, String> {
        let decoded = bs58::decode(s)
            .into_vec()
            .map_err(|e| format!("Base58 decode error: {}", e))?;
        ciborium::de::from_reader(&decoded[..]).map_err(|e| format!("Deserialization error: {}", e))
    }
}

// Helper struct to store member display info
struct MemberDisplay {
    nickname: String,
    member_id: MemberId,
    is_owner: bool,
    is_self: bool,
    invited_you: bool,     // Direct inviter
    sponsored_you: bool,   // Upstream in invite chain
    invited_by_you: bool,  // Direct invitee
    in_your_network: bool, // Downstream in invite chain
}

// Helper functions to check member relationships
fn is_member_owner(member_id: MemberId, owner_id: MemberId) -> bool {
    member_id == owner_id
}

fn is_member_self(member_id: MemberId, self_id: MemberId) -> bool {
    member_id == self_id
}

fn did_member_invite_you(
    member_id: MemberId,
    members: &MembersV1,
    self_id: MemberId,
    params: &ChatRoomParametersV1,
) -> bool {
    members.is_inviter_of(member_id, self_id, params)
}

fn is_member_sponsor(
    member_id: MemberId,
    members: &MembersV1,
    self_id: MemberId,
    params: &ChatRoomParametersV1,
) -> bool {
    // Check if member is in invite chain but not direct inviter
    if let Some(self_member) = members.members.iter().find(|m| m.member.id() == self_id) {
        if let Ok(chain) = members.get_invite_chain(self_member, params) {
            return chain.iter().any(|m| m.member.id() == member_id);
        }
    }
    false
}

fn is_in_your_network(member_id: MemberId, members: &MembersV1, self_id: MemberId) -> bool {
    // Check if this member was invited by someone you invited
    members.members.iter().any(|m| {
        m.member.id() == member_id
            && members.members.iter().any(|inviter| {
                inviter.member.id() == m.member.invited_by
                    && did_you_invite_member(inviter.member.id(), members, self_id)
            })
    })
}

fn did_you_invite_member(member_id: MemberId, members: &MembersV1, self_id: MemberId) -> bool {
    members
        .members
        .iter()
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
        // Direct invitee
        tags.push("🔑");
    } else if member.in_your_network {
        // Downstream in invite chain
        tags.push("🌐");
    }
    if member.invited_you {
        // Direct inviter
        tags.push("🎪");
    } else if member.sponsored_you {
        // Upstream in invite chain
        tags.push("🔭");
    }

    if tags.is_empty() {
        member.nickname.clone()
    } else {
        format!("{} {}", member.nickname, tags.join(" "))
    }
}

#[component]
pub fn MemberList() -> Element {
    let mut invite_modal_active = use_signal(|| false);

    let members = use_memo(move || {
        let current = CURRENT_ROOM.read();
        let room_owner = current.owner_key.clone()?;
        let rooms = ROOMS.read();
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
            invited_you: did_member_invite_you(
                owner_id,
                members,
                self_member_id,
                &ChatRoomParametersV1 {
                    owner: room_owner.clone(),
                },
            ),
            sponsored_you: false,   // Owner can't be upstream
            invited_by_you: false,  // Owner can't be invited
            in_your_network: false, // Owner can't be downstream
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
                invited_you: did_member_invite_you(
                    member_id,
                    members,
                    self_member_id,
                    &ChatRoomParametersV1 {
                        owner: room_owner.clone(),
                    },
                ),
                sponsored_you: is_member_sponsor(
                    member_id,
                    members,
                    self_member_id,
                    &ChatRoomParametersV1 {
                        owner: room_owner.clone(),
                    },
                ),
                invited_by_you: did_you_invite_member(member_id, members, self_member_id),
                in_your_network: is_in_your_network(member_id, members, self_member_id),
            };

            all_members.push((format_member_display(&member_display), member_id));
        }

        Some(all_members)
    })()
    .unwrap_or_default();

    let handle_member_click = move |member_id| {
        MEMBER_INFO_MODAL.with_mut(|signal| {
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
