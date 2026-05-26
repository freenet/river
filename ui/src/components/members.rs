use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
use crate::components::app::{
    MobileView, CURRENT_ROOM, MEMBER_INFO_MODAL, MOBILE_VIEW, ROOMS, SYNC_STATUS,
};
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{FaArrowLeft, FaFileExport, FaUserPlus, FaUsers};
use dioxus_free_icons::Icon;
use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::room_state::identity::IdentityExport;
use river_core::room_state::member::MembersV1;
use river_core::room_state::member::{AuthorizedMember, MemberId};
use river_core::room_state::ChatRoomParametersV1;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::constants::ROOM_CONTRACT_WASM;
use crate::util::to_cbor_vec;
use freenet_stdlib::prelude::{ContractCode, ContractKey, Parameters};

pub mod invite_member_modal;
pub mod member_info_modal;
use self::invite_member_modal::InviteMemberModal;

/// Pill-shaped indicator showing the live WebSocket connection state to
/// the local Freenet node. Rendered in `RoomList`'s bottom section so it
/// is visible to ALL users — including first-time / invite-flow users
/// who have no rooms yet. Bug #5 (Ivvor on Matrix, 2026-05-17): the
/// indicator previously lived inside `MemberList`, which returns empty
/// when no room is selected, leaving brand-new users with no signal
/// that their node WebSocket was broken.
///
/// Signal-safety note (AGENTS.md "Dioxus WASM Signal Safety Rules"):
/// `SYNC_STATUS` is read via `try_read()` and the value is snapshotted
/// once per render. The synchronizer writes to `SYNC_STATUS` from
/// places that can fire subscriber notifications during the write
/// guard's Drop on Firefox mobile; an infallible `.read()` here would
/// risk the documented `RefCell already borrowed` panic. If the read
/// fails (signal currently mid-write), we fall back to "Connecting..."
/// — the same neutral state used on initial app boot — and the next
/// render will pick up the real value.
#[component]
pub fn ConnectionStatusIndicator() -> Element {
    // Snapshot the status once per render. `try_read()` returns Err if
    // another writer holds the RefCell; fall back to a neutral state.
    let status: SynchronizerStatus = SYNC_STATUS
        .try_read()
        .map(|r| r.clone())
        .unwrap_or(SynchronizerStatus::Connecting);

    let (pill_classes, dot_classes, label) = match &status {
        SynchronizerStatus::Connected => (
            "bg-success-bg text-green-700 dark:text-green-400 border border-green-200 dark:border-green-800",
            "bg-green-500",
            "Connected".to_string(),
        ),
        SynchronizerStatus::Connecting => (
            "bg-warning-bg text-yellow-700 dark:text-yellow-400 border border-yellow-200 dark:border-yellow-800",
            "bg-yellow-500",
            "Connecting...".to_string(),
        ),
        SynchronizerStatus::Disconnected => (
            "bg-error-bg text-red-700 dark:text-red-400 border border-red-200 dark:border-red-800",
            "bg-red-500",
            "Disconnected".to_string(),
        ),
        SynchronizerStatus::Error(msg) => (
            "bg-error-bg text-red-700 dark:text-red-400 border border-red-200 dark:border-red-800",
            "bg-red-500",
            format!("Error: {}", msg),
        ),
    };

    rsx! {
        div { class: "px-3 pb-3 flex-shrink-0",
            div {
                "aria-label": "WebSocket connection status",
                "data-testid": "connection-status-indicator",
                class: "w-full px-3 py-1.5 rounded-full flex items-center justify-center text-xs font-medium {pill_classes}",
                div { class: "w-2 h-2 rounded-full mr-2 {dot_classes}" }
                span { "{label}" }
            }
        }
    }
}

