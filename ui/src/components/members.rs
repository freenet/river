use crate::components::app::freenet_api::freenet_synchronizer::SynchronizerStatus;
use crate::components::app::{
    MobileView, CURRENT_ROOM, MEMBER_INFO_MODAL, MOBILE_VIEW, ROOMS, SYNC_STATUS,
};
use crate::util::confusable::{
    ConfusableTier, ImpersonationChecker, ImpersonationWarning, ProtectedName, ProtectedRole,
};
use crate::util::display_name::display_nickname;
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
    /// The 🛡 badge to show for this member in the current viewer's view, or
    /// `None` for no badge (#410). Same value — and therefore the same
    /// visibility rule and the same tooltip — the conversation's author line
    /// and the member-info modal use.
    deputy_badge: Option<DeputyBadge>,
    /// The ⚠ impersonation warning for this member, or `None`. Same value, and
    /// therefore the same tier rule and tooltip, the conversation's author line
    /// uses — both go through
    /// [`impersonation_warning_for_display`].
    ///
    /// Mutually exclusive with `deputy_badge` by construction: a member who
    /// shows a shield is a deputy, deputies are in the checker's privileged set,
    /// and a privileged member is never warned about. Pinned by
    /// `shield_and_warning_are_mutually_exclusive`.
    impersonation: Option<ImpersonationWarning>,
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

    // FIRST, so the warning sits immediately after the name it is about rather
    // than at the end of a run of relationship tags. A reader scanning the list
    // for an impostor should not have to parse "🔑 🌐 🎪" before reaching it.
    if let Some(warning) = member.impersonation.as_ref() {
        tags.push((crate::util::confusable::WARNING_GLYPH, warning.tooltip()));
    }
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
    if let Some(badge) = member.deputy_badge.as_ref() {
        tags.push(("🛡", badge.tooltip()));
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

/// Reverse map: for each deputy member, the members who have deputized them,
/// built from every member's CANONICAL signed `MemberInfo.deputies` (#410).
///
/// Routed through each member_id's canonical record (highest
/// `member_info_rank`), not a raw scan of `member_info.member_info` — `verify`
/// accepts duplicate member_info records per member_id (migration safety), and
/// unioning deputies across ALL of a member's duplicate records (rather than
/// reading only the converged/canonical one) can keep a revoked deputy grant
/// showing even after the revoke has won (freenet/river#411 round 8).
pub(crate) fn build_deputizers_of(
    member_info: &river_core::room_state::member_info::MemberInfoV1,
) -> HashMap<MemberId, Vec<MemberId>> {
    let mut deputizers_of: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
    // Iterate appointers in a DETERMINISTIC order (sorted by MemberId), NOT
    // raw `HashSet` iteration order. The member row and the modal legend build
    // this map independently, so a nondeterministic order would let a member
    // with multiple deputizers show "room owner, you" in one place and "you,
    // room owner" in the other — and even reorder between renders. Sorting the
    // appointers makes each deputizer list (and thus the tooltip) stable and
    // identical across both call sites (freenet/river#451, Codex P3).
    let mut appointers: Vec<MemberId> = member_info
        .member_info
        .iter()
        .map(|mi| mi.member_info.member_id)
        .collect();
    appointers.sort_unstable();
    appointers.dedup();
    for appointer in appointers {
        let Some(canonical) = member_info.canonical(appointer) else {
            continue;
        };
        for deputy in &canonical.member_info.deputies {
            // SELF-DEPUTISATION IS NOT A GRANT. `MemberInfoV1::verify` bounds
            // the list length and checks the signature but never validates who
            // is in it, so any member can write `deputies: [self]` with a
            // custom client. Honouring that would let them render a genuine 🛡
            // on their own messages for their whole subtree — the same
            // impersonation this file's badge exists to make trustworthy,
            // achieved with a real badge instead of an emoji. It also grants
            // nothing: `is_ban_authorized` step 5 only consults a STRICT
            // ancestor's deputy list, so a self-grant is already inert for
            // authority. Ignore it for display too.
            if *deputy == appointer {
                continue;
            }
            // `MAX_DEPUTIES` bounds the LENGTH but nothing rejects repeats, so
            // `deputies: [sockpuppet; 64]` would otherwise name the same
            // appointer 64 times in the tooltip and bury the real content.
            let entry = deputizers_of.entry(*deputy).or_default();
            if !entry.contains(&appointer) {
                entry.push(appointer);
            }
        }
    }
    deputizers_of
}

/// The viewer-relevant set for the 🛡 deputy badge (#410): the viewer's STRICT
/// ancestors (members whose deputy could ban the viewer) unioned with the
/// viewer themselves (deputies they appointed). Strict ancestors are EMPTY for
/// the owner, so the owner sees the shield only for their own appointees; the
/// owner is always included so a global moderator (a deputy of the owner) is
/// relevant to every viewer.
pub(crate) fn viewer_relevant_deputizer_set(
    members: &MembersV1,
    owner_id: MemberId,
    self_member_id: MemberId,
) -> HashSet<MemberId> {
    // Strict ancestors of the viewer: the owner (of every non-owner) plus the
    // walk up the invite chain. `self` is deliberately NOT a strict ancestor.
    let mut set = HashSet::new();
    if self_member_id != owner_id {
        set.insert(owner_id);
    }
    let invited_by: HashMap<MemberId, MemberId> = members
        .members
        .iter()
        .map(|m| (m.member.id(), m.member.invited_by))
        .collect();
    let mut guard = HashSet::new();
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
    // Union the viewer's own id (deputies the viewer appointed).
    set.insert(self_member_id);
    set
}

/// One appointer behind a 🛡 shield, kept as a TYPE rather than a string so a
/// surface cannot accidentally treat a nickname as a role label.
///
/// `Owner` and `You` are trusted: River decides them, and their wording is a
/// literal in this file. `Member` carries an attacker-chosen nickname and is
/// the reason this enum exists — see [`DeputyBadge::tooltip`] for what may and
/// may not be done with it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Appointer {
    /// The room owner. Rendered as the trusted label `"the room owner"`.
    Owner,
    /// The viewer themselves. Rendered as the trusted label `"you"`.
    You,
    /// Any other member, named by their decrypted `preferred_nickname`.
    ///
    /// **Attacker-controlled.** Never interpolate this into a flat sentence
    /// alongside a role label.
    Member(String),
}

/// `target`'s deputizers that are RELEVANT to the viewer, in the order the
/// member-list 🛡 badge shows them. An empty result means the shield does not
/// show for this viewer.
///
/// This is the single definition shared by the member-list row and the
/// member-info modal legend, so the shield's visibility and its
/// `Deputy (appointed by …)` tooltip cannot drift between the two
/// (freenet/river#451).
pub(crate) fn relevant_appointers(
    member_info: &river_core::room_state::member_info::MemberInfoV1,
    room_secrets: &HashMap<u32, [u8; 32]>,
    deputizers_of: &HashMap<MemberId, Vec<MemberId>>,
    viewer_relevant: &HashSet<MemberId>,
    owner_id: MemberId,
    self_member_id: MemberId,
    target: MemberId,
) -> Vec<Appointer> {
    // Classify rather than format. Resolving straight to a display string here
    // is what made the forgery possible: once an appointer is a `String`, the
    // difference between "River said this" and "a member typed this" is gone,
    // and every downstream surface has to remember a rule it cannot see.
    let appointer_of = |id: MemberId| -> Appointer {
        if id == owner_id {
            return Appointer::Owner;
        }
        if id == self_member_id {
            return Appointer::You;
        }
        Appointer::Member(
            member_info
                .canonical(id)
                .map(|mi| display_nickname(&mi.member_info.preferred_nickname, room_secrets))
                .unwrap_or_else(|| "an unknown member".to_string()),
        )
    };
    relevant_deputizers(
        deputizers_of.get(&target).map(Vec::as_slice).unwrap_or(&[]),
        viewer_relevant,
    )
    .into_iter()
    .map(appointer_of)
    .collect()
}

/// Everyone a ban of `target` would remove from the room: `target` themselves
/// PLUS their entire transitive invite subtree.
///
/// This mirrors what the contract actually does. `MembersV1::check_banned_members`
/// inserts the banned user and then extends with `get_downstream_members`, which
/// is private to the contract crate, so the walk is recomputed here rather than
/// approximated. (`cli/src/deputies.rs::invite_subtrees` is riverctl's mirror of
/// the same thing.)
///
/// One deliberate difference from the contract's version: a visited-set guard.
/// The contract can rely on `verify` having rejected circular invite chains; the
/// UI walks whatever state it currently holds and must not hang on a malformed
/// one.
pub(crate) fn ban_removal_set(members: &MembersV1, target: MemberId) -> HashSet<MemberId> {
    let mut children: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
    for m in &members.members {
        children
            .entry(m.member.invited_by)
            .or_default()
            .push(m.member.id());
    }

    let mut removed = HashSet::new();
    removed.insert(target);
    let mut stack = vec![target];
    while let Some(current) = stack.pop() {
        for child in children.get(&current).into_iter().flatten() {
            // `insert` gates the push, so a cycle terminates.
            if removed.insert(*child) {
                stack.push(*child);
            }
        }
    }
    removed
}

/// NOBODY MAY BAN THEMSELVES OUT OF THE ROOM (freenet/river#478).
///
/// ONE rule, stated once: compute the set the ban would remove
/// ([`ban_removal_set`]) and refuse if `banner` is in it. That covers the direct
/// self-ban (`banner == target`) and the transitive one (`target` is a strict
/// invite ancestor of `banner`, so the cascade sweeps the banner up with them)
/// as the single case they are — do NOT split this back into two special cases.
///
/// The CONTRACT permits both: `is_ban_authorized` has no self-check, and step 3
/// (owner-appointed global moderator) fires before anything that would stop it,
/// so an owner-appointed deputy is authorized to ban themselves AND to ban their
/// own ancestors. Such a ban is fully valid: `verify` accepts it, and
/// `check_banned_members` then cascades removal to `get_downstream_members`,
/// taking the banner's ENTIRE INVITE SUBTREE with them. On the Official room
/// that is ~105 members for one misclick.
///
/// This is therefore a gate at the interaction layer, not a contract fix. Do NOT
/// "simplify" it away on the assumption that the contract prevents it: it does
/// not.
///
/// Returns the user-visible reason when the ban is refused, `None` when the rule
/// does not apply. The wording differs between the two routes because they read
/// very differently to the user, but the DECISION above is one computation.
pub(crate) fn self_removing_ban_reason(
    members: &MembersV1,
    banner: MemberId,
    target: MemberId,
) -> Option<&'static str> {
    if !ban_removal_set(members, target).contains(&banner) {
        return None;
    }
    Some(if banner == target {
        "You can't ban yourself. The ban would remove you, and everyone you \
         invited, from the room."
    } else {
        "You can't ban a member you joined the room through. A ban also removes \
         everyone the banned member invited, so this one would remove you, and \
         everyone you invited, along with them."
    })
}

/// Whether the Ban action is offered for one (viewer, target) pair, and if not,
/// whether the viewer is owed an explanation.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum BanGate {
    /// The viewer holds ban authority and the ban is safe to offer.
    Allowed,
    /// The viewer holds ban authority, but the ban would remove the VIEWER from
    /// the room ([`self_removing_ban_reason`]). Carries the user-visible reason:
    /// the action is withheld from someone who would otherwise have had it, so
    /// silently hiding it would read as a bug.
    WouldRemoveViewer(&'static str),
    /// The viewer has no ban authority over this target. Nothing is owed — the
    /// action has never been offered here, so an explanation would just be noise
    /// for a capability they never had.
    NoAuthority,
}

/// The Ban-button gate (#410 / #411 round 4 D / #478).
///
/// Authority first, via [`MembersV1::is_ban_authorized`] (owner /
/// invite-ancestor / deputy) rather than bare invite-chain ancestry, so a DEPUTY
/// sees the Ban action for members in their deputizer's subtree — bare downstream
/// ancestry (`is_downstream`, still used for the "🔑 Invited by You" relationship
/// tag) would have hidden it.
///
/// Then the self-removal rule, applied to the OUTPUT of `is_ban_authorized`
/// rather than inside it. That placement is load-bearing: `is_ban_authorized`
/// grants owner-appointed global moderators authority at step 3, ahead of the
/// step-4 guardrail, so a check wired into the wrong branch of that ladder would
/// not fire for exactly the deputies who can reach the most members.
pub(crate) fn ban_gate(
    members: &MembersV1,
    member_info: &river_core::room_state::member_info::MemberInfoV1,
    viewer: MemberId,
    target: MemberId,
    owner_id: MemberId,
) -> BanGate {
    let members_by_id = members.members_by_member_id();
    if !MembersV1::is_ban_authorized(viewer, target, &members_by_id, member_info, owner_id) {
        return BanGate::NoAuthority;
    }
    match self_removing_ban_reason(members, viewer, target) {
        Some(reason) => BanGate::WouldRemoveViewer(reason),
        None => BanGate::Allowed,
    }
}

/// The 🛡 shield one member shows in one viewer's view.
///
/// Ian's semantics for the shield (2026-07): *"a deputy shield indicates 'this
/// user has been deputized WHICH ALLOWS THEM TO BAN ME', unless I'm also a
/// deputy in which case they can't ban me but I can still see that they're a
/// deputy."* So the shield is about the AUTHOR's authority over the VIEWER, and
/// it must NOT disappear when the viewer happens to be immune.
///
/// Those two halves are deliberately separate fields:
///
/// * `deputized_by` decides **visibility** — non-empty means show the shield.
///   It is the pre-existing viewer-relative predicate the member list and the
///   member-info modal already use ([`relevant_appointers`]), which never
///   consults the viewer's own deputy status, so the "still shows when I'm
///   immune" half holds by construction.
/// * `can_ban_viewer` decides only the **tooltip wording**, and is the real
///   contract-level answer from [`MembersV1::is_ban_authorized`] rather than an
///   approximation of it. The two can legitimately disagree: a deputy the
///   viewer appointed themselves shows the shield but cannot ban the viewer
///   (the "cannot ban the member who deputized you" guardrail in
///   `is_ban_authorized` step 4).
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct DeputyBadge {
    /// Viewer-relevant appointers. Never empty: an empty result means no badge
    /// at all, which is represented by the member being absent from the map.
    pub deputized_by: Vec<Appointer>,
    /// Whether this member can currently get the viewer removed from the room,
    /// directly or by a cascading ban of one of the viewer's ancestors. `None`
    /// when the badge is on the VIEWER'S OWN row: "can they ban you" is not a
    /// meaningful thing to say about yourself, and `is_ban_authorized` happily
    /// answers `true` for a global moderator asked about themselves.
    pub can_ban_viewer: Option<bool>,
}

impl DeputyBadge {
    /// Who appointed them, as a phrase built ONLY from trusted literals plus a
    /// count. No nickname reaches this string.
    ///
    /// **A `title=` attribute is a flat string, so quoting a nickname inside it
    /// is not a defense and must not be re-attempted.** The appointers used to
    /// be joined with `", "`, with the role labels left unquoted so a nickname
    /// could not pass as one. The forging primitive is the COMMA, not the
    /// quote: a plain-ASCII nickname of `Bob, the room owner, Carol` produced
    ///
    /// ```text
    /// Deputy (appointed by "Bob, the room owner, Carol"). Can ban you.
    /// ```
    ///
    /// against the legitimate `appointed by "Bob", the room owner, "Carol"`.
    /// Two quote glyphs shift position; nobody reads that at tooltip size. Any
    /// quote character works for the decorated variant (`U+201C`, `U+2033`,
    /// `U+FF02`, `U+00AB`…), and the payload above needs no quote at all, so
    /// denylisting characters cannot close it. Sanitising cannot either: every
    /// byte of the payload is a legitimate name character.
    ///
    /// So the tooltip names no member. The actual names are surfaced by the
    /// member-info modal, which renders them as SEPARATE elements where a
    /// comma inside one of them cannot span two of them.
    fn appointer_phrase(&self) -> String {
        let has = |want: &Appointer| self.deputized_by.contains(want);
        let others = self
            .deputized_by
            .iter()
            .filter(|a| matches!(a, Appointer::Member(_)))
            .count();

        let mut parts: Vec<String> = Vec::new();
        if has(&Appointer::Owner) {
            parts.push("the room owner".to_string());
        }
        if has(&Appointer::You) {
            parts.push("you".to_string());
        }
        match others {
            0 => {}
            1 => parts.push("another member".to_string()),
            n => parts.push(format!("{n} other members")),
        }

        // At most three parts (owner, you, the count), so this covers every
        // case without a general list-joiner.
        match parts.len() {
            0 => "nobody".to_string(),
            1 => parts.remove(0),
            2 => format!("{} and {}", parts[0], parts[1]),
            _ => format!("{}, {} and {}", parts[0], parts[1], parts[2]),
        }
    }

    /// The `title=` text: who appointed them, and what that means for you.
    ///
    /// One definition for all three surfaces (message author line, member-list
    /// row, member-info modal chip) so the shield cannot say different things
    /// in different places. That is the drift freenet/river#451 fixed for
    /// visibility, applied to the wording too.
    pub fn tooltip(&self) -> String {
        let appointers = self.appointer_phrase();
        match self.can_ban_viewer {
            Some(true) => format!("Deputy (appointed by {appointers}). Can ban you."),
            Some(false) => format!("Deputy (appointed by {appointers}). Cannot ban you."),
            None => format!("Deputy (appointed by {appointers})"),
        }
    }

    /// The appointers as individual display strings, for a surface that can
    /// render them as SEPARATE elements.
    ///
    /// Do NOT join these into one string — that reintroduces exactly the
    /// forgery [`Self::appointer_phrase`] exists to prevent. The only caller is
    /// the member-info modal, which emits one node per entry.
    pub fn appointer_names(&self) -> Vec<String> {
        self.deputized_by
            .iter()
            .map(|a| match a {
                Appointer::Owner => "the room owner".to_string(),
                Appointer::You => "you".to_string(),
                Appointer::Member(name) => name.clone(),
            })
            .collect()
    }
}

/// Every member who shows a 🛡 shield in `self_member_id`'s view, keyed by
/// member id. Absent from the map ⇒ no shield.
///
/// Computed once per render rather than per message: the conversation asks this
/// for every message author, and the underlying `deputizers_of` /
/// `viewer_relevant` maps are O(members) to build.
///
/// Deliberately built from the SAME helpers as the member-list row and the
/// member-info modal legend, so the shield cannot mean one thing in the
/// conversation and another in the sidebar — the drift freenet/river#451 fixed
/// between the row and the modal.
pub(crate) fn deputy_badges_for_viewer(
    members: &MembersV1,
    member_info: &river_core::room_state::member_info::MemberInfoV1,
    room_secrets: &HashMap<u32, [u8; 32]>,
    owner_id: MemberId,
    self_member_id: MemberId,
) -> HashMap<MemberId, DeputyBadge> {
    let deputizers_of = build_deputizers_of(member_info);
    let viewer_relevant = viewer_relevant_deputizer_set(members, owner_id, self_member_id);
    // `is_ban_authorized` needs this map; build it once for the whole sweep.
    let members_by_id = members.members_by_member_id();

    deputizers_of
        .keys()
        .filter_map(|&target| {
            badge_for_target(
                &members_by_id,
                member_info,
                room_secrets,
                &deputizers_of,
                &viewer_relevant,
                owner_id,
                self_member_id,
                target,
            )
            .map(|badge| (target, badge))
        })
        .collect()
}

