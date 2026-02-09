#![allow(dead_code)]

use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
use crate::components::app::{CURRENT_ROOM, MEMBER_INFO_MODAL, ROOMS, SYNC_STATUS};
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{FaUserPlus, FaUsers};
use dioxus_free_icons::Icon;
use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::room_state::member::MembersV1;
use river_core::room_state::member::{AuthorizedMember, MemberId};
use river_core::room_state::ChatRoomParametersV1;
use serde::{Deserialize, Serialize};

pub mod invite_member_modal;
pub mod member_info_modal;
use self::invite_member_modal::InviteMemberModal;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
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
        // Create HTML with tooltips for each icon
        let mut html = member.nickname.clone();
        html.push(' ');

        for tag in tags {
            let tooltip = match tag {
                "👑" => "Room Owner",
                "⭐" => "You",
                "🔑" => "Invited by You",
                "🌐" => "In Your Network",
                "🎪" => "Invited You",
                "🔭" => "In Your Invite Chain",
                _ => "",
            };

            html.push_str(&format!(
                "<span class=\"member-icon\" title=\"{}\">{}</span> ",
                tooltip, tag
            ));
        }

        html
    }
}

#[component]
pub fn MemberList() -> Element {
    let mut invite_modal_active = use_signal(|| false);

    let members = use_memo(move || {
        let room_owner = CURRENT_ROOM.read().owner_key?;

        let rooms_read = ROOMS.read();
        let room_data = rooms_read.map.get(&room_owner)?;
        let room_state = room_data.room_state.clone();
        let self_member_id: MemberId = room_data.self_sk.verifying_key().into();
        let owner_id: MemberId = room_owner.into();

        let member_info = &room_state.member_info;
        let members = &room_state.members;
        let room_secrets = &room_data.secrets;

        let mut all_members = Vec::new();

        // Process owner first
        let owner_nickname = member_info
            .member_info
            .iter()
            .find(|mi| mi.member_info.member_id == owner_id)
            .map(|mi| {
                match unseal_bytes_with_secrets(&mi.member_info.preferred_nickname, room_secrets) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                    Err(_) => mi.member_info.preferred_nickname.to_string_lossy(),
                }
            })
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
                &ChatRoomParametersV1 { owner: room_owner },
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
                .map(|mi| {
                    match unseal_bytes_with_secrets(
                        &mi.member_info.preferred_nickname,
                        room_secrets,
                    ) {
                        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                        Err(_) => mi.member_info.preferred_nickname.to_string_lossy(),
                    }
                })
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
                    &ChatRoomParametersV1 { owner: room_owner },
                ),
                sponsored_you: is_member_sponsor(
                    member_id,
                    members,
                    self_member_id,
                    &ChatRoomParametersV1 { owner: room_owner },
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

    // Don't show members panel if no room is selected
    let has_room = CURRENT_ROOM.read().owner_key.is_some();
    if !has_room {
        return rsx! {};
    }

    rsx! {
        aside { class: "w-56 flex-shrink-0 bg-panel border-l border-border flex flex-col",
            // Header
            div { class: "px-4 py-3 border-b border-border flex-shrink-0",
                h2 { class: "text-sm font-semibold text-text-muted uppercase tracking-wide flex items-center gap-2",
                    Icon { icon: FaUsers, width: 16, height: 16 }
                    span { "Members" }
                }
            }

            // Member list - scrollable independently
            ul { class: "flex-1 px-2 py-2 space-y-0.5 overflow-y-auto min-h-0",
                for (display_name, member_id) in members {
                    li { key: "{member_id}",
                        button {
                            class: "w-full text-left px-3 py-1.5 rounded-lg text-sm text-text hover:bg-surface transition-colors truncate",
                            title: "Member ID: {member_id}",
                            onclick: move |_| handle_member_click(member_id),
                            span {
                                dangerous_inner_html: "{display_name}"
                            }
                        }
                    }
                }
            }

            // Invite button - fixed at bottom
            div { class: "p-3 border-t border-border flex-shrink-0",
                button {
                    class: "w-full flex items-center justify-center gap-2 px-3 py-2 bg-accent hover:bg-accent-hover text-white text-sm font-medium rounded-lg transition-colors",
                    onclick: move |_| invite_modal_active.set(true),
                    Icon { icon: FaUserPlus, width: 14, height: 14 }
                    span { "Invite Member" }
                }
            }

            // Connection status indicator - fixed at bottom
            div { class: "px-3 pb-3 flex-shrink-0",
                div {
                    class: format!(
                        "w-full px-3 py-1.5 rounded-full flex items-center justify-center text-xs font-medium {}",
                        match &*SYNC_STATUS.read() {
                            SynchronizerStatus::Connected => "bg-success-bg text-green-700 dark:text-green-400 border border-green-200 dark:border-green-800",
                            SynchronizerStatus::Connecting => "bg-warning-bg text-yellow-700 dark:text-yellow-400 border border-yellow-200 dark:border-yellow-800",
                            SynchronizerStatus::Disconnected | SynchronizerStatus::Error(_) => "bg-error-bg text-red-700 dark:text-red-400 border border-red-200 dark:border-red-800",
                        }
                    ),
                    div {
                        class: format!(
                            "w-2 h-2 rounded-full mr-2 {}",
                            match &*SYNC_STATUS.read() {
                                SynchronizerStatus::Connected => "bg-green-500",
                                SynchronizerStatus::Connecting => "bg-yellow-500",
                                SynchronizerStatus::Disconnected | SynchronizerStatus::Error(_) => "bg-red-500",
                            }
                        ),
                    }
                    span {
                        {
                            match &*SYNC_STATUS.read() {
                                SynchronizerStatus::Connected => "Connected".to_string(),
                                SynchronizerStatus::Connecting => "Connecting...".to_string(),
                                SynchronizerStatus::Disconnected => "Disconnected".to_string(),
                                SynchronizerStatus::Error(ref msg) => format!("Error: {}", msg),
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