/// Collect the room secrets an inviter holds into the `(version, secret)`
/// list embedded in an [`Invitation`].
///
/// Sorted ascending by version so the invitation has a deterministic CBOR
/// encoding (the encoded string is fingerprinted for processed-invite
/// dedup, so it must be stable across decode/re-encode cycles). Returns an
/// empty `Vec` for an empty input — a public room, or a private room whose
/// inviting member holds no secret yet.
pub fn collect_invitation_secrets(secrets: &HashMap<u32, [u8; 32]>) -> Vec<(u32, [u8; 32])> {
    let mut out: Vec<(u32, [u8; 32])> = secrets.iter().map(|(&v, &s)| (v, s)).collect();
    out.sort_unstable_by_key(|(v, _)| *v);
    out
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct Invitation {
    pub room: VerifyingKey,
    pub invitee_signing_key: SigningKey,
    pub invitee: AuthorizedMember,
    /// The room's symmetric secrets, one `(version, secret)` per version
    /// the inviting member holds. Lets the invitee decrypt a private room
    /// immediately on join, instead of being stuck on
    /// `[Encrypted message - secret vN not available]` until the room
    /// owner's chat-delegate comes online and back-fills an
    /// `encrypted_secrets` blob (Bug #6 / PR #276). Works even when a
    /// non-owner issues the invitation — the inviter already holds the
    /// secret; the room contract is untouched.
    ///
    /// Carried in plaintext, NOT ECIES-wrapped. That is not a confidentiality
    /// regression: the invitation already carries `invitee_signing_key` in
    /// the clear, so the whole artifact is a bearer credential — anyone who
    /// can read these bytes can already read everything the room secret
    /// protects. Plaintext also avoids decrypting attacker-influenced
    /// ciphertext on the join path (`river_core::ecies::decrypt` panics on a
    /// malformed blob, and the release build is `panic = "abort"`).
    ///
    /// Empty for public rooms and for invitations created before this field
    /// existed (`#[serde(default)]` keeps old links decodable).
    #[serde(default)]
    pub room_secrets: Vec<(u32, [u8; 32])>,
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

/// Hand-written `Debug` that REDACTS `room_secrets`. The derived `Debug`
/// for `[u8; 32]` is fully transparent, so `{:?}`-logging an `Invitation`
/// (e.g. `info!("...{:?}", invitation)`) would print every room-secret
/// byte to the browser console. `room` and `invitee` are non-sensitive;
/// `SigningKey`'s own `Debug` is already non-exhaustive (it does not print
/// the secret), so it is safe to delegate to.
impl std::fmt::Debug for Invitation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Invitation")
            .field("room", &self.room)
            .field("invitee_signing_key", &self.invitee_signing_key)
            .field("invitee", &self.invitee)
            .field(
                "room_secrets",
                &format_args!("<{} room secret(s) redacted>", self.room_secrets.len()),
            )
            .finish()
    }
}

struct MemberDisplay {
    nickname: String,
    _member_id: MemberId,
    is_owner: bool,
    is_self: bool,
    invited_you: bool,
    sponsored_you: bool,
    invited_by_you: bool,
    in_your_network: bool,
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

/// Structured render parts for a member row. Returned by
/// `member_display_parts` so the row can be rendered with plain Dioxus
/// text + icon children — no `dangerous_inner_html`, no HTML
/// concatenation. Member nicknames come from a member's own signed
/// `MemberInfoV1.preferred_nickname` blob and are attacker-controllable
/// bytes; rendering them via `dangerous_inner_html` previously allowed
/// a stored XSS (freenet/river#227).
#[derive(Clone, PartialEq)]
struct MemberDisplayParts {
    nickname: String,
    tags: Vec<(&'static str, &'static str)>,
}

fn member_display_parts(member: &MemberDisplay) -> MemberDisplayParts {
    let mut tags: Vec<(&'static str, &'static str)> = Vec::new();

    if member.is_owner {
        tags.push(("👑", "Room Owner"));
    }
    if member.is_self {
        tags.push(("⭐", "You"));
    }
    if member.invited_by_you {
        tags.push(("🔑", "Invited by You"));
    } else if member.in_your_network {
        tags.push(("🌐", "In Your Network"));
    }
    if member.invited_you {
        tags.push(("🎪", "Invited You"));
    } else if member.sponsored_you {
        tags.push(("🔭", "In Your Invite Chain"));
    }

    MemberDisplayParts {
        nickname: member.nickname.clone(),
        tags,
    }
}

/// Order member IDs by DFS pre-order traversal of the invite tree.
/// Owner is the root; within siblings, order matches `members.members`
/// (sorted by MemberId after CRDT convergence).
/// Members with broken invite chains are appended at the end.
fn invite_tree_order(owner_id: MemberId, members: &MembersV1) -> Vec<MemberId> {
    let mut children_of: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
    for member in members.members.iter() {
        children_of
            .entry(member.member.invited_by)
            .or_default()
            .push(member.member.id());
    }

    let mut ordered = Vec::new();
    let mut visited = HashSet::new();
    let mut stack = vec![owner_id];
    while let Some(current) = stack.pop() {
        if !visited.insert(current) {
            continue;
        }
        ordered.push(current);
        if let Some(kids) = children_of.get(&current) {
            for &kid in kids.iter().rev() {
                stack.push(kid);
            }
        }
    }

    // Append any members not reachable from the owner (orphaned invite chains)
    for member in members.members.iter() {
        let id = member.member.id();
        if !visited.contains(&id) {
            ordered.push(id);
        }
    }

    ordered
}

#[component]
pub fn MemberList() -> Element {
    let mut invite_modal_active = use_signal(|| false);
    let mut export_modal_active = use_signal(|| false);

    let members = use_memo(move || {
        let room_owner = CURRENT_ROOM.read().owner_key?;

        let rooms_read = ROOMS.try_read().ok()?;
        let room_data = rooms_read.map.get(&room_owner)?;
        let room_state = room_data.room_state.clone();
        let self_member_id: MemberId = room_data.self_sk.verifying_key().into();
        let owner_id: MemberId = room_owner.into();

        let member_info = &room_state.member_info;
        let members = &room_state.members;
        let room_secrets = &room_data.secrets;

        let params = ChatRoomParametersV1 { owner: room_owner };

        let ordered_ids = invite_tree_order(owner_id, members);

        // Build display list in tree order
        let mut all_members = Vec::new();
        for &member_id in &ordered_ids {
            let is_owner = member_id == owner_id;

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
                _member_id: member_id,
                is_owner,
                is_self: member_id == self_member_id,
                invited_you: members.is_inviter_of(member_id, self_member_id, &params),
                sponsored_you: if is_owner {
                    false
                } else {
                    is_member_sponsor(member_id, members, self_member_id, &params)
                },
                invited_by_you: if is_owner {
                    false
                } else {
                    did_you_invite_member(member_id, members, self_member_id)
                },
                in_your_network: if is_owner {
                    false
                } else {
                    is_in_your_network(member_id, members, self_member_id)
                },
            };

            all_members.push((member_display_parts(&member_display), member_id));
        }

        Some(all_members)
    })()
    .unwrap_or_default();