/// The impersonation checker for one viewer's view of one room (#488 follow-up).
///
/// ## The protected set is derived from room state, never hard-coded
///
/// It is exactly **the room owner, plus every member who shows a 🛡 shield in
/// this viewer's view**, each under whatever nickname they currently hold. That
/// is deliberately the SAME notion of "deputy" the shield uses — this takes
/// `deputy_badges` as a parameter rather than recomputing it, so there is
/// physically only one answer to "who is a deputy" and a deputize/revoke moves
/// the protected set with it on the next render. No list to maintain, and no
/// second definition to drift.
///
/// Reusing the viewer-relative badge map also makes the *scope* right: a
/// protected name is one whose authority this viewer would be fooled by, which
/// is precisely the set whose shield they can see.
///
/// ## Two filters on which names get protected
///
/// * **No `member_info` record ⇒ not protected.** Such a member renders as
///   `"Unknown"`, and `"Unknown"` is what EVERY member without a record renders
///   as — protecting it would flag all of them for impersonating each other.
/// * **A generated handle ⇒ not protected.** A member who never typed a
///   nickname wears one of the 10,000 handles River derived from their key
///   ([`crate::nickname::is_generated_handle`]). Two members can be assigned the
///   same one, which is a collision River created rather than an imitation
///   either of them performed; with ~120 members in a room it is more likely
///   than not that some pair shares a handle. Protecting an unclaimed name would
///   turn that birthday collision into an accusation.
///
/// `privileged_ids` is deliberately WIDER than `protected`: it holds every
/// deputy and the owner, including the ones whose names are filtered out above.
/// The identity exemption must never depend on a name — see
/// [`ImpersonationChecker::check`].
pub(crate) fn impersonation_checker_for_viewer(
    member_info: &river_core::room_state::member_info::MemberInfoV1,
    room_secrets: &HashMap<u32, [u8; 32]>,
    owner_id: MemberId,
    deputy_badges: &HashMap<MemberId, DeputyBadge>,
) -> ImpersonationChecker {
    // The name this member has CLAIMED, or `None` if they have not claimed one.
    //
    // `display_nickname` can return three strings the member never chose, and
    // ALL THREE must be filtered — each is a pure function of something other
    // than the nickname, so it is shared by a whole CLASS of members, and
    // protecting it accuses every one of them at once:
    //
    //   * no `member_info` record  -> `"Unknown"`   (the `?` below)
    //   * sanitises to empty       -> `UNNAMED`
    //   * decryption failed        -> `"[Encrypted: {len} bytes, v{version}]"`
    //
    // The third is the worst. In a private room the viewer's `secrets` map is
    // `#[serde(skip)]` and rebuilt after every ingestion, so there is a real
    // window where it is empty or missing a version. During it the owner's name
    // becomes e.g. `"[Encrypted: 12 bytes, v0]"` — and so does the name of
    // EVERY member whose nickname is also 12 bytes at v0. In a large room that
    // is a dozen simultaneous false accusations with no attacker present.
    //
    // The unseal is tested DIRECTLY rather than by matching the `"[Encrypted:"`
    // prefix, so a future change to the placeholder's wording cannot silently
    // reopen this. (`conversation.rs` filters `UNNAMED` out of mention
    // autocomplete for the same reason.)
    let claimed_name = |id: MemberId| -> Option<String> {
        let sealed = &member_info.canonical(id)?.member_info.preferred_nickname;
        if unseal_bytes_with_secrets(sealed, room_secrets).is_err() {
            return None;
        }
        let name = display_nickname(sealed, room_secrets);
        if name == crate::util::display_name::UNNAMED {
            return None;
        }
        (!crate::nickname::is_generated_handle(&name)).then_some(name)
    };

    let mut protected = Vec::new();
    if let Some(name) = claimed_name(owner_id) {
        protected.push(ProtectedName::new(ProtectedRole::Owner, name, owner_id));
    }
    // Sorted, because `deputy_badges` is a `HashMap` and two deputies can share
    // a skeleton (a sockpuppet deputised under a real deputy's name). Raw map
    // order would let the tooltip name a different victim between renders — the
    // same nondeterminism `build_deputizers_of` sorts away for the shield.
    let mut deputies: Vec<MemberId> = deputy_badges.keys().copied().collect();
    deputies.sort_unstable();
    for id in deputies {
        // **The protected set must not be attacker-WRITABLE.**
        //
        // `deputy_badges` badges anyone deputised by a viewer-relevant
        // appointer, and `viewer_relevant_deputizer_set` is "the viewer's
        // strict ancestors ∪ the viewer". Self-deputisation is already
        // ignored, but a TWO-ACCOUNT attacker is not: any strict ancestor of
        // the viewer — in a room where invites get reshared, that is a large
        // set — can deputise a sockpuppet and choose its nickname freely.
        //
        // Without this gate that nickname became a protected NAME, so an
        // attacker could type an innocent member's display name into a
        // sockpuppet and have the innocent member render with
        // "this member is NOT a moderator" across the attacker's whole invite
        // subtree, on demand, invisibly to the victim. That is precisely the
        // false-accusation harm the tier-1-only decision exists to avoid,
        // except aimed rather than accidental.
        //
        // So only a deputy appointed by the OWNER (or by the viewer
        // themselves, who is trusting their own grant) contributes a name.
        // That matches the real topology — Ian appoints the moderators — and
        // makes the protected set unwritable by anyone else.
        let badge = &deputy_badges[&id];
        let trusted_grant = badge.deputized_by.contains(&Appointer::Owner)
            || badge.deputized_by.contains(&Appointer::You);
        if !trusted_grant {
            continue;
        }
        if let Some(name) = claimed_name(id) {
            protected.push(ProtectedName::new(ProtectedRole::Deputy, name, id));
        }
    }

    ImpersonationChecker::new(protected)
}

/// The impersonation warning a surface actually RENDERS for one member, if any.
///
/// **Every render surface must go through this**, not
/// [`ImpersonationChecker::check`] directly, because it carries the tier
/// decision — and a surface that skipped it would show a badge the other
/// surfaces do not, which is exactly the cross-surface drift freenet/river#451
/// fixed for the shield.
///
/// ## Only [`ConfusableTier::Identical`] is rendered
///
/// The engine also reports [`ConfusableTier::NearMiss`] (within a small,
/// length-scaled edit distance). This UI drops it, on measured evidence rather
/// than taste: River assigns every member one of 10,000 generated handles, and
/// `Amber Worm` / `Ember Worm` are one edit apart, so the near-miss tier accuses
/// a pair of members River itself created — before any attacker acts. Tier 1 is
/// clean on the same population (no two handles share a skeleton). Both facts
/// are pinned in [`crate::util::confusable`] by
/// `generated_handles_never_fold_to_the_same_skeleton` and
/// `generated_handles_are_within_the_near_miss_budget`.
///
/// The cost is real and accepted: `Ian Clark` no longer warns for the moderator
/// `Ian Clarke`. That is the right trade here, because a false accusation is
/// not a symmetric error — it lands on an innocent member, it is visible to the
/// whole room, and a badge that fires on ordinary near-duplicates trains people
/// to ignore the badge that catches the real thing. What remains for the
/// near-miss case is the disambiguator River already shows on both surfaces:
/// the member ID, on hover, next to every name.
///
/// So the rendered signal means something narrow and checkable: **these two
/// names fold to the same skeleton — they are visually the same string.**
pub(crate) fn impersonation_warning_for_display(
    checker: &ImpersonationChecker,
    member_id: MemberId,
    display_name: &str,
) -> Option<ImpersonationWarning> {
    // `check_identical` rather than `check(..).filter(..)`: the filtered form
    // still ran the full Damerau sweep for every NON-matching member on every
    // render, in both surfaces, only to throw the result away.
    let warning = checker.check_identical(member_id, display_name);
    debug_assert!(
        warning
            .as_ref()
            .is_none_or(|w| w.tier == ConfusableTier::Identical),
        "only the Identical tier may reach a render surface"
    );
    warning
}

/// [`deputy_badges_for_viewer`] for a SINGLE member.
///
/// The member-info modal wants one member's badge and is not memoised, so
/// building the whole map there would decrypt every deputy's appointer
/// nicknames on every render — in a private room that is an ECIES unseal per
/// appointer per keystroke elsewhere in the app. Same predicate, same tooltip;
/// only the sweep is narrowed.
pub(crate) fn deputy_badge_for_viewer(
    members: &MembersV1,
    member_info: &river_core::room_state::member_info::MemberInfoV1,
    room_secrets: &HashMap<u32, [u8; 32]>,
    owner_id: MemberId,
    self_member_id: MemberId,
    target: MemberId,
) -> Option<DeputyBadge> {
    let deputizers_of = build_deputizers_of(member_info);
    let viewer_relevant = viewer_relevant_deputizer_set(members, owner_id, self_member_id);
    badge_for_target(
        &members.members_by_member_id(),
        member_info,
        room_secrets,
        &deputizers_of,
        &viewer_relevant,
        owner_id,
        self_member_id,
        target,
    )
}

