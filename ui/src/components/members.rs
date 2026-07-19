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
    /// Display names of the members who have deputized this member (the owner
    /// shows as "room owner"). Empty means not a deputy. Drives the 🛡 badge
    /// and its tooltip (#410).
    deputized_by: Vec<String>,
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
    tags: Vec<(&'static str, String)>,
}

fn member_display_parts(member: &MemberDisplay) -> MemberDisplayParts {
    let mut tags: Vec<(&'static str, String)> = Vec::new();

    if member.is_owner {
        tags.push(("👑", "Room Owner".to_string()));
    }
    if member.is_self {
        tags.push(("⭐", "You".to_string()));
    }
    if member.invited_by_you {
        tags.push(("🔑", "Invited by You".to_string()));
    } else if member.in_your_network {
        tags.push(("🌐", "In Your Network".to_string()));
    }
    if member.invited_you {
        tags.push(("🎪", "Invited You".to_string()));
    } else if member.sponsored_you {
        tags.push(("🔭", "In Your Invite Chain".to_string()));
    }
    if !member.deputized_by.is_empty() {
        tags.push((
            "🛡",
            format!("Deputy (appointed by {})", member.deputized_by.join(", ")),
        ));
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

/// Depth of `id` in the invite tree (owner = 0). `usize::MAX` if `id` is not
/// connected to the owner (broken chain) or hits a cycle.
fn invite_depth(
    id: MemberId,
    owner_id: MemberId,
    inviter_of: &HashMap<MemberId, MemberId>,
) -> usize {
    let mut d = 0usize;
    let mut cur = id;
    let mut guard = HashSet::new();
    while cur != owner_id {
        if !guard.insert(cur) {
            return usize::MAX; // cycle
        }
        match inviter_of.get(&cur) {
            Some(&next) => {
                d += 1;
                cur = next;
            }
            None => return usize::MAX, // not connected to owner
        }
    }
    d
}

/// Order the member list as a DISPLAY tree (#410), VIEWER-SCOPED to
/// viewer-relevant authority: a member is re-parented under a deputizer only if
/// that deputizer is in `viewer_relevant` — either a strict ancestor of the
/// viewer (their deputy could ban the viewer) OR the viewer themselves (the
/// viewer appointed this deputy). This is the SAME condition the 🛡 badge uses.
/// Rules:
/// - display-parent = the deputizer in `viewer_relevant` highest in the invite
///   tree (min invite depth; the owner, depth 0, wins), else the member's
///   inviter (unchanged position);
/// - a repositioned deputy carries their own invite-subtree with them;
/// - within a parent's children, repositioned deputies list before regular
///   invitees; each group keeps invite-tree order;
/// - CYCLE GUARD: if re-parenting a member under their deputizer would make the
///   member an ancestor of that deputizer (mutual / descendant deputization),
///   fall back to the inviter (and treat them as a regular invitee).
///
/// So an owner-deputized global mod rises to the top in EVERY view (including
/// the owner's own — the owner is in their own `viewer_relevant`); a non-owner
/// A's deputy rises under A for viewers in A's subtree AND in A's own view; a
/// deputy whose deputizers neither can-ban the viewer nor are the viewer keeps
/// their normal invite-tree position.
///
/// Display-only: every member appears exactly once; no authority/contract change.
fn deputy_display_order(
    owner_id: MemberId,
    members: &MembersV1,
    deputizers_of: &HashMap<MemberId, Vec<MemberId>>,
    viewer_relevant: &HashSet<MemberId>,
) -> Vec<MemberId> {
    let inviter_of: HashMap<MemberId, MemberId> = members
        .members
        .iter()
        .map(|m| (m.member.id(), m.member.invited_by))
        .collect();

    // Stable base order (invite tree) — used to order sibling groups and break ties.
    let base_order = invite_tree_order(owner_id, members);
    let base_rank: HashMap<MemberId, usize> = base_order
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();

    // display_parent starts as the inviter; deputization may re-parent it.
    let mut display_parent: HashMap<MemberId, MemberId> = inviter_of.clone();
    let mut repositioned: HashSet<MemberId> = HashSet::new();

    // Is `ancestor` an ancestor of `node` in the current display tree?
    let is_ancestor =
        |ancestor: MemberId, node: MemberId, dp: &HashMap<MemberId, MemberId>| -> bool {
            let mut cur = node;
            let mut guard = HashSet::new();
            loop {
                if cur == ancestor {
                    return true;
                }
                if cur == owner_id || !guard.insert(cur) {
                    return false;
                }
                match dp.get(&cur) {
                    Some(&p) => cur = p,
                    None => return false,
                }
            }
        };

    // Process top-down (base order) so higher deputizers settle first.
    for &m in &base_order {
        if m == owner_id {
            continue;
        }
        let Some(deps) = deputizers_of.get(&m) else {
            continue;
        };
        // Only consider VIEWER-RELEVANT deputizers: a strict ancestor of the
        // viewer (their deputy could ban the viewer) or the viewer themselves
        // (the viewer appointed the deputy). Among those, choose the one highest
        // in the invite tree (owner wins). Tie-break by base order. If none is
        // relevant, the member keeps their normal invite-tree position.
        let chosen = deps
            .iter()
            .copied()
            .filter(|&d| viewer_relevant.contains(&d))
            .min_by_key(|&d| {
                (
                    invite_depth(d, owner_id, &inviter_of),
                    *base_rank.get(&d).unwrap_or(&usize::MAX),
                )
            });
        let Some(d) = chosen else {
            continue;
        };
        let inviter = inviter_of.get(&m).copied().unwrap_or(owner_id);
        if d == inviter {
            // Deputized by their own inviter: no move, but still a deputy (shown first).
            repositioned.insert(m);
        } else if !is_ancestor(m, d, &display_parent) {
            display_parent.insert(m, d);
            repositioned.insert(m);
        }
        // else: re-parenting would cycle → keep inviter, treat as regular invitee.
    }

    // Build display children: repositioned (deputies) first, then regular
    // invitees; each group in invite-tree order.
    let mut children: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
    for &m in &base_order {
        if m == owner_id {
            continue;
        }
        let p = display_parent.get(&m).copied().unwrap_or(owner_id);
        children.entry(p).or_default().push(m);
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|&c| {
            (
                !repositioned.contains(&c),
                *base_rank.get(&c).unwrap_or(&usize::MAX),
            )
        });
    }

    // DFS from the owner.
    let mut ordered = Vec::new();
    let mut visited = HashSet::new();
    let mut stack = vec![owner_id];
    while let Some(cur) = stack.pop() {
        if !visited.insert(cur) {
            continue;
        }
        ordered.push(cur);
        if let Some(kids) = children.get(&cur) {
            for &kid in kids.iter().rev() {
                stack.push(kid);
            }
        }
    }

    // Append any members unreachable from the owner (broken chains), in base order.
    for &m in &base_order {
        if !visited.contains(&m) {
            ordered.push(m);
        }
    }

    ordered
}

/// Filter a member's full set of deputizers to those the VIEWER cares about
/// (#410), preserving order: a deputizer in `viewer_relevant` — either a strict
/// ancestor of the viewer (their deputy could ban the viewer) OR the viewer
/// themselves (the viewer appointed this deputy). Drives which members get the
/// 🛡 badge and whose names its tooltip lists. `viewer_relevant` includes the
/// owner for every viewer (so a global moderator is relevant to everyone,
/// including the owner's own view) and the viewer's own id (so a mod you
/// appointed shows the shield in your view).
fn relevant_deputizers(
    deputizers: &[MemberId],
    viewer_relevant: &std::collections::HashSet<MemberId>,
) -> Vec<MemberId> {
    deputizers
        .iter()
        .copied()
        .filter(|id| viewer_relevant.contains(id))
        .collect()
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

        // Reverse map: for each deputy member, who has deputized them (#410).
        // Built from every member's signed `MemberInfo.deputies`, so the 🛡
        // badge tooltip can name the appointer(s) rather than a generic label,
        // and so the list can be ordered by deputizer.
        //
        // Routed through each member_id's CANONICAL record (highest
        // member_info_rank), not a raw scan of `member_info.member_info` —
        // `verify` accepts duplicate member_info records per member_id
        // (migration safety), and unioning deputies across ALL of a member's
        // duplicate records (rather than reading only the converged/canonical
        // one) can keep a revoked deputy grant showing here even after the
        // revoke has won (freenet/river#411 round 8).
        let mut deputizers_of: std::collections::HashMap<MemberId, Vec<MemberId>> =
            std::collections::HashMap::new();
        let member_ids_with_info: std::collections::HashSet<MemberId> = member_info
            .member_info
            .iter()
            .map(|mi| mi.member_info.member_id)
            .collect();
        for appointer in member_ids_with_info {
            let Some(canonical) = member_info.canonical(appointer) else {
                continue;
            };
            for deputy in &canonical.member_info.deputies {
                deputizers_of.entry(*deputy).or_default().push(appointer);
            }
        }

        // The viewer's STRICT ancestors — the members whose invite subtree
        // contains self, i.e. who could ban self. `self` is NOT included, and it
        // is EMPTY when the viewer is the owner (nobody can ban the owner). This
        // is the strict base for `viewer_relevant` below, which unions in the
        // viewer's own id to also cover deputies the viewer appointed (#410).
        let self_ancestors: std::collections::HashSet<MemberId> = {
            let mut set = std::collections::HashSet::new();
            // The owner is a strict ancestor of every non-owner (but not of
            // themselves — hence the guard, so the owner's set stays empty).
            if self_member_id != owner_id {
                set.insert(owner_id);
            }
            let invited_by: std::collections::HashMap<MemberId, MemberId> = members
                .members
                .iter()
                .map(|m| (m.member.id(), m.member.invited_by))
                .collect();
            let mut guard = std::collections::HashSet::new();
            guard.insert(self_member_id);
            let mut cur = invited_by.get(&self_member_id).copied();
            while let Some(c) = cur {
                if !guard.insert(c) {
                    break; // cycle guard
                }
                set.insert(c);
                if c == owner_id {
                    break;
                }
                cur = invited_by.get(&c).copied();
            }
            set
        };

        // The relevance set for BOTH the 🛡 badge and the display ordering
        // (#410, Ian's final call): a deputizer matters to this viewer if it is a
        // strict ancestor of the viewer (their deputy could ban the viewer) OR is
        // the viewer themselves (the viewer appointed the deputy). `self_ancestors`
        // stays STRICT (empty-for-owner); we union the viewer's own id here so a
        // mod you appointed — and a mod the OWNER appointed, in the owner's own
        // view — gets the badge and top/under-you positioning.
        let viewer_relevant: std::collections::HashSet<MemberId> = {
            let mut set = self_ancestors.clone();
            set.insert(self_member_id);
            set
        };

        // Order the list as a DISPLAY tree, VIEWER-SCOPED: a member renders under
        // a deputizer only if that deputizer is viewer-relevant — so a global mod
        // rises to the top for everyone (including the owner's own view), a
        // non-owner's deputy rises within that member's subtree and in that
        // member's own view, and a deputy you appointed rises under you (#410).
        let ordered_ids = deputy_display_order(owner_id, members, &deputizers_of, &viewer_relevant);

        // Resolve an appointer's id to a display name (owner -> "room owner",
        // self -> "you").
        let name_of = |id: MemberId| -> String {
            if id == owner_id {
                return "room owner".to_string();
            }
            if id == self_member_id {
                return "you".to_string();
            }
            member_info
                .canonical(id)
                .map(|mi| {
                    match unseal_bytes_with_secrets(
                        &mi.member_info.preferred_nickname,
                        room_secrets,
                    ) {
                        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                        Err(_) => mi.member_info.preferred_nickname.to_string_lossy(),
                    }
                })
                .unwrap_or_else(|| "Unknown".to_string())
        };

        // Build display list in tree order
        let mut all_members = Vec::new();
        for &member_id in &ordered_ids {
            let is_owner = member_id == owner_id;

            let nickname = member_info
                .canonical(member_id)
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
                // The 🛡 badge shows when a deputy is viewer-relevant (#410):
                // a deputizer that is a strict ancestor of self (their deputy
                // could ban the viewer) OR is the viewer themselves (you
                // appointed them). A deputy of the OWNER (global mod) shows in
                // every view including the owner's own; a mod you appointed
                // shows in your view; a deputy of an unrelated subtree is hidden.
                deputized_by: relevant_deputizers(
                    deputizers_of
                        .get(&member_id)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                    &viewer_relevant,
                )
                .into_iter()
                .map(&name_of)
                .collect(),
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
        aside {
            // Stable hook for the connection-indicator regression tests
            // (freenet/river#274): the members rail is the PRE-FIX location
            // of the connection pill (Bug #5). Tests assert this rail
            // carries no indicator, anchoring on the testid instead of the
            // brittle visible text "Active Members".
            "data-testid": "members-rail",
            class: "w-full md:w-56 flex-shrink-0 bg-panel border-l border-border flex flex-col",
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
            ul {
                "data-testid": "member-list",
                class: "flex-1 px-2 py-2 space-y-0.5 overflow-y-auto min-h-0",
                for (parts, member_id) in members {
                    li {
                        key: "{member_id}",
                        // Stable per-member hook for automation (freenet/river#25).
                        // Entity-ID pattern: `member-item-{member_id}`.
                        "data-testid": "member-item-{member_id}",
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
                    "data-testid": "invite-member-button",
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
                    "data-testid": "export-id-button",
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

                        // Look up member_info from cached or current state.
                        // Routed through `canonical` (highest member_info_rank:
                        // version, then signature bytes), not a version-only
                        // `max_by_key`, so a same-version duplicate can't export
                        // the losing record (freenet/river#411 round 8).
                        let member_info = room_data.self_member_info.clone().or_else(|| {
                            let member_id = MemberId::from(&verifying_key);
                            room_data
                                .room_state
                                .member_info
                                .canonical(member_id)
                                .cloned()
                        });

                        let export = IdentityExport {
                            room_owner: owner_key,
                            signing_key: room_data.self_sk.clone(),
                            authorized_member,
                            invite_chain,
                            member_info,
                            room_name,
                            // Carry the chosen nickname in plaintext so an
                            // export taken before the private-room join-heal
                            // sealed `member_info` doesn't lose it on
                            // re-import (freenet/river#298).
                            self_nickname: room_data.self_nickname.clone(),
                            // Carry the invitation-carried room secrets so a
                            // non-owner of a private room keeps the secret
                            // across a device migration and can still forward
                            // useful `room_secrets` via new invitations
                            // (freenet/river#306). Empty for public rooms and
                            // for owners.
                            invitation_secrets: room_data.invitation_secrets.clone(),
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

/// Whether a room identity is already stored for `owner_key`.
///
/// When true, importing a fresh identity for that room would REPLACE the
/// stored one, losing access to the current signing key unless it was
/// exported first. The import flow therefore prompts for confirmation
/// rather than refusing outright (freenet/river#414). Pure — no signal
/// access — so the decision is unit-testable.
fn import_room_identity_exists(rooms: &crate::room_data::Rooms, owner_key: &VerifyingKey) -> bool {
    rooms.map.contains_key(owner_key)
}

/// Resolve which identity a Replace-confirm imports.
///
/// It MUST be the `snapshot` captured when the overwrite warning was shown.
/// The `_live_token` (the current, still-editable textarea contents) is
/// deliberately IGNORED so that editing the token after the warning appears
/// cannot redirect the overwrite to a different room (freenet/river#414):
/// otherwise a room-A warning followed by pasting room-B's token and clicking
/// Replace would overwrite room B without ever confirming THAT replacement.
/// Returns `None` when there is no pending snapshot (nothing to confirm).
fn resolve_confirmed_import(
    snapshot: Option<IdentityExport>,
    _live_token: &str,
) -> Option<IdentityExport> {
    snapshot
}

/// Build the [`RoomData`](crate::room_data::RoomData) for an imported
/// identity from its export.
///
/// Pure (no signal access) so the import/overwrite behaviour is
/// unit-testable. The returned `RoomData` carries the imported `self_sk`,
/// so inserting it into `Rooms::map` under `owner_vk` **replaces** any
/// existing identity for that room — this is what makes overwrite-on-import
/// work (freenet/river#414). The room state starts empty and is fully
/// populated on the next network sync.
fn build_imported_room_data(export: IdentityExport) -> crate::room_data::RoomData {
    let owner_key = export.room_owner;

    // Compute contract key from owner key + current WASM
    let params = ChatRoomParametersV1 { owner: owner_key };
    let params_bytes = to_cbor_vec(&params);
    let contract_code = ContractCode::from(ROOM_CONTRACT_WASM);
    let contract_key =
        ContractKey::from_params_and_code(Parameters::from(params_bytes), &contract_code);

    // Create RoomData from the import, using room name from export if available
    let mut initial_state = river_core::room_state::ChatRoomStateV1::default();
    if let Some(ref name) = export.room_name {
        initial_state.configuration.configuration.display =
            river_core::room_state::privacy::RoomDisplayMetadata::public(name.clone(), None);
    }
    crate::room_data::RoomData {
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
        // Imported room: the heal prefers `self_member_info` from the export
        // when present. If the export pre-dates the member_info seal (a
        // private-room identity exported before the join's self-heal ran)
        // `export.member_info` is `None`, but the export still carries the
        // chosen nickname in `self_nickname`, so the heal restores it instead
        // of minting a generated default (freenet/river#298).
        self_nickname: export.self_nickname,
        previous_contract_key: None,
        // Restore the invitation-carried room secrets so a non-owner of a
        // private room keeps the secret across a device migration
        // (freenet/river#306). Folded into the `#[serde(skip)]` `secrets` map
        // by `repopulate_secrets_from_state` on the next sync. Empty for
        // public rooms, owners, and pre-#306 exports.
        invitation_secrets: export.invitation_secrets,
    }
}

/// Insert an imported room into the map, **replacing** any existing identity
/// for `owner_vk` and clearing a prior leave tombstone.
///
/// Importing is an explicit rejoin: clear any prior leave tombstone for this
/// owner_key so the merge path doesn't silently filter the room out on next
/// reload (freenet/river#247). Separated from the signal/delegate plumbing so
/// the overwrite semantics are unit-testable (freenet/river#414).
fn apply_imported_room(rooms: &mut crate::room_data::Rooms, room_data: crate::room_data::RoomData) {
    let owner_key = room_data.owner_vk;
    rooms.removed_rooms.remove(&owner_key);
    rooms.map.insert(owner_key, room_data);
}

/// Complete an identity import: insert (overwriting any existing identity),
/// select the room, mark it needing sync, and migrate the imported signing
/// key to the delegate. Shared by the first-time import path and the
/// overwrite-confirm path (freenet/river#414).
fn complete_identity_import(
    export: IdentityExport,
    mut success_msg: Signal<Option<String>>,
    mut error_msg: Signal<Option<String>>,
) {
    let room_data = build_imported_room_data(export);
    let owner_key = room_data.owner_vk;

    // Migrate the imported signing key to the delegate immediately. Without
    // this, the delegate may have a stale key from a prior session, causing
    // all message signatures to be rejected by the contract ("State
    // verification failed: Invalid signature").
    let signing_key_for_migration = room_data.self_sk.clone();
    let room_key_bytes = owner_key.to_bytes();

    // Defer signal mutations to a clean execution context to prevent RefCell
    // re-entrant borrow panics.
    //
    // KNOWN LIMITATION — multi-tab reversal (freenet/river#420). This overwrite
    // updates THIS session's identity and re-saves the per-room delegate slot,
    // but a SECOND tab/device for the same room still holding the OLD identity
    // will write it back as `RoomSlot::Present` on its next save.
    // `chat_delegate::reconcile_room_present` is local-authoritative on a
    // self_sk conflict (last-writer-wins; there is no identity generation to
    // decide which is newer), so on the next cold load a stale tab can silently
    // undo the replacement. Full multi-tab identity coordination is out of scope
    // for this get-unstuck escape hatch; the proper fix is a persisted
    // identity-generation counter (see #420). The confirm dialog tells the user
    // to close other tabs/devices first, which avoids the reversal in practice.
    crate::util::defer(move || {
        // Whether this import REPLACES a DIFFERENT identity already stored for
        // the room (computed inside the same borrow that inserts, so a transient
        // read failure can't abort the import). Drives the DM-state prune below.
        let mut identity_changed = false;
        ROOMS.with_mut(|rooms| {
            if let Some(existing) = rooms.map.get(&owner_key) {
                identity_changed = existing.self_sk != signing_key_for_migration;
            }
            // Record the explicit rejoin so the per-room save overwrites a
            // remote `Tombstone` slot with `Present` rather than adopting the
            // leave (freenet/river#345 round-9), then insert (replacing any
            // existing identity for this room — freenet/river#414).
            crate::components::app::chat_delegate::mark_room_rejoined(owner_key);
            apply_imported_room(rooms, room_data);
        });

        // Overwriting a DIFFERENT identity: prune the OLD identity's cached
        // outbound-DM plaintext + archive state so it doesn't leak into (or
        // wrongly hide threads for) the new identity — symmetric to the CLI
        // `identity import --force` prune (freenet/river#414). Only on a real
        // key change; a brand-new import or a same-key re-import prunes nothing.
        if identity_changed {
            crate::components::app::chat_delegate::prune_dm_state_for_room(owner_key);
        }

        CURRENT_ROOM.with_mut(|current| {
            current.owner_key = Some(owner_key);
        });

        // If this import REPLACED an already-tracked room (an overwrite of a
        // Subscribed room), reset its sync entry so it takes the same GET-first
        // fetch a brand-new import gets, instead of `needs_to_send_update`
        // shipping a bogus delta off the now-empty placeholder state
        // (freenet/river#414). No-op for a brand-new import (not yet tracked),
        // which is already on the GET-first path.
        crate::components::app::sync_info::SYNC_INFO
            .with_mut(|sync_info| sync_info.reset_room_for_resync(&owner_key));

        crate::components::app::mark_needs_sync(owner_key);

        // Migrate signing key to delegate in background
        crate::util::safe_spawn_local(async move {
            let result =
                crate::signing::migrate_signing_key(room_key_bytes, &signing_key_for_migration)
                    .await;
            match result {
                crate::signing::MigrationResult::Stored
                | crate::signing::MigrationResult::StaleKeyOverwritten
                | crate::signing::MigrationResult::AlreadyCurrent => {
                    dioxus::logger::tracing::info!("Import: signing key migrated to delegate");
                    crate::util::defer(move || {
                        let mut sanitized = false;
                        ROOMS.with_mut(|rooms| {
                            if let Some(rd) = rooms.map.get_mut(&owner_key) {
                                // Guard a rapid second replacement: only mark
                                // migrated if the room's CURRENT identity is
                                // still the one we just migrated. If a newer
                                // import replaced it while this migration ran,
                                // its own migration owns `key_migrated_to_delegate`
                                // — don't mark it for a superseded key
                                // (freenet/river#414).
                                if rd.self_sk != signing_key_for_migration {
                                    return;
                                }
                                rd.key_migrated_to_delegate = true;
                                // Remove any messages with invalid signatures
                                // left by a stale delegate key
                                let params = ChatRoomParametersV1 { owner: owner_key };
                                let removed = crate::signing::remove_unverifiable_messages(
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

#[component]
pub fn ImportIdentityModal(is_active: Signal<bool>) -> Element {
    let mut token_input = use_signal(String::new);
    let mut error_msg = use_signal(|| None::<String>);
    let mut success_msg = use_signal(|| None::<String>);
    // The parsed import awaiting overwrite confirmation. `Some` means a room
    // identity already exists for this token's owner, so we prompt to confirm
    // replacing it rather than importing silently (freenet/river#414). This
    // is a SNAPSHOT of the token that was checked: Replace consumes it, NOT a
    // fresh read of the (still-editable) textarea, so editing the token after
    // the warning appears cannot redirect the overwrite to a different room.
    let mut pending_import = use_signal(|| None::<IdentityExport>);

    if !*is_active.read() {
        return rsx! {};
    }

    // Reset-and-close, matching the deferred pattern in `join_with_code_modal`
    // and `.claude/rules/dioxus-signal-safety.md`: signal mutations from event
    // handlers run inside `crate::util::defer()` so they execute in a clean
    // Dioxus context (no re-entrant `RefCell` borrow, root scope present).
    let reset_and_close = move || {
        crate::util::defer(move || {
            is_active.set(false);
            error_msg.set(None);
            success_msg.set(None);
            pending_import.set(None);
            token_input.set(String::new());
        });
    };

    let handle_import = move |_| {
        let input = token_input.read().clone();
        match IdentityExport::from_armored_string(&input) {
            Ok(export) => {
                let owner_key = export.room_owner;

                // If we already have an identity for this room, importing would
                // replace it (and lose the current signing key unless it was
                // exported). Snapshot the CHECKED token and prompt for
                // confirmation instead of refusing (freenet/river#414).
                let already_exists = {
                    let Ok(rooms) = ROOMS.try_read() else {
                        return;
                    };
                    import_room_identity_exists(&rooms, &owner_key)
                };
                if already_exists {
                    crate::util::defer(move || {
                        pending_import.set(Some(export));
                        error_msg.set(None);
                        success_msg.set(None);
                    });
                    return;
                }

                complete_identity_import(export, success_msg, error_msg);
            }
            Err(e) => {
                crate::util::defer(move || {
                    error_msg.set(Some(format!("Invalid token: {}", e)));
                    success_msg.set(None);
                });
            }
        }
    };

    // User confirmed replacing the existing identity: import the SNAPSHOT
    // captured when the warning was shown — never a fresh read of the editable
    // textarea (freenet/river#414).
    let handle_replace_confirm = move |_| {
        let live_token = token_input.read().clone();
        let snapshot = pending_import.read().clone();
        let Some(export) = resolve_confirmed_import(snapshot, &live_token) else {
            return;
        };
        // Belt-and-suspenders: bail on a torn ROOMS read rather than acting on
        // inconsistent state. Existence does not change the action — we import
        // the SNAPSHOT either way (complete_identity_import inserts whether or
        // not the room still exists), so the read only guards consistency.
        if ROOMS.try_read().is_err() {
            return;
        }
        crate::util::defer(move || {
            pending_import.set(None);
        });
        complete_identity_import(export, success_msg, error_msg);
    };

    // User backed out of the overwrite: drop the snapshot and return to the
    // input state, keeping the pasted token so they can reconsider.
    let handle_replace_cancel = move |_| {
        crate::util::defer(move || {
            pending_import.set(None);
        });
    };

    rsx! {
        div {
            class: "fixed inset-0 bg-black/50 flex items-center justify-center z-50",
            onclick: move |_| reset_and_close(),
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
                    // Controlled input: set the value signal synchronously (the
                    // documented signal-safety exception — a deferred write to a
                    // controlled input's bound value lags the DOM and drops
                    // keystrokes). Editing the token invalidates any pending
                    // overwrite confirmation so the warning can't outlive the
                    // token it was raised for (freenet/river#414). The
                    // `pending_import` clear IS deferred, though: the component
                    // subscribes to it (the confirm-vs-input branch below), so a
                    // synchronous clear could re-render mid-write and hit the
                    // Firefox-mobile `RefCell already borrowed` panic. Only defer
                    // when something is actually pending, so a normal keystroke
                    // doesn't schedule a setTimeout.
                    oninput: move |e| {
                        token_input.set(e.value());
                        if pending_import.try_read().is_ok_and(|p| p.is_some()) {
                            crate::util::defer(move || {
                                pending_import.set(None);
                            });
                        }
                    },
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
                if pending_import.read().is_some() {
                    // A room identity already exists — warn before replacing it.
                    div {
                        "data-testid": "import-identity-replace-warning",
                        class: "mt-3 text-sm text-amber-400 bg-amber-500/10 border border-amber-500/30 rounded-lg p-3",
                        "This room already has an identity. Importing will REPLACE it \u{2014} you'll lose access to your current identity for this room unless you've exported it first."
                    }
                    // Multi-tab reversal caveat (freenet/river#420): another
                    // session still on the old identity can write it back and undo
                    // the switch on next load, so tell the user to close them first.
                    div {
                        "data-testid": "import-identity-replace-multitab-warning",
                        class: "mt-2 text-sm text-amber-400 bg-amber-500/10 border border-amber-500/30 rounded-lg p-3",
                        "Close any other tabs or devices open to this room first. A session still using the old identity can write it back and undo the switch."
                    }
                    div { class: "flex justify-end gap-3 mt-4",
                        button {
                            "data-testid": "import-identity-replace-cancel",
                            class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text text-sm rounded-lg transition-colors border border-border",
                            onclick: handle_replace_cancel,
                            "Cancel"
                        }
                        button {
                            "data-testid": "import-identity-replace-confirm",
                            class: "px-4 py-2 bg-red-600 hover:bg-red-700 text-white text-sm font-medium rounded-lg transition-colors",
                            onclick: handle_replace_confirm,
                            "Replace identity"
                        }
                    }
                } else {
                    div { class: "flex justify-end gap-3 mt-4",
                        button {
                            class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text text-sm rounded-lg transition-colors border border-border",
                            onclick: move |_| reset_and_close(),
                            "Cancel"
                        }
                        button {
                            "data-testid": "import-identity-submit",
                            class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white text-sm font-medium rounded-lg transition-colors",
                            onclick: handle_import,
                            "Import"
                        }
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

    /// An empty [`Rooms`](crate::room_data::Rooms) for the overwrite-import
    /// tests (`Rooms` derives no `Default`, so build it field-by-field —
    /// mirrors the `empty_rooms` helper in `chat_delegate.rs`).
    fn empty_rooms() -> crate::room_data::Rooms {
        crate::room_data::Rooms {
            map: HashMap::new(),
            current_room_key: None,
            removed_rooms: HashSet::new(),
            notification_modes: HashMap::new(),
            room_order: Vec::new(),
            migrated_rooms: Vec::new(),
        }
    }

    /// A minimal owner export whose `signing_key` is `self_sk` for the room
    /// owned by `owner_sk`. Enough to drive `build_imported_room_data` /
    /// `apply_imported_room` in the overwrite tests.
    fn export_for(owner_sk: &SigningKey, self_sk: &SigningKey) -> IdentityExport {
        let owner_vk = owner_sk.verifying_key();
        let self_vk = self_sk.verifying_key();
        IdentityExport {
            room_owner: owner_vk,
            signing_key: self_sk.clone(),
            authorized_member: authorized_member(owner_sk, &self_vk),
            invite_chain: vec![],
            member_info: None,
            room_name: None,
            self_nickname: None,
            invitation_secrets: HashMap::new(),
        }
    }

    /// freenet/river#414: importing into a room that already has an identity
    /// routes to the overwrite-confirm path, NOT a hard error. The component
    /// branches on `import_room_identity_exists`, so pin that decision: true
    /// when the room is present, false when absent.
    #[test]
    fn existing_room_import_routes_to_confirm() {
        let owner_sk = SigningKey::from_bytes(&[41u8; 32]);
        let owner_vk = owner_sk.verifying_key();

        let mut rooms = empty_rooms();
        assert!(
            !import_room_identity_exists(&rooms, &owner_vk),
            "no stored identity yet: first-time import path (no confirm)"
        );

        // Seed an existing identity for the room.
        let existing_sk = SigningKey::from_bytes(&[42u8; 32]);
        let existing = build_imported_room_data(export_for(&owner_sk, &existing_sk));
        rooms.map.insert(owner_vk, existing);

        assert!(
            import_room_identity_exists(&rooms, &owner_vk),
            "an identity now exists: import must prompt for overwrite confirmation, not refuse"
        );
    }

    /// freenet/river#414: confirming an overwrite replaces the stored signing
    /// key with the imported one and clears any leave tombstone.
    #[test]
    fn confirming_overwrite_replaces_stored_identity() {
        let owner_sk = SigningKey::from_bytes(&[43u8; 32]);
        let owner_vk = owner_sk.verifying_key();

        let old_sk = SigningKey::from_bytes(&[44u8; 32]);
        let new_sk = SigningKey::from_bytes(&[45u8; 32]);
        assert_ne!(old_sk.to_bytes(), new_sk.to_bytes());

        // Start with the OLD identity stored, and a stale leave tombstone.
        let mut rooms = empty_rooms();
        rooms.map.insert(
            owner_vk,
            build_imported_room_data(export_for(&owner_sk, &old_sk)),
        );
        rooms.removed_rooms.insert(owner_vk);

        // The confirm path re-parses the token and rebuilds RoomData: it must
        // carry the NEW signing key for the SAME room.
        let room_data = build_imported_room_data(export_for(&owner_sk, &new_sk));
        assert_eq!(room_data.owner_vk, owner_vk);
        assert_eq!(
            room_data.self_sk.to_bytes(),
            new_sk.to_bytes(),
            "rebuilt RoomData must carry the imported signing key"
        );

        // Applying it overwrites the stored identity and clears the tombstone.
        apply_imported_room(&mut rooms, room_data);

        assert_eq!(
            rooms.map.len(),
            1,
            "overwrite replaces in place, not appends a second entry"
        );
        assert_eq!(
            rooms.map.get(&owner_vk).unwrap().self_sk.to_bytes(),
            new_sk.to_bytes(),
            "stored identity must now be the imported one"
        );
        assert!(
            !rooms.removed_rooms.contains(&owner_vk),
            "import is an explicit rejoin: the leave tombstone must be cleared"
        );
    }

    /// freenet/river#414 (Codex round 2): confirming an overwrite imports the
    /// token SNAPSHOTTED when the warning appeared, NOT a fresh read of the
    /// (still-editable) textarea. Guards the wrong-room data-loss where a
    /// room-A warning + textarea swapped to room-B + Replace would overwrite
    /// room B without ever confirming that replacement.
    #[test]
    fn confirm_imports_snapshot_not_edited_textarea() {
        let owner_a = SigningKey::from_bytes(&[51u8; 32]);
        let owner_b = SigningKey::from_bytes(&[52u8; 32]);
        assert_ne!(
            owner_a.verifying_key(),
            owner_b.verifying_key(),
            "rooms A and B must differ for the test to be meaningful"
        );

        // Snapshot captured when the warning was shown, for room A.
        let snapshot = export_for(&owner_a, &SigningKey::from_bytes(&[53u8; 32]));

        // The user edits the textarea to room B's token AFTER the warning.
        let edited_live_token =
            export_for(&owner_b, &SigningKey::from_bytes(&[54u8; 32])).to_armored_string();

        // The confirm path resolves to the SNAPSHOT (room A), never the edited
        // live token (room B).
        let resolved = resolve_confirmed_import(Some(snapshot.clone()), &edited_live_token)
            .expect("a pending snapshot must resolve to an import");
        assert_eq!(
            resolved.room_owner,
            owner_a.verifying_key(),
            "must import the snapshot's room (A)"
        );
        assert_ne!(
            resolved.room_owner,
            owner_b.verifying_key(),
            "must NOT import the edited textarea's room (B)"
        );

        // And the RoomData built for the insert targets room A.
        let room_data = build_imported_room_data(resolved);
        assert_eq!(room_data.owner_vk, owner_a.verifying_key());

        // No snapshot → nothing to confirm.
        assert!(resolve_confirmed_import(None, &edited_live_token).is_none());
    }

    /// Frozen cross-side wire-format fixture (issue freenet/river#302/#305).
    ///
    /// A base58(CBOR)-encoded [`Invitation`] with every field populated and
    /// two `room_secrets` entries (non-contiguous versions 0 and 3). The
    /// **same string literal** appears in the CLI at `cli/src/api.rs`
    /// (`invitation_tests::INVITATION_FIXED_FIXTURE_V302`). Both sides decode
    /// it, assert every field, then re-encode and assert the bytes are
    /// byte-identical — so a `#[serde(rename = …)]` slip, a field reorder, a
    /// serde-attr drift, or a field added to one side but not the other can no
    /// longer compile-and-test-clean while silently breaking the CLI↔UI
    /// invitation exchange.
    ///
    /// **Do NOT regenerate this string casually.** It pins the on-wire
    /// format. If a future change legitimately alters the encoding, both
    /// copies (here and in the CLI) must change together and the diff must be
    /// reviewed as a wire-format change. The string was produced once,
    /// deterministically, from the seeds in
    /// [`fixed_fixture_expected_invitation`] (ed25519 signing is deterministic
    /// per RFC 8032, so the bytes are reproducible).
    const INVITATION_FIXED_FIXTURE_V302: &str = "6DdkgteQ42ZdqjP42dauXJKUPV7Pb4YG5wxPzvBDezf3pwCkWX5ENtvTM8Eb9bVzDTG986W4SEY6MVx653EuNkBYhfTx7FM7uFHy3bJng5xoq8S6gfwuau9AgvWEixELwY7Pn9hErx6rymdPeBrpBouZgKkSLCbSqteJL3r1x8adRXkJVfDd8N9P1L9Uorah6J6sxisDuBcT3TZ71zmWaHkWwEptej7DUNUxCruLXjLGcJdWUaYP2YRAP5siqbNUz1rL9Jh5ZK7t8sq2p7WBSJasSyLuSJhDDw2qmRs5nGexupvbcimptn1xQBdzNa6q3bgzt8Qka3Ror5AD7iN6UNpGQPqwgrmvX6g8q2zVMDKh1JeEP9tezNtpmige3WvwRMg2wKk7pFnLNaeGyutEVQrsrd73D9TsB1Mkz86WwxMU8pKvonLgr2TB9yJdiX1BBkDPRZ6yE2bEzxyeo3PZ6t9Nw4WVszSBnFDkAKzAnCoHdo9qpm6n4iY5R6rsANPn75WDiUM16UyqzVsYdWH2JhoVuvpz7D8HUgbGcjTDsMxi33aERdtd7vG24oDMMsKYYNP6VGdXfyRWKm7LUk9M1hFyD1Sf9FZksUxpp924mRNyaJUCniR9pY984jDUrNE3gCuK1PoF9ShtCvEd";

    /// The exact `Invitation` the frozen [`INVITATION_FIXED_FIXTURE_V302`]
    /// string decodes to. Reconstructs it from the same fixed seeds used to
    /// generate the fixture: inviter `[1u8; 32]`, invitee `[2u8; 32]`, owner
    /// `[3u8; 32]`, with the inviter (a non-owner) signing the member. The CLI
    /// keeps a byte-identical counterpart; keep the two in step.
    fn fixed_fixture_expected_invitation() -> Invitation {
        let inviter = SigningKey::from_bytes(&[1u8; 32]);
        let invitee_signing_key = SigningKey::from_bytes(&[2u8; 32]);
        let owner_vk = SigningKey::from_bytes(&[3u8; 32]).verifying_key();
        let member = Member {
            owner_member_id: owner_vk.into(),
            invited_by: inviter.verifying_key().into(),
            member_vk: invitee_signing_key.verifying_key(),
        };
        Invitation {
            room: owner_vk,
            invitee_signing_key,
            invitee: AuthorizedMember::new(member, &inviter),
            room_secrets: vec![(0u32, [0xA1u8; 32]), (3u32, [0xB2u8; 32])],
        }
    }

    /// Cross-side fixed-vector test (issue freenet/river#305). Decodes the
    /// frozen [`INVITATION_FIXED_FIXTURE_V302`] string, asserts every field,
    /// then re-encodes and asserts the bytes are byte-identical to the
    /// fixture. The CLI runs the identical test against the same string in
    /// `cli/src/api.rs`, so the two sides cannot silently diverge on the
    /// invitation wire format.
    #[test]
    fn invitation_decodes_frozen_cross_side_fixture() {
        let decoded = Invitation::from_encoded_string(INVITATION_FIXED_FIXTURE_V302)
            .expect("frozen fixture must decode on the UI side");

        let expected = fixed_fixture_expected_invitation();

        // Assert every field individually so a drift points at the exact
        // field that diverged, not just "the structs differ".
        assert_eq!(decoded.room, expected.room, "room field drifted");
        assert_eq!(
            decoded.invitee_signing_key.to_bytes(),
            expected.invitee_signing_key.to_bytes(),
            "invitee_signing_key field drifted"
        );
        assert_eq!(decoded.invitee, expected.invitee, "invitee field drifted");
        assert_eq!(
            decoded.room_secrets, expected.room_secrets,
            "room_secrets field drifted"
        );
        assert_eq!(
            decoded.room_secrets,
            vec![(0u32, [0xA1u8; 32]), (3u32, [0xB2u8; 32])],
            "room_secrets must carry the two frozen entries exactly"
        );
        assert_eq!(decoded, expected, "decoded invitation must match expected");

        // Re-encode and assert byte-identical to the frozen string. This is
        // the load-bearing assertion: it proves the UI's serializer emits the
        // same bytes the fixture was frozen at, so a serde-attr or field-order
        // change would fail here.
        let reencoded = decoded.to_encoded_string();
        assert_eq!(
            reencoded, INVITATION_FIXED_FIXTURE_V302,
            "re-encoding the decoded invitation must reproduce the frozen \
             fixture byte-for-byte; the UI wire format has drifted from the \
             frozen vector (and therefore from the CLI)"
        );
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
            deputized_by: Vec::new(),
        }
    }

    /// The 🛡 deputy badge shows when a deputy is VIEWER-RELEVANT (#410, Ian's
    /// final call): the deputizer is a strict ancestor of the viewer (their
    /// deputy could ban the viewer) OR is the viewer themselves (you appointed
    /// them). The relevance set passed in is `viewer_ancestors ∪ {viewer}`.
    #[test]
    fn relevant_deputizers_scopes_to_viewer() {
        use freenet_scaffold::util::FastHash;
        let mid = |n: i64| MemberId(FastHash(n));
        let owner = mid(1);
        let a = mid(2); // a strict ancestor of the viewer
        let viewer = mid(4);
        let unrelated = mid(9); // a member in some OTHER subtree
                                // viewer_relevant = strict ancestors {owner, a} ∪ {viewer}.
        let relevant: std::collections::HashSet<MemberId> =
            [owner, a, viewer].into_iter().collect();

        // Deputy of the OWNER (global mod) → relevant.
        assert_eq!(relevant_deputizers(&[owner], &relevant), vec![owner]);
        // Deputy of a strict ancestor of the viewer → relevant.
        assert_eq!(relevant_deputizers(&[a], &relevant), vec![a]);
        // Deputy of an unrelated member → not relevant → hidden.
        assert!(relevant_deputizers(&[unrelated], &relevant).is_empty());
        // A deputy the VIEWER appointed → relevant again ("you appointed them").
        assert_eq!(relevant_deputizers(&[viewer], &relevant), vec![viewer]);
        // Mixed input keeps only the viewer-relevant deputizers, in order.
        assert_eq!(
            relevant_deputizers(&[owner, unrelated, viewer], &relevant),
            vec![owner, viewer]
        );

        // Owner viewing: strict ancestors are EMPTY, but viewer_relevant =
        // {owner} (the owner's own id), so a mod the OWNER appointed shows the
        // shield in the owner's own view. A deputy of an unrelated member is
        // still hidden.
        let owner_relevant: std::collections::HashSet<MemberId> = [owner].into_iter().collect();
        assert_eq!(relevant_deputizers(&[owner], &owner_relevant), vec![owner]);
        assert!(relevant_deputizers(&[unrelated], &owner_relevant).is_empty());
    }

    // Helpers for the display-ordering tests: build a real `MembersV1` (ids are
    // derived from verifying keys, so we can't fabricate them).
    fn authed(
        sk: &SigningKey,
        inviter_id: MemberId,
        inviter_sk: &SigningKey,
        owner_id: MemberId,
    ) -> AuthorizedMember {
        use river_core::room_state::member::Member;
        AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: inviter_id,
                member_vk: sk.verifying_key(),
            },
            inviter_sk,
        )
    }

    /// For a viewer in A's subtree: an owner-deputized global mod rises to the
    /// top, and A's deputy re-parents directly under A. Every member once.
    #[test]
    fn deputy_display_order_places_relevant_deputies_under_deputizer() {
        use rand::rngs::OsRng;
        let owner_sk = SigningKey::generate(&mut OsRng);
        let a_sk = SigningKey::generate(&mut OsRng);
        let b_sk = SigningKey::generate(&mut OsRng);
        let c_sk = SigningKey::generate(&mut OsRng);
        let d_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let a_id: MemberId = a_sk.verifying_key().into();
        let b_id: MemberId = b_sk.verifying_key().into();
        let c_id: MemberId = c_sk.verifying_key().into();
        let d_id: MemberId = d_sk.verifying_key().into();

        // owner -> A -> B -> D ; owner -> C
        let members = MembersV1 {
            members: vec![
                authed(&a_sk, owner_id, &owner_sk, owner_id),
                authed(&b_sk, a_id, &a_sk, owner_id),
                authed(&c_sk, owner_id, &owner_sk, owner_id),
                authed(&d_sk, b_id, &b_sk, owner_id),
            ],
        };

        // owner deputizes C (global mod); A deputizes D.
        let mut deputizers_of: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
        deputizers_of.insert(c_id, vec![owner_id]);
        deputizers_of.insert(d_id, vec![a_id]);

        // Viewer is B (in A's subtree): strict ancestors {owner, A}, so
        // viewer_relevant = {owner, A, B}. Both C's and D's deputizers (owner, A)
        // can ban the viewer, so both reposition; nobody is deputized by B.
        let viewer_relevant: HashSet<MemberId> = [owner_id, a_id, b_id].into_iter().collect();
        let order = deputy_display_order(owner_id, &members, &deputizers_of, &viewer_relevant);

        // C (owner-deputized) before owner's invitee A; D re-parented under A.
        assert_eq!(order, vec![owner_id, c_id, a_id, d_id, b_id]);
        let uniq: HashSet<MemberId> = order.iter().copied().collect();
        assert_eq!(uniq.len(), order.len(), "no duplicates");
        assert_eq!(uniq.len(), 5, "every member appears exactly once");
    }

    /// Viewer-scoped: a deputy whose deputizer CANNOT ban the viewer keeps their
    /// normal invite-tree position (not repositioned). Same room as above, but
    /// the viewer is C (a direct child of owner, ancestors = {owner}); A is not
    /// an ancestor of C, so A's deputy D stays under its inviter B.
    #[test]
    fn deputy_display_order_is_viewer_scoped() {
        use rand::rngs::OsRng;
        let owner_sk = SigningKey::generate(&mut OsRng);
        let a_sk = SigningKey::generate(&mut OsRng);
        let b_sk = SigningKey::generate(&mut OsRng);
        let c_sk = SigningKey::generate(&mut OsRng);
        let d_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let a_id: MemberId = a_sk.verifying_key().into();
        let b_id: MemberId = b_sk.verifying_key().into();
        let c_id: MemberId = c_sk.verifying_key().into();
        let d_id: MemberId = d_sk.verifying_key().into();

        let members = MembersV1 {
            members: vec![
                authed(&a_sk, owner_id, &owner_sk, owner_id),
                authed(&b_sk, a_id, &a_sk, owner_id),
                authed(&c_sk, owner_id, &owner_sk, owner_id),
                authed(&d_sk, b_id, &b_sk, owner_id),
            ],
        };
        let mut deputizers_of: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
        deputizers_of.insert(c_id, vec![owner_id]);
        deputizers_of.insert(d_id, vec![a_id]);

        // Viewer C: strict ancestors {owner}, so viewer_relevant = {owner, C}.
        // Owner can ban C (C repositions to top), but A is not relevant (A ∉
        // {owner, C}), so D is NOT repositioned — it stays under B. Nobody is
        // deputized by C, so adding C to the set changes nothing.
        let viewer_relevant: HashSet<MemberId> = [owner_id, c_id].into_iter().collect();
        let order = deputy_display_order(owner_id, &members, &deputizers_of, &viewer_relevant);

        // C at top (global mod), then A, then A's invite-subtree B -> D unchanged.
        assert_eq!(order, vec![owner_id, c_id, a_id, b_id, d_id]);
        let pos = |id: MemberId| order.iter().position(|&x| x == id).unwrap();
        assert!(
            pos(b_id) < pos(d_id),
            "D stays under B (not repositioned under A)"
        );
        assert_eq!(order.iter().copied().collect::<HashSet<_>>().len(), 5);
    }

    /// The owner sees mods THEY appointed float to the top of their own view
    /// (#410, Ian's final call). The owner's strict-ancestor set is empty, but
    /// `viewer_relevant = {} ∪ {owner}` = `{owner}`, so an owner-appointed global
    /// mod is repositioned (shown first) even in the owner's own view — it is NO
    /// LONGER a plain invite tree.
    #[test]
    fn deputy_display_order_owner_sees_own_appointees_at_top() {
        use rand::rngs::OsRng;
        let owner_sk = SigningKey::generate(&mut OsRng);
        let a_sk = SigningKey::generate(&mut OsRng);
        let c_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let a_id: MemberId = a_sk.verifying_key().into();
        let c_id: MemberId = c_sk.verifying_key().into();

        let members = MembersV1 {
            members: vec![
                authed(&a_sk, owner_id, &owner_sk, owner_id),
                authed(&c_sk, owner_id, &owner_sk, owner_id),
            ],
        };
        let mut deputizers_of: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
        deputizers_of.insert(c_id, vec![owner_id]); // C is a global mod

        // Owner viewing: strict ancestors empty, so viewer_relevant = {owner}.
        let owner_relevant: HashSet<MemberId> = [owner_id].into_iter().collect();
        let order = deputy_display_order(owner_id, &members, &deputizers_of, &owner_relevant);

        // C (owner-deputized) now sorts before A in the owner's OWN view. C's
        // inviter is the owner, so it stays under the owner but leads the
        // repositioned-deputies-first group.
        assert_eq!(order, vec![owner_id, c_id, a_id]);
    }

    /// A deputy the (non-owner) VIEWER appointed rises DIRECTLY under the viewer
    /// in the viewer's own view (#410, Ian's final call — the "you appointed
    /// them" clause applies to ordering too), even when that deputy lives in a
    /// different invite subtree.
    #[test]
    fn deputy_display_order_self_appointed_deputy_rises_under_viewer() {
        use rand::rngs::OsRng;
        let owner_sk = SigningKey::generate(&mut OsRng);
        let a_sk = SigningKey::generate(&mut OsRng);
        let v_sk = SigningKey::generate(&mut OsRng);
        let c_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let a_id: MemberId = a_sk.verifying_key().into();
        let v_id: MemberId = v_sk.verifying_key().into();
        let c_id: MemberId = c_sk.verifying_key().into();

        // owner -> A -> V (the viewer) ; owner -> C (a different subtree).
        let members = MembersV1 {
            members: vec![
                authed(&a_sk, owner_id, &owner_sk, owner_id),
                authed(&v_sk, a_id, &a_sk, owner_id),
                authed(&c_sk, owner_id, &owner_sk, owner_id),
            ],
        };
        // V appoints C (C is invited by the owner, not by V).
        let mut deputizers_of: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
        deputizers_of.insert(c_id, vec![v_id]);

        // Viewer V: strict ancestors {owner, A}, so viewer_relevant =
        // {owner, A, V}. V (∈ relevant) deputized C, so C re-parents under V.
        let viewer_relevant: HashSet<MemberId> = [owner_id, a_id, v_id].into_iter().collect();
        let order = deputy_display_order(owner_id, &members, &deputizers_of, &viewer_relevant);

        // C moves out of the owner's subtree and under V.
        assert_eq!(order, vec![owner_id, a_id, v_id, c_id]);
        let pos = |id: MemberId| order.iter().position(|&x| x == id).unwrap();
        assert!(
            pos(v_id) < pos(c_id),
            "self-appointed deputy sits under viewer"
        );
        assert_eq!(order.iter().copied().collect::<HashSet<_>>().len(), 4);
    }

    /// Mutual/descendant deputization must not create a cycle: the guard falls
    /// back to the inviter, and every member appears exactly once.
    #[test]
    fn deputy_display_order_cycle_falls_back_to_inviter() {
        use rand::rngs::OsRng;
        let owner_sk = SigningKey::generate(&mut OsRng);
        let a_sk = SigningKey::generate(&mut OsRng);
        let b_sk = SigningKey::generate(&mut OsRng);
        let v_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let a_id: MemberId = a_sk.verifying_key().into();
        let b_id: MemberId = b_sk.verifying_key().into();
        let v_id: MemberId = v_sk.verifying_key().into();

        // owner -> A -> B -> V ; B (a descendant) deputizes A (its ancestor).
        let members = MembersV1 {
            members: vec![
                authed(&a_sk, owner_id, &owner_sk, owner_id),
                authed(&b_sk, a_id, &a_sk, owner_id),
                authed(&v_sk, b_id, &b_sk, owner_id),
            ],
        };
        let mut deputizers_of: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
        deputizers_of.insert(a_id, vec![b_id]);

        // Viewer V: strict ancestors {owner, A, B}, so viewer_relevant =
        // {owner, A, B, V}. B (∈ relevant) deputized A, but re-parenting A under B
        // would cycle (A is B's ancestor) → guard keeps A under the owner.
        let viewer_relevant: HashSet<MemberId> = [owner_id, a_id, b_id, v_id].into_iter().collect();
        let order = deputy_display_order(owner_id, &members, &deputizers_of, &viewer_relevant);

        let uniq: HashSet<MemberId> = order.iter().copied().collect();
        assert_eq!(
            uniq.len(),
            order.len(),
            "cycle guard must not duplicate members"
        );
        assert_eq!(
            uniq.len(),
            4,
            "every member (owner, A, B, V) appears exactly once"
        );
        assert_eq!(order[0], owner_id, "owner is the root");
        let pos = |id: MemberId| order.iter().position(|&x| x == id).unwrap();
        assert!(
            pos(a_id) < pos(b_id),
            "A stays above B (cycle guard kept A under owner)"
        );
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

    /// Source-grep pin (freenet/river#414): `complete_identity_import` MUST
    /// reset the room's sync entry via `reset_room_for_resync` so an overwrite
    /// of an already-`Subscribed` room re-GETs full state instead of shipping a
    /// bogus delta off the empty placeholder. Catches a refactor that drops the
    /// reset (which unit tests on the pure helpers alone would not notice, since
    /// the wiring lives in the deferred signal block).
    #[test]
    fn complete_identity_import_resets_sync_for_overwrite() {
        let prod = production_source();
        assert!(
            prod.contains("reset_room_for_resync"),
            "complete_identity_import must call SYNC_INFO.reset_room_for_resync \
             so an overwrite import re-GETs full state (freenet/river#414); \
             without it a Subscribed room ships a delta off empty state."
        );
    }

    /// Source-grep pin (freenet/river#414, Codex round 4): the token
    /// `oninput` must NOT clear `pending_import` synchronously — the component
    /// subscribes to that signal, so a synchronous clear can re-render mid-write
    /// and hit the Firefox-mobile `RefCell already borrowed` panic. The clear is
    /// wrapped in `crate::util::defer()`, guarded so a normal keystroke doesn't
    /// schedule one. Pin both: the guarded/deferred form is present, and the
    /// bare synchronous pair is gone.
    #[test]
    fn oninput_defers_pending_import_clear() {
        let prod = production_source();
        assert!(
            prod.contains("if pending_import.try_read().is_ok_and(|p| p.is_some()) {"),
            "the token oninput must guard + defer the pending_import clear"
        );
        // Whitespace-normalized so indentation/rustfmt changes can't defeat it.
        let normalized = prod.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            !normalized.contains("token_input.set(e.value()); pending_import.set(None);"),
            "the token oninput must not clear pending_import synchronously right \
             after setting the value (freenet/river#414) — defer the clear"
        );
    }

    /// Source-grep pin (freenet/river#414, Codex round 5): the UI overwrite path
    /// must prune the OLD identity's DM state when the imported key changes,
    /// symmetric to the CLI `--force` prune. Catches a refactor that drops the
    /// `prune_dm_state_for_room` wiring from the deferred signal block.
    #[test]
    fn complete_identity_import_prunes_dm_state_on_key_change() {
        let prod = production_source();
        assert!(
            prod.contains("prune_dm_state_for_room(owner_key)"),
            "complete_identity_import must prune the old identity's DM state on an \
             overwrite that changes self_sk (freenet/river#414)"
        );
        // Gated on the key actually changing.
        assert!(
            prod.contains("if identity_changed {"),
            "the DM-state prune must be gated on the identity actually changing"
        );
    }

    /// Source-grep pin (freenet/river#420): the overwrite-confirm dialog must
    /// carry the multi-tab reversal warning telling the user to close other
    /// sessions for the room first (the documented limitation of the #414
    /// escape hatch).
    #[test]
    fn overwrite_confirm_dialog_warns_about_multitab_reversal() {
        let prod = production_source();
        assert!(
            prod.contains("import-identity-replace-multitab-warning"),
            "the confirm dialog must show the multi-tab reversal warning (#420)"
        );
        assert!(
            prod.contains("Close any other tabs or devices open to this room first"),
            "the multi-tab warning must tell the user to close other sessions first"
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