    let handle_member_click = move |member_id| {
        crate::util::defer(move || {
            MEMBER_INFO_MODAL.with_mut(|signal| {
                signal.member = Some(member_id);
            });
        });
    };

    // Don't show members panel if no room is selected
    let has_room = CURRENT_ROOM.read().owner_key.is_some();
    if !has_room {
        return rsx! {};
    }

    rsx! {
        aside { class: "w-full md:w-56 flex-shrink-0 bg-panel border-l border-border flex flex-col",
            // Header
            div { class: "px-4 py-3 border-b border-border flex-shrink-0",
                div { class: "flex items-center gap-2",
                    // Mobile back button
                    button {
                        class: "md:hidden p-1 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
                        onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Chat),
                        Icon { icon: FaArrowLeft, width: 14, height: 14 }
                    }
                    h2 { class: "text-sm font-semibold text-text-muted uppercase tracking-wide flex items-center gap-2",
                        Icon { icon: FaUsers, width: 16, height: 16 }
                        span { "Active Members" }
                    }
                }
            }

            // Member list - scrollable independently
            ul { class: "flex-1 px-2 py-2 space-y-0.5 overflow-y-auto min-h-0",
                for (parts, member_id) in members {
                    li { key: "{member_id}",
                        button {
                            class: "w-full text-left px-3 py-1.5 rounded-lg text-sm text-text hover:bg-surface transition-colors truncate",
                            title: "Member ID: {member_id}",
                            onclick: move |_| handle_member_click(member_id),
                            // Nickname rendered as a plain text node — attacker-controlled
                            // bytes from `MemberInfoV1.preferred_nickname` MUST NOT be
                            // routed through `dangerous_inner_html` (freenet/river#227).
                            span { "{parts.nickname}" }
                            for (icon, tooltip) in parts.tags {
                                span {
                                    class: "member-icon",
                                    title: "{tooltip}",
                                    " {icon}"
                                }
                            }
                        }
                    }
                }
            }

            // Action buttons - fixed at bottom
            div { class: "p-3 border-t border-border flex-shrink-0 space-y-2",
                button {
                    class: "w-full flex items-center justify-center gap-2 px-3 py-2 bg-accent hover:bg-accent-hover text-white text-sm font-medium rounded-lg transition-colors",
                    onclick: move |_| invite_modal_active.set(true),
                    Icon { icon: FaUserPlus, width: 14, height: 14 }
                    span { "Invite Member" }
                }
                // The "Direct Messages" button used to live here, but
                // zorolin (#244 feedback, 2026-05-16) and Ian agreed it
                // belonged in the left rail next to Rooms — that's where
                // it now lives via `DmRailSection`. Per-room and
                // cross-room DM discovery are both surfaced there.
                button {
                    class: "w-full flex items-center justify-center gap-1.5 px-2 py-1.5 bg-surface hover:bg-surface-hover text-text-muted text-xs font-medium rounded-lg transition-colors border border-border",
                    onclick: move |_| export_modal_active.set(true),
                    Icon { icon: FaFileExport, width: 12, height: 12 }
                    span { "Export ID" }
                }
            }

            // Connection status indicator is rendered by `RoomList` so it
            // remains visible even when no room is selected (Bug #5,
            // 2026-05-17). RoomList is the always-rendered left rail; the
            // member panel returns empty when `CURRENT_ROOM` is None, which
            // previously hid the indicator from first-time / invite-flow
            // users with no rooms yet.
        }
        InviteMemberModal {
            is_active: invite_modal_active
        }
        ExportIdentityModal {
            is_active: export_modal_active
        }
    }
}