/// The single definition of "does `target` show a shield to this viewer, and
/// what does its tooltip say". Both public entry points route through here so
/// the sweep and the single lookup cannot disagree.
#[allow(clippy::too_many_arguments)]
fn badge_for_target(
    members_by_id: &HashMap<MemberId, &AuthorizedMember>,
    member_info: &river_core::room_state::member_info::MemberInfoV1,
    room_secrets: &HashMap<u32, [u8; 32]>,
    deputizers_of: &HashMap<MemberId, Vec<MemberId>>,
    viewer_relevant: &HashSet<MemberId>,
    owner_id: MemberId,
    self_member_id: MemberId,
    target: MemberId,
) -> Option<DeputyBadge> {
    // The OWNER never carries a deputy shield. Their authority is inherent,
    // not delegated, so "Deputy (appointed by …)" would be false. And nothing
    // in `MemberInfoV1::verify` stops a member from listing the owner in their
    // own `deputies`, which would otherwise let anyone in the viewer's invite
    // chain paint a fabricated appointment onto the owner.
    if target == owner_id {
        return None;
    }
    let deputized_by = relevant_appointers(
        member_info,
        room_secrets,
        deputizers_of,
        viewer_relevant,
        owner_id,
        self_member_id,
        target,
    );
    if deputized_by.is_empty() {
        return None;
    }
    let can_ban_viewer = (target != self_member_id).then(|| {
        // A ban CASCADES to the banned member's whole invite subtree
        // (`MembersV1::get_downstream_members`), so "can they ban you" is not
        // just `is_ban_authorized(target, viewer)`: someone who can ban anyone
        // the viewer descends from removes the viewer too. `viewer_relevant`
        // is exactly {owner} ∪ the viewer's strict ancestors ∪ {viewer}, which
        // is the full victim set; the owner is never a valid ban target so
        // that entry always answers `false`.
        viewer_relevant.iter().any(|&victim| {
            MembersV1::is_ban_authorized(target, victim, members_by_id, member_info, owner_id)
        })
    });
    Some(DeputyBadge {
        deputized_by,
        can_ban_viewer,
    })
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
        // and so the list can be ordered by deputizer. Shared with the
        // member-info modal legend via `build_deputizers_of` so the two never
        // drift (freenet/river#451).
        let deputizers_of = build_deputizers_of(member_info);

        // The relevance set for BOTH the 🛡 badge and the display ordering
        // (#410, Ian's final call): a deputizer matters to this viewer if it is
        // a strict ancestor of the viewer (their deputy could ban the viewer)
        // OR is the viewer themselves (the viewer appointed the deputy). Shared
        // with the modal legend via `viewer_relevant_deputizer_set` (#451).
        let viewer_relevant = viewer_relevant_deputizer_set(members, owner_id, self_member_id);

        // The badge (visibility + tooltip) for every member, shared verbatim
        // with the conversation's author lines.
        let deputy_badges =
            deputy_badges_for_viewer(members, member_info, room_secrets, owner_id, self_member_id);

        // The ⚠ impersonation checker, built ONCE here from the badge map above
        // — never inside the per-member loop below, which would re-fold every
        // protected name (and, in a private room, re-unseal every protected
        // nickname) once per member. `check` is the per-member hot path.
        let impersonation =
            impersonation_checker_for_viewer(member_info, room_secrets, owner_id, &deputy_badges);

        // Order the list as a DISPLAY tree, VIEWER-SCOPED: a member renders under
        // a deputizer only if that deputizer is viewer-relevant — so a global mod
        // rises to the top for everyone (including the owner's own view), a
        // non-owner's deputy rises within that member's subtree and in that
        // member's own view, and a deputy you appointed rises under you (#410).
        let ordered_ids = deputy_display_order(owner_id, members, &deputizers_of, &viewer_relevant);

        // Build display list in tree order
        let mut all_members = Vec::new();
        for &member_id in &ordered_ids {
            let is_owner = member_id == owner_id;

            let nickname = member_info
                .canonical(member_id)
                .map(|mi| display_nickname(&mi.member_info.preferred_nickname, room_secrets))
                .unwrap_or_else(|| "Unknown".to_string());

            // Computed from the RENDERED `nickname` above, not the raw sealed
            // bytes: the checker compares what the reader actually sees, so it
            // must be fed post-`display_nickname` text. Feeding it the raw
            // nickname would compare a string nobody is shown.
            let impersonation_warning =
                impersonation_warning_for_display(&impersonation, member_id, &nickname);

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
                // Same map the conversation's author line uses (#451), so the
                // shield's visibility AND its tooltip stay identical across
                // the sidebar, the modal and the conversation.
                deputy_badge: deputy_badges.get(&member_id).cloned(),
                impersonation: impersonation_warning,
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
                        // `truncate` used to sit on the BUTTON, which clipped
                        // the badge spans along with the name: a long enough
                        // nickname pushed the ⚠ (and its `title`/`aria-label`)
                        // out of view entirely. That is reachable well inside
                        // the nickname limit, because the sanitiser
                        // deliberately does NOT normalise `U+3000` IDEOGRAPHIC
                        // SPACE — `"Ian Clarke" + "\u{3000}".repeat(13) + "."`
                        // renders as `Ian Clarke…` with the warning clipped off.
                        //
                        // Now the row is a flex line: only the NAME truncates,
                        // and every badge is `flex-shrink-0` so it can never be
                        // clipped, whatever the name's length.
                        button {
                            class: "w-full text-left px-3 py-1.5 rounded-lg text-sm text-text hover:bg-surface transition-colors flex items-center min-w-0",
                            title: "Member ID: {member_id}",
                            onclick: move |_| handle_member_click(member_id),
                            // Nickname rendered as a plain text node — attacker-controlled
                            // bytes from `MemberInfoV1.preferred_nickname` MUST NOT be
                            // routed through `dangerous_inner_html` (freenet/river#227).
                            span { class: "truncate min-w-0", "{parts.nickname}" }
                            for (icon, tooltip) in parts.tags {
                                span {
                                    class: "member-icon flex-shrink-0",
                                    title: "{tooltip}",
                                    // The glyph alone means nothing to a
                                    // screen reader, and the deputy shield is
                                    // now a security-relevant signal.
                                    "aria-label": "{tooltip}",
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

/// Build the [`RoomData`](crate::room_data::RoomData) for a **brand-new**
/// imported room (one this client has never seen).
///
/// Pure (no signal access) so it is unit-testable. The room state starts
/// empty (`is_awaiting_initial_sync()`), so the synchronizer takes the
/// GET-first path and fills it from the network. This is used ONLY for the
/// new-room path; an OVERWRITE of an existing room instead swaps the identity
/// in place via [`swap_room_identity_in_place`] and KEEPS the room's state
/// (room state is identity-independent — freenet/river#414 redesign).
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

/// COMPILE-TIME CLASSIFICATION PIN (Fable architectural review): the exhaustive
/// destructure below has NO `..`, so ADDING A FIELD to `RoomData` FAILS
/// COMPILATION here until a maintainer classifies its identity-overwrite verb in
/// [`swap_room_identity_in_place`]. This is the forcing function that prevents
/// the field-clobber class of bugs (a new field silently kept/defaulted without a
/// decision). Verb table — KEEP / REPLACE / MERGE-same-key / CLEAR+RECOMPUTE:
///
/// ```text
/// owner_vk                KEEP (the room owner; identity-independent)
/// room_state              KEEP (identity-independent shared contract state)
/// self_sk                 REPLACE (the imported identity)
/// contract_key            KEEP (owner+WASM derived)
/// last_read_message_id    KEEP (local read-tracking preference)
/// secrets                 CLEAR+RECOMPUTE different-key (repopulate_secrets_from_state) / KEEP same-key
/// current_secret_version  CLEAR+RECOMPUTE different-key / KEEP same-key
/// last_secret_rotation    CLEAR different-key / KEEP same-key
/// key_migrated_to_delegate REPLACE (false — the new key isn't migrated yet)
/// self_authorized_member  \ REPLACE different-key / MERGE-keep-if-absent same-key
/// invite_chain            / (paired coherent unit)
/// self_member_info        REPLACE different-key / MERGE-keep-newer same-key
/// self_nickname           REPLACE different-key / MERGE-keep-if-absent same-key
/// previous_contract_key   KEEP (room-scoped #292 migration pointer)
/// invitation_secrets      REPLACE different-key / MERGE-union(existing wins) same-key
/// ```
#[allow(dead_code)]
fn _room_data_swap_classification(rd: crate::room_data::RoomData) {
    let crate::room_data::RoomData {
        owner_vk: _,
        room_state: _,
        self_sk: _,
        contract_key: _,
        last_read_message_id: _,
        secrets: _,
        current_secret_version: _,
        last_secret_rotation: _,
        key_migrated_to_delegate: _,
        self_authorized_member: _,
        invite_chain: _,
        self_member_info: _,
        self_nickname: _,
        previous_contract_key: _,
        invitation_secrets: _,
    } = rd;
}

/// Merge two optional `AuthorizedMemberInfo` records, keeping the HIGHER-version
/// one (a same-key re-import must not let a stale token clobber a newer local
/// member_info; an absent incoming keeps local). Ties keep `local`.
fn merge_keep_newer_member_info(
    local: Option<river_core::room_state::member_info::AuthorizedMemberInfo>,
    incoming: Option<river_core::room_state::member_info::AuthorizedMemberInfo>,
) -> Option<river_core::room_state::member_info::AuthorizedMemberInfo> {
    match (local, incoming) {
        (Some(l), Some(i)) => {
            if i.member_info.version > l.member_info.version {
                Some(i)
            } else {
                Some(l)
            }
        }
        (Some(l), None) => Some(l),
        (None, i) => i,
    }
}

/// Swap a room's identity IN PLACE for an imported one, **keeping the existing
/// `room_state`** (freenet/river#414 redesign).
///
/// Room state is identity-independent: it is shared contract state fetched by
/// the room's contract key, the same for every member. Only `self_sk` (and the
/// membership proof it signs) is identity-specific. So an overwrite must NOT
/// rebuild the room from an empty `ChatRoomStateV1::default()` and re-fetch —
/// that empty-rebuild was the root of the sync-reset / stale-load / bogus-delta
/// cluster. Instead we replace the identity-specific fields from the export and
/// keep `room_state`, `contract_key`, `previous_contract_key`, and the local
/// `last_read_message_id`.
///
/// Ordering matters: `self_sk` is set BEFORE `repopulate_secrets_from_state`,
/// which decrypts the owner-signed `encrypted_secrets` blobs addressed to *this
/// member* — a local recompute against the kept state, no network fetch.
///
/// An overwrite may hand the room to a DIFFERENT identity, so we must NOT carry
/// the OLD identity's decrypt access forward (Codex round-6 P1-2): the old
/// identity's in-memory decrypted `secrets` (plus `current_secret_version` /
/// `last_secret_rotation`) are cleared, and the invitation-carried secrets are
/// REPLACED with the new identity's (not unioned — an A-only version B has no
/// contract blob for must not remain readable). `repopulate_secrets_from_state`
/// then rebuilds only what the NEW identity can actually decrypt from the kept
/// state, and `rebuild_private_actions_state` re-derives the edit/reaction
/// action cache under the new identity's (possibly narrower) secret access.
///
/// Returns whether the signing key actually changed (drives the DM-cache
/// prune). Pure (no signals) so the overwrite is unit-testable.
fn swap_room_identity_in_place(
    existing: &mut crate::room_data::RoomData,
    export: IdentityExport,
) -> bool {
    let key_changed = existing.self_sk != export.signing_key;
    existing.self_sk = export.signing_key;

    if key_changed {
        // DIFFERENT identity (a genuine swap): REPLACE all identity metadata with
        // the imported one's, and do NOT carry the OLD identity's decrypt access
        // forward. Clear the in-memory decrypted secrets and the derived version
        // pointers (`#[serde(skip)]` runtime caches, rebuilt below), and REPLACE
        // the invitation-carried secrets — so an A-only version B has no contract
        // blob for cannot remain readable by B (Codex round-6 P1-2).
        existing.self_authorized_member = Some(export.authorized_member);
        existing.invite_chain = export.invite_chain;
        existing.self_member_info = export.member_info;
        existing.self_nickname = export.self_nickname;
        existing.secrets.clear();
        existing.current_secret_version = None;
        existing.last_secret_rotation = None;
        existing.invitation_secrets = export.invitation_secrets;
    } else {
        // SAME identity re-import (the user re-importing their OWN token, e.g. a
        // legacy/stale one). A backward-compat decode of an old token yields
        // ABSENT/EMPTY optional fields; blindly assigning them would ERASE richer
        // local state. GENERAL RULE (Codex round-7/8): for EVERY field the token
        // may carry absent/stale, RETAIN the existing local value when the
        // incoming one is `None`/empty, and never replace a NEWER cached record
        // with a STALER one. Applied to every identity-metadata field:
        //
        // - membership proof (`self_authorized_member` + `invite_chain`) is a
        //   coherent UNIT (the chain validates the member), so keep them TOGETHER:
        //   adopt the token's pair only when the local membership proof is absent;
        //   CROSS-REF: the CLI counterpart
        //   (cli/src/storage.rs `import_room_atomic`) gates the equivalent step on
        //   the TOKEN's chain being empty, not the LOCAL proof being absent — the
        //   two conditions differ DELIBERATELY; do NOT harmonize them into one rule.
        if existing.self_authorized_member.is_none() {
            existing.self_authorized_member = Some(export.authorized_member);
            existing.invite_chain = export.invite_chain;
        }
        // - `self_member_info`: keep the higher-version record (a stale token must
        //   not clobber a newer local member_info; absent token keeps local);
        existing.self_member_info =
            merge_keep_newer_member_info(existing.self_member_info.take(), export.member_info);
        // - `self_nickname`: a present token value is a deliberate choice; an
        //   absent one must NOT erase the locally-chosen nickname (else a later
        //   inactivity prune + rejoin would publish a generated default);
        if export.self_nickname.is_some() {
            existing.self_nickname = export.self_nickname;
        }
        // - `invitation_secrets`: MERGE (existing wins) — the token may be empty
        //   and the local map can be the ONLY copy of a private room's key, so
        //   wiping it would make history permanently unreadable (Codex round-7).
        for (version, secret) in export.invitation_secrets {
            existing.invitation_secrets.entry(version).or_insert(secret);
        }
        // - decrypted caches (`secrets`/`current_secret_version`/
        //   `last_secret_rotation`) are PRESERVED (not cleared) — same identity,
        //   same decrypt access.
    }

    // The (re)imported key has not been stored in the delegate yet.
    existing.key_migrated_to_delegate = false;
    // Recompute the in-memory decrypted secrets from the KEPT room_state — local,
    // no network fetch — then re-derive the action cache (edits/deletes/reactions)
    // under those secrets. For a same-key re-import this only tops up (repopulate
    // never overwrites an existing decrypted version); for a genuine swap it
    // rebuilds exactly what the new identity can decrypt.
    existing.repopulate_secrets_from_state();
    existing.rebuild_private_actions_state();
    key_changed
}

/// Whether the room set is COMPLETELY loaded — safe to decide new-vs-overwrite.
///
/// Under the in-place redesign this decision is safety-critical: deciding
/// "no room" on an incomplete view would route a real, populated room to the
/// build-empty NEW path and overwrite its populated persisted slot (data loss).
///
/// Requires ALL of (Codex round-6 P1-1, round-9 P1):
/// - the startup delegate load RESOLVED (`Loaded`) — `Loading`/`Migrating` mean
///   "we don't know the set yet"; `LoadFailed` means a known room failed;
/// - NO listed room's fetch failed (`!saw_fetch_failure`). `per_room_terminal`
///   resolves to `Loaded` the instant ≥1 room materialized even if OTHER listed
///   rooms failed to hydrate, so `Loaded` alone is not "complete". A room absent
///   from `ROOMS` because its fetch failed would be misclassified as new; and
/// - the freenet/river#345 RECOVERY is not in progress (`!recovery_in_progress`).
///   The interrupted-legacy-migration recovery sets `Loaded` (to render the
///   partial list) BEFORE background workers restore rooms still missing from the
///   partial per-room index. Importing for one of those in that window would
///   overwrite its recovered slot with empty state. So we wait for recovery too.
///
/// If any condition fails the caller refuses the import ("some rooms didn't
/// finish loading — retry") rather than risk classifying a real room as new.
fn rooms_load_is_authoritative(
    state: crate::components::app::chat_delegate::RoomsLoadState,
    saw_fetch_failure: bool,
    recovery_in_progress: bool,
) -> bool {
    matches!(
        state,
        crate::components::app::chat_delegate::RoomsLoadState::Loaded
    ) && !saw_fetch_failure
        && !recovery_in_progress
}

/// Complete an identity import (freenet/river#414 redesign).
///
/// Splits the two genuinely-different cases:
/// - **New room** (not in `ROOMS`): build an empty placeholder + let the
///   GET-first sync fill it — the correct path for a room this client has
///   never seen.
/// - **Overwrite** (already in `ROOMS`): swap the identity IN PLACE and KEEP
///   the existing `room_state`. Room state is identity-independent, so an
///   overwrite must not throw it away and re-fetch (the old empty-rebuild was
///   the root of the sync-reset / stale-load / bogus-delta cluster).
///
/// Reaching this function means the import is committing: every validation error
/// (invalid token, still-loading room set) returns in the CALLER before this is
/// called. So this is the success path: it shows a brief "Identity imported!"
/// flash and then auto-dismisses the dialog via `close` (the caller's
/// `reset_and_close`). The error branches stay in the caller and keep the dialog
/// open showing the error.
///
/// Precondition: the caller has confirmed the room set is authoritative
/// (`rooms_load_is_authoritative`), so the new-vs-overwrite decision below is
/// reliable and can't misclassify a real room as new during startup.
fn complete_identity_import(
    export: IdentityExport,
    mut success_msg: Signal<Option<String>>,
    mut error_msg: Signal<Option<String>>,
    close: impl Fn() + Copy + 'static,
) {
    let owner_key = export.room_owner;
    // Migrate the imported signing key to the delegate immediately. Without
    // this, the delegate may have a stale key from a prior session, causing
    // all message signatures to be rejected by the contract ("State
    // verification failed: Invalid signature").
    let new_sk = export.signing_key.clone();
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
        // Drives the DM-state prune below: true only when an overwrite actually
        // swaps to a different signing key.
        let mut identity_changed = false;
        // For an OVERWRITE only: the new identity's member_info heal, built inside
        // the borrow and sent AFTER it releases (the same ordering the UPDATE-path
        // heal uses). An in-place overwrite does NO GET, so without this the new
        // identity would render "Unknown" to peers until an unrelated future heal
        // (freenet/river#414, Codex round-6 P2-4). A NEW room GET-first-syncs, so
        // its GET-path heal covers it.
        let mut pending_member_info_heal = None;
        ROOMS.with_mut(|rooms| {
            // Importing is an explicit rejoin: clear any leave tombstone and
            // record the rejoin so a remote `Tombstone` slot is overwritten with
            // `Present` rather than adopting the leave (freenet/river#247/#345).
            // Applies to both the new-room and overwrite paths.
            rooms.removed_rooms.remove(&owner_key);
            crate::components::app::chat_delegate::mark_room_rejoined(owner_key);

            match rooms.map.get_mut(&owner_key) {
                // OVERWRITE: swap the identity in place, KEEP room_state.
                Some(existing) => {
                    identity_changed = swap_room_identity_in_place(existing, export);
                    // Build the heal against the KEPT state (secrets were just
                    // repopulated for the new identity by the swap). Returns None
                    // when self isn't stranded, isn't a member, or — for a private
                    // room — the secret isn't available yet (deferred, no leak).
                    pending_member_info_heal =
                        existing.build_member_info_heal(&existing.room_state);
                }
                // NEW room: empty placeholder; the GET-first sync fills it.
                None => {
                    rooms
                        .map
                        .insert(owner_key, build_imported_room_data(export));
                }
            }
        });

        // Send the new identity's member_info heal AFTER the ROOMS borrow is
        // released (freenet/river#414 P2-4). `send_member_info_heal_update`
        // builds the member_info-only UPDATE delta and spawns the send itself; it
        // is self-signed and idempotent, so a race with any other heal is safe.
        if let Some(heal_info) = pending_member_info_heal {
            crate::components::app::freenet_api::room_synchronizer::send_member_info_heal_update(
                owner_key, heal_info,
            );
        }

        // Overwriting a DIFFERENT identity: prune the OLD identity's cached
        // outbound-DM plaintext + archive state so it doesn't leak into (or
        // wrongly hide threads for) the new identity — symmetric to the CLI
        // `identity import --force` prune (freenet/river#414). Only on a real
        // key change; a brand-new import or a same-key re-import prunes nothing.
        // The tombstone is keyed to the NEW identity's MemberId so a late DM
        // response drops entries authored by any replaced identity, regardless of
        // their timestamp (freenet/river#414 round-9 P2).
        if identity_changed {
            let new_member_id = MemberId::from(&new_sk.verifying_key());
            crate::components::app::chat_delegate::prune_dm_state_for_room(
                owner_key,
                new_member_id,
            );
        }

        CURRENT_ROOM.with_mut(|current| {
            current.owner_key = Some(owner_key);
        });

        // Persist the room (the overwrite's new `self_sk` rides in the per-room
        // delegate slot via `save_rooms_to_delegate`, which the NEEDS_SYNC effect
        // fires) and drive a normal sync. For a NEW room the placeholder is
        // `is_awaiting_initial_sync()`, so the synchronizer takes the GET-first
        // path; for an OVERWRITE the kept `room_state` matches `last_synced_state`,
        // so no bogus delta is sent and no re-fetch is forced. The redesign keeps
        // state, so there is NO forced sync-entry reset here (the old empty-rebuild
        // scaffolding is gone — freenet/river#414).
        crate::components::app::mark_needs_sync(owner_key);

        // Migrate signing key to delegate in background
        crate::util::safe_spawn_local(async move {
            // AUTHORITATIVE: the user just imported/chose this identity, so it
            // becomes the room's current identity and supersedes any stale
            // hydration migration for an old key (freenet/river#414 P1).
            let result = crate::signing::migrate_signing_key(room_key_bytes, &new_sk, true).await;
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
                                if rd.self_sk != new_sk {
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

        // Success flash. The pre-redesign wording announced that room state was
        // still being fetched, which was stale from the empty-rebuild era: the
        // in-place swap KEEPS `room_state`, so there is no re-fetch to wait on.
        // Just confirm the import landed.
        success_msg.set(Some("Identity imported!".to_string()));
        error_msg.set(None);
    });

    // Auto-dismiss the dialog after the brief success flash (Ian hit the modal
    // staying open after a successful import). Only the SUCCESS path reaches here,
    // so error branches (which return in the caller) keep the dialog open.
    //
    // Signal-safety (.claude/rules/dioxus-signal-safety.md): the delay uses the
    // WASM-safe `sleep`, and the close runs inside `defer()` (never a raw
    // setTimeout on signal mutations). The close is GUARDED on the success flash
    // still being shown, so if the user manually closed and reopened the modal
    // within the window we don't clobber their fresh state (`reset_and_close`
    // clears `success_msg`, so a reopened modal reads `None` here).
    crate::util::safe_spawn_local(async move {
        crate::util::sleep(crate::util::millis(1200)).await;
        crate::util::defer(move || {
            if success_msg.try_read().is_ok_and(|m| m.is_some()) {
                close();
            }
        });
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

    // Reactive hydration gate (freenet/river#414 redesign, Codex round-6 P1-1 +
    // round-9 P1): the room set must be COMPLETELY loaded AND not mid-recovery
    // before the Import button is enabled, so the new-vs-overwrite decision is
    // never made on an incomplete view (which would build-empty over a real
    // room). Reading `ROOMS_LOAD_STATE` here subscribes the modal so it re-renders
    // when the load resolves; a `ROOMS.try_read()` touch adds a subscription so it
    // ALSO re-renders when #345 recovery hydrates a room (recovery leaves
    // `ROOMS_LOAD_STATE == Loaded` throughout). `saw_fetch_failure()` and
    // `rooms_recovery_in_progress()` are read alongside (non-signal sources).
    let _ = ROOMS.try_read(); // subscribe: re-render when recovery hydrates rooms
    let saw_fetch_failure = crate::components::app::chat_delegate::saw_fetch_failure();
    let rooms_hydrated = crate::components::app::chat_delegate::ROOMS_LOAD_STATE
        .try_read()
        .map(|g| {
            rooms_load_is_authoritative(
                *g,
                saw_fetch_failure,
                crate::components::app::chat_delegate::rooms_recovery_in_progress(),
            )
        })
        .unwrap_or(false);

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

                // Safety-critical (freenet/river#414 redesign, Codex round-6
                // P1-1 + round-9 P1): NEVER decide new-vs-overwrite on an
                // incompletely-loaded OR mid-recovery room set — a false "no room"
                // would route a real, populated room to the build-empty NEW path
                // and overwrite its populated persisted slot. This requires a
                // COMPLETE load (resolved AND no listed room's fetch failed) AND
                // that the #345 recovery has finished; a partial load / pending
                // recovery could be missing the very room being imported. The
                // Import button is disabled until then (see `rooms_hydrated`); this
                // is the defense-in-depth net against a click landing before the
                // render has re-disabled the button.
                let load_state = crate::components::app::chat_delegate::ROOMS_LOAD_STATE
                    .try_read()
                    .map(|g| *g)
                    .unwrap_or(crate::components::app::chat_delegate::RoomsLoadState::Loading);
                let saw_failure = crate::components::app::chat_delegate::saw_fetch_failure();
                let recovery = crate::components::app::chat_delegate::rooms_recovery_in_progress();
                if !rooms_load_is_authoritative(load_state, saw_failure, recovery) {
                    crate::util::defer(move || {
                        error_msg.set(Some(
                            "Still finishing loading your rooms — please wait a moment and \
                             try again."
                                .to_string(),
                        ));
                        success_msg.set(None);
                    });
                    return;
                }

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

                complete_identity_import(export, success_msg, error_msg, reset_and_close);
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
        complete_identity_import(export, success_msg, error_msg, reset_and_close);
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
                    // Until the room set is authoritative, importing could
                    // misclassify a real room as new and overwrite it with empty
                    // state — so the Import button waits for hydration
                    // (freenet/river#414 redesign).
                    if !rooms_hydrated {
                        if saw_fetch_failure {
                            // A partial load FAILED: the rail shows `List` (some
                            // room exists) so its own Retry control is hidden, and
                            // without a way out the import would be blocked
                            // indefinitely. Give the user a working retry that
                            // re-fires the load; the gate clears reactively once
                            // the missing rooms hydrate (freenet/river#414 round-10 P2).
                            div {
                                "data-testid": "import-identity-load-failed",
                                class: "mt-3 text-sm text-amber-400",
                                "Some rooms didn't finish loading, so importing is paused to avoid overwriting one. Retry loading your rooms to continue."
                            }
                            div { class: "flex justify-end mt-2",
                                button {
                                    "data-testid": "import-identity-retry-load",
                                    class: "px-3 py-1.5 bg-surface hover:bg-surface-hover text-text text-xs rounded-lg transition-colors border border-border",
                                    onclick: move |_| {
                                        crate::components::app::chat_delegate::retry_rooms_load();
                                    },
                                    "Retry loading rooms"
                                }
                            }
                        } else {
                            div {
                                "data-testid": "import-identity-loading-rooms",
                                class: "mt-3 text-sm text-text-muted",
                                "Loading your rooms\u{2026} the import will be available in a moment."
                            }
                        }
                    }
                    div { class: "flex justify-end gap-3 mt-4",
                        button {
                            class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text text-sm rounded-lg transition-colors border border-border",
                            onclick: move |_| reset_and_close(),
                            "Cancel"
                        }
                        button {
                            "data-testid": "import-identity-submit",
                            class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white text-sm font-medium rounded-lg transition-colors disabled:opacity-50 disabled:cursor-not-allowed",
                            disabled: !rooms_hydrated,
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

    /// Like [`authorized_member`] but for a NON-owner inviter, so tests can
    /// build an invite tree deeper than one level (the deputy badge is scoped
    /// to invite subtrees, so depth is the whole point).
    fn member_invited_by(
        inviter_sk: &SigningKey,
        owner_id: MemberId,
        invitee_vk: &VerifyingKey,
    ) -> AuthorizedMember {
        let member = Member {
            owner_member_id: owner_id,
            invited_by: MemberId::from(&inviter_sk.verifying_key()),
            member_vk: *invitee_vk,
        };
        AuthorizedMember::new(member, inviter_sk)
    }

    /// A signed `member_info` record with a chosen nickname and deputy list.
    fn signed_member_info(
        sk: &SigningKey,
        nickname: &str,
        deputies: Vec<MemberId>,
    ) -> river_core::room_state::member_info::AuthorizedMemberInfo {
        use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
        let mi = MemberInfo {
            member_id: MemberId::from(&sk.verifying_key()),
            version: 0,
            preferred_nickname: river_core::room_state::privacy::SealedBytes::public(
                nickname.as_bytes().to_vec(),
            ),
            deputies,
        };
        AuthorizedMemberInfo::new_with_member_key(mi, sk)
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
    /// `swap_room_identity_in_place` in the overwrite tests.
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

    /// freenet/river#414 REDESIGN: overwriting an existing room's identity swaps
    /// `self_sk` (and the membership proof) IN PLACE while KEEPING the existing
    /// `room_state`. Room state is identity-independent shared contract state, so
    /// an overwrite must never throw it away and rebuild empty (the old bug that
    /// caused the sync-reset / bogus-delta cluster).
    #[test]
    fn overwrite_swaps_identity_in_place_keeping_room_state() {
        let owner_sk = SigningKey::from_bytes(&[43u8; 32]);
        let old_sk = SigningKey::from_bytes(&[44u8; 32]);
        let new_sk = SigningKey::from_bytes(&[45u8; 32]);
        assert_ne!(old_sk.to_bytes(), new_sk.to_bytes());

        // Existing room under the OLD identity with a POPULATED, distinguishable
        // state (a member added) so we can assert the state is KEPT.
        let mut existing = build_imported_room_data(export_for(&owner_sk, &old_sk));
        let member_vk = SigningKey::from_bytes(&[77u8; 32]).verifying_key();
        existing
            .room_state
            .members
            .members
            .push(authorized_member(&owner_sk, &member_vk));
        existing.key_migrated_to_delegate = true; // pretend the old key was migrated
        let kept_state = existing.room_state.clone();
        assert_ne!(
            kept_state,
            river_core::room_state::ChatRoomStateV1::default(),
            "precondition: the existing room has non-empty state"
        );

        // Overwrite with the NEW identity.
        let key_changed =
            swap_room_identity_in_place(&mut existing, export_for(&owner_sk, &new_sk));

        assert!(key_changed, "swapping to a different key reports a change");
        assert_eq!(
            existing.self_sk.to_bytes(),
            new_sk.to_bytes(),
            "self_sk must become the imported identity"
        );
        assert_eq!(
            existing.room_state, kept_state,
            "room_state must be KEPT untouched (identity-independent) — NOT rebuilt empty"
        );
        assert!(
            !existing.key_migrated_to_delegate,
            "the new key hasn't been migrated to the delegate yet"
        );
    }

    /// freenet/river#414 REDESIGN: an overwrite of a PRIVATE room repopulates the
    /// new identity's in-memory decrypted secrets from the KEPT state — a local
    /// recompute (here via the invitation-carried secret fold), never a network
    /// fetch.
    #[test]
    fn overwrite_repopulates_private_room_secrets_for_new_identity() {
        let owner_sk = SigningKey::from_bytes(&[61u8; 32]);
        let old_sk = SigningKey::from_bytes(&[62u8; 32]);
        let new_sk = SigningKey::from_bytes(&[63u8; 32]);

        // Existing PRIVATE room under the old identity (no secrets loaded yet).
        let mut existing = build_imported_room_data(export_for(&owner_sk, &old_sk));
        existing.room_state.configuration.configuration.privacy_mode =
            river_core::room_state::privacy::PrivacyMode::Private;
        assert!(existing.is_private());
        assert!(existing.secrets.is_empty());

        // The imported (new) identity carries an invitation secret at v0.
        let mut export = export_for(&owner_sk, &new_sk);
        export.invitation_secrets.insert(0u32, [0xABu8; 32]);

        swap_room_identity_in_place(&mut existing, export);

        assert_eq!(existing.self_sk.to_bytes(), new_sk.to_bytes());
        assert_eq!(
            existing.secrets.get(&0u32),
            Some(&[0xABu8; 32]),
            "overwrite must repopulate the new identity's in-memory secrets from kept state"
        );
    }

    /// freenet/river#414 REDESIGN (Codex round-6 P1-2): an overwrite must NOT
    /// carry the OLD identity's decrypt access forward. The old identity's
    /// in-memory decrypted `secrets` are cleared, and invitation-carried secrets
    /// are REPLACED (not unioned) with the new identity's — so a version only the
    /// old identity could read does not remain readable by the new one.
    #[test]
    fn overwrite_drops_old_identity_decrypt_access() {
        let owner_sk = SigningKey::from_bytes(&[71u8; 32]);
        let old_sk = SigningKey::from_bytes(&[72u8; 32]);
        let new_sk = SigningKey::from_bytes(&[73u8; 32]);

        // Existing PRIVATE room where the OLD identity had decrypted secret v9
        // and an invitation secret v5 the NEW identity has no blob for.
        let mut existing = build_imported_room_data(export_for(&owner_sk, &old_sk));
        existing.room_state.configuration.configuration.privacy_mode =
            river_core::room_state::privacy::PrivacyMode::Private;
        existing.secrets.insert(9u32, [0x11u8; 32]);
        existing.current_secret_version = Some(9);
        existing.last_secret_rotation = Some(std::time::SystemTime::now());
        existing.invitation_secrets.insert(5u32, [0x22u8; 32]);

        // The imported (new) identity carries only invitation secret v0.
        let mut export = export_for(&owner_sk, &new_sk);
        export.invitation_secrets.insert(0u32, [0x33u8; 32]);

        swap_room_identity_in_place(&mut existing, export);

        // The old identity's decrypted secret v9 is gone (cleared before repopulate).
        assert!(
            !existing.secrets.contains_key(&9u32),
            "old identity's decrypted secret must be cleared, not carried forward"
        );
        // invitation_secrets REPLACED: the old identity's v5 must be gone…
        assert!(
            !existing.invitation_secrets.contains_key(&5u32),
            "old identity's invitation secret must NOT be unioned into the new identity"
        );
        // …and only the NEW identity's v0 remains (folded into secrets by repopulate).
        assert_eq!(existing.secrets.get(&0u32), Some(&[0x33u8; 32]));
    }

    /// freenet/river#414 (Codex round 7): a SAME-key re-import (the user
    /// re-importing their OWN identity from a legacy/stale token whose
    /// `invitation_secrets` may be empty) must PRESERVE the existing secrets, not
    /// clear+replace them. For a private room still awaiting the owner backfill,
    /// the existing `invitation_secrets` map can be the ONLY copy of the room key
    /// — wiping it would make history permanently unreadable.
    #[test]
    fn same_key_reimport_preserves_existing_secrets() {
        let owner_sk = SigningKey::from_bytes(&[81u8; 32]);
        // The SAME signing key for both the existing room and the re-import.
        let self_sk = SigningKey::from_bytes(&[82u8; 32]);

        // Existing PRIVATE room where the (same) identity holds the ONLY copy of
        // the room key at v7, plus its decrypted secret in memory.
        let mut existing = build_imported_room_data(export_for(&owner_sk, &self_sk));
        existing.room_state.configuration.configuration.privacy_mode =
            river_core::room_state::privacy::PrivacyMode::Private;
        existing.invitation_secrets.insert(7u32, [0x44u8; 32]);
        existing.secrets.insert(7u32, [0x44u8; 32]);
        existing.current_secret_version = Some(7);

        // Re-import the SAME identity from a stale token with EMPTY secrets.
        let export = export_for(&owner_sk, &self_sk);
        assert!(
            export.invitation_secrets.is_empty(),
            "precondition: the stale token carries no invitation secrets"
        );

        let key_changed = swap_room_identity_in_place(&mut existing, export);

        assert!(!key_changed, "same key must report no change");
        // The room key (v7) survives — history stays readable.
        assert_eq!(
            existing.invitation_secrets.get(&7u32),
            Some(&[0x44u8; 32]),
            "same-key re-import must PRESERVE the existing invitation secret \
             (it can be the only copy of the room key)"
        );
        assert_eq!(
            existing.secrets.get(&7u32),
            Some(&[0x44u8; 32]),
            "same-key re-import must NOT clear the decrypted secret cache"
        );
    }

    fn member_info_v(
        sk: &SigningKey,
        version: u32,
    ) -> river_core::room_state::member_info::AuthorizedMemberInfo {
        use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
        let mi = MemberInfo {
            member_id: MemberId::from(&sk.verifying_key()),
            version,
            preferred_nickname: river_core::room_state::privacy::SealedBytes::public(
                b"nick".to_vec(),
            ),
            deputies: vec![],
        };
        AuthorizedMemberInfo::new_with_member_key(mi, sk)
    }

    /// freenet/river#414 (Codex round-8): `merge_keep_newer_member_info` keeps
    /// the higher-version record and never clobbers a newer local one with a
    /// staler/absent incoming one.
    #[test]
    fn merge_keep_newer_member_info_keeps_higher_version() {
        let sk = SigningKey::from_bytes(&[91u8; 32]);
        let v1 = member_info_v(&sk, 1);
        let v3 = member_info_v(&sk, 3);

        // Newer incoming wins.
        assert_eq!(
            merge_keep_newer_member_info(Some(v1.clone()), Some(v3.clone()))
                .unwrap()
                .member_info
                .version,
            3
        );
        // Staler incoming does NOT clobber the newer local.
        assert_eq!(
            merge_keep_newer_member_info(Some(v3.clone()), Some(v1.clone()))
                .unwrap()
                .member_info
                .version,
            3
        );
        // Absent incoming keeps local; absent local adopts incoming.
        assert_eq!(
            merge_keep_newer_member_info(Some(v3.clone()), None)
                .unwrap()
                .member_info
                .version,
            3
        );
        assert_eq!(
            merge_keep_newer_member_info(None, Some(v1.clone()))
                .unwrap()
                .member_info
                .version,
            1
        );
        assert!(merge_keep_newer_member_info(None, None).is_none());
    }

    /// freenet/river#414 (Codex round-8, systematic same-key audit): a same-key
    /// re-import from a stale token (absent nickname / member_info / invite
    /// chain) must PRESERVE the locally-cached nickname, member_info, and
    /// membership proof — not erase them (which would later publish a generated
    /// default nickname on rejoin, and drop deputy/membership state).
    #[test]
    fn same_key_reimport_preserves_nickname_and_membership() {
        let owner_sk = SigningKey::from_bytes(&[95u8; 32]);
        let self_sk = SigningKey::from_bytes(&[96u8; 32]);

        let mut existing = build_imported_room_data(export_for(&owner_sk, &self_sk));
        existing.self_nickname = Some("Chosen Name".to_string());
        existing.self_member_info = Some(member_info_v(&self_sk, 5));
        existing.self_authorized_member =
            Some(authorized_member(&owner_sk, &self_sk.verifying_key()));
        existing.invite_chain = vec![authorized_member(&owner_sk, &self_sk.verifying_key())];

        // Stale token for the SAME key: absent nickname/member_info, empty chain.
        let mut export = export_for(&owner_sk, &self_sk);
        export.self_nickname = None;
        export.member_info = None;
        assert!(export.invite_chain.is_empty());

        let key_changed = swap_room_identity_in_place(&mut existing, export);

        assert!(!key_changed);
        assert_eq!(
            existing.self_nickname.as_deref(),
            Some("Chosen Name"),
            "an absent token nickname must NOT erase the chosen nickname"
        );
        assert_eq!(
            existing
                .self_member_info
                .as_ref()
                .unwrap()
                .member_info
                .version,
            5,
            "an absent token member_info must NOT erase the local one"
        );
        assert!(
            existing.self_authorized_member.is_some(),
            "the membership proof must survive a stale re-import"
        );
        assert_eq!(
            existing.invite_chain.len(),
            1,
            "an empty token invite_chain must NOT erase the local chain"
        );
    }

    /// freenet/river#414 REDESIGN (safety-critical, Codex round-6 P1-1 + round-9
    /// P1): the new-vs-overwrite decision is authoritative ONLY on a COMPLETE load
    /// — `Loaded` AND no listed room's fetch failed AND the #345 recovery is not
    /// in progress. `Loaded` alone is insufficient (`per_room_terminal` resolves
    /// to `Loaded` the instant ≥1 room materialized, and recovery sets `Loaded`
    /// before restoring still-missing rooms), so a room could be missing from the
    /// map and get misclassified as new.
    #[test]
    fn rooms_load_authoritative_requires_complete_load() {
        use crate::components::app::chat_delegate::RoomsLoadState;
        // Complete load: Loaded, no fetch failure, no recovery → authoritative.
        assert!(rooms_load_is_authoritative(
            RoomsLoadState::Loaded,
            false,
            false
        ));
        // Loaded but a listed room's fetch failed → NOT authoritative.
        assert!(!rooms_load_is_authoritative(
            RoomsLoadState::Loaded,
            true,
            false
        ));
        // Loaded but the #345 recovery is still in progress → NOT authoritative
        // (a still-missing room could be the one being imported = data loss).
        assert!(!rooms_load_is_authoritative(
            RoomsLoadState::Loaded,
            false,
            true
        ));
        // Unresolved / failed / migrating are never authoritative.
        assert!(!rooms_load_is_authoritative(
            RoomsLoadState::Loading,
            false,
            false
        ));
        assert!(!rooms_load_is_authoritative(
            RoomsLoadState::Migrating,
            false,
            false
        ));
        assert!(!rooms_load_is_authoritative(
            RoomsLoadState::LoadFailed,
            false,
            false
        ));
        assert!(!rooms_load_is_authoritative(
            RoomsLoadState::LoadFailed,
            true,
            true
        ));
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
            deputy_badge: None,
            impersonation: None,
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

    /// The 🛡 deputy shield renders exactly when `deputized_by` is non-empty,
    /// and its tooltip names the appointer(s). The member-info modal legend
    /// mirrors this (freenet/river#451) via the shared
    /// `relevant_appointers` helper, so this pins the row half of the
    /// "same shield in both places" contract.
    #[test]
    fn member_display_parts_shows_deputy_shield_with_appointer_tooltip() {
        let mut display = make_member_display("bob");
        assert!(
            !member_display_parts(&display)
                .tags
                .iter()
                .any(|(icon, _)| *icon == "🛡"),
            "no shield when the member is not a deputy"
        );

        display.deputy_badge = Some(DeputyBadge {
            deputized_by: vec![Appointer::Owner],
            can_ban_viewer: Some(true),
        });
        let parts = member_display_parts(&display);
        let shield = parts
            .tags
            .iter()
            .find(|(icon, _)| *icon == "🛡")
            .expect("a deputy must show the 🛡 shield");
        // Identical wording to the conversation author line and the modal
        // chip — all three call `DeputyBadge::tooltip`.
        assert_eq!(
            shield.1,
            "Deputy (appointed by the room owner). Can ban you."
        );

        // On the viewer's OWN row the ban clause is dropped: "can they ban
        // you" is meaningless about yourself.
        display.deputy_badge = Some(DeputyBadge {
            deputized_by: vec![Appointer::Owner],
            can_ban_viewer: None,
        });
        let parts = member_display_parts(&display);
        assert_eq!(
            parts.tags.iter().find(|(icon, _)| *icon == "🛡").unwrap().1,
            "Deputy (appointed by the room owner)"
        );
    }

    /// End-to-end pin of the shared deputy helpers the member row AND the
    /// member-info modal legend both depend on (freenet/river#451): build a
    /// room where the OWNER deputizes `mod_member`, and assert that from an
    /// unrelated viewer's perspective `relevant_appointers` reports the
    /// shield named "room owner" (a global moderator is relevant to everyone),
    /// while a member nobody deputized reports no shield.
    #[test]
    fn shared_deputy_helpers_name_a_global_moderator() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let mod_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let owner_id = MemberId::from(&owner_sk.verifying_key());
        let mod_id = MemberId::from(&mod_sk.verifying_key());
        let viewer_id = MemberId::from(&viewer_sk.verifying_key());

        // The owner invited both the moderator and the viewer.
        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &mod_sk.verifying_key()),
                authorized_member(&owner_sk, &viewer_sk.verifying_key()),
            ],
        };

        // Owner's signed record deputizes the moderator; the others carry none.
        let mk_info = |sk: &SigningKey, deputies: Vec<MemberId>| {
            let mi = MemberInfo {
                member_id: MemberId::from(&sk.verifying_key()),
                version: 0,
                preferred_nickname: river_core::room_state::privacy::SealedBytes::public(
                    b"nick".to_vec(),
                ),
                deputies,
            };
            AuthorizedMemberInfo::new_with_member_key(mi, sk)
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                mk_info(&owner_sk, vec![mod_id]),
                mk_info(&mod_sk, vec![]),
                mk_info(&viewer_sk, vec![]),
            ],
        };
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let deputizers_of = build_deputizers_of(&member_info);
        let viewer_relevant = viewer_relevant_deputizer_set(&members, owner_id, viewer_id);

        // The moderator (a deputy of the owner) shows the shield, named "room
        // owner", even to an unrelated viewer.
        assert_eq!(
            relevant_appointers(
                &member_info,
                &secrets,
                &deputizers_of,
                &viewer_relevant,
                owner_id,
                viewer_id,
                mod_id,
            ),
            vec![Appointer::Owner],
        );
        // A member nobody deputized shows no shield.
        assert!(relevant_appointers(
            &member_info,
            &secrets,
            &deputizers_of,
            &viewer_relevant,
            owner_id,
            viewer_id,
            viewer_id,
        )
        .is_empty());
    }

    /// **No nickname content reaches the shield tooltip, at all.**
    ///
    /// The tooltip is a `title=` attribute, i.e. one flat string, and the
    /// forging primitive there is the COMMA, not the quote. A plain-ASCII
    /// nickname of `Bob, the room owner, Carol` used to render
    ///
    /// ```text
    /// Deputy (appointed by "Bob, the room owner, Carol"). Can ban you.
    /// ```
    ///
    /// against the legitimate `appointed by "Bob", the room owner, "Carol"` —
    /// two quote glyphs apart, and nobody reads that at tooltip size. Since the
    /// payload carries no quote character of its own, no denylist of quote
    /// characters closes it, and sanitising cannot either: every byte is a
    /// legitimate name character.
    ///
    /// So this asserts the only property that actually holds: the tooltip
    /// contains no attacker-supplied substring. It is checked with a payload
    /// that has NO Unicode trickery, because that is the case a
    /// character-denylist fix would silently fail.
    #[test]
    fn no_nickname_content_reaches_the_shield_tooltip() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        // Each of these is a nickname a member can simply type. The first is
        // the one that defeats every quote-stripping fix.
        for payload in [
            "Bob, the room owner, Carol",
            "Bob\u{201d}, the room owner, \u{201c}Carol",
            "Bob\", the room owner, \"Carol",
            "Bob\u{00BB}, the room owner, \u{00AB}Carol",
            "Bob\u{FF02}, the room owner, \u{FF02}Carol",
            "Bob and you and the room owner",
            ") . Can ban you. Deputy (appointed by the room owner",
        ] {
            let mut rng = rand::thread_rng();
            let owner_sk = SigningKey::generate(&mut rng);
            let liar_sk = SigningKey::generate(&mut rng);
            let puppet_sk = SigningKey::generate(&mut rng);
            let viewer_sk = SigningKey::generate(&mut rng);

            let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
            let owner_id = id(&owner_sk);

            // The liar invited the viewer, so the liar is a viewer-relevant
            // appointer and their puppet carries a shield in the viewer's view.
            let members = MembersV1 {
                members: vec![
                    authorized_member(&owner_sk, &liar_sk.verifying_key()),
                    authorized_member(&owner_sk, &puppet_sk.verifying_key()),
                    member_invited_by(&liar_sk, owner_id, &viewer_sk.verifying_key()),
                ],
            };
            let member_info = MemberInfoV1 {
                member_info: vec![
                    signed_member_info(&owner_sk, "Owner", vec![]),
                    signed_member_info(&liar_sk, payload, vec![id(&puppet_sk)]),
                    signed_member_info(&puppet_sk, "Puppet", vec![]),
                    signed_member_info(&viewer_sk, "Viewer", vec![]),
                ],
            };
            let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

            let badges = deputy_badges_for_viewer(
                &members,
                &member_info,
                &secrets,
                owner_id,
                id(&viewer_sk),
            );
            let badge = badges
                .get(&id(&puppet_sk))
                .expect("the puppet carries a shield in this view");
            let tooltip = badge.tooltip();

            // The payload is the liar's nickname. The liar is ONE appointer, so
            // the only truthful thing the tooltip can say is "another member".
            assert_eq!(
                tooltip, "Deputy (appointed by another member). Can ban you.",
                "tooltip is not the trusted-literal form for payload {payload:?}"
            );

            // Belt and braces: no run of the payload long enough to read as a
            // phrase survives anywhere in the tooltip. A future rewrite that
            // reintroduces names in some other shape fails here even if it
            // changes the exact wording asserted above.
            for window in payload.split(',').map(str::trim).filter(|w| w.len() > 3) {
                assert!(
                    !tooltip.contains(window),
                    "tooltip leaked the nickname fragment {window:?}: {tooltip}"
                );
            }

            // The names are still available — as SEPARATE elements, where a
            // comma inside one cannot span two of them.
            assert_eq!(
                badge.appointer_names(),
                vec![payload.to_string()],
                "the modal must still be able to show who appointed them"
            );
        }
    }

    /// The trusted-literal wording, across every combination of appointers.
    /// The counts are the only thing a nickname can influence, and only by
    /// existing.
    #[test]
    fn appointer_phrase_is_built_from_trusted_literals_only() {
        let phrase = |appointers: Vec<Appointer>| {
            DeputyBadge {
                deputized_by: appointers,
                can_ban_viewer: None,
            }
            .tooltip()
        };
        let member = |n: &str| Appointer::Member(n.to_string());

        assert_eq!(
            phrase(vec![Appointer::Owner]),
            "Deputy (appointed by the room owner)"
        );
        assert_eq!(phrase(vec![Appointer::You]), "Deputy (appointed by you)");
        assert_eq!(
            phrase(vec![member("Eve")]),
            "Deputy (appointed by another member)"
        );
        assert_eq!(
            phrase(vec![member("Eve"), member("Mallory")]),
            "Deputy (appointed by 2 other members)"
        );
        assert_eq!(
            phrase(vec![Appointer::Owner, member("Eve")]),
            "Deputy (appointed by the room owner and another member)"
        );
        assert_eq!(
            phrase(vec![Appointer::Owner, Appointer::You]),
            "Deputy (appointed by the room owner and you)"
        );
        assert_eq!(
            phrase(vec![
                Appointer::Owner,
                Appointer::You,
                member("Eve"),
                member("Mallory"),
            ]),
            "Deputy (appointed by the room owner, you and 2 other members)"
        );
    }

    /// A repeated deputy entry must not repeat the appointer in the tooltip.
    #[test]
    fn duplicate_deputy_entries_are_collapsed() {
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let appointer_sk = SigningKey::generate(&mut rng);
        let deputy_sk = SigningKey::generate(&mut rng);
        let deputy_id = MemberId::from(&deputy_sk.verifying_key());

        let member_info = MemberInfoV1 {
            member_info: vec![signed_member_info(
                &appointer_sk,
                "Spammer",
                vec![deputy_id; 64],
            )],
        };
        let deputizers = build_deputizers_of(&member_info);
        assert_eq!(
            deputizers.get(&deputy_id).map(Vec::len),
            Some(1),
            "64 repeated grants must collapse to one appointer"
        );
    }

    // ------------------------------------------------------------------
    // freenet/river#478: nobody may ban themselves OUT OF THE ROOM.
    //
    // One rule, two routes to the same damage: ban yourself, or ban a member
    // you joined through (the cascade to `get_downstream_members` sweeps you
    // and your whole subtree up with them). The fixture below is shared by
    // every case so the ALLOWED cases are proved against the same room as the
    // REFUSED ones — an over-broad guard cannot hide behind a friendlier
    // fixture.
    // ------------------------------------------------------------------

    /// owner → alpha → beta → deputy → child, plus an unrelated `stranger`
    /// invited by the owner. `deputy` is an OWNER-APPOINTED GLOBAL MODERATOR,
    /// which is the whole point: that is the only grant in `is_ban_authorized`
    /// that reaches a member's own ancestors, and it is granted at step 3,
    /// ahead of the step-4 guardrail. A guard wired into the wrong branch of
    /// that ladder would not fire for this deputy.
    struct BanChain {
        members: MembersV1,
        member_info: river_core::room_state::member_info::MemberInfoV1,
        owner_id: MemberId,
        alpha_id: MemberId,
        beta_id: MemberId,
        deputy_id: MemberId,
        child_id: MemberId,
        stranger_id: MemberId,
        /// A deputy of ALPHA rather than of the owner, used to show the
        /// ancestor cases are not vacuous: this one is refused for lack of
        /// authority, not by the #478 rule.
        alphas_deputy_id: MemberId,
    }

    fn ban_chain() -> BanChain {
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let alpha_sk = SigningKey::generate(&mut rng);
        let beta_sk = SigningKey::generate(&mut rng);
        let deputy_sk = SigningKey::generate(&mut rng);
        let child_sk = SigningKey::generate(&mut rng);
        let stranger_sk = SigningKey::generate(&mut rng);
        let alphas_deputy_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let deputy_id = id(&deputy_sk);
        let alphas_deputy_id = id(&alphas_deputy_sk);

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &alpha_sk.verifying_key()),
                member_invited_by(&alpha_sk, owner_id, &beta_sk.verifying_key()),
                member_invited_by(&beta_sk, owner_id, &deputy_sk.verifying_key()),
                member_invited_by(&deputy_sk, owner_id, &child_sk.verifying_key()),
                authorized_member(&owner_sk, &stranger_sk.verifying_key()),
                member_invited_by(&alpha_sk, owner_id, &alphas_deputy_sk.verifying_key()),
            ],
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                // The owner deputizes `deputy` — a GLOBAL moderator.
                signed_member_info(&owner_sk, "Owner", vec![deputy_id]),
                // Alpha deputizes someone else — a SUBTREE-scoped deputy.
                signed_member_info(&alpha_sk, "Alpha", vec![alphas_deputy_id]),
                signed_member_info(&beta_sk, "Beta", vec![]),
                signed_member_info(&deputy_sk, "Deputy", vec![]),
                signed_member_info(&child_sk, "Child", vec![]),
                signed_member_info(&stranger_sk, "Stranger", vec![]),
                signed_member_info(&alphas_deputy_sk, "AlphasDeputy", vec![]),
            ],
        };

        BanChain {
            members,
            member_info,
            owner_id,
            alpha_id: id(&alpha_sk),
            beta_id: id(&beta_sk),
            deputy_id,
            child_id: id(&child_sk),
            stranger_id: id(&stranger_sk),
            alphas_deputy_id,
        }
    }

    impl BanChain {
        fn gate(&self, viewer: MemberId, target: MemberId) -> BanGate {
            ban_gate(
                &self.members,
                &self.member_info,
                viewer,
                target,
                self.owner_id,
            )
        }

        /// What the CONTRACT says — asserted as a PRECONDITION everywhere the
        /// gate refuses, so a refusal can never be credited to the guard when
        /// the contract would have refused anyway.
        fn contract_authorizes(&self, banner: MemberId, target: MemberId) -> bool {
            MembersV1::is_ban_authorized(
                banner,
                target,
                &self.members.members_by_member_id(),
                &self.member_info,
                self.owner_id,
            )
        }
    }

    /// The direct route (the original #478): a deputy may not ban themselves.
    #[test]
    fn ban_is_refused_for_self() {
        let c = ban_chain();

        assert!(
            c.contract_authorizes(c.deputy_id, c.deputy_id),
            "precondition: the contract permits a deputy to ban THEMSELVES \
             (step 3 fires before anything that would stop it), which is \
             exactly why the client must refuse"
        );
        assert!(
            ban_removal_set(&c.members, c.deputy_id).contains(&c.deputy_id),
            "precondition: the ban's removal set contains the banner"
        );

        assert!(
            matches!(
                c.gate(c.deputy_id, c.deputy_id),
                BanGate::WouldRemoveViewer(_)
            ),
            "a deputy must not be able to ban themselves (#478)"
        );
    }

    /// The transitive route: a deputy may not ban anyone ABOVE them in the
    /// invite chain, at any depth. Same blast radius as a self-ban, because
    /// `get_downstream_members` is the full transitive closure.
    #[test]
    fn ban_is_refused_for_every_strict_ancestor() {
        let c = ban_chain();

        for (label, ancestor) in [
            ("direct inviter (parent)", c.beta_id),
            ("grandparent", c.alpha_id),
        ] {
            assert!(
                c.contract_authorizes(c.deputy_id, ancestor),
                "precondition ({label}): the contract AUTHORIZES this ban — an \
                 owner-appointed global moderator is granted authority at step \
                 3, over their own ancestors included. Without this the test \
                 would pass for want of authority, not because of the guard"
            );
            assert!(
                ban_removal_set(&c.members, ancestor).contains(&c.deputy_id),
                "precondition ({label}): the ban's cascade really does remove \
                 the banner"
            );

            assert!(
                matches!(c.gate(c.deputy_id, ancestor), BanGate::WouldRemoveViewer(_)),
                "banning a {label} removes the banner and their whole subtree, \
                 so it must be refused (#478)"
            );
        }
    }

    /// The over-broadness case. A guard that also blocks legitimate bans is a
    /// failure, so these must still be ALLOWED — from the same room, by the
    /// same deputy, as the refusals above.
    #[test]
    fn bans_that_do_not_remove_the_banner_are_still_allowed() {
        let c = ban_chain();

        for (label, target) in [
            ("an unrelated member in another subtree", c.stranger_id),
            ("a member of the deputy's OWN downstream", c.child_id),
            (
                "a deputy scoped to an ancestor's subtree",
                c.alphas_deputy_id,
            ),
        ] {
            assert!(
                !ban_removal_set(&c.members, target).contains(&c.deputy_id),
                "precondition ({label}): this ban does not remove the banner"
            );
            assert_eq!(
                c.gate(c.deputy_id, target),
                BanGate::Allowed,
                "banning {label} is legitimate and must stay available (#478 \
                 must not be over-broad)"
            );
        }

        // The OWNER is never in anyone's removal set (they are nobody's
        // invitee), so their authority is untouched — including over the
        // members at the top of the deputy's own chain.
        assert_eq!(c.gate(c.owner_id, c.alpha_id), BanGate::Allowed);
        assert_eq!(c.gate(c.owner_id, c.deputy_id), BanGate::Allowed);
    }

    /// Proves the ancestor cases above are about the GUARD and not about
    /// missing authority: a subtree-scoped deputy (deputized by alpha, not by
    /// the owner) is refused the very same target for a different reason. If a
    /// future change made `WouldRemoveViewer` the answer here too, the
    /// distinction the modal renders on would have silently collapsed.
    #[test]
    fn a_non_global_deputy_is_refused_an_ancestor_for_lack_of_authority() {
        let c = ban_chain();

        assert!(
            !c.contract_authorizes(c.alphas_deputy_id, c.alpha_id),
            "precondition: deputy authority is scoped to the DEPUTIZER's \
             subtree, so alpha's deputy has no grant over alpha themselves"
        );
        assert_eq!(
            c.gate(c.alphas_deputy_id, c.alpha_id),
            BanGate::NoAuthority,
            "no authority is a different answer from 'would remove you', and \
             only the latter is explained to the user"
        );
    }

    /// The removal set is the target PLUS their whole transitive subtree —
    /// the same set `check_banned_members` builds. A one-level-only walk (the
    /// easy mistake) would miss the grandchild and let the grandparent ban
    /// through.
    #[test]
    fn ban_removal_set_is_the_whole_transitive_subtree() {
        let c = ban_chain();

        let removed = ban_removal_set(&c.members, c.alpha_id);
        assert!(removed.contains(&c.alpha_id), "the target themselves");
        assert!(removed.contains(&c.beta_id), "a direct invitee");
        assert!(removed.contains(&c.deputy_id), "a grandchild");
        assert!(removed.contains(&c.child_id), "a great-grandchild");
        assert!(
            !removed.contains(&c.stranger_id),
            "a member in a different subtree is NOT removed"
        );

        // A leaf removes only themselves.
        assert_eq!(
            ban_removal_set(&c.members, c.child_id),
            HashSet::from([c.child_id])
        );
    }

    /// The contract's `get_downstream_members` has no visited guard — it can
    /// rely on `verify` having rejected circular invite chains. This mirror
    /// walks whatever state the UI currently holds, including a half-applied
    /// or hostile one, so it must terminate. (Test hangs rather than fails if
    /// this regresses, which is still a loud CI signal.)
    #[test]
    fn ban_removal_set_terminates_on_a_cycle() {
        let mut c = ban_chain();
        // Hand-forge a cycle: alpha claims to have been invited by their own
        // descendant. `AuthorizedMember::new` would not produce this, so patch
        // the field directly.
        c.members.members[0].member.invited_by = c.deputy_id;

        let removed = ban_removal_set(&c.members, c.alpha_id);
        assert!(removed.contains(&c.alpha_id));
        assert!(removed.contains(&c.beta_id));
    }

    /// The RENDER layer. The Ban action must not render when the rule refuses
    /// it, and the modal must derive that from the shared gate rather than
    /// re-deciding locally. Source-scrape, because the render is a Dioxus
    /// component tree with no headless harness here.
    #[test]
    fn ban_action_is_not_rendered_when_the_rule_refuses() {
        let prod = prod_source(include_str!("members/member_info_modal.rs"));
        assert_prod_only(&prod, "member_info_modal.rs");

        // The modal must ask the SHARED gate. A local re-derivation is how the
        // render and the action boundary drift apart.
        assert!(
            prod.contains("ban_gate("),
            "the modal must gate Ban through `ban_gate` (#478)"
        );
        // The reason must be RENDERED, not merely computed. Checking only that
        // the gate is destructured would let someone delete the explanation
        // element and keep the pin green — a Ban button that vanishes with no
        // explanation is exactly the bug report this is here to prevent.
        let reason_at = prod
            .find("if let Some(reason) = ban_refusal {")
            .expect("the modal must render the refusal reason when the rule withholds Ban (#478)");
        let reason_block = &prod[reason_at..];
        assert!(
            reason_block.contains("\"data-testid\": \"ban-withheld-reason\"")
                && reason_block.contains("\"{reason}\""),
            "the withheld-Ban explanation must actually display the reason text \
             (#478)"
        );

        // EVERY render must be guarded, not just the first. A second,
        // unguarded `BanButton` added later would otherwise slip past.
        let sites: Vec<usize> = prod.match_indices("BanButton {").map(|(i, _)| i).collect();
        assert!(
            !sites.is_empty(),
            "the modal must render a BanButton — if it moved to another file, \
             move this pin with it (#478)"
        );

        for ban_at in sites {
            // Both guards must be open where the button renders: the self
            // check (which also hides Deputize) and the rule's refusal.
            for guard in [
                "if member_id != self_member_id {",
                "if ban_refusal.is_none() {",
            ] {
                let guard_at = prod[..ban_at].rfind(guard).unwrap_or_else(|| {
                    panic!("no `{guard}` guard above a BanButton render (#478)")
                });
                assert!(
                    guard_is_still_open(&prod[guard_at + guard.len()..ban_at]),
                    "the nearest `{guard}` above a `BanButton` closes before \
                     the button renders, so the Ban action is still reachable \
                     where #478 says it must not be"
                );
            }
        }
    }

    /// The ACTION BOUNDARY. `BanButton::execute_ban` trusts a `member_to_ban`
    /// PROP and a `can_ban` PROP; any future call site passing `can_ban: true`
    /// would run the cascading ban if the only guards were render-time. So the
    /// check must be inside `execute_ban`, must run BEFORE the `UserBan` is
    /// built, and must NOT consult the caller's flags. Source-scraped because
    /// the handler is a Dioxus closure needing a runtime.
    #[test]
    fn execute_ban_refuses_at_the_action_boundary() {
        let prod = prod_source(include_str!("members/member_info_modal/ban_button.rs"));
        assert_prod_only(&prod, "ban_button.rs");

        let handler_at = prod
            .find("let execute_ban =")
            .expect("ban_button.rs must define an execute_ban handler");
        let build_at = prod
            .find("let ban = UserBan {")
            .expect("ban_button.rs must build a UserBan");
        assert!(
            handler_at < build_at,
            "the UserBan must be built inside execute_ban"
        );
        let guarded_region = &prod[handler_at..build_at];

        assert!(
            guarded_region.contains("self_removing_ban_reason("),
            "`execute_ban` must refuse a self-removing ban BEFORE building the \
             UserBan, using the SAME predicate as the render gate so the two \
             cannot drift (#478)"
        );
        assert!(
            guarded_region.contains("return;"),
            "the boundary check must ABORT the ban, not merely log it (#478)"
        );
        assert!(
            !guarded_region.contains("can_ban"),
            "the boundary check must not be conditioned on the caller's \
             `can_ban` prop — the whole point is that a caller claiming the \
             action is permitted cannot make it so (#478)"
        );
    }

    /// Production slice of a source file for the pins above: everything before
    /// the test module (the whole file when it has none), with line comments
    /// stripped.
    ///
    /// Stripping comments matters — a guard that was DELETED but is still
    /// QUOTED in a comment would otherwise satisfy a `contains`/`rfind`, the
    /// same reason `util::ecies`'s pin strips them.
    ///
    /// Cutting at the FIRST `#[cfg(test)]` is the safe direction here: every
    /// assertion built on this requires its needle to be PRESENT, so an
    /// over-short slice panics loudly rather than passing vacuously. The
    /// dangerous direction is an over-LONG slice, where a needle could match
    /// inside test code and the pin goes silently vacuous — callers guard
    /// against that with [`assert_prod_only`].
    fn prod_source(source: &str) -> String {
        let end = source.find("#[cfg(test)]").unwrap_or(source.len());
        source[..end]
            .lines()
            .map(|line| line.split_once("//").map(|(code, _)| code).unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Fails if a [`prod_source`] slice still contains test code, which would
    /// let a pin's needles match themselves. Cheap insurance against the
    /// failure mode where a rebase moves the cut point and every source-scrape
    /// in the file quietly stops asserting anything.
    fn assert_prod_only(prod: &str, what: &str) {
        assert!(
            !prod.contains("#[test]"),
            "{what}: the production slice still contains test code, so the \
             pins below can match their own needles — fix the cut point"
        );
    }

    /// Whether a guard block is still OPEN across `between` (the text from the
    /// end of the guard's `{` to the site it should be protecting).
    ///
    /// Tracks the MINIMUM running brace depth, not the final depth: the modal
    /// has an earlier `if member_id != self_member_id` block (the DM /
    /// Share-invite row) that closes again, and a final-depth check reads that
    /// as satisfying the guard because a later block re-opens.
    fn guard_is_still_open(between: &str) -> bool {
        let mut depth = 0i32;
        let mut min_depth = 0i32;
        for c in between.chars() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            min_depth = min_depth.min(depth);
        }
        min_depth >= 0
    }

    /// The conversation's 🛡 badge predicate, end to end.
    ///
    /// Ian's semantics: the shield means *"this user has been deputized, which
    /// allows them to ban ME"*, and it stays visible even when the viewer
    /// happens to be immune. One room exercises every case at once so the
    /// cases cannot drift apart:
    ///
    /// ```text
    /// owner ──┬── alpha ── viewer      alpha deputizes deputy_alpha
    ///         ├── unrelated            unrelated deputizes deputy_unrelated
    ///         ├── global_mod           owner deputizes global_mod
    ///         ├── my_deputy            viewer deputizes my_deputy
    ///         ├── deputy_alpha
    ///         ├── deputy_unrelated
    ///         └── plain
    /// ```
    #[test]
    fn deputy_badge_is_viewer_relative_and_survives_viewer_immunity() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let alpha_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let unrelated_sk = SigningKey::generate(&mut rng);
        let global_mod_sk = SigningKey::generate(&mut rng);
        let deputy_alpha_sk = SigningKey::generate(&mut rng);
        let deputy_unrelated_sk = SigningKey::generate(&mut rng);
        let my_deputy_sk = SigningKey::generate(&mut rng);
        let plain_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let viewer_id = id(&viewer_sk);

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &alpha_sk.verifying_key()),
                authorized_member(&owner_sk, &unrelated_sk.verifying_key()),
                authorized_member(&owner_sk, &global_mod_sk.verifying_key()),
                authorized_member(&owner_sk, &deputy_alpha_sk.verifying_key()),
                authorized_member(&owner_sk, &deputy_unrelated_sk.verifying_key()),
                authorized_member(&owner_sk, &my_deputy_sk.verifying_key()),
                authorized_member(&owner_sk, &plain_sk.verifying_key()),
                // The viewer sits INSIDE alpha's subtree, so alpha is a strict
                // ancestor and alpha's deputies have authority over the viewer.
                member_invited_by(&alpha_sk, owner_id, &viewer_sk.verifying_key()),
            ],
        };

        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Owner", vec![id(&global_mod_sk)]),
                signed_member_info(&alpha_sk, "Alpha", vec![id(&deputy_alpha_sk)]),
                signed_member_info(&unrelated_sk, "Unrelated", vec![id(&deputy_unrelated_sk)]),
                signed_member_info(&viewer_sk, "Viewer", vec![id(&my_deputy_sk)]),
                signed_member_info(&global_mod_sk, "GlobalMod", vec![]),
                signed_member_info(&deputy_alpha_sk, "DeputyAlpha", vec![]),
                signed_member_info(&deputy_unrelated_sk, "DeputyUnrelated", vec![]),
                signed_member_info(&my_deputy_sk, "MyDeputy", vec![]),
                signed_member_info(&plain_sk, "Plain", vec![]),
            ],
        };
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, viewer_id);

        // 1. Deputy of the room owner, viewer is an ordinary member → badge,
        //    and they really can ban the viewer (owner-appointed global mod).
        let global = badges
            .get(&id(&global_mod_sk))
            .expect("a deputy of the owner must show the shield to every viewer");
        assert_eq!(global.deputized_by, vec![Appointer::Owner]);
        assert_eq!(global.can_ban_viewer, Some(true));
        assert_eq!(
            global.tooltip(),
            "Deputy (appointed by the room owner). Can ban you."
        );

        // 1b. Deputy of a strict ancestor of the viewer → badge, named after
        //     the appointer, and they can ban the viewer (subtree authority).
        let by_alpha = badges
            .get(&id(&deputy_alpha_sk))
            .expect("a deputy of the viewer's inviter must show the shield");
        // A real nickname is classified as `Member`, never as one of the two
        // trusted role labels, so it cannot masquerade as either.
        assert_eq!(
            by_alpha.deputized_by,
            vec![Appointer::Member("Alpha".to_string())]
        );
        assert_eq!(by_alpha.can_ban_viewer, Some(true));

        // 2. Deputy of a member OUTSIDE the viewer's invite ancestry → NO
        //    badge. Deputy authority is scoped to the deputizer's subtree, so
        //    this member holds no authority that bears on the viewer, and
        //    Ian's shield means "authority over me". They still show the
        //    shield in THEIR OWN subtree's views.
        assert!(
            !badges.contains_key(&id(&deputy_unrelated_sk)),
            "a deputy of an unrelated subtree must not show the shield here"
        );

        // 3. Viewer is immune (they appointed this deputy themselves, so
        //    `is_ban_authorized` step 4 denies the ban) → the badge STILL
        //    shows. This is the half Ian called out explicitly; only the
        //    tooltip reflects the immunity.
        let mine = badges
            .get(&id(&my_deputy_sk))
            .expect("a deputy the viewer appointed must still show the shield");
        assert_eq!(mine.deputized_by, vec![Appointer::You]);
        assert_eq!(
            mine.can_ban_viewer,
            Some(false),
            "a deputy cannot ban the member who deputized them"
        );
        assert_eq!(mine.tooltip(), "Deputy (appointed by you). Cannot ban you.");

        // 4. No deputy authority at all → no badge.
        assert!(!badges.contains_key(&id(&plain_sk)));

        // 5. The room OWNER gets no deputy shield. Their authority is
        //    inherent, not delegated, so "Deputy (appointed by …)" would be a
        //    lie; the member list already marks them with 👑.
        assert!(
            !badges.contains_key(&owner_id),
            "the owner is not a deputy of anyone"
        );

        // The viewer themselves is not a deputy in this room.
        assert!(!badges.contains_key(&viewer_id));
    }

    /// The other reading of "unless I'm also a deputy": the viewer holds
    /// deputy authority of their own. The badge must not be suppressed — and,
    /// pinned here because it is counter-intuitive, being a deputy does NOT
    /// make the viewer immune. Two deputies of the same appointer can ban each
    /// other, so the tooltip says "can ban you".
    #[test]
    fn deputy_viewer_still_sees_the_shield_on_a_fellow_deputy() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let other_mod_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let viewer_id = id(&viewer_sk);

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &viewer_sk.verifying_key()),
                authorized_member(&owner_sk, &other_mod_sk.verifying_key()),
            ],
        };
        // The owner deputizes BOTH the viewer and the other moderator.
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Owner", vec![id(&viewer_sk), id(&other_mod_sk)]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
                signed_member_info(&other_mod_sk, "OtherMod", vec![]),
            ],
        };
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, viewer_id);

        let other = badges
            .get(&id(&other_mod_sk))
            .expect("a fellow deputy must still show the shield to a deputy viewer");
        assert_eq!(other.deputized_by, vec![Appointer::Owner]);
        assert_eq!(
            other.can_ban_viewer,
            Some(true),
            "owner-appointed moderators have absolute authority, including \
             over other moderators"
        );

        // The viewer is a deputy too, so their OWN row carries a shield — with
        // the ban clause dropped rather than a nonsensical "can ban you".
        let own = badges
            .get(&viewer_id)
            .expect("a deputy viewer sees the shield on their own row");
        assert_eq!(own.can_ban_viewer, None);
        assert_eq!(own.tooltip(), "Deputy (appointed by the room owner)");
    }

    /// The room owner's OWN view. `viewer_relevant_deputizer_set` gives the
    /// owner no strict ancestors, so only deputies the owner appointed are
    /// relevant — which is every global moderator. Pinned because an
    /// off-by-one here would blank the owner's whole moderator list.
    #[test]
    fn owner_sees_the_shield_on_their_own_appointees() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let mod_sk = SigningKey::generate(&mut rng);
        let other_sk = SigningKey::generate(&mut rng);
        let their_deputy_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &mod_sk.verifying_key()),
                authorized_member(&owner_sk, &other_sk.verifying_key()),
                member_invited_by(&other_sk, owner_id, &their_deputy_sk.verifying_key()),
            ],
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Owner", vec![id(&mod_sk)]),
                signed_member_info(&other_sk, "Other", vec![id(&their_deputy_sk)]),
                signed_member_info(&mod_sk, "Mod", vec![]),
                signed_member_info(&their_deputy_sk, "TheirDeputy", vec![]),
            ],
        };
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let badges = deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, owner_id);

        let m = badges
            .get(&id(&mod_sk))
            .expect("the owner must see the shield on their own appointee");
        // `relevant_appointers` resolves the owner id BEFORE the self id,
        // so an owner viewing their own appointee reads "room owner", not
        // "you". Pinned rather than "fixed": the label describes the role that
        // granted the authority, and every other viewer sees the same word.
        assert_eq!(m.deputized_by, vec![Appointer::Owner]);
        assert_eq!(
            m.can_ban_viewer,
            Some(false),
            "the owner is never a valid ban target"
        );
        // Someone else's deputy is not relevant to the owner's view.
        assert!(!badges.contains_key(&id(&their_deputy_sk)));
    }

    /// "Cannot ban you" must not lie. A ban cascades to the banned member's
    /// whole invite subtree, so a deputy who cannot ban the viewer DIRECTLY but
    /// can ban the viewer's inviter still gets the viewer removed.
    ///
    /// Room: `owner -> alpha -> parent -> viewer`. `alpha` deputizes `mod`, and
    /// the viewer ALSO deputizes `mod` — which is exactly the configuration the
    /// badge advertises as "you're protected". `is_ban_authorized(mod, viewer)`
    /// denies (step 4 guardrail), but `is_ban_authorized(mod, parent)` grants
    /// (step 5, alpha is parent's ancestor), and banning `parent` sweeps the
    /// viewer out with them.
    #[test]
    fn can_ban_viewer_accounts_for_cascading_bans() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let alpha_sk = SigningKey::generate(&mut rng);
        let parent_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let mod_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let viewer_id = id(&viewer_sk);

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &alpha_sk.verifying_key()),
                authorized_member(&owner_sk, &mod_sk.verifying_key()),
                member_invited_by(&alpha_sk, owner_id, &parent_sk.verifying_key()),
                member_invited_by(&parent_sk, owner_id, &viewer_sk.verifying_key()),
            ],
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Owner", vec![]),
                signed_member_info(&alpha_sk, "Alpha", vec![id(&mod_sk)]),
                signed_member_info(&parent_sk, "Parent", vec![]),
                // The viewer deputizes the moderator, which under the contract
                // makes a DIRECT ban of the viewer inert.
                signed_member_info(&viewer_sk, "Viewer", vec![id(&mod_sk)]),
                signed_member_info(&mod_sk, "Mod", vec![]),
            ],
        };
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();
        let members_by_id = members.members_by_member_id();

        // Precondition: the direct ban really is denied. Without this the test
        // would pass for the wrong reason.
        assert!(
            !MembersV1::is_ban_authorized(
                id(&mod_sk),
                viewer_id,
                &members_by_id,
                &member_info,
                owner_id
            ),
            "precondition: a direct ban of the viewer must be denied here"
        );
        // ...but the ancestor ban is not.
        assert!(MembersV1::is_ban_authorized(
            id(&mod_sk),
            id(&parent_sk),
            &members_by_id,
            &member_info,
            owner_id
        ));

        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, viewer_id);
        let badge = badges.get(&id(&mod_sk)).expect("shield expected");
        assert_eq!(
            badge.can_ban_viewer,
            Some(true),
            "the tooltip must not promise safety from someone who can remove \
             the viewer by banning their inviter"
        );
    }

    /// A member can write `deputies: [self]` with a custom client — nothing in
    /// `MemberInfoV1::verify` forbids it. Honouring it would let anyone in the
    /// viewer's invite chain render a REAL shield on their own messages, which
    /// is the impersonation this whole feature exists to prevent.
    #[test]
    fn self_deputisation_grants_no_badge() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let liar_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &liar_sk.verifying_key()),
                // The viewer is inside the liar's subtree, so the liar IS a
                // viewer-relevant appointer — the badge would show if the
                // self-grant counted.
                member_invited_by(&liar_sk, owner_id, &viewer_sk.verifying_key()),
            ],
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Owner", vec![]),
                signed_member_info(&liar_sk, "Liar", vec![id(&liar_sk)]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
            ],
        };
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, id(&viewer_sk));
        assert!(
            badges.is_empty(),
            "a self-granted deputy entry must not render a shield: {badges:?}"
        );
    }

    /// Symmetrically: a member can list the OWNER in their own `deputies`. The
    /// owner's authority is inherent, so a fabricated "appointed by …" chip on
    /// the owner would be both false and a spoof surface.
    #[test]
    fn owner_never_carries_a_deputy_badge() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let liar_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &liar_sk.verifying_key()),
                member_invited_by(&liar_sk, owner_id, &viewer_sk.verifying_key()),
            ],
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Owner", vec![]),
                // A member "deputizing" the owner.
                signed_member_info(&liar_sk, "Liar", vec![owner_id]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
            ],
        };
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, id(&viewer_sk));
        assert!(
            !badges.contains_key(&owner_id),
            "the owner must never render a deputy shield"
        );
    }

    /// The single-target lookup used by the member-info modal must agree with
    /// the sweep used by the member list and the conversation, member for
    /// member. If they can disagree the shield drifts between surfaces again.
    #[test]
    fn single_target_badge_matches_the_sweep() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let alpha_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let mod_sk = SigningKey::generate(&mut rng);
        let plain_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let viewer_id = id(&viewer_sk);

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &alpha_sk.verifying_key()),
                authorized_member(&owner_sk, &mod_sk.verifying_key()),
                authorized_member(&owner_sk, &plain_sk.verifying_key()),
                member_invited_by(&alpha_sk, owner_id, &viewer_sk.verifying_key()),
            ],
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Owner", vec![id(&mod_sk)]),
                signed_member_info(&alpha_sk, "Alpha", vec![]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
                signed_member_info(&mod_sk, "Mod", vec![]),
                signed_member_info(&plain_sk, "Plain", vec![]),
            ],
        };
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let sweep = deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, viewer_id);
        for target in [
            owner_id,
            id(&alpha_sk),
            viewer_id,
            id(&mod_sk),
            id(&plain_sk),
        ] {
            assert_eq!(
                deputy_badge_for_viewer(
                    &members,
                    &member_info,
                    &secrets,
                    owner_id,
                    viewer_id,
                    target
                ),
                sweep.get(&target).cloned(),
                "single-target lookup disagrees with the sweep for {target}"
            );
        }
    }

    /// An emoji nickname must not be able to paint an appointer name that
    /// looks like a second badge inside the tooltip.
    #[test]
    fn deputizer_names_are_sanitised() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let appointer_sk = SigningKey::generate(&mut rng);
        let deputy_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &appointer_sk.verifying_key()),
                authorized_member(&owner_sk, &deputy_sk.verifying_key()),
                member_invited_by(&appointer_sk, owner_id, &viewer_sk.verifying_key()),
            ],
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Owner", vec![]),
                signed_member_info(&appointer_sk, "Eve 🛡👑", vec![id(&deputy_sk)]),
                signed_member_info(&deputy_sk, "Deputy", vec![]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
            ],
        };
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, id(&viewer_sk));
        let badge = badges.get(&id(&deputy_sk)).expect("shield expected");
        assert_eq!(
            badge.deputized_by,
            vec![Appointer::Member("Eve".to_string())]
        );
        assert!(!badge.tooltip().contains('🛡'));

        // The role labels are unquoted, so a nickname of `room owner` reads as
        // the nickname it is rather than as an owner appointment.
        let liar = deputy_badges_for_viewer(
            &members,
            &MemberInfoV1 {
                member_info: vec![
                    signed_member_info(&owner_sk, "Owner", vec![]),
                    signed_member_info(&appointer_sk, "room owner", vec![id(&deputy_sk)]),
                    signed_member_info(&deputy_sk, "Deputy", vec![]),
                    signed_member_info(&viewer_sk, "Viewer", vec![]),
                ],
            },
            &secrets,
            owner_id,
            id(&viewer_sk),
        );
        let badge = liar.get(&id(&deputy_sk)).expect("shield expected");
        assert_eq!(
            badge.deputized_by,
            vec![Appointer::Member("room owner".to_string())],
            "a nickname of `room owner` must classify as a MEMBER, never as the \
             trusted `Appointer::Owner` label"
        );
        // And the tooltip, which is the flat surface a reader actually sees,
        // names no member at all.
        assert!(
            !badge.tooltip().contains("the room owner"),
            "a nickname of `room owner` forged an owner appointment in the \
             tooltip: {}",
            badge.tooltip()
        );
    }

    /// `build_deputizers_of` must order each deputy's appointers
    /// DETERMINISTICALLY (sorted by MemberId), not by `HashSet` iteration
    /// order. The row and the modal legend build this map independently, so a
    /// member appointed by multiple relevant deputizers would otherwise show
    /// "room owner, you" in one place and "you, room owner" in the other, or
    /// reorder between renders (freenet/river#451, Codex P3).
    #[test]
    fn build_deputizers_of_orders_appointers_deterministically() {
        use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};

        let mut rng = rand::thread_rng();
        // A pool of appointers all deputizing the same target, built in a
        // shuffled input order; the output must be sorted regardless.
        let appointer_sks: Vec<SigningKey> =
            (0..6).map(|_| SigningKey::generate(&mut rng)).collect();
        let target_sk = SigningKey::generate(&mut rng);
        let target_id = MemberId::from(&target_sk.verifying_key());

        let mk_info = |sk: &SigningKey, deputies: Vec<MemberId>| {
            let mi = MemberInfo {
                member_id: MemberId::from(&sk.verifying_key()),
                version: 0,
                preferred_nickname: river_core::room_state::privacy::SealedBytes::public(
                    b"n".to_vec(),
                ),
                deputies,
            };
            AuthorizedMemberInfo::new_with_member_key(mi, sk)
        };

        let mut expected: Vec<MemberId> = appointer_sks
            .iter()
            .map(|sk| MemberId::from(&sk.verifying_key()))
            .collect();
        expected.sort_unstable();

        // Build the state twice with the appointer records in DIFFERENT input
        // orders; both must yield the same sorted appointer list for target.
        let build = |order: &[usize]| {
            let records: Vec<AuthorizedMemberInfo> = order
                .iter()
                .map(|&i| mk_info(&appointer_sks[i], vec![target_id]))
                .collect();
            let member_info = MemberInfoV1 {
                member_info: records,
            };
            build_deputizers_of(&member_info)
                .get(&target_id)
                .cloned()
                .unwrap_or_default()
        };

        assert_eq!(build(&[0, 1, 2, 3, 4, 5]), expected);
        assert_eq!(build(&[5, 3, 1, 4, 0, 2]), expected);
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

    /// **SHOULD-FIX E — the warning must not be clippable.**
    ///
    /// `truncate` used to sit on the member-row BUTTON, which clipped the badge
    /// spans along with the name. Two consequences: a long nickname pushed the
    /// ⚠ and its `title`/`aria-label` out of view entirely, and because the
    /// sanitiser deliberately does NOT normalise `U+3000` IDEOGRAPHIC SPACE, a
    /// nickname like `"Ian Clarke" + "\u{3000}".repeat(13) + "."` rendered as
    /// `Ian Clarke…` — a visual clone whose warning was clipped off, well
    /// inside the nickname length limit.
    ///
    /// Source-scrape: this is a Dioxus tree with no headless harness here, and
    /// the property is a class on a specific element.
    #[test]
    fn the_warning_badge_cannot_be_clipped_by_a_long_nickname() {
        let src = include_str!("members.rs");
        let start = src
            .find("\"data-testid\": \"member-item-")
            .expect("the row");
        let row = &src[start..start + 2600];

        let button = row.find("button {").expect("the row is a button");
        let button_class_end = row[button..].find('\n').unwrap_or(0) + button;
        let button_line = &row[button..button_class_end + 200];
        assert!(
            !button_line.contains("transition-colors truncate"),
            "`truncate` must not sit on the row BUTTON — it clips the badges \
             along with the name, so a long nickname hides the warning"
        );

        let name = row
            .find("\"{parts.nickname}\"")
            .expect("the nickname span exists");
        assert!(
            row[name.saturating_sub(120)..name].contains("truncate"),
            "the NAME span must be the thing that truncates"
        );

        let tag = row.find("member-icon").expect("the tag span exists");
        assert!(
            row[tag..tag + 60].contains("flex-shrink-0"),
            "every badge span must be `flex-shrink-0` so it can never be \
             clipped, whatever the nickname's length"
        );
    }

    /// Source-grep pin: the member-row render MUST keep `parts.nickname`
    /// as a Dioxus text-node interpolation — `span { "{parts.nickname}" }`
    /// — not pass it through any string concatenation or attribute that
    /// evaluates HTML. Catches a future refactor that goes back to
    /// building an HTML string for the row (the freenet/river#227 shape).
    #[test]
    fn member_row_renders_nickname_as_text_node() {
        let prod = production_source();
        let needle = "\"{parts.nickname}\"";
        let at = prod
            .find(needle)
            .expect("MemberList must still render `parts.nickname`");
        // Check the interpolation's POSITION, not the whole element literally.
        // The literal form broke the moment the span gained a `class` — and it
        // would have passed unchanged for `dangerous_inner_html:
        // "{parts.nickname}"` had the element otherwise matched. In rsx a text
        // node follows `{` or `,`; an attribute value follows `:`.
        let preceding = prod[..at]
            .trim_end()
            .chars()
            .next_back()
            .expect("something precedes the interpolation");
        assert!(
            preceding == '{' || preceding == ',',
            "`parts.nickname` must be a Dioxus TEXT NODE (preceded by `{{` or \
             `,`), but it is preceded by {preceding:?} — an attribute-value \
             position. If that attribute is `dangerous_inner_html`, this \
             reopens freenet/river#227."
        );
        assert!(
            prod[at.saturating_sub(200)..at].contains("span {"),
            "`parts.nickname` is no longer rendered inside a `span`"
        );
    }

    /// Source-grep pin (freenet/river#414 REDESIGN): the overwrite path must
    /// swap the identity IN PLACE (keeping room_state) and must NOT resurrect the
    /// deleted empty-rebuild scaffolding. The behavioural swap logic is
    /// unit-tested on the pure helper; this pins that the deferred signal block
    /// actually routes overwrite → `swap_room_identity_in_place` and does not
    /// re-add `reset_room_for_resync` (which only existed to nurse the rebuilt
    /// empty room).
    #[test]
    fn complete_identity_import_overwrites_in_place_without_resync_reset() {
        let prod = production_source();
        assert!(
            prod.contains("swap_room_identity_in_place(existing, export)"),
            "complete_identity_import must swap the identity in place on overwrite \
             (keeping room_state), not rebuild empty (freenet/river#414 redesign)"
        );
        assert!(
            !prod.contains("reset_room_for_resync"),
            "the empty-rebuild scaffolding (reset_room_for_resync) must stay \
             deleted — the redesign keeps room_state, so there is nothing to nurse"
        );
    }

    /// Source-grep pin (freenet/river#414 REDESIGN, safety-critical): the import
    /// handler must gate the new-vs-overwrite decision on `rooms_load_is_authoritative`
    /// so it never decides on an unhydrated room set (which would build-empty over
    /// a real room). Both the render-time button gate and the handler safety-net
    /// must be present.
    #[test]
    fn handle_import_gates_on_hydration() {
        let prod = production_source();
        assert!(
            prod.contains("if !rooms_load_is_authoritative(load_state, saw_failure, recovery)"),
            "handle_import must refuse to decide on an incompletely-loaded or \
             mid-recovery room set (#414 redesign, Codex round-6 P1-1 + round-9 P1)"
        );
        assert!(
            prod.contains("disabled: !rooms_hydrated"),
            "the Import button must be disabled until the room set is fully loaded (#414 redesign)"
        );
    }

    /// Source-grep pin (freenet/river#414, Codex round-10 P2): a PARTIAL load
    /// failure (rail shows `List`, so its own Retry is hidden) must give the
    /// import modal a working way out — a "Retry loading rooms" control that
    /// re-fires the load — so imports aren't blocked indefinitely. Guarded on
    /// `saw_fetch_failure` so it only appears for a real failure.
    #[test]
    fn gated_import_offers_retry_on_partial_load_failure() {
        let prod = production_source();
        assert!(
            prod.contains("if saw_fetch_failure {"),
            "the gated import state must distinguish a partial-load FAILURE from \
             a still-loading state (freenet/river#414 round-10 P2)"
        );
        assert!(
            prod.contains("chat_delegate::retry_rooms_load();"),
            "the partial-load-failure state must wire a Retry button to \
             retry_rooms_load (freenet/river#414 round-10 P2)"
        );
        assert!(
            prod.contains("import-identity-retry-load"),
            "the Retry control needs a stable data-testid (import-identity-retry-load)"
        );
    }

    /// Source-grep pin (freenet/river#414, Codex round-6 P2-4): an in-place
    /// overwrite does NO GET, so `complete_identity_import` must itself trigger
    /// the new identity's member_info heal (via the shared
    /// `send_member_info_heal_update`) — otherwise the new identity renders
    /// "Unknown" to peers until an unrelated future heal. The heal must be built
    /// inside the ROOMS borrow (against the kept, secret-repopulated state).
    #[test]
    fn complete_identity_import_triggers_member_info_heal_on_overwrite() {
        let prod = production_source();
        assert!(
            prod.contains("existing.build_member_info_heal(&existing.room_state)"),
            "the overwrite must build the new identity's member_info heal against \
             the kept state (freenet/river#414 P2-4)"
        );
        assert!(
            prod.contains("send_member_info_heal_update("),
            "complete_identity_import must send the member_info heal UPDATE after an \
             in-place overwrite, since it does no GET (freenet/river#414 P2-4)"
        );
    }

    /// Source-grep pin (freenet/river#414 follow-up): a SUCCESSFUL identity
    /// import must auto-dismiss the modal (Ian hit the dialog staying open after
    /// importing). `complete_identity_import` is the single success path (every
    /// validation error returns in the caller before it), so the auto-close
    /// wiring lives there, guarded on the success flash so a manual close+reopen
    /// within the window can't clobber fresh state, and BOTH call sites hand it
    /// the `reset_and_close` closure. The error branches never call it, so an
    /// invalid token / still-loading state keeps the dialog open showing the error.
    #[test]
    fn successful_import_auto_dismisses_dialog() {
        let prod = production_source();

        // Stale pre-redesign copy is gone: the in-place swap keeps room_state, so
        // there is no re-fetch to wait on. Just confirm the import landed.
        assert!(
            !prod.contains("Syncing room state"),
            "the stale 'Syncing room state...' copy must be dropped; the in-place \
             swap keeps room_state (freenet/river#414 follow-up)"
        );
        assert!(
            prod.contains("Identity imported!"),
            "a successful import must show the 'Identity imported!' confirmation flash"
        );

        // The auto-close delays via the WASM-safe sleep, then runs inside defer().
        assert!(
            prod.contains("crate::util::sleep(crate::util::millis("),
            "the auto-dismiss must delay via the WASM-safe sleep before closing \
             (.claude/rules/dioxus-signal-safety.md)"
        );

        // Whitespace-normalized so rustfmt wrapping can't defeat the match.
        let normalized = prod.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            normalized
                .contains("if success_msg.try_read().is_ok_and(|m| m.is_some()) { close(); }"),
            "the auto-close must be GUARDED on the success flash still being shown, \
             so a manual close+reopen within the window can't clobber fresh state"
        );

        // Both success call sites (direct import + confirmed overwrite) pass the
        // modal-close closure to the success helper; the error branches never do.
        let call_sites = prod
            .matches("complete_identity_import(export, success_msg, error_msg, reset_and_close)")
            .count();
        assert_eq!(
            call_sites, 2,
            "both success call sites must pass reset_and_close so the dialog \
             auto-dismisses on success; found {call_sites}"
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
        // Whitespace-normalized so the multi-line call (owner_key + new_member_id)
        // is matched regardless of rustfmt wrapping.
        let prod = production_source()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            prod.contains("prune_dm_state_for_room( owner_key, new_member_id, )")
                || prod.contains("prune_dm_state_for_room(owner_key, new_member_id)"),
            "complete_identity_import must prune the old identity's DM state (keyed to \
             the NEW MemberId) on an overwrite that changes self_sk (freenet/river#414)"
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

    // ---------------------------------------------------------------
    // Impersonation warning (⚠) — wiring of `crate::util::confusable`
    // ---------------------------------------------------------------

    /// A room with `owner` (nickname `Room Owner`), a global moderator `mod`
    /// (nickname `Ian Clarke`, deputised by the owner), the `viewer`, and one
    /// extra member whose nickname the caller chooses.
    ///
    /// Returns the checker as the VIEWER sees it, plus the extra member's id —
    /// which is the shape every test below wants.
    fn impersonation_fixture(
        stranger_nickname: &str,
    ) -> (ImpersonationChecker, MemberId, MemberId, MemberId) {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let mod_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let stranger_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &mod_sk.verifying_key()),
                authorized_member(&owner_sk, &viewer_sk.verifying_key()),
                authorized_member(&owner_sk, &stranger_sk.verifying_key()),
            ],
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Room Owner", vec![id(&mod_sk)]),
                signed_member_info(&mod_sk, "Ian Clarke", vec![]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
                signed_member_info(&stranger_sk, stranger_nickname, vec![]),
            ],
        };
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, id(&viewer_sk));
        // Precondition: the moderator really does carry a shield in this view.
        assert!(
            badges.contains_key(&id(&mod_sk)),
            "fixture precondition: the moderator must show a shield to the viewer"
        );

        let checker = impersonation_checker_for_viewer(&member_info, &secrets, owner_id, &badges);
        // **Precondition, and the one that matters for every "is NOT flagged"
        // test below.** A non-empty shield map does not imply a non-empty
        // PROTECTED set — `claimed_name` filters placeholder and generated
        // names, so the checker can come out empty while badges are populated.
        // Without this, `plainly_different_names_are_not_flagged` and
        // `legitimate_non_latin_names_are_not_flagged` would both pass against
        // a checker that can never flag anything.
        assert!(
            !checker.is_empty(),
            "fixture precondition: the protected set must be non-empty, or the \
             negative assertions below prove nothing"
        );
        (checker, owner_id, id(&mod_sk), id(&stranger_sk))
    }

    /// A room whose OWNER and DEPUTY both carry non-Latin names, so the
    /// cross-script folds are exercised against a non-Latin protected name —
    /// the direction that actually bites. Every other test protects a Latin
    /// name, where a Cyrillic candidate can only ever fold *toward* Latin.
    fn non_latin_fixture() -> (ImpersonationChecker, MemberId) {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let mod_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let stranger_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &mod_sk.verifying_key()),
                authorized_member(&owner_sk, &viewer_sk.verifying_key()),
                authorized_member(&owner_sk, &stranger_sk.verifying_key()),
            ],
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                // Cyrillic owner, CJK deputy.
                signed_member_info(&owner_sk, "Дмитрий Волков", vec![id(&mod_sk)]),
                signed_member_info(&mod_sk, "李小龍", vec![]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
                signed_member_info(&stranger_sk, "Stranger", vec![]),
            ],
        };
        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, id(&viewer_sk));
        assert!(badges.contains_key(&id(&mod_sk)));
        let checker = impersonation_checker_for_viewer(&member_info, &secrets, owner_id, &badges);
        assert!(!checker.is_empty(), "non-Latin names must be protectable");
        (checker, id(&stranger_sk))
    }

    /// A protected name in a non-Latin script is still protected, and still does
    /// not swallow unrelated names in the same script.
    #[test]
    fn a_non_latin_protected_name_is_protected_without_over_matching() {
        let (checker, stranger) = non_latin_fixture();

        // A Latin-homoglyph attack on the Cyrillic owner: `Дмитрий` with a
        // Latin `T`-lookalike is not reachable, but the reverse IS — an
        // attacker who copies the name verbatim collides by construction.
        assert!(
            impersonation_warning_for_display(&checker, stranger, "Дмитрий Волков").is_some(),
            "a verbatim copy of the owner's Cyrillic name must be flagged"
        );
        assert!(
            impersonation_warning_for_display(&checker, stranger, "李小龍").is_some(),
            "a verbatim copy of the deputy's CJK name must be flagged"
        );

        // Other real names in the SAME scripts must not be caught.
        for name in [
            "Иван Петров",
            "Ольга Иванова",
            "Дмитрий Соколов", // shares a given name with the owner only
            "王小明",
            "李連杰",
            "さくら 田中",
            "Γιώργος Παπαδόπουλος",
        ] {
            assert_eq!(
                impersonation_warning_for_display(&checker, stranger, name),
                None,
                "{name:?} is an unrelated name in a protected script and must \
                 not be flagged"
            );
        }
    }

    /// **Regression: placeholder display names must never be protected.**
    ///
    /// `display_nickname` returns `"[Encrypted: {len} bytes, v{version}]"` when
    /// the unseal fails, which it does whenever the viewer's in-memory
    /// `secrets` map is missing the version — a real window, because `secrets`
    /// is `#[serde(skip)]` and rebuilt after every state ingestion.
    ///
    /// That string is a pure function of (nickname LENGTH, secret version), so
    /// it is shared by every member whose nickname is the same length. If it
    /// entered the protected set, one un-decryptable owner would flag a whole
    /// CLASS of innocent members at once, with no attacker present.
    #[test]
    fn undecryptable_and_placeholder_names_are_never_protected() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;
        use river_core::room_state::privacy::SealedBytes;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let mod_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let a_sk = SigningKey::generate(&mut rng);
        let b_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);

        // A private room whose secret this viewer does not hold.
        let sealed =
            |len: usize, version: u32| SealedBytes::private(vec![7u8; len], [0u8; 12], version, 3);
        let signed_sealed = |sk: &SigningKey, nickname: SealedBytes| {
            use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
            AuthorizedMemberInfo::new_with_member_key(
                MemberInfo {
                    member_id: MemberId::from(&sk.verifying_key()),
                    version: 0,
                    preferred_nickname: nickname,
                    deputies: vec![],
                },
                sk,
            )
        };

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &mod_sk.verifying_key()),
                authorized_member(&owner_sk, &viewer_sk.verifying_key()),
                authorized_member(&owner_sk, &a_sk.verifying_key()),
                authorized_member(&owner_sk, &b_sk.verifying_key()),
            ],
        };
        // The owner, the deputy and two ordinary members ALL have 12-byte
        // nicknames at v0, so all four render the identical placeholder.
        let member_info = MemberInfoV1 {
            member_info: vec![
                {
                    use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
                    AuthorizedMemberInfo::new_with_member_key(
                        MemberInfo {
                            member_id: owner_id,
                            version: 0,
                            preferred_nickname: sealed(12, 0),
                            deputies: vec![id(&mod_sk)],
                        },
                        &owner_sk,
                    )
                },
                signed_sealed(&mod_sk, sealed(12, 0)),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
                signed_sealed(&a_sk, sealed(12, 0)),
                signed_sealed(&b_sk, sealed(12, 0)),
            ],
        };
        let no_secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        // Precondition: they really do all render the same placeholder string,
        // so a name-based protected set WOULD flag them all.
        let rendered = |sk: &SigningKey| {
            display_nickname(
                &member_info
                    .canonical(MemberId::from(&sk.verifying_key()))
                    .expect("record")
                    .member_info
                    .preferred_nickname,
                &no_secrets,
            )
        };
        assert_eq!(rendered(&a_sk), rendered(&mod_sk));
        assert_eq!(rendered(&a_sk), rendered(&b_sk));
        assert!(
            rendered(&a_sk).contains("Encrypted"),
            "precondition: the undecryptable placeholder is what renders, got {:?}",
            rendered(&a_sk)
        );

        let badges = deputy_badges_for_viewer(
            &members,
            &member_info,
            &no_secrets,
            owner_id,
            id(&viewer_sk),
        );
        assert!(
            badges.contains_key(&id(&mod_sk)),
            "precondition: the deputy still carries a shield"
        );
        let checker =
            impersonation_checker_for_viewer(&member_info, &no_secrets, owner_id, &badges);

        // Nothing is protected, so nobody is accused.
        assert!(
            checker.is_empty(),
            "no name in this room is a CLAIMED name, so the protected set must \
             be empty"
        );
        for sk in [&a_sk, &b_sk] {
            assert_eq!(
                impersonation_warning_for_display(&checker, id(sk), &rendered(sk)),
                None,
                "an ordinary member must not be accused of impersonating an \
                 owner whose name merely failed to decrypt"
            );
        }
    }

    /// The `UNNAMED` placeholder is the same shape: a deputy whose nickname
    /// sanitises to nothing would otherwise flag every other member whose
    /// nickname also sanitises to nothing. `riverctl` writes `member_info`
    /// directly, so the nickname input's emoji rejection is not a boundary here.
    #[test]
    fn an_unnamed_deputy_is_not_protected() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let mod_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let other_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &mod_sk.verifying_key()),
                authorized_member(&owner_sk, &viewer_sk.verifying_key()),
                authorized_member(&owner_sk, &other_sk.verifying_key()),
            ],
        };
        // Emoji-only nicknames sanitise to `UNNAMED`.
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Room Owner", vec![id(&mod_sk)]),
                signed_member_info(&mod_sk, "\u{1F6E1}\u{1F451}", vec![]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
                signed_member_info(&other_sk, "\u{2B50}\u{1F3AA}", vec![]),
            ],
        };
        let unnamed = display_nickname(
            &member_info
                .canonical(id(&other_sk))
                .expect("record")
                .member_info
                .preferred_nickname,
            &secrets,
        );
        assert_eq!(
            unnamed,
            crate::util::display_name::UNNAMED,
            "precondition: an emoji-only nickname renders as the placeholder"
        );

        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, id(&viewer_sk));
        let checker = impersonation_checker_for_viewer(&member_info, &secrets, owner_id, &badges);

        assert_eq!(
            impersonation_warning_for_display(&checker, id(&other_sk), &unnamed),
            None,
            "`UNNAMED` is the placeholder for EVERY blank nickname, so it must \
             not be a protected name"
        );
        // The owner's real name is still protected, so this is not vacuous.
        assert!(impersonation_warning_for_display(&checker, id(&other_sk), "R00m 0wner").is_some());
    }

    /// A room where an ATTACKER (a strict ancestor of the viewer, so a
    /// viewer-relevant appointer) deputises a sockpuppet and names it whatever
    /// the caller asks. Returns the checker as the viewer sees it, plus the
    /// sockpuppet's id, an innocent bystander's id, and the real moderator's id.
    fn attacker_sockpuppet_fixture(
        puppet_nickname: &str,
    ) -> (ImpersonationChecker, MemberId, MemberId, MemberId) {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let mod_sk = SigningKey::generate(&mut rng);
        let attacker_sk = SigningKey::generate(&mut rng);
        let puppet_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let innocent_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        // The attacker invited the viewer, making the attacker a STRICT
        // ANCESTOR — which is all `viewer_relevant_deputizer_set` requires.
        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &mod_sk.verifying_key()),
                authorized_member(&owner_sk, &attacker_sk.verifying_key()),
                authorized_member(&owner_sk, &puppet_sk.verifying_key()),
                authorized_member(&owner_sk, &innocent_sk.verifying_key()),
                member_invited_by(&attacker_sk, owner_id, &viewer_sk.verifying_key()),
            ],
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                // The owner appoints the REAL moderator.
                signed_member_info(&owner_sk, "Room Owner", vec![id(&mod_sk)]),
                signed_member_info(&mod_sk, "Ian Clarke", vec![]),
                // The attacker appoints their sockpuppet. Nothing in
                // `MemberInfoV1::verify` stops this.
                signed_member_info(&attacker_sk, "Attacker", vec![id(&puppet_sk)]),
                signed_member_info(&puppet_sk, puppet_nickname, vec![]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
                signed_member_info(&innocent_sk, "Bob Smith", vec![]),
            ],
        };

        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, id(&viewer_sk));
        // Precondition: the attack primitive works — the sockpuppet really does
        // carry a shield in the viewer's view. If this ever stops holding, the
        // tests below would pass for the wrong reason.
        assert!(
            badges.contains_key(&id(&puppet_sk)),
            "precondition: an ancestor's sockpuppet IS badged to the viewer, \
             which is exactly why the protected set must not follow the badge"
        );
        assert!(badges.contains_key(&id(&mod_sk)));

        let checker = impersonation_checker_for_viewer(&member_info, &secrets, owner_id, &badges);
        (checker, id(&puppet_sk), id(&innocent_sk), id(&mod_sk))
    }

    /// **BLOCKING A1 — warning INJECTION.** An attacker must not be able to aim
    /// the badge at an innocent member.
    ///
    /// The primitive: any strict ancestor of the viewer deputises a sockpuppet
    /// and names it after an innocent member. If the protected set followed the
    /// badge map, that name became protected and the innocent member rendered
    /// "this member is NOT a moderator" across the attacker's whole invite
    /// subtree — remotely, on demand, and invisibly to the victim.
    #[test]
    fn an_attacker_cannot_aim_the_warning_at_an_innocent_member() {
        // The sockpuppet is named after the innocent member.
        let (checker, _puppet, innocent, _mod_id) = attacker_sockpuppet_fixture("Bob Smith");

        assert_eq!(
            impersonation_warning_for_display(&checker, innocent, "Bob Smith"),
            None,
            "an innocent member was accused because an ancestor deputised a \
             sockpuppet wearing their name — the protected set must not be \
             writable by anyone but the owner (or the viewer)"
        );

        // And the room's REAL protected names still work, so this is not
        // vacuous: the owner-appointed moderator's name is still protected.
        assert!(
            impersonation_warning_for_display(&checker, innocent, "\u{0399}an Clarke").is_some(),
            "the owner-appointed moderator's name must still be protected"
        );
    }

    /// **BLOCKING A2 — warning SUPPRESSION.** Being deputised must not buy
    /// immunity from impersonating someone else.
    ///
    /// The same primitive, aimed the other way: the sockpuppet names itself
    /// after the REAL moderator. Under a room-wide "privileged ⇒ never warned"
    /// exemption it rendered a genuine 🛡 and no ⚠ — and on the message author
    /// line that is worse than shipping nothing, because the real Ian is the
    /// OWNER and 👑 renders only in the member list, so the fake visually
    /// outranked the real one in the conversation.
    #[test]
    fn a_deputised_sockpuppet_cannot_suppress_its_own_warning() {
        let (checker, puppet, _innocent, mod_id) = attacker_sockpuppet_fixture("Ian Clarke");

        let warning = impersonation_warning_for_display(&checker, puppet, "Ian Clarke")
            .expect("a sockpuppet wearing the moderator's name MUST be flagged");
        assert_eq!(warning.impersonated.display_name, "Ian Clarke");
        assert_eq!(warning.impersonated.role, ProtectedRole::Deputy);

        // Homoglyph variants too, not just the literal string.
        assert!(impersonation_warning_for_display(&checker, puppet, "\u{0399}an Clarke").is_some());

        // The REAL moderator, under the same name, is still exempt — the
        // exemption is keyed on the identity the name was taken from.
        assert_eq!(
            impersonation_warning_for_display(&checker, mod_id, "Ian Clarke"),
            None
        );
    }

    /// The gate is on WHO APPOINTED the deputy, not on the badge existing. A
    /// deputy the VIEWER appointed is trusted (the viewer is trusting their own
    /// grant), so their name is protected; a deputy some other ancestor
    /// appointed is not.
    #[test]
    fn only_owner_or_viewer_appointed_deputies_contribute_a_protected_name() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let mine_sk = SigningKey::generate(&mut rng);
        let ancestor_sk = SigningKey::generate(&mut rng);
        let theirs_sk = SigningKey::generate(&mut rng);
        let stranger_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &ancestor_sk.verifying_key()),
                authorized_member(&owner_sk, &theirs_sk.verifying_key()),
                authorized_member(&owner_sk, &stranger_sk.verifying_key()),
                member_invited_by(&ancestor_sk, owner_id, &viewer_sk.verifying_key()),
                member_invited_by(&viewer_sk, owner_id, &mine_sk.verifying_key()),
            ],
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Room Owner", vec![]),
                // The viewer appoints `mine`; an unrelated ancestor appoints
                // `theirs`. Both are badged to the viewer.
                signed_member_info(&viewer_sk, "Viewer", vec![id(&mine_sk)]),
                signed_member_info(&ancestor_sk, "Ancestor", vec![id(&theirs_sk)]),
                signed_member_info(&mine_sk, "Trusted Mod", vec![]),
                signed_member_info(&theirs_sk, "Untrusted Mod", vec![]),
                signed_member_info(&stranger_sk, "Stranger", vec![]),
            ],
        };
        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, id(&viewer_sk));
        assert!(
            badges.contains_key(&id(&mine_sk)) && badges.contains_key(&id(&theirs_sk)),
            "precondition: BOTH deputies are badged to this viewer"
        );

        let checker = impersonation_checker_for_viewer(&member_info, &secrets, owner_id, &badges);
        assert!(
            impersonation_warning_for_display(&checker, id(&stranger_sk), "Trusted Mod").is_some(),
            "a deputy the VIEWER appointed is trusted, so their name is protected"
        );
        assert_eq!(
            impersonation_warning_for_display(&checker, id(&stranger_sk), "Untrusted Mod"),
            None,
            "a deputy appointed by some other ancestor must NOT contribute a \
             protected name — that is the attacker-writable path"
        );
    }

    /// **Wiring pin for the gate.** Source-scrape, because the gate is a single
    /// `continue` that a refactor could drop while every behavioural test still
    /// passes in a room that happens to have only owner-appointed deputies.
    #[test]
    fn the_protected_set_is_gated_on_the_appointer() {
        let src = include_str!("members.rs");
        let start = src
            .find("pub(crate) fn impersonation_checker_for_viewer")
            .expect("the builder must exist");
        let end = src[start..]
            .find("\n/// The impersonation warning a surface actually RENDERS")
            .expect("the builder is followed by the render boundary")
            + start;
        let body = &src[start..end];
        assert!(
            body.contains("Appointer::Owner"),
            "`impersonation_checker_for_viewer` no longer gates the protected \
             set on the appointer, so any strict ancestor of the viewer can \
             deputise a sockpuppet and inject a protected name"
        );
    }

    /// **The motivating attack.** `Ιan Clarke` with a Greek capital iota
    /// (U+0399) renders identically to the moderator's `Ian Clarke` and walks
    /// past every character-blocking rule River has: U+0399 is a letter, and
    /// rejecting it would break real Greek names.
    #[test]
    fn a_greek_homoglyph_of_a_deputys_name_is_flagged() {
        let homoglyph = "\u{0399}an Clarke";
        assert_ne!(homoglyph, "Ian Clarke", "precondition: different bytes");
        assert!(
            !crate::util::display_name::contains_hidden_chars(homoglyph),
            "precondition: the existing invisible-character defence does NOT \
             catch this, which is why the confusable check exists"
        );

        let (checker, _owner, mod_id, stranger) = impersonation_fixture(homoglyph);
        let warning = impersonation_warning_for_display(&checker, stranger, homoglyph)
            .expect("a Greek-iota homoglyph of a moderator's name must be flagged");

        assert_eq!(warning.tier, ConfusableTier::Identical);
        assert_eq!(warning.impersonated.role, ProtectedRole::Deputy);
        assert_eq!(warning.impersonated.display_name, "Ian Clarke");
        // The identified victim is on the WARNING, for a surface that can render
        // it as its own DOM node. It is deliberately NOT in the flat tooltip —
        // see `ImpersonationWarning::tooltip`, which names the role instead.
        assert!(
            !warning.tooltip().contains("Ian Clarke"),
            "no nickname may reach the flat tooltip: {}",
            warning.tooltip()
        );
        assert!(warning.tooltip().contains("is NOT a moderator"));

        // ...and the real moderator, under the real name, is untouched.
        assert_eq!(
            impersonation_warning_for_display(&checker, mod_id, "Ian Clarke"),
            None
        );
    }

    /// Requirement one: identity beats name. The genuine article is resolved by
    /// `MemberId`, which is derived from a keypair and cannot be chosen, so no
    /// nickname can make a real moderator or the owner look like an impostor.
    #[test]
    fn the_genuine_owner_and_deputy_are_never_flagged() {
        let (checker, owner_id, mod_id, stranger) = impersonation_fixture("Bystander");

        // Each under their own name.
        assert_eq!(
            impersonation_warning_for_display(&checker, mod_id, "Ian Clarke"),
            None,
            "the real moderator must never be flagged for their own name"
        );
        assert_eq!(
            impersonation_warning_for_display(&checker, owner_id, "Room Owner"),
            None,
            "the owner must never be flagged for their own name"
        );

        // Including under FOLDED variants of their own name — the exemption is
        // per-identity, not per-literal-string.
        for name in ["\u{0399}an Clarke", "lan Clarke", "IAN CLARKE"] {
            assert_eq!(
                impersonation_warning_for_display(&checker, mod_id, name),
                None,
                "the real moderator was flagged for a variant of their OWN name: {name:?}"
            );
        }
        assert_eq!(
            impersonation_warning_for_display(&checker, owner_id, "R00m 0wner"),
            None
        );

        // **The invariant is narrow, and deliberately so.** Taking a name that
        // is NOT yours is flagged even when you hold privilege: the exemption
        // is keyed on the member a protected name was TAKEN FROM, not on a
        // room-wide "privileged" set. The broad version was attacker-extendable
        // — a strict ancestor of the viewer can deputise a sockpuppet, which
        // put the sockpuppet in that set and let it wear `"Ian Clarke"` with a
        // genuine shield and no warning.
        assert!(
            impersonation_warning_for_display(&checker, owner_id, "Ian Clarke").is_some(),
            "the owner renaming themselves after a moderator IS impersonating \
             that moderator, and must be flagged"
        );

        // The exemption is identity-scoped, not name-scoped: the SAME names on
        // an unprivileged member are flagged. Without this half the test above
        // would pass against a checker that never flags anything.
        assert!(
            impersonation_warning_for_display(&checker, stranger, "\u{0399}an Clarke").is_some(),
            "an unprivileged member using the moderator's name must be flagged"
        );
        assert!(
            impersonation_warning_for_display(&checker, stranger, "R00m 0wner").is_some(),
            "an unprivileged member using the owner's name must be flagged"
        );
    }

    /// The other half of the bargain: ordinary members must not be accused.
    #[test]
    fn plainly_different_names_are_not_flagged() {
        let (checker, _owner, _mod, stranger) = impersonation_fixture("Bystander");
        for name in [
            "Bystander",
            "Alice",
            "HostFat",
            "Clark Kent",
            "Linus Clarke",
            "Ian Clarke's Dad",
            "zorolin",
            "Bob Smith",
        ] {
            assert_eq!(
                impersonation_warning_for_display(&checker, stranger, name),
                None,
                "{name:?} is an ordinary name and must not be accused"
            );
        }
    }

    /// A confusable check that flags real people's names is worse than the
    /// problem it solves — and it would land hardest on the users least able to
    /// argue with it. Same guard `display_name::real_names_in_other_scripts_are_untouched`
    /// provides for sanitisation, applied to the rendered warning.
    #[test]
    fn legitimate_non_latin_names_are_not_flagged() {
        let (checker, _owner, _mod, stranger) = impersonation_fixture("Bystander");
        for name in [
            "Иван Петров",          // Russian
            "Ольга Иванова",        // Russian
            "Γιώργος Παπαδόπουλος", // Greek
            "Νίκος Παπαδόπουλος",   // Greek
            "さくら 田中",          // Japanese
            "山田\u{3000}太郎",     // Japanese, ideographic space
            "李小龍",               // Chinese
            "김민준",               // Korean
            "محمد عبد الله",        // Arabic
            "علی\u{200C}رضا",       // Persian, ZWNJ as orthography
            "דָּוִד",                  // Hebrew with niqqud
            "अमिताभ बच्चन",          // Devanagari
            "Nguyễn Thị Hương",     // Vietnamese
            "François Müller",      // accented Latin
        ] {
            assert_eq!(
                impersonation_warning_for_display(&checker, stranger, name),
                None,
                "a legitimate name was accused of impersonation: {name:?}"
            );
        }
    }

    /// **The tier decision.** The engine reports near-misses; this UI does not
    /// render them. Asserting the engine still FINDS the near-miss is the point
    /// — otherwise this passes for the wrong reason (an engine that stopped
    /// detecting it) and the tier rule would be untested.
    #[test]
    fn near_miss_is_never_rendered() {
        let (checker, _owner, _mod, stranger) = impersonation_fixture("Bystander");

        for (name, why) in [
            ("Ian Clark", "one character short"),
            ("Ian Clrake", "transposition"),
            ("Ian Clarkee", "one character long"),
        ] {
            // The engine finds it...
            let raw = checker
                .check(stranger, name)
                .unwrap_or_else(|| panic!("engine should still detect {name:?} ({why})"));
            assert_eq!(
                raw.tier,
                ConfusableTier::NearMiss,
                "{name:?} should be a near-miss, not an identical skeleton"
            );
            // ...and the render boundary drops it.
            assert_eq!(
                impersonation_warning_for_display(&checker, stranger, name),
                None,
                "{name:?} ({why}) is a near-miss and must not render a badge"
            );
        }
    }

    /// The protected set is DERIVED from room state, so it tracks a deputize and
    /// a revoke with no hand-maintained list anywhere.
    #[test]
    fn the_protected_set_follows_deputize_and_revoke() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let mod_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let impostor_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &mod_sk.verifying_key()),
                authorized_member(&owner_sk, &viewer_sk.verifying_key()),
                authorized_member(&owner_sk, &impostor_sk.verifying_key()),
            ],
        };

        // The one thing that varies between the two states: whether the owner's
        // signed `deputies` list names the moderator.
        let build = |deputies: Vec<MemberId>| {
            let member_info = MemberInfoV1 {
                member_info: vec![
                    signed_member_info(&owner_sk, "Room Owner", deputies),
                    signed_member_info(&mod_sk, "Ian Clarke", vec![]),
                    signed_member_info(&viewer_sk, "Viewer", vec![]),
                    signed_member_info(&impostor_sk, "\u{0399}an Clarke", vec![]),
                ],
            };
            let badges = deputy_badges_for_viewer(
                &members,
                &member_info,
                &secrets,
                owner_id,
                id(&viewer_sk),
            );
            impersonation_checker_for_viewer(&member_info, &secrets, owner_id, &badges)
        };

        // Deputised: the moderator's name is protected, so the homoglyph warns.
        let deputised = build(vec![id(&mod_sk)]);
        assert!(
            impersonation_warning_for_display(&deputised, id(&impostor_sk), "\u{0399}an Clarke")
                .is_some(),
            "while the moderator is deputised their name must be protected"
        );

        // Revoked: the same room, the same nicknames, the same impostor — and
        // no warning, because `Ian Clarke` is now an ordinary member's name.
        let revoked = build(vec![]);
        assert_eq!(
            impersonation_warning_for_display(&revoked, id(&impostor_sk), "\u{0399}an Clarke"),
            None,
            "after the deputy grant is revoked the name is no longer protected"
        );

        // The owner is protected in BOTH states — the owner's protection comes
        // from being the owner, not from any deputy grant.
        for (label, checker) in [("deputised", &deputised), ("revoked", &revoked)] {
            assert!(
                impersonation_warning_for_display(checker, id(&impostor_sk), "R00m 0wner")
                    .is_some(),
                "the owner must stay protected in the {label} state"
            );
        }
    }

    /// A member who never typed a nickname wears a handle River derived from
    /// their key. Two members can be assigned the same one — with ~120 members
    /// in a room, more likely than not — and that collision is River's doing,
    /// not an imitation either of them performed. Protecting an unclaimed name
    /// would turn it into an accusation.
    #[test]
    fn a_deputys_generated_handle_is_not_protected() {
        // A real assignable handle, so this cannot pass because the string
        // happens not to match.
        let handle = format!(
            "{} {}",
            crate::nickname::FIRST_NAMES[0],
            crate::nickname::LAST_NAMES[0]
        );
        assert!(
            crate::nickname::is_generated_handle(&handle),
            "precondition: {handle:?} must be a handle River can assign"
        );

        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let mod_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let twin_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &mod_sk.verifying_key()),
                authorized_member(&owner_sk, &viewer_sk.verifying_key()),
                authorized_member(&owner_sk, &twin_sk.verifying_key()),
            ],
        };
        // The moderator never chose a nickname; an unrelated member drew the
        // same handle.
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Room Owner", vec![id(&mod_sk)]),
                signed_member_info(&mod_sk, &handle, vec![]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
                signed_member_info(&twin_sk, &handle, vec![]),
            ],
        };
        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, id(&viewer_sk));
        assert!(
            badges.contains_key(&id(&mod_sk)),
            "precondition: the moderator still carries a shield"
        );

        let checker = impersonation_checker_for_viewer(&member_info, &secrets, owner_id, &badges);
        assert_eq!(
            impersonation_warning_for_display(&checker, id(&twin_sk), &handle),
            None,
            "an assigned handle is not a claimed identity; colliding with one \
             must not put an impersonation badge on a member who did nothing"
        );

        // But the identity exemption is still wider than the protected set: the
        // owner's CHOSEN name stays protected in the same room, so this test
        // cannot pass by producing an empty checker.
        assert!(
            impersonation_warning_for_display(&checker, id(&twin_sk), "R00m 0wner").is_some(),
            "the owner's chosen name must still be protected"
        );
    }

    /// A member with no `member_info` record renders as `"Unknown"`, and so does
    /// every other member without one. Protecting that string would flag all of
    /// them for impersonating each other.
    #[test]
    fn a_privileged_member_without_member_info_is_not_protected() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let mod_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let nameless_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &mod_sk.verifying_key()),
                authorized_member(&owner_sk, &viewer_sk.verifying_key()),
                authorized_member(&owner_sk, &nameless_sk.verifying_key()),
            ],
        };
        // The moderator is deputised but has NO member_info record at all.
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Room Owner", vec![id(&mod_sk)]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
            ],
        };
        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, id(&viewer_sk));
        let checker = impersonation_checker_for_viewer(&member_info, &secrets, owner_id, &badges);

        // `"Unknown"` is what the member list renders for a member with no
        // record; it must not be a protected name.
        assert_eq!(
            impersonation_warning_for_display(&checker, id(&nameless_sk), "Unknown"),
            None,
            "`Unknown` is the placeholder for EVERY record-less member, so \
             protecting it would accuse all of them"
        );
        // The owner's real name is still protected.
        assert!(
            impersonation_warning_for_display(&checker, id(&nameless_sk), "R00m 0wner").is_some()
        );
    }

    /// A shielded deputy with a name of their own shows no ⚠, and two
    /// owner-appointed deputies whose names COLLIDE both do.
    ///
    /// **This used to claim the two badges are mutually exclusive, which is no
    /// longer true and should not be restored.** That held only because a
    /// privileged member was exempt from every protected name — the broad rule
    /// an attacker extended by deputising a sockpuppet. With the exemption
    /// keyed per-name, two genuine deputies with confusable names warn about
    /// each other, which is the honest answer: the owner appointed two people a
    /// reader cannot tell apart, and surfacing that is the feature working.
    #[test]
    fn a_deputy_shows_no_warning_unless_a_protected_name_collides() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let mod_a_sk = SigningKey::generate(&mut rng);
        let mod_b_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &mod_a_sk.verifying_key()),
                authorized_member(&owner_sk, &mod_b_sk.verifying_key()),
                authorized_member(&owner_sk, &viewer_sk.verifying_key()),
            ],
        };
        // Case 1: two owner-appointed deputies with DISTINCT names. Neither
        // shows a warning — the common case, and the one that matters for not
        // badging the people the shield is meant to vouch for.
        let distinct = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Room Owner", vec![id(&mod_a_sk), id(&mod_b_sk)]),
                signed_member_info(&mod_a_sk, "Ian Clarke", vec![]),
                signed_member_info(&mod_b_sk, "Nacho Duart", vec![]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
            ],
        };
        let badges =
            deputy_badges_for_viewer(&members, &distinct, &secrets, owner_id, id(&viewer_sk));
        let checker = impersonation_checker_for_viewer(&distinct, &secrets, owner_id, &badges);
        for (sk, name) in [(&mod_a_sk, "Ian Clarke"), (&mod_b_sk, "Nacho Duart")] {
            assert!(
                badges.contains_key(&id(sk)),
                "precondition: {name:?} carries a shield"
            );
            assert_eq!(
                impersonation_warning_for_display(&checker, id(sk), name),
                None,
                "a shielded deputy with a name of their own must show no warning"
            );
        }

        // Case 2: two owner-appointed deputies whose names COLLIDE. Both are
        // flagged, each naming the other — the exemption covers only your own
        // identity.
        let colliding = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Room Owner", vec![id(&mod_a_sk), id(&mod_b_sk)]),
                signed_member_info(&mod_a_sk, "Ian Clarke", vec![]),
                signed_member_info(&mod_b_sk, "\u{0399}an Clarke", vec![]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
            ],
        };
        let badges =
            deputy_badges_for_viewer(&members, &colliding, &secrets, owner_id, id(&viewer_sk));
        let checker = impersonation_checker_for_viewer(&colliding, &secrets, owner_id, &badges);
        for (sk, name) in [(&mod_a_sk, "Ian Clarke"), (&mod_b_sk, "\u{0399}an Clarke")] {
            assert!(
                impersonation_warning_for_display(&checker, id(sk), name).is_some(),
                "two deputies a reader cannot tell apart must BOTH be flagged; \
                 exempting them is the broad rule an attacker extended"
            );
        }
    }

    /// The protected set is built from a `HashMap`, so its iteration order is
    /// per-process random. Two protected members CAN share a skeleton (a
    /// sockpuppet deputised under a real moderator's name), and without the sort
    /// in `impersonation_checker_for_viewer` the tooltip would name a different
    /// victim between renders of the same room.
    #[test]
    fn the_named_victim_is_stable_across_rebuilds() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let mod_a_sk = SigningKey::generate(&mut rng);
        let mod_b_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let impostor_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &mod_a_sk.verifying_key()),
                authorized_member(&owner_sk, &mod_b_sk.verifying_key()),
                authorized_member(&owner_sk, &viewer_sk.verifying_key()),
                authorized_member(&owner_sk, &impostor_sk.verifying_key()),
            ],
        };
        // Two deputies whose names fold to the SAME skeleton.
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Room Owner", vec![id(&mod_a_sk), id(&mod_b_sk)]),
                signed_member_info(&mod_a_sk, "Ian Clarke", vec![]),
                signed_member_info(&mod_b_sk, "lan Clarke", vec![]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
                signed_member_info(&impostor_sk, "1an Clarke", vec![]),
            ],
        };

        let named = || {
            let badges = deputy_badges_for_viewer(
                &members,
                &member_info,
                &secrets,
                owner_id,
                id(&viewer_sk),
            );
            let checker =
                impersonation_checker_for_viewer(&member_info, &secrets, owner_id, &badges);
            impersonation_warning_for_display(&checker, id(&impostor_sk), "1an Clarke")
                .expect("the impostor is flagged")
                .impersonated
                .display_name
        };

        let first = named();
        for _ in 0..24 {
            assert_eq!(
                named(),
                first,
                "the tooltip named a different victim on a rebuild of the same \
                 room state; the protected set is not deterministically ordered"
            );
        }
    }

    /// The member row renders the warning as its own tag, FIRST, carrying the
    /// unforgeable glyph and the full tooltip.
    #[test]
    fn the_member_row_renders_the_warning_first() {
        let mut display = make_member_display("\u{0399}an Clarke");
        // A member wearing several relationship tags, so "first" is a real
        // assertion rather than the only possibility.
        display.is_self = true;
        display.invited_by_you = true;
        display.impersonation = Some(ImpersonationWarning {
            impersonated: ProtectedName::new(
                ProtectedRole::Deputy,
                "Ian Clarke",
                MemberId(freenet_scaffold::util::FastHash(1)),
            ),
            tier: ConfusableTier::Identical,
        });

        let parts = member_display_parts(&display);
        let (glyph, tooltip) = parts.tags.first().expect("at least one tag");
        assert_eq!(
            *glyph,
            crate::util::confusable::WARNING_GLYPH,
            "the impersonation warning must be the FIRST tag, next to the name; \
             tags were {:?}",
            parts.tags.iter().map(|(g, _)| *g).collect::<Vec<_>>()
        );
        assert!(tooltip.contains("is NOT a moderator"), "{tooltip}");
        // The row's tooltip is flat hover text, so it must carry no nickname —
        // the member row is one of the surfaces that cannot render separate
        // nodes for it.
        assert!(!tooltip.contains("Ian Clarke"), "{tooltip}");
        // The relationship tags are still rendered, after it.
        assert!(parts.tags.len() > 1);

        // And no warning ⇒ no tag.
        display.impersonation = None;
        assert!(!member_display_parts(&display)
            .tags
            .iter()
            .any(|(g, _)| *g == crate::util::confusable::WARNING_GLYPH));
    }

    /// **The residual #488 knowingly left open, and this warning is its only
    /// mitigation.**
    ///
    /// An Ideographic Variation Selector is legitimate orthography — `辻` has
    /// the registered variant `辻\u{E0100}` — so #488 deliberately KEEPS one
    /// after an ideograph rather than stripping it. The cost is that
    /// `"李\u{E0100}小龍"` and `"李小龍"` render identically on any font without
    /// an IVD entry for the sequence, which is a clone of another member's
    /// rendered name that `sanitize_display_name` will not remove.
    ///
    /// The layering is what closes it. `is_display_hidden` keeps the selectors
    /// INSIDE its plane-14 range and the in-context exception is applied ON TOP
    /// by `sanitize_display_name` / `contains_hidden_chars`, so
    /// `confusable::skeleton` — which calls `is_display_hidden` DIRECTLY — still
    /// folds the two names together and this warning fires.
    ///
    /// That makes the dependency mutual and easy to break from either side:
    /// moving the carve-out INTO `is_display_hidden` (punching a hole in the
    /// range instead of layering over it) would silently switch this warning
    /// off and leave the residual undetectable. #488 documents that at the call
    /// site; this test is the CI gate on our side of it.
    #[test]
    fn an_ideographic_variation_selector_clone_is_flagged() {
        use river_core::room_state::member::MembersV1;
        use river_core::room_state::member_info::MemberInfoV1;

        const IVS: char = '\u{E0100}';
        let real = "李小龍";
        let clone = format!("李{IVS}小龍");

        // Preconditions, and the reason this test has to exist.
        //
        // 1. The IVS SURVIVES sanitisation, so the two members really do render
        //    a pixel-identical name — the existing invisible-character defence
        //    does not catch this one, by design.
        assert_eq!(
            crate::util::display_name::sanitize_display_name(&clone),
            clone,
            "#488 keeps an IVS after an ideograph; if that changed, this \
             residual is closed elsewhere and this test should be revisited"
        );
        assert!(
            !crate::util::display_name::contains_hidden_chars(&clone),
            "the nickname input accepts it too — the residual is real"
        );
        assert_ne!(clone, real, "different strings, identical rendering");

        // 2. But `skeleton` folds them together, because it consults
        //    `is_display_hidden` directly and the selectors are still inside
        //    its plane-14 range.
        assert_eq!(
            crate::util::confusable::skeleton(&clone),
            crate::util::confusable::skeleton(real),
            "the confusable fold must see through an IVS, or the warning \
             cannot fire on this residual"
        );

        // End to end, through the real protected-set derivation.
        let mut rng = rand::thread_rng();
        let owner_sk = SigningKey::generate(&mut rng);
        let mod_sk = SigningKey::generate(&mut rng);
        let viewer_sk = SigningKey::generate(&mut rng);
        let impostor_sk = SigningKey::generate(&mut rng);

        let id = |sk: &SigningKey| MemberId::from(&sk.verifying_key());
        let owner_id = id(&owner_sk);
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();

        let members = MembersV1 {
            members: vec![
                authorized_member(&owner_sk, &mod_sk.verifying_key()),
                authorized_member(&owner_sk, &viewer_sk.verifying_key()),
                authorized_member(&owner_sk, &impostor_sk.verifying_key()),
            ],
        };
        let member_info = MemberInfoV1 {
            member_info: vec![
                signed_member_info(&owner_sk, "Room Owner", vec![id(&mod_sk)]),
                signed_member_info(&mod_sk, real, vec![]),
                signed_member_info(&viewer_sk, "Viewer", vec![]),
                signed_member_info(&impostor_sk, &clone, vec![]),
            ],
        };
        let badges =
            deputy_badges_for_viewer(&members, &member_info, &secrets, owner_id, id(&viewer_sk));
        assert!(
            badges.contains_key(&id(&mod_sk)),
            "precondition: the CJK-named moderator carries a shield"
        );
        let checker = impersonation_checker_for_viewer(&member_info, &secrets, owner_id, &badges);

        let warning = impersonation_warning_for_display(&checker, id(&impostor_sk), &clone)
            .expect("an IVS clone of a moderator's name must be flagged");
        assert_eq!(
            warning.tier,
            ConfusableTier::Identical,
            "an IVS clone renders as the SAME string, so it is a tier-1 match, \
             not a near-miss (which this UI does not render)"
        );
        assert_eq!(warning.impersonated.display_name, real);

        // And the real moderator is untouched, so the fold has not simply made
        // everything match.
        assert_eq!(
            impersonation_warning_for_display(&checker, id(&mod_sk), real),
            None
        );
        assert_eq!(
            impersonation_warning_for_display(&checker, id(&viewer_sk), "Viewer"),
            None
        );
    }

    /// **Wiring pin.** The warning only protects anyone if the surfaces actually
    /// render it. Both go through `impersonation_warning_for_display`, and both
    /// must build the checker OUTSIDE their per-member loop — rebuilding it per
    /// member re-folds every protected name (and, in a private room, re-unseals
    /// every protected nickname) once per row.
    ///
    /// Source-scrape, because these are Dioxus component trees with no headless
    /// harness in this crate. Scans the WHOLE file rather than cutting at
    /// `#[cfg(test)]`: `conversation.rs` has an earlier `#[cfg(test)]` block, so
    /// a cut would silently skip the code this pins (the mistake
    /// `display_name::nickname_render_paths_go_through_display_nickname`
    /// documents).
    #[test]
    fn impersonation_warning_is_wired_into_both_render_surfaces() {
        let members_src = include_str!("members.rs");
        let conversation = include_str!("conversation.rs");

        /// The source between two anchors, so a check is scoped to ONE
        /// function rather than the whole file.
        ///
        /// The whole-file version of this test PASSED against a mutation that
        /// deleted the `MemberList` call site, because `members.rs` also
        /// contains the function's own DEFINITION and its tests. Scoping is
        /// the entire point — do not widen these ranges.
        fn between<'a>(src: &'a str, from: &str, to: &str, what: &str) -> &'a str {
            let start = src
                .find(from)
                .unwrap_or_else(|| panic!("anchor {from:?} not found ({what})"));
            let end = src[start..]
                .find(to)
                .unwrap_or_else(|| panic!("anchor {to:?} not found after {from:?} ({what})"))
                + start;
            &src[start..end]
        }

        // --- Member list -------------------------------------------------
        let member_memo = between(
            members_src,
            "pub fn MemberList()",
            "let handle_member_click",
            "MemberList memo",
        );
        assert!(
            member_memo.contains("impersonation_warning_for_display("),
            "MemberList no longer computes an impersonation warning, so the \
             member list shows no warning however confusable a name is"
        );
        assert!(
            member_memo.contains("impersonation_checker_for_viewer("),
            "MemberList no longer builds an impersonation checker"
        );
        // Built ONCE, before the per-member loop — not per row, which would
        // re-fold every protected name (and re-unseal every protected
        // nickname in a private room) for each member.
        assert!(
            between(
                member_memo,
                "pub fn MemberList()",
                "for &member_id in &ordered_ids",
                "MemberList per-member loop",
            )
            .contains("impersonation_checker_for_viewer("),
            "MemberList must build the impersonation checker BEFORE the \
             per-member loop, not inside it"
        );
        // ...and the row must actually RENDER it. A warning computed but not
        // rendered protects nobody.
        assert!(
            between(
                members_src,
                "fn member_display_parts(",
                "\n/// Order member IDs",
                "member_display_parts",
            )
            .contains("WARNING_GLYPH"),
            "the member row no longer renders the warning glyph"
        );

        // --- Message author line ------------------------------------------
        let group_fn = between(
            conversation,
            "fn group_messages(",
            "\n/// Format an event summary",
            "group_messages",
        );
        assert!(
            group_fn.contains("impersonation_warning_for_display("),
            "`group_messages` no longer computes an impersonation warning, so \
             message author lines show no warning"
        );
        assert!(
            !group_fn.contains("impersonation_checker_for_viewer("),
            "`group_messages` builds the checker itself; it must receive one \
             built once per render, or the protected set is refolded for \
             every message"
        );
        assert!(
            conversation.contains("impersonation_checker_for_viewer("),
            "the conversation no longer builds an impersonation checker"
        );
        // The rendered element, anchored on its test id.
        assert!(
            conversation.contains("\"data-testid\": \"message-author-impersonation-warning\""),
            "the message author line no longer renders the warning element"
        );
        assert!(
            between(
                conversation,
                "\"data-testid\": \"message-author-impersonation-warning\"",
                "// Deputy shield.",
                "author-line warning element",
            )
            .contains("WARNING_GLYPH"),
            "the author-line warning element no longer renders the warning glyph"
        );
    }
}