#[component]
fn ExportIdentityModal(is_active: Signal<bool>) -> Element {
    const COPY_BUTTON_DEFAULT: &str = "Copy to Clipboard";
    let mut token_text = use_signal(String::new);
    // Label flips to "Copied!" on click and is reset by the close-side effect
    // below so reopening always starts on the default label.
    let mut copy_button_text = use_signal(|| COPY_BUTTON_DEFAULT.to_string());

    // Reset modal state whenever the modal is dismissed, regardless of which
    // close path the user took (backdrop click, Close button, or any future
    // path like an X icon or Escape key handler).
    use_effect(move || {
        if !*is_active.read() {
            token_text.set(String::new());
            copy_button_text.set(COPY_BUTTON_DEFAULT.to_string());
        }
    });

    // Generate the export token when modal opens
    use_effect(move || {
        if *is_active.read() {
            let room_owner = CURRENT_ROOM.read().owner_key;
            if let Some(owner_key) = room_owner {
                let Ok(rooms_read) = ROOMS.try_read() else {
                    return;
                };
                if let Some(room_data) = rooms_read.map.get(&owner_key) {
                    let verifying_key = room_data.self_sk.verifying_key();

                    // Resolve the AuthorizedMember and invite chain for export:
                    // 1. Use cached self_authorized_member if available
                    // 2. For owners: create a self-signed AuthorizedMember
                    // 3. For non-owners: look up from current room state
                    let resolved = if let Some(ref am) = room_data.self_authorized_member {
                        Some((am.clone(), room_data.invite_chain.clone()))
                    } else if verifying_key == room_data.owner_vk {
                        let owner_id = MemberId::from(&owner_key);
                        let member = river_core::room_state::member::Member {
                            owner_member_id: owner_id,
                            invited_by: owner_id,
                            member_vk: owner_key,
                        };
                        Some((AuthorizedMember::new(member, &room_data.self_sk), vec![]))
                    } else {
                        // Look up member and invite chain from current room state
                        let params = ChatRoomParametersV1 { owner: owner_key };
                        room_data
                            .room_state
                            .members
                            .members
                            .iter()
                            .find(|m| m.member.member_vk == verifying_key)
                            .and_then(|m| {
                                // Require a valid invite chain — an export with a broken
                                // chain would fail validation on import
                                room_data
                                    .room_state
                                    .members
                                    .get_invite_chain(m, &params)
                                    .ok()
                                    .map(|chain| (m.clone(), chain))
                            })
                    };

                    if let Some((authorized_member, invite_chain)) = resolved {
                        // Extract room name for inclusion in export (None if encrypted and undecryptable)
                        let sealed_name = &room_data
                            .room_state
                            .configuration
                            .configuration
                            .display
                            .name;
                        let room_name = unseal_bytes_with_secrets(sealed_name, &room_data.secrets)
                            .ok()
                            .map(|bytes| String::from_utf8_lossy(&bytes).to_string());

                        // Look up member_info from cached or current state
                        let member_info = room_data.self_member_info.clone().or_else(|| {
                            let member_id = MemberId::from(&verifying_key);
                            room_data
                                .room_state
                                .member_info
                                .member_info
                                .iter()
                                .filter(|i| i.member_info.member_id == member_id)
                                .max_by_key(|i| i.member_info.version)
                                .cloned()
                        });

                        let export = IdentityExport {
                            room_owner: owner_key,
                            signing_key: room_data.self_sk.clone(),
                            authorized_member,
                            invite_chain,
                            member_info,
                            room_name,
                        };
                        token_text.set(export.to_armored_string());
                    } else {
                        token_text.set(
                            "Cannot export: membership data not available. \
                             Try sending a message first."
                                .to_string(),
                        );
                    }
                }
            }
        }
    });

    if !*is_active.read() {
        return rsx! {};
    }

    let handle_copy = move |_| {
        let text = token_text.read().clone();
        crate::util::copy_to_clipboard(&text);
        copy_button_text.set("Copied!".to_string());
    };

    rsx! {
        div {
            class: "fixed inset-0 bg-black/50 flex items-center justify-center z-50",
            onclick: move |_| is_active.set(false),
            div {
                class: "bg-panel border border-border rounded-xl shadow-lg p-6 max-w-xl w-full mx-4",
                onclick: move |e| e.stop_propagation(),
                h3 { class: "text-lg font-semibold text-text mb-4",
                    "Export Identity"
                }
                p { class: "text-sm text-text-muted mb-3",
                    "Copy this token and import it in another River client (UI or riverctl) to use the same identity."
                }
                p { class: "text-sm text-yellow-500 font-medium mb-3",
                    "⚠ This token contains your private key. Treat it like a password — do not share it publicly."
                }
                textarea {
                    class: "w-full h-40 bg-surface border border-border rounded-lg p-3 text-xs font-mono text-text resize-none",
                    readonly: true,
                    value: "{token_text}",
                }
                div { class: "flex justify-end gap-3 mt-4",
                    button {
                        class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text text-sm rounded-lg transition-colors border border-border",
                        onclick: move |_| is_active.set(false),
                        "Close"
                    }
                    button {
                        class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white text-sm font-medium rounded-lg transition-colors",
                        onclick: handle_copy,
                        "{copy_button_text}"
                    }
                }
            }
        }
    }
}

#[component]
pub fn ImportIdentityModal(is_active: Signal<bool>) -> Element {
    let mut token_input = use_signal(String::new);
    let mut error_msg = use_signal(|| None::<String>);
    let mut success_msg = use_signal(|| None::<String>);

    if !*is_active.read() {
        return rsx! {};
    }

    let handle_import = move |_| {
        let input = token_input.read().clone();
        match IdentityExport::from_armored_string(&input) {
            Ok(export) => {
                let owner_key = export.room_owner;

                // Check if we already have this room
                {
                    let Ok(rooms) = ROOMS.try_read() else {
                        return;
                    };
                    if rooms.map.contains_key(&owner_key) {
                        error_msg.set(Some(
                            "You already have an identity for this room.".to_string(),
                        ));
                        return;
                    }
                }

                // Compute contract key from owner key + current WASM
                let params = ChatRoomParametersV1 { owner: owner_key };
                let params_bytes = to_cbor_vec(&params);
                let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
                let contract_key = ContractKey::from_params_and_code(
                    Parameters::from(params_bytes),
                    &contract_code,
                );

                // Create RoomData from the import, using room name from export if available
                let mut initial_state = river_core::room_state::ChatRoomStateV1::default();
                if let Some(ref name) = export.room_name {
                    initial_state.configuration.configuration.display =
                        river_core::room_state::privacy::RoomDisplayMetadata::public(
                            name.clone(),
                            None,
                        );
                }
                let room_data = crate::room_data::RoomData {
                    owner_vk: owner_key,
                    room_state: initial_state, // Will be fully populated on sync
                    self_sk: export.signing_key,
                    contract_key,
                    last_read_message_id: None,
                    secrets: HashMap::new(),
                    current_secret_version: None,
                    last_secret_rotation: None,
                    key_migrated_to_delegate: false,
                    self_authorized_member: Some(export.authorized_member),
                    invite_chain: export.invite_chain,
                    self_member_info: export.member_info,
                    // Imported room: the heal uses `self_member_info` from the
                    // export when present. If the export pre-dates the
                    // member_info seal (a private-room identity exported
                    // before the join's self-heal ran) `export.member_info`
                    // is `None` and the heal falls back to a generated
                    // default — `IdentityExport` does not carry the plaintext
                    // nickname. Narrow window; acceptable.
                    self_nickname: None,
                    previous_contract_key: None,
                    invitation_secrets: std::collections::HashMap::new(),
                };

                // Migrate the imported signing key to the delegate immediately.
                // Without this, the delegate may have a stale key from a prior
                // session, causing all message signatures to be rejected by the
                // contract ("State verification failed: Invalid signature").
                let signing_key_for_migration = room_data.self_sk.clone();
                let room_key_bytes = owner_key.to_bytes();

                // Defer signal mutations to a clean execution context to
                // prevent RefCell re-entrant borrow panics.
                crate::util::defer(move || {
                    ROOMS.with_mut(|rooms| {
                        // Importing a room is an explicit rejoin — clear any
                        // prior leave tombstone for this owner_key so the
                        // merge path doesn't silently filter the room out
                        // on next reload (freenet/river#247).
                        rooms.removed_rooms.remove(&owner_key);
                        rooms.map.insert(owner_key, room_data);
                    });

                    CURRENT_ROOM.with_mut(|current| {
                        current.owner_key = Some(owner_key);
                    });

                    crate::components::app::mark_needs_sync(owner_key);

                    // Migrate signing key to delegate in background
                    crate::util::safe_spawn_local(async move {
                        let result = crate::signing::migrate_signing_key(
                            room_key_bytes,
                            &signing_key_for_migration,
                        )
                        .await;
                        match result {
                            crate::signing::MigrationResult::Stored
                            | crate::signing::MigrationResult::StaleKeyOverwritten
                            | crate::signing::MigrationResult::AlreadyCurrent => {
                                dioxus::logger::tracing::info!(
                                    "Import: signing key migrated to delegate"
                                );
                                crate::util::defer(move || {
                                    let mut sanitized = false;
                                    ROOMS.with_mut(|rooms| {
                                        if let Some(rd) = rooms.map.get_mut(&owner_key) {
                                            rd.key_migrated_to_delegate = true;
                                            // Remove any messages with invalid signatures
                                            // left by a stale delegate key
                                            let params = ChatRoomParametersV1 { owner: owner_key };
                                            let removed =
                                                crate::signing::remove_unverifiable_messages(
                                                    &mut rd.room_state,
                                                    &params,
                                                );
                                            sanitized = removed > 0;
                                        }
                                    });
                                    if sanitized {
                                        crate::components::app::mark_needs_sync(owner_key);
                                    }
                                });
                            }
                            crate::signing::MigrationResult::Failed => {
                                dioxus::logger::tracing::warn!(
                                    "Import: delegate key migration failed, will use fallback signing"
                                );
                            }
                        }
                    });

                    success_msg.set(Some("Identity imported! Syncing room state...".to_string()));
                    error_msg.set(None);
                });
            }
            Err(e) => {
                error_msg.set(Some(format!("Invalid token: {}", e)));
                success_msg.set(None);
            }
        }
    };

    rsx! {
        div {
            class: "fixed inset-0 bg-black/50 flex items-center justify-center z-50",
            onclick: move |_| {
                is_active.set(false);
                error_msg.set(None);
                success_msg.set(None);
                token_input.set(String::new());
            },
            div {
                class: "bg-panel border border-border rounded-xl shadow-lg p-6 max-w-lg w-full mx-4",
                onclick: move |e| e.stop_propagation(),
                h3 { class: "text-lg font-semibold text-text mb-4",
                    "Import Identity"
                }
                p { class: "text-sm text-text-muted mb-3",
                    "Paste a River identity token exported from another client."
                }
                textarea {
                    class: "w-full h-40 bg-surface border border-border rounded-lg p-3 text-xs font-mono text-text resize-none",
                    placeholder: "-----BEGIN RIVER IDENTITY-----\n...\n-----END RIVER IDENTITY-----",
                    value: "{token_input}",
                    oninput: move |e| token_input.set(e.value()),
                }
                if let Some(err) = &*error_msg.read() {
                    div { class: "mt-2 text-sm text-red-400",
                        "{err}"
                    }
                }
                if let Some(msg) = &*success_msg.read() {
                    div { class: "mt-2 text-sm text-green-400",
                        "{msg}"
                    }
                }
                div { class: "flex justify-end gap-3 mt-4",
                    button {
                        class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text text-sm rounded-lg transition-colors border border-border",
                        onclick: move |_| {
                            is_active.set(false);
                            error_msg.set(None);
                            success_msg.set(None);
                            token_input.set(String::new());
                        },
                        "Cancel"
                    }
                    button {
                        class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white text-sm font-medium rounded-lg transition-colors",
                        onclick: handle_import,
                        "Import"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use river_core::room_state::member::Member;

    fn authorized_member(owner_sk: &SigningKey, invitee_vk: &VerifyingKey) -> AuthorizedMember {
        let owner_id = MemberId::from(&owner_sk.verifying_key());
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: *invitee_vk,
        };
        AuthorizedMember::new(member, owner_sk)
    }

    #[test]
    fn collect_invitation_secrets_is_sorted_by_version() {
        let mut secrets = HashMap::new();
        secrets.insert(2u32, [11u8; 32]);
        secrets.insert(0u32, [7u8; 32]);
        secrets.insert(1u32, [9u8; 32]);

        let collected = collect_invitation_secrets(&secrets);
        assert_eq!(
            collected,
            vec![(0, [7u8; 32]), (1, [9u8; 32]), (2, [11u8; 32])]
        );
    }

    #[test]
    fn collect_invitation_secrets_empty_input_is_empty() {
        assert!(collect_invitation_secrets(&HashMap::new()).is_empty());
    }

    #[test]
    fn invitation_cbor_round_trip_preserves_room_secrets() {
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let invitee_vk = invitee_sk.verifying_key();

        let mut secrets = HashMap::new();
        secrets.insert(0u32, [1u8; 32]);
        secrets.insert(1u32, [2u8; 32]);

        let invitation = Invitation {
            room: owner_sk.verifying_key(),
            invitee_signing_key: invitee_sk.clone(),
            invitee: authorized_member(&owner_sk, &invitee_vk),
            room_secrets: collect_invitation_secrets(&secrets),
        };

        let decoded = Invitation::from_encoded_string(&invitation.to_encoded_string())
            .expect("invitation should round-trip");
        assert_eq!(decoded, invitation);
        assert_eq!(
            decoded.room_secrets.into_iter().collect::<HashMap<_, _>>(),
            secrets
        );
    }

    #[test]
    fn invitation_encoding_is_deterministic_with_room_secrets() {
        // The encoded string is fingerprinted for processed-invite dedup,
        // so it must be byte-stable across re-encodes.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);

        let mut secrets = HashMap::new();
        secrets.insert(0u32, [5u8; 32]);
        secrets.insert(7u32, [6u8; 32]);
        secrets.insert(3u32, [4u8; 32]);

        let invitation = Invitation {
            room: owner_sk.verifying_key(),
            invitee_signing_key: invitee_sk.clone(),
            invitee: authorized_member(&owner_sk, &invitee_sk.verifying_key()),
            room_secrets: collect_invitation_secrets(&secrets),
        };
        assert_eq!(
            invitation.to_encoded_string(),
            invitation.to_encoded_string()
        );
    }

    fn make_member_display(nickname: &str) -> MemberDisplay {
        MemberDisplay {
            nickname: nickname.to_string(),
            _member_id: MemberId(freenet_scaffold::util::FastHash(0)),
            is_owner: false,
            is_self: false,
            invited_you: false,
            sponsored_you: false,
            invited_by_you: false,
            in_your_network: false,
        }
    }

    /// Regression test for freenet/river#227 (stored XSS via nickname).
    /// `member_display_parts` MUST keep the nickname intact as a separate
    /// field so the renderer can emit it as a Dioxus text node — NOT as a
    /// pre-built HTML string. The renderer used to splat the return value
    /// through `dangerous_inner_html`, so a nickname like
    /// `<img src=x onerror=...>` executed in every viewer's browser.
    #[test]
    fn member_display_parts_keeps_nickname_unescaped_and_separated() {
        let display = make_member_display("<img src=x onerror=alert(1)>");
        let parts = member_display_parts(&display);

        // Nickname is returned verbatim — the renderer is responsible for
        // emitting it as a text node, not HTML. If a future refactor goes
        // back to building an HTML string here, this test won't catch it
        // directly, but the absence of any `dangerous_inner_html` in the
        // member-row rsx! block (see `MemberList`) is the structural
        // guarantee.
        assert_eq!(parts.nickname, "<img src=x onerror=alert(1)>");
        assert!(parts.tags.is_empty());
    }

    #[test]
    fn member_display_parts_collects_tags_for_owner_and_self() {
        let mut display = make_member_display("alice");
        display.is_owner = true;
        display.is_self = true;
        let parts = member_display_parts(&display);

        assert_eq!(parts.nickname, "alice");
        let icons: Vec<&str> = parts.tags.iter().map(|(icon, _)| *icon).collect();
        assert!(icons.contains(&"👑"));
        assert!(icons.contains(&"⭐"));
    }

    /// Production-code slice of this file (everything before the
    /// `#[cfg(test)]` test module). Used by the two source-grep pins
    /// below so that prose / examples in the test module — which may
    /// legitimately *mention* the attribute name or attack pattern —
    /// can't either disarm or accidentally trip the assertions.
    fn production_source() -> &'static str {
        let source = include_str!("members.rs");
        let marker = "#[cfg(test)]";
        let cut = source
            .find(marker)
            .expect("members.rs should have a #[cfg(test)] block");
        &source[..cut]
    }

    /// Source-grep pin: NOTHING in `members.rs`'s production code may use
    /// the Dioxus unsafe attribute. The freenet/river#227 XSS came from
    /// routing the attacker-controlled `member.nickname` through that
    /// attribute. None of this file's components (member list, identity
    /// import/export) render markdown or any other source that needs it,
    /// so a blanket production-side ban is the strongest regression gate.
    ///
    /// The check tolerates whitespace before the `:` (`attr : "..."`,
    /// `attr  :`, etc.) so a rustfmt edge case can't silently disarm the
    /// pin. The attribute name itself isn't valid Rust as a bare
    /// identifier here, so a doc-comment mention is the only way it
    /// can appear in the production slice — and the assertion error
    /// message tells you to delete it or move it to test code.
    #[test]
    fn members_rs_production_does_not_use_dangerous_inner_html() {
        let prod = production_source();
        // Find any `dangerous_inner_html` occurrence and verify it is
        // NOT followed (after optional whitespace) by `:` — i.e. it is
        // not a Dioxus attribute use. A bare mention in a code comment
        // is OK (a future doc-comment in production code shouldn't
        // generally happen, but tolerating it avoids brittle failures).
        let mut search = prod;
        while let Some(idx) = search.find("dangerous_inner_html") {
            let after = &search[idx + "dangerous_inner_html".len()..];
            let after_ws = after.trim_start_matches([' ', '\t']);
            assert!(
                !after_ws.starts_with(':'),
                "members.rs production code must not use \
                 dangerous_inner_html: as a Dioxus attribute — \
                 member nicknames are attacker-controlled \
                 (freenet/river#227). Render as a Dioxus text node \
                 instead."
            );
            search = &after[1..];
        }
    }

    /// Source-grep pin: the member-row render MUST keep `parts.nickname`
    /// as a Dioxus text-node interpolation — `span { "{parts.nickname}" }`
    /// — not pass it through any string concatenation or attribute that
    /// evaluates HTML. Catches a future refactor that goes back to
    /// building an HTML string for the row (the freenet/river#227 shape).
    #[test]
    fn member_row_renders_nickname_as_text_node() {
        let prod = production_source();
        assert!(
            prod.contains("span { \"{parts.nickname}\" }"),
            "MemberList must render the nickname as a Dioxus text node \
             (`span {{ \"{{parts.nickname}}\" }}`). Concatenating it into \
             an HTML string reopens freenet/river#227."
        );
    }

    #[test]
    fn legacy_invitation_without_room_secrets_decodes_to_empty() {
        // Backward-compat: an invitation encoded before `room_secrets`
        // existed must still decode, with the field defaulting to empty.
        #[derive(Serialize)]
        struct LegacyInvitation {
            room: VerifyingKey,
            invitee_signing_key: SigningKey,
            invitee: AuthorizedMember,
        }
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let invitee_sk = SigningKey::generate(&mut rng);
        let legacy = LegacyInvitation {
            room: owner_sk.verifying_key(),
            invitee_signing_key: invitee_sk.clone(),
            invitee: authorized_member(&owner_sk, &invitee_sk.verifying_key()),
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&legacy, &mut bytes).unwrap();
        let encoded = bs58::encode(bytes).into_string();

        let decoded =
            Invitation::from_encoded_string(&encoded).expect("legacy invitation should decode");
        assert!(decoded.room_secrets.is_empty());
    }
}
