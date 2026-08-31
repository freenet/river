//! Read-side deputy queries (deputy ban authority, freenet/river#410).
//!
//! Deputies are **per-deputizer**, never a global flag. Each member's own
//! signed `MemberInfo` carries a `deputies: Vec<MemberId>` listing the members
//! *they* have deputized, and that grant only scopes to that deputizer's invite
//! subtree (the room owner's subtree being everyone). The UI's shield badges are
//! viewer-scoped — they show who *you* deputized — which is a recurring source
//! of confusion, so every rendering here names the deputizer explicitly.
//!
//! Two directions are supported:
//!
//! * **forward** — [`RoomDeputies::deputies_of`]: "who has X deputized?"
//! * **reverse** — [`RoomDeputies::deputizers_of`]: "who has deputized X?",
//!   which is what answers "is this member a deputy of anyone (the owner, the
//!   invite bot, …)?"
//!
//! Every read routes through [`MemberInfoV1::deputies_of`], which selects the
//! CANONICAL (highest-rank) `member_info` record per member. That is
//! load-bearing: `MemberInfoV1::verify` deliberately accepts a state carrying
//! more than one record for the same member (migration-safety), so a bare
//! first-match scan of `member_info.member_info` can read a LOSING — e.g.
//! already-revoked — record and report deputy authority that the contract does
//! not actually enforce.
//!
//! This module is READ-ONLY. It never builds a delta and never touches contract
//! state. It does own one predicate the WRITE path consults —
//! [`self_removing_ban_reason`] (freenet/river#478) — because the invite-subtree
//! walk it needs already lives here as [`invite_subtrees`]; it is still a pure
//! query over a fetched state.

use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::{AuthorizedMember, MemberId, MembersV1};
use river_core::room_state::member_info::MemberInfoV1;
use river_core::room_state::ChatRoomStateV1;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// One party of a deputy grant, resolved for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeputyParty {
    /// The member's short id, as printed by `member list`.
    pub member_id: String,
    /// The member's nickname, or `null` when they have no `member_info` record
    /// in this room (e.g. a deputy who was pruned for inactivity but is still
    /// listed in their deputizer's signed record).
    pub nickname: Option<String>,
    /// Whether this party is the room owner.
    pub is_owner: bool,
    /// Whether this party is currently in the room: present in
    /// `members.members`, **or** the room owner (who is never in that list —
    /// see `MembersV1::is_ban_authorized`).
    pub in_room: bool,
}

/// How far a grant reaches, when it is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeputyScope {
    /// Granted by the room owner, whose invite subtree is the whole room.
    RoomWide,
    /// Granted by a regular member: authority only within that member's own
    /// invite subtree, which may be empty.
    InviteSubtree,
}

/// A single `deputizer -> deputy` grant, resolved for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeputyGrant {
    pub deputizer: DeputyParty,
    pub deputy: DeputyParty,
    pub scope: DeputyScope,
    /// How many OTHER members, within this grant's scope, the deputy can ban.
    ///
    /// This is what separates "the grant exists" from "the grant does
    /// anything". The per-target decision is never re-derived locally: each
    /// candidate is decided by calling `MembersV1::is_ban_authorized` itself.
    /// Do NOT reintroduce a local copy of its rules, which is what an earlier
    /// version got wrong twice. The scope is the grant's: every member for an
    /// owner grant, otherwise the deputizer's invite subtree.
    ///
    /// There are exactly TWO deliberate deviations from the raw predicate, both
    /// narrowing:
    ///
    /// * The deputy is not counted against themselves. `is_ban_authorized` has
    ///   no `banner == target` check and self-ban really is permitted, but a
    ///   deputy who can only ban themselves holds no moderation authority, and
    ///   counting it would give a script filtering `members_deputy_can_ban > 0` a
    ///   false positive. Nothing is hidden by this: if the self-ban cascade
    ///   would remove anyone else, those members are already counted
    ///   individually (they are the deputy's descendants, hence in scope and
    ///   authorized via step 2).
    /// * An inactive grant reports `0` outright, without consulting the
    ///   predicate at all. See [`Self::active`] for why that is safe.
    ///
    /// The room owner needs no exclusion: they are never in `members.members`
    /// and `is_ban_authorized` refuses `target == owner` outright.
    ///
    /// `0` whenever [`Self::active`] is false, since the contract honours
    /// nothing then. Frequently `0` for a LIVE grant too: most members have
    /// invited nobody, so a grant from them reaches no one. That is the common
    /// case, not a corner.
    ///
    /// This counts CAPABILITY, not attribution: a target the deputy could also
    /// ban by another route (they are that target's own invite ancestor, say)
    /// still counts, so revoking THIS grant may not change the number. The
    /// alternative would print `0` for an owner-appointed moderator who can
    /// plainly ban the whole subtree, which is the same class of false negative
    /// this field exists to avoid. It also counts DIRECT targets: a ban
    /// cascades to the target's own invite subtree
    /// (`MembersV1::get_downstream_members`), so the number understates removal
    /// blast radius for every target, not just the excluded self.
    pub members_deputy_can_ban: usize,
    /// Whether the grant is structurally live: both parties are currently in the
    /// room. The load-bearing half is the DEPUTY: `is_ban_authorized` gates both
    /// deputy branches on the banner being a current member, so a grant naming a
    /// pruned deputy is honoured by nobody. The deputizer half is
    /// defence-in-depth: `MemberInfoV1::verify` rejects a `member_info` record
    /// whose member is neither the owner nor a current member, so a deputizer
    /// absent from the room cannot occur in a state the contract accepted.
    ///
    /// `active` does NOT mean the deputy can ban a given person; see
    /// [`Self::members_deputy_can_ban`] and [`Self::scope`] for how far it goes.
    pub active: bool,
}

/// Why a short member id could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No member id in the room matched.
    NotFound,
    /// The input matched more than one member id (as a prefix, or as a
    /// case-insensitive full match). Carries the matching ids sorted by their
    /// printed form, so the message lists them the way the user sees them.
    Ambiguous(Vec<String>),
}

/// Deputy queries over one fetched room state.
///
/// Borrows the state; construct one per fetch. `secrets` is the private-room
/// decryption map from `ApiClient::room_display_secrets` (empty for a public
/// room), used only to render nicknames.
///
/// The canonical deputy lists are resolved ONCE in [`Self::new`], so a caller
/// annotating every row of a member listing does not re-scan `member_info` per
/// row. Resolution still goes through [`MemberInfoV1::deputies_of`], which stays
/// the single source of truth for which record wins.
pub struct RoomDeputies<'a> {
    state: &'a ChatRoomStateV1,
    secrets: &'a HashMap<u32, [u8; 32]>,
    owner_id: MemberId,
    current_members: HashSet<MemberId>,
    /// Canonical `deputizer -> deputies` for every member holding a
    /// `member_info` record. Values are sorted by printed id and deduplicated.
    deputies_by_deputizer: BTreeMap<MemberId, Vec<MemberId>>,
    /// Every id this room mentions anywhere (see [`Self::resolve_short_id`]).
    known_ids: BTreeSet<MemberId>,
    /// `member -> the members they invited, directly or indirectly`, for every
    /// member with a non-empty subtree.
    subtrees: HashMap<MemberId, HashSet<MemberId>>,
    /// Index required by `MembersV1::is_ban_authorized`, built once.
    members_by_id: HashMap<MemberId, &'a AuthorizedMember>,
    /// The room owner's verifying key. The owner is never in `members_by_id`
    /// (they are not in `members.members`), so their key is held separately to
    /// answer [`Self::verifying_key`] for the owner.
    owner_vk: VerifyingKey,
}

impl<'a> RoomDeputies<'a> {
    pub fn new(
        state: &'a ChatRoomStateV1,
        owner_vk: &VerifyingKey,
        secrets: &'a HashMap<u32, [u8; 32]>,
    ) -> Self {
        let owner_id = MemberId::from(owner_vk);
        let current_members: HashSet<MemberId> = state
            .members
            .members
            .iter()
            .map(|m| m.member.id())
            .collect();

        // Ids that could hold a `deputies` list: every member with a
        // `member_info` record. Deputies live nowhere else, so a member without
        // a record has, by definition, deputized no one.
        let candidates: BTreeSet<MemberId> = state
            .member_info
            .member_info
            .iter()
            .map(|info| info.member_info.member_id)
            .collect();

        let mut deputies_by_deputizer: BTreeMap<MemberId, Vec<MemberId>> = BTreeMap::new();
        for deputizer in candidates {
            let mut ids: Vec<MemberId> = state.member_info.deputies_of(deputizer).to_vec();
            // Sort by (printed id, id) rather than printed id alone: the printed
            // form is 40 bits of a 64-bit hash, so two distinct ids CAN print
            // the same, and under a printed-id-only sort they need not land
            // adjacent, which would defeat `dedup`.
            ids.sort_by_key(|id| (id.to_string(), *id));
            ids.dedup();
            deputies_by_deputizer.insert(deputizer, ids);
        }

        // Every id the room mentions anywhere: the owner, current members,
        // members with a `member_info` record, and ids appearing in ANY
        // deputies list. The last source matters: a deputy pruned for
        // inactivity keeps no `member_info` record but is still named by their
        // deputizer's signed record, and "is this (now absent) member still
        // someone's deputy?" is a question worth being able to ask.
        let mut known_ids: BTreeSet<MemberId> = BTreeSet::new();
        known_ids.insert(owner_id);
        known_ids.extend(current_members.iter().copied());
        for (deputizer, deputies) in &deputies_by_deputizer {
            known_ids.insert(*deputizer);
            known_ids.extend(deputies.iter().copied());
        }

        let subtrees = invite_subtrees(state);
        let members_by_id = state.members.members_by_member_id();

        Self {
            state,
            secrets,
            owner_id,
            current_members,
            deputies_by_deputizer,
            known_ids,
            subtrees,
            members_by_id,
            owner_vk: *owner_vk,
        }
    }

    /// The member's ed25519 verifying key — their cryptographic identity — when
    /// this room state carries it: the room owner, or a current member of
    /// `members.members`. Returns `None` for a member named only by a
    /// deputizer's signed record (e.g. one pruned for inactivity), for whom no
    /// key is stored.
    ///
    /// This is the collision-PROOF identifier. The `member_id` label printed
    /// elsewhere is a 40-bit truncation of a 64-bit non-cryptographic hash, so
    /// anything making a trust decision about a member (e.g. a bot allow-list)
    /// MUST compare this key, never the short label. See the note on
    /// `MemberId` in `river-core` (`room_state::member`).
    pub fn verifying_key(&self, id: MemberId) -> Option<VerifyingKey> {
        if id == self.owner_id {
            Some(self.owner_vk)
        } else {
            self.members_by_id.get(&id).map(|m| m.member.member_vk)
        }
    }

    fn member_info(&self) -> &MemberInfoV1 {
        &self.state.member_info
    }

    /// Resolve a user-supplied short id against every id the room mentions.
    ///
    /// Matching mirrors `ban_member` / `member deputize`: a case-sensitive
    /// prefix match, or a case-insensitive match against the first 8 characters.
    /// Unlike those write paths, an input matching more than one member is
    /// reported as [`ResolveError::Ambiguous`] rather than silently resolving to
    /// whichever record happens to come first in the state vector. (The write
    /// paths keep their first-match behaviour; converging them would change
    /// existing commands, so it is deliberately left alone here.)
    pub fn resolve_short_id(&self, short: &str) -> Result<MemberId, ResolveError> {
        if short.is_empty() {
            return Err(ResolveError::NotFound);
        }
        let matches: Vec<MemberId> = self
            .known_ids
            .iter()
            .copied()
            .filter(|id| {
                let s = id.to_string();
                s.starts_with(short) || s[..8.min(s.len())].eq_ignore_ascii_case(short)
            })
            .collect();
        match matches.len() {
            0 => Err(ResolveError::NotFound),
            1 => Ok(matches[0]),
            _ => {
                // `known_ids` is ordered by the underlying hash, which has no
                // relation to the 8-char base32 the user sees; sort by the
                // printed form so the message reads in the order they'd expect.
                let mut ids: Vec<String> = matches.iter().map(|id| id.to_string()).collect();
                ids.sort();
                Err(ResolveError::Ambiguous(ids))
            }
        }
    }

    /// Whether `id` is currently in the room (a member, or the owner).
    fn in_room(&self, id: MemberId) -> bool {
        id == self.owner_id || self.current_members.contains(&id)
    }

    /// Every member holding a `member_info` record, deduplicated and ordered.
    ///
    /// This is the canonical row set for a member listing: the raw
    /// `member_info.member_info` vector can hold several records for one member
    /// (`MemberInfoV1::verify` accepts duplicates for migration-safety), so
    /// iterating it directly yields duplicate rows carrying losing nicknames.
    pub fn members_with_info(&self) -> impl Iterator<Item = MemberId> + '_ {
        self.deputies_by_deputizer.keys().copied()
    }

    /// Resolve one member for display.
    ///
    /// `nickname` is the FAITHFUL decoded value, not an escaped one: it is
    /// serialized into `-f json`, where a relay or bridge needs the real string
    /// and JSON escaping already makes it safe. Escaping happens at the terminal
    /// print sites ([`party_label`] / [`display_nickname`]).
    pub fn party(&self, id: MemberId) -> DeputyParty {
        let nickname = self.member_info().canonical(id).map(|info| {
            crate::api::unseal_nickname_display(&info.member_info.preferred_nickname, self.secrets)
        });
        DeputyParty {
            member_id: id.to_string(),
            nickname,
            is_owner: id == self.owner_id,
            in_room: self.in_room(id),
        }
    }

    /// How many members within a grant's scope the deputy can actually ban.
    ///
    /// The per-target decision is `MembersV1::is_ban_authorized`, the contract's
    /// OWN public predicate, never a local re-derivation of its rules. That
    /// matters: the rules do not compose the way a summary would suggest. The
    /// "you cannot ban the member who deputized you" guardrail is step 4,
    /// checked AFTER the absolute grants, so a target who deputized this deputy
    /// is still bannable when the deputy is that target's own invite ancestor,
    /// or is an owner-appointed global moderator. An earlier version of this
    /// function approximated the guardrail by hand and undercounted both cases.
    ///
    /// ONE local narrowing is applied on top: targets whose ban would remove the
    /// deputy themselves are not counted (see the filter below). Scope is the
    /// grant's: every member for an owner grant (whose subtree is the room),
    /// otherwise the deputizer's invite subtree.
    fn reach_of(&self, deputizer: MemberId, deputy: MemberId) -> usize {
        let empty = HashSet::new();
        let targets = if deputizer == self.owner_id {
            &self.current_members
        } else {
            self.subtrees.get(&deputizer).unwrap_or(&empty)
        };
        targets
            .iter()
            .filter(|target| {
                // Exclude every target whose ban would remove the DEPUTY: the
                // deputy themselves, and the deputy's own invite ancestors
                // (whose cascade sweeps the deputy up). `is_ban_authorized` has
                // no such check and both really are contract-permitted (step 3
                // grants an owner-appointed global moderator authority over
                // their own ancestors), but counting them makes the number
                // answer the wrong question: authority you can only spend by
                // removing yourself is not moderation authority.
                //
                // riverctl's own WRITE path refuses exactly this set --
                // `ban_member` goes through `self_removing_ban_reason` -- so
                // the read path must not advertise a capability the CLI will
                // not exercise. Keep the two in step: if either is relaxed,
                // revisit the other.
                !self.ban_would_remove(deputy, **target)
                    && MembersV1::is_ban_authorized(
                        deputy,
                        **target,
                        &self.members_by_id,
                        self.member_info(),
                        self.owner_id,
                    )
            })
            .count()
    }

    /// Whether a ban of `target` would remove `banner` from the room — the same
    /// question [`self_removing_ban_reason`] answers, off the subtrees this
    /// struct already built.
    fn ban_would_remove(&self, banner: MemberId, target: MemberId) -> bool {
        banner == target
            || self
                .subtrees
                .get(&target)
                .is_some_and(|downstream| downstream.contains(&banner))
    }

    /// Build the resolved view of a single `deputizer -> deputy` grant.
    pub fn grant(&self, deputizer: MemberId, deputy: MemberId) -> DeputyGrant {
        let active = self.in_room(deputizer) && self.in_room(deputy);
        DeputyGrant {
            scope: if deputizer == self.owner_id {
                DeputyScope::RoomWide
            } else {
                DeputyScope::InviteSubtree
            },
            // Zero when the grant is inert, so a script filtering
            // `members_deputy_can_ban > 0` for real moderation authority cannot get a
            // false positive from a grant the contract would refuse outright.
            members_deputy_can_ban: if active {
                self.reach_of(deputizer, deputy)
            } else {
                0
            },
            active,
            deputizer: self.party(deputizer),
            deputy: self.party(deputy),
        }
    }

    /// Forward lookup: the members `deputizer` has deputized, sorted by id.
    ///
    /// Reads the CANONICAL record, so a revoked grant lingering in a duplicate
    /// lower-rank record is not reported.
    pub fn deputies_of(&self, deputizer: MemberId) -> &[MemberId] {
        self.deputies_by_deputizer
            .get(&deputizer)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Reverse lookup: every member who has deputized `deputy`, sorted by id.
    pub fn deputizers_of(&self, deputy: MemberId) -> Vec<MemberId> {
        let mut ids: Vec<MemberId> = self
            .deputies_by_deputizer
            .iter()
            .filter(|(_, deputies)| deputies.contains(&deputy))
            .map(|(deputizer, _)| *deputizer)
            .collect();
        ids.sort_by_key(|id| id.to_string());
        ids
    }

    /// Every grant in the room, sorted by `(deputizer, deputy)` id.
    pub fn all_grants(&self) -> Vec<DeputyGrant> {
        let mut grants = Vec::new();
        for (deputizer, deputies) in &self.deputies_by_deputizer {
            for deputy in deputies {
                grants.push(self.grant(*deputizer, *deputy));
            }
        }
        grants.sort_by(|a, b| {
            (&a.deputizer.member_id, &a.deputy.member_id)
                .cmp(&(&b.deputizer.member_id, &b.deputy.member_id))
        });
        grants
    }

    /// `deputy -> [deputizer, …]` for every grant in the room, so a full member
    /// listing can be annotated with one pass instead of a reverse lookup per
    /// row.
    pub fn deputizers_by_deputy(&self) -> HashMap<MemberId, Vec<MemberId>> {
        let mut map: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
        for (deputizer, deputies) in &self.deputies_by_deputizer {
            for deputy in deputies {
                map.entry(*deputy).or_default().push(*deputizer);
            }
        }
        for deputizers in map.values_mut() {
            deputizers.sort_by_key(|id| id.to_string());
        }
        map
    }
}

/// `member -> the members they invited, directly or indirectly`, for every
/// member with a non-empty invite subtree.
///
/// Computes the same set `MembersV1::get_downstream_members` does (which is
/// private to the contract crate), so the reach reported here matches the
/// subtree `is_ban_authorized` grants authority over. It additionally carries a
/// visited-set guard that the contract's version omits: the contract can rely on
/// `verify` having rejected circular invite chains, whereas this reads a fetched
/// state directly and must not hang on a malformed one.
///
/// The owner gets an entry like anyone else (they are the `invited_by` of the
/// members they invited), but [`RoomDeputies::reach_of`] does not consult it:
/// an owner grant is scoped to the whole member list, not to a subtree.
fn invite_subtrees(state: &ChatRoomStateV1) -> HashMap<MemberId, HashSet<MemberId>> {
    let mut children: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
    for member in &state.members.members {
        children
            .entry(member.member.invited_by)
            .or_default()
            .push(member.member.id());
    }

    let mut subtrees = HashMap::new();
    for root in children.keys().copied() {
        let mut seen: HashSet<MemberId> = HashSet::new();
        let mut stack = vec![root];
        while let Some(current) = stack.pop() {
            for child in children.get(&current).into_iter().flatten() {
                // `*child != root` keeps a member out of their own subtree (a
                // strict-ancestor relation, matching `is_ban_authorized`), and
                // `seen.insert` gates the push so a cycle terminates.
                if *child != root && seen.insert(*child) {
                    stack.push(*child);
                }
            }
        }
        if !seen.is_empty() {
            subtrees.insert(root, seen);
        }
    }
    subtrees
}

/// Everyone a ban of `target` would remove from the room: `target` themselves
/// PLUS their entire transitive invite subtree — exactly what the contract's
/// `check_banned_members` inserts (the banned user) and then extends with
/// (`get_downstream_members`).
///
/// Builds every subtree to read one, which is deliberate: [`invite_subtrees`] is
/// the already-tested walk, carries the cycle guard the contract omits, and a
/// room is small enough that a one-off single-root variant would only be a
/// second thing to keep correct.
pub fn ban_removal_set(state: &ChatRoomStateV1, target: MemberId) -> HashSet<MemberId> {
    let mut removed = invite_subtrees(state).remove(&target).unwrap_or_default();
    removed.insert(target);
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
/// The CONTRACT permits both: `MembersV1::is_ban_authorized` has no self-check,
/// and step 3 (owner-appointed global moderator) fires before anything that
/// would stop it, so an owner-appointed deputy is authorized to ban themselves
/// AND their own ancestors. The resulting ban is fully valid and cascades to the
/// banner's whole invite subtree. So this is a client-side gate, not a contract
/// fix — and it must be applied to the OUTPUT of `is_ban_authorized`, never
/// wired into one branch of its priority ladder, or it would miss exactly the
/// global moderators who can reach the most members.
///
/// The UI enforces the identical rule in
/// `ui/src/components/members.rs::self_removing_ban_reason`; the two clients are
/// deliberately kept in step (the rule cannot live in `river-core` without
/// changing the room-contract WASM, and therefore the contract key).
///
/// Returns the user-visible reason when the ban is refused, `None` when the rule
/// does not apply.
pub fn self_removing_ban_reason(
    state: &ChatRoomStateV1,
    banner: MemberId,
    target: MemberId,
) -> Option<&'static str> {
    if !ban_removal_set(state, target).contains(&banner) {
        return None;
    }
    Some(if banner == target {
        "Cannot ban yourself. The ban would remove you, and everyone you \
         invited, from the room."
    } else {
        "Cannot ban a member you joined the room through. A ban also removes \
         everyone the banned member invited, so this one would remove you, and \
         everyone you invited, along with them."
    })
}

/// Render a nickname for TERMINAL output: quoted, with every non-printable
/// character escaped to a visible `\u{…}` form.
///
/// Nicknames are attacker-controlled and `unseal_nickname_display` is a lossy
/// UTF-8 decode with no filtering. Unescaped, a nickname could forge an entire
/// output row: a newline starts one, and a bidi override (U+202E) reverses the
/// rest of the line including the member id and the deputy annotation. That was
/// cosmetic before deputy status was printed; a forged row now claims moderation
/// authority. The quotes matter independently of escaping, because a nickname of
/// purely printable characters like `Bob (AAAAAAAA)  deputy of: Room Owner`
/// otherwise renders as a plausible second row, and `colored` drops the colour
/// that would distinguish it as soon as output is piped.
///
/// `str`'s `Debug` is the escape used deliberately: it covers control (Cc),
/// format (Cf, which is where the bidi overrides and zero-width joiners live)
/// and separator (Zl/Zp) characters via the standard library's own printability
/// table, rather than a hand-maintained range list that would silently miss a
/// category. It escapes `"` and `\` but leaves `'` alone, so ordinary names read
/// normally.
pub fn display_nickname(nickname: &str) -> String {
    format!("{:?}", nickname)
}

/// Like [`display_nickname`], but WITHOUT the surrounding quote pair — for
/// splicing an escaped name into the MIDDLE of other text (e.g. an
/// `@mention` substituted inline into a message or reply preview,
/// freenet/river#474), rather than presenting it as a standalone row/column.
///
/// The quoting `display_nickname` adds is there to defend a COLUMN or LABEL
/// site: a nickname of purely printable characters can otherwise forge a
/// plausible second output row (see its doc comment). That defense does not
/// transfer inline — inside a message body the attacker already controls
/// every surrounding byte and can type a fake row as plain prose with no
/// mention at all — so quoting there buys no security and costs legibility
/// on every ordinary, non-hostile mention (`hey @"Alice" can you review?`).
///
/// Reuses the EXACT SAME escape table as `display_nickname` (`str`'s
/// `Debug`), so this is not a second, hand-maintained escaping scheme: it is
/// `display_nickname`'s output with the outer quote character trimmed off
/// each end. `{:?}` on a `&str` always emits exactly one ASCII `"` at each
/// end (never doubled, never omitted), so trimming one byte from each side is
/// exact — including on an empty name, where the two quote bytes are all
/// there is.
pub fn escape_nickname_inline(nickname: &str) -> String {
    let quoted = display_nickname(nickname);
    quoted[1..quoted.len() - 1].to_string()
}

/// Render a party as `"Nickname" (SHORTID)`, or `(unknown) (SHORTID)` when they
/// have no `member_info` record in this room. The nickname is escaped and quoted
/// by [`display_nickname`]; `(unknown)` is unquoted precisely so it cannot be
/// confused with a member whose nickname is literally `unknown`.
pub fn party_label(party: &DeputyParty) -> String {
    match &party.nickname {
        Some(nickname) => format!("{} ({})", display_nickname(nickname), party.member_id),
        None => format!("(unknown) ({})", party.member_id),
    }
}

/// `"1 other member"` / `"N other members"`.
///
/// "other" is load-bearing in every count this module prints: the number
/// excludes the deputy themselves, so the bare form would assert a room or
/// subtree size one short of the real one. `debug room-state` prints the true
/// member count two lines above the grant list, where that would read as a
/// contradiction.
fn other_members(count: usize) -> String {
    if count == 1 {
        "1 other member".to_string()
    } else {
        format!("{count} other members")
    }
}

/// One-line explanation of what a grant currently confers, for human output.
///
/// Names the DEPUTIZER rather than leaning on the surrounding row, because the
/// ambiguity this command exists to remove is exactly "a deputy of whom?". The
/// deputy is left implicit: every call site already prints them, either as the
/// row label or via the `deputizer -> deputy` arrow in `debug room-state`.
///
/// Reports the grant's actual REACH, not just that it exists. A deputy of
/// someone who has invited nobody holds authority over an empty set, which the
/// old "may ban within X's invite subtree" phrasing overstated.
pub fn grant_status_line(grant: &DeputyGrant) -> String {
    if !grant.active {
        return match (grant.deputizer.in_room, grant.deputy.in_room) {
            (false, false) => format!(
                "inactive: neither {} nor {} is currently in this room",
                party_label(&grant.deputizer),
                party_label(&grant.deputy)
            ),
            (_, false) => format!(
                "inactive: {} is not currently in this room",
                party_label(&grant.deputy)
            ),
            _ => format!(
                "inactive: {} is not currently in this room",
                party_label(&grant.deputizer)
            ),
        };
    }
    match grant.scope {
        // `is_ban_authorized` refuses `target == owner` outright, so even an
        // owner-appointed deputy cannot ban the owner.
        DeputyScope::RoomWide if grant.members_deputy_can_ban == 0 => {
            "active but reaches no one: there is no other member in the room to ban \
             (granted by the room owner)"
                .to_string()
        }
        DeputyScope::RoomWide if grant.members_deputy_can_ban == 1 => {
            "active: may ban the only other member in the room (granted by the room owner)"
                .to_string()
        }
        // "other members", not "members": the count excludes the deputy, so
        // the bare form would assert a room size one short of the real one, and
        // `debug room-state` prints the true member count two lines above it.
        DeputyScope::RoomWide => format!(
            "active: may ban any of the {} in the room (granted by the room owner)",
            other_members(grant.members_deputy_can_ban)
        ),
        // "nobody else": when the deputy is themselves inside the deputizer's
        // subtree, the contract does authorize them to ban themselves, so the
        // unqualified form would be false.
        DeputyScope::InviteSubtree if grant.members_deputy_can_ban == 0 => format!(
            "active but reaches no one: there is currently nobody else in {}'s \
             invite subtree that this deputy may ban",
            party_label(&grant.deputizer)
        ),
        DeputyScope::InviteSubtree => format!(
            "active: may ban {} in {}'s invite subtree",
            other_members(grant.members_deputy_can_ban),
            party_label(&grant.deputizer)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::member::{AuthorizedMember, Member};
    use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};

    /// The cli crate's dalek build does not enable the `rand` `generate`
    /// helper, so keys come from fixed seeds (mirrors `api.rs`'s cli tests).
    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn id(sk: &SigningKey) -> MemberId {
        sk.verifying_key().into()
    }

    /// A room where `owner` owns it and every listed key is a current member
    /// invited directly by the owner.
    fn room(owner: &SigningKey, members: &[&SigningKey]) -> ChatRoomStateV1 {
        let mut state = ChatRoomStateV1::default();
        for sk in members {
            push_member(&mut state, owner, owner, sk);
        }
        state
    }

    /// Add `sk` as a member invited by `inviter` (signed by the inviter, as
    /// `AuthorizedMember::new` asserts).
    fn push_member(
        state: &mut ChatRoomStateV1,
        owner: &SigningKey,
        inviter: &SigningKey,
        sk: &SigningKey,
    ) {
        let member = Member {
            owner_member_id: id(owner),
            invited_by: id(inviter),
            member_vk: sk.verifying_key(),
        };
        state
            .members
            .members
            .push(AuthorizedMember::new(member, inviter));
    }

    /// Push a signed `member_info` record for `sk` at `version` with `deputies`.
    fn push_info(
        state: &mut ChatRoomStateV1,
        sk: &SigningKey,
        version: u32,
        nickname: &str,
        deputies: Vec<MemberId>,
    ) {
        let mut info = MemberInfo::new_public(id(sk), version, nickname.to_string());
        info.deputies = deputies;
        state
            .member_info
            .member_info
            .push(AuthorizedMemberInfo::new_with_member_key(info, sk));
    }

    fn no_secrets() -> HashMap<u32, [u8; 32]> {
        HashMap::new()
    }

    #[test]
    fn verifying_key_resolves_owner_and_members_and_is_none_for_unknown() {
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);
        let stranger = key(9); // never a member of this room
        let state = room(&owner, &[&alice, &bob]);
        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        // Owner: keyed separately since the owner is never in `members.members`.
        assert_eq!(d.verifying_key(id(&owner)), Some(owner.verifying_key()));
        // Current members resolve to their real signing identity.
        assert_eq!(d.verifying_key(id(&alice)), Some(alice.verifying_key()));
        assert_eq!(d.verifying_key(id(&bob)), Some(bob.verifying_key()));
        // An id with no member/owner record yields None, never a wrong key.
        assert_eq!(d.verifying_key(id(&stranger)), None);
    }

    #[test]
    fn forward_and_reverse_lookup_agree_on_a_grant() {
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);
        let mut state = room(&owner, &[&alice, &bob]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &alice, 0, "Alice", vec![id(&bob)]);
        push_info(&mut state, &bob, 0, "Bob", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        assert_eq!(d.deputies_of(id(&alice)), [id(&bob)]);
        assert_eq!(d.deputizers_of(id(&bob)), vec![id(&alice)]);

        // The reverse direction is NOT symmetric: Bob deputized nobody.
        assert!(d.deputies_of(id(&bob)).is_empty());
        assert!(d.deputizers_of(id(&alice)).is_empty());
    }

    #[test]
    fn reverse_lookup_finds_every_deputizer_of_one_member() {
        // The motivating question: "is this member a deputy of anyone (the
        // owner, the invite bot, ...)?" Two independent members deputize the
        // same target.
        let owner = key(1);
        let alice = key(2);
        let bot = key(3);
        let mut state = room(&owner, &[&alice, &bot]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![id(&bot)]);
        push_info(&mut state, &alice, 0, "Alice", vec![id(&bot)]);
        push_info(&mut state, &bot, 0, "Invite Bot", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        let mut expected = vec![id(&owner), id(&alice)];
        expected.sort_by_key(|i| i.to_string());
        assert_eq!(d.deputizers_of(id(&bot)), expected);

        // The owner's grant is room-wide; a regular member's is subtree-scoped.
        let owner_grant = d.grant(id(&owner), id(&bot));
        assert_eq!(owner_grant.scope, DeputyScope::RoomWide);
        assert!(owner_grant.active);
        assert!(owner_grant.deputizer.is_owner);
        assert!(
            owner_grant.deputizer.in_room,
            "the owner is never in members.members but is always in the room"
        );
        assert_eq!(
            owner_grant.members_deputy_can_ban, 1,
            "an owner grant reaches every member EXCEPT the deputy themselves \
             (self-ban is contract-permitted but is not moderation authority)"
        );

        let alice_grant = d.grant(id(&alice), id(&bot));
        assert_eq!(alice_grant.scope, DeputyScope::InviteSubtree);
        assert!(alice_grant.active);
        assert!(!alice_grant.deputizer.is_owner);
    }

    #[test]
    fn empty_deputies_reports_nothing_in_either_direction() {
        let owner = key(1);
        let alice = key(2);
        let mut state = room(&owner, &[&alice]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &alice, 0, "Alice", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        assert!(d.deputies_of(id(&alice)).is_empty());
        assert!(d.deputizers_of(id(&alice)).is_empty());
        assert!(d.all_grants().is_empty());
        assert!(d.deputizers_by_deputy().is_empty());
    }

    #[test]
    fn member_with_no_member_info_record_has_no_deputies() {
        // A member present in `members` but with no `member_info` entry: the
        // lookup must return empty rather than panicking or inventing a grant.
        let owner = key(1);
        let alice = key(2);
        let mut state = room(&owner, &[&alice]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        assert!(d.deputies_of(id(&alice)).is_empty());
        let party = d.party(id(&alice));
        assert_eq!(party.nickname, None, "no member_info means no nickname");
        assert!(party.in_room, "still a current member");
    }

    #[test]
    fn resolve_short_id_matches_case_insensitively_and_rejects_unknown() {
        let owner = key(1);
        let alice = key(2);
        let mut state = room(&owner, &[&alice]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &alice, 0, "Alice", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        let alice_id = id(&alice).to_string();
        assert_eq!(d.resolve_short_id(&alice_id), Ok(id(&alice)));
        assert_eq!(
            d.resolve_short_id(&alice_id.to_lowercase()),
            Ok(id(&alice)),
            "member ids are printed uppercase base32; accept a lowercased copy"
        );

        // A member id that is in no room list at all.
        assert_eq!(
            d.resolve_short_id("ZZZZZZZZ"),
            Err(ResolveError::NotFound),
            "an id belonging to no member must not resolve"
        );
        assert_eq!(d.resolve_short_id(""), Err(ResolveError::NotFound));
    }

    #[test]
    fn resolve_short_id_reports_ambiguity_rather_than_guessing() {
        // A prefix shared by two ids must refuse rather than pick whichever
        // comes first in the state vector.
        //
        // The colliding pair is SEARCHED for over a wide seed range rather than
        // assumed: with 10 random ids over a 32-symbol alphabet there is a ~21%
        // chance no two share a first character, so asserting a pigeonhole that
        // does not hold would make this test a latent flake.
        let owner = key(1);
        let candidates: Vec<SigningKey> = (2u8..=200).map(key).collect();
        let mut first_seen: HashMap<char, usize> = HashMap::new();
        let mut pair: Option<(usize, usize)> = None;
        for (i, sk) in candidates.iter().enumerate() {
            let c = id(sk).to_string().chars().next().unwrap();
            match first_seen.get(&c) {
                Some(j) => {
                    pair = Some((*j, i));
                    break;
                }
                None => {
                    first_seen.insert(c, i);
                }
            }
        }
        // 199 ids over a 32-symbol alphabet: a collision is guaranteed.
        let (a, b) = pair.expect("199 ids over 32 symbols must collide on the first character");
        let (first, second) = (&candidates[a], &candidates[b]);
        let shared = id(first).to_string().chars().next().unwrap();

        let mut state = room(&owner, &[first, second]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, first, 0, "First", vec![]);
        push_info(&mut state, second, 0, "Second", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        match d.resolve_short_id(&shared.to_string()) {
            Err(ResolveError::Ambiguous(ids)) => {
                assert!(ids.contains(&id(first).to_string()));
                assert!(ids.contains(&id(second).to_string()));
                let mut sorted = ids.clone();
                sorted.sort();
                assert_eq!(
                    ids, sorted,
                    "the matches must be listed in printed-id order, not in \
                     the underlying hash order the id set is stored in"
                );
            }
            other => panic!("expected an ambiguity error, got {other:?}"),
        }

        // The full 8-character id still resolves unambiguously.
        assert_eq!(
            d.resolve_short_id(&id(first).to_string()),
            Ok(id(first)),
            "a full id must not be reported as ambiguous"
        );
    }

    #[test]
    fn reads_the_canonical_record_so_a_revoked_grant_is_not_reported() {
        // `MemberInfoV1::verify` accepts duplicate records for one member, so a
        // client can hold BOTH a grant (v1) and its revoke (v2). Reporting the
        // losing record would tell the user a revoked deputy is still active.
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);
        let mut state = room(&owner, &[&alice, &bob]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &bob, 0, "Bob", vec![]);
        // The LOSING record (the v1 grant) is pushed FIRST, deliberately: a bare
        // first-match scan of `member_info.member_info` would read it and report
        // a revoked deputy as live. With the revoke pushed first the test would
        // pass even against that bug, which is exactly what it exists to catch.
        push_info(&mut state, &alice, 1, "Alice", vec![id(&bob)]);
        push_info(&mut state, &alice, 2, "Alice", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        assert!(
            d.deputies_of(id(&alice)).is_empty(),
            "the v2 revoke is canonical; the v1 grant must not be reported"
        );
        assert!(
            d.deputizers_of(id(&bob)).is_empty(),
            "the reverse lookup must honour the same canonical record"
        );
        assert!(d.all_grants().is_empty());
    }

    #[test]
    fn duplicate_records_collapse_to_one_row_with_the_canonical_nickname() {
        // The listing surface must be per-MEMBER, not per-record: two records
        // for Alice must yield one row, carrying the winning record's nickname.
        let owner = key(1);
        let alice = key(2);
        let mut state = room(&owner, &[&alice]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &alice, 1, "Old Alice", vec![]);
        push_info(&mut state, &alice, 2, "New Alice", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        let rows: Vec<MemberId> = d.members_with_info().collect();
        assert_eq!(rows.len(), 2, "owner + alice, not owner + alice + alice");
        assert_eq!(
            d.party(id(&alice)).nickname.as_deref(),
            Some("New Alice"),
            "the canonical (highest-version) record wins"
        );
    }

    #[test]
    fn a_grant_naming_a_pruned_member_is_reported_but_marked_inactive() {
        // A deputy pruned for inactivity keeps no `member_info` record but is
        // still named by their deputizer's signed record. `is_ban_authorized`
        // honours deputy authority only for CURRENT members, so the grant is
        // inert: surface it, but do not claim it confers anything.
        let owner = key(1);
        let alice = key(2);
        let pruned = key(3);
        let mut state = room(&owner, &[&alice]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &alice, 0, "Alice", vec![id(&pruned)]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        assert_eq!(d.deputies_of(id(&alice)), [id(&pruned)]);
        let grant = d.grant(id(&alice), id(&pruned));
        assert!(!grant.active, "the deputy is no longer a member");
        assert_eq!(grant.deputy.nickname, None);
        assert!(!grant.deputy.in_room);
        assert!(grant_status_line(&grant).starts_with("inactive:"));

        // The pruned id is still resolvable, so `deputized-by` can answer for
        // it (mirroring the revoke path's own-deputies fallback).
        assert_eq!(
            d.resolve_short_id(&id(&pruned).to_string()),
            Ok(id(&pruned))
        );
        assert_eq!(d.deputizers_of(id(&pruned)), vec![id(&alice)]);
    }

    #[test]
    fn a_grant_from_an_absent_deputizer_is_inactive() {
        // Defence-in-depth only: `MemberInfoV1::verify` rejects a `member_info`
        // record whose member is neither the owner nor a current member, so a
        // deputizer absent from `members` cannot occur in a state the contract
        // accepted. Pinned anyway so a hand-built or partially-applied state
        // cannot make the CLI claim authority for someone who has left.
        let owner = key(1);
        let gone = key(2);
        let bob = key(3);
        let mut state = room(&owner, &[&bob]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &gone, 0, "Gone", vec![id(&bob)]);
        push_info(&mut state, &bob, 0, "Bob", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        let grant = d.grant(id(&gone), id(&bob));
        assert!(!grant.active);
        assert!(!grant.deputizer.in_room);
        assert!(grant.deputy.in_room);

        // The status line must name the DEPUTIZER as the absent party here,
        // not the deputy (who is present).
        let line = grant_status_line(&grant);
        assert!(line.starts_with("inactive:"), "{line}");
        assert!(line.contains(&id(&gone).to_string()), "{line}");
        assert!(!line.contains(&id(&bob).to_string()), "{line}");
    }

    #[test]
    fn all_grants_and_index_cover_every_grant_deterministically() {
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);
        let carol = key(4);
        let mut state = room(&owner, &[&alice, &bob, &carol]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![id(&carol)]);
        push_info(&mut state, &alice, 0, "Alice", vec![id(&bob), id(&carol)]);
        push_info(&mut state, &bob, 0, "Bob", vec![]);
        push_info(&mut state, &carol, 0, "Carol", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        let grants = d.all_grants();
        assert_eq!(grants.len(), 3);
        let mut sorted = grants.clone();
        sorted.sort_by(|a, b| {
            (&a.deputizer.member_id, &a.deputy.member_id)
                .cmp(&(&b.deputizer.member_id, &b.deputy.member_id))
        });
        assert_eq!(
            grants, sorted,
            "all_grants must be deterministically ordered"
        );

        let index = d.deputizers_by_deputy();
        assert_eq!(index.get(&id(&bob)).unwrap(), &vec![id(&alice)]);
        let mut carol_deputizers = vec![id(&owner), id(&alice)];
        carol_deputizers.sort_by_key(|i| i.to_string());
        assert_eq!(index.get(&id(&carol)).unwrap(), &carol_deputizers);
        assert!(!index.contains_key(&id(&alice)));
    }

    #[test]
    fn reach_is_the_deputizers_invite_subtree_not_merely_that_a_grant_exists() {
        // The whole point of `members_deputy_can_ban`: a deputy of someone who invited
        // nobody has authority over an empty set, and saying "may ban within
        // their invite subtree" would overstate it. Tree:
        //   owner -> alice -> carol -> dave
        //   owner -> bob (invited nobody)
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);
        let carol = key(4);
        let dave = key(5);
        let deputy = key(6);

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_member(&mut state, &owner, &owner, &bob);
        push_member(&mut state, &owner, &alice, &carol);
        push_member(&mut state, &owner, &carol, &dave);
        push_member(&mut state, &owner, &owner, &deputy);

        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &alice, 0, "Alice", vec![id(&deputy)]);
        push_info(&mut state, &bob, 0, "Bob", vec![id(&deputy)]);
        push_info(&mut state, &deputy, 0, "Deputy", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        // Alice's subtree is carol + dave (transitive), so 2.
        let from_alice = d.grant(id(&alice), id(&deputy));
        assert!(from_alice.active);
        assert_eq!(from_alice.members_deputy_can_ban, 2);
        let line = grant_status_line(&from_alice);
        assert!(line.contains("2 other members"), "{line}");
        assert!(line.contains("Alice"), "must name the deputizer: {line}");

        // Bob invited nobody, so his grant is live but reaches no one. This
        // must NOT read as though the deputy can ban within a real subtree.
        let from_bob = d.grant(id(&bob), id(&deputy));
        assert!(from_bob.active);
        assert_eq!(from_bob.members_deputy_can_ban, 0);
        let bob_line = grant_status_line(&from_bob);
        assert!(
            bob_line.contains("reaches no one"),
            "an empty subtree must be stated plainly: {bob_line}"
        );
        assert!(
            bob_line.contains("nobody else"),
            "the deputy CAN ban themselves when they are inside the subtree, so \
             the unqualified 'nobody' would be false: {bob_line}"
        );
        assert!(bob_line.contains("Bob"), "{bob_line}");
    }

    /// Sizes of every invite subtree, for the tests that only care about counts.
    fn subtree_sizes(state: &ChatRoomStateV1) -> HashMap<MemberId, usize> {
        invite_subtrees(state)
            .into_iter()
            .map(|(k, v)| (k, v.len()))
            .collect()
    }

    #[test]
    fn invite_subtrees_survives_a_cycle_and_a_self_invite() {
        // The contract's own `get_downstream_members` has NO visited guard (it
        // relies on `verify` rejecting circular invite chains). This mirror
        // reads a fetched state directly, so it must not hang on a malformed
        // one.
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_member(&mut state, &owner, &alice, &bob);
        // Hand-forged cycle: alice claims to have been invited by bob (her own
        // descendant). `AuthorizedMember::new` would assert, so build it raw.
        state.members.members[0].member.invited_by = id(&bob);

        let sizes = subtree_sizes(&state);
        assert_eq!(sizes.get(&id(&alice)).copied().unwrap_or(0), 1);
        assert_eq!(sizes.get(&id(&bob)).copied().unwrap_or(0), 1);

        // A cycle the root is NOT part of is what the visited set is for, and
        // it IS reachable: `invited_by` is only a function while each member id
        // appears once in `members.members`, and nothing in `MembersV1::verify`
        // enforces that. A second entry for `alice` naming `bob` as her inviter
        // closes an alice/bob cycle BELOW the owner, so the walk from `owner`
        // enters it. `*child != root` does not help there (neither cycle member
        // is the root); only `seen.insert` gating the push terminates it.
        //
        // Run on a worker thread with a deadline so a regression FAILS rather
        // than hanging the whole test binary.
        let mut disjoint = ChatRoomStateV1::default();
        push_member(&mut disjoint, &owner, &owner, &alice);
        push_member(&mut disjoint, &owner, &alice, &bob);
        push_member(&mut disjoint, &owner, &bob, &alice);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(subtree_sizes(&disjoint));
        });
        let sizes = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("invite_subtrees must terminate on a cycle below the root");
        assert_eq!(
            sizes.get(&id(&owner)).copied(),
            Some(2),
            "the owner reaches alice and bob, each counted once"
        );

        // A member who invited themselves must not be counted as their own
        // descendant, and must not loop.
        let mut selfie = ChatRoomStateV1::default();
        push_member(&mut selfie, &owner, &owner, &alice);
        selfie.members.members[0].member.invited_by = id(&alice);
        assert_eq!(
            subtree_sizes(&selfie).get(&id(&alice)).copied(),
            None,
            "self-invite is not a subtree"
        );

        // A deep chain: each ancestor counts every descendant below them.
        let carol = key(4);
        let dave = key(5);
        let mut chain = ChatRoomStateV1::default();
        push_member(&mut chain, &owner, &owner, &alice);
        push_member(&mut chain, &owner, &alice, &bob);
        push_member(&mut chain, &owner, &bob, &carol);
        push_member(&mut chain, &owner, &carol, &dave);
        let sizes = subtree_sizes(&chain);
        assert_eq!(sizes.get(&id(&alice)).copied(), Some(3));
        assert_eq!(sizes.get(&id(&bob)).copied(), Some(2));
        assert_eq!(sizes.get(&id(&carol)).copied(), Some(1));
        assert_eq!(sizes.get(&id(&dave)).copied(), None);
        assert_eq!(
            sizes.get(&id(&owner)).copied(),
            Some(4),
            "the owner's own entry counts the whole chain"
        );
    }

    #[test]
    fn human_status_line_names_the_deputizer_not_a_bare_badge() {
        // The UI's viewer-scoped shield badge is the confusion this command
        // exists to avoid: every line must say WHOSE authority is in play.
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);
        let carol = key(4);
        let mut state = room(&owner, &[&alice, &bob]);
        push_member(&mut state, &owner, &alice, &carol);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &alice, 0, "Alice", vec![id(&bob)]);
        push_info(&mut state, &bob, 0, "Bob", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        let line = grant_status_line(&d.grant(id(&alice), id(&bob)));
        assert!(line.contains("Alice"), "must name the deputizer: {line}");
        assert!(line.contains("invite subtree"), "must scope it: {line}");

        let owner_line = grant_status_line(&d.grant(id(&owner), id(&bob)));
        assert!(
            owner_line.contains("room owner"),
            "an owner grant must be attributed to the owner: {owner_line}"
        );

        assert_eq!(
            party_label(&d.party(id(&alice))),
            format!("\"Alice\" ({})", id(&alice))
        );
        assert_eq!(
            party_label(&d.party(id(&key(9)))),
            format!("(unknown) ({})", id(&key(9))),
            "a member with no member_info record renders as (unknown)"
        );
    }

    #[test]
    fn a_hostile_nickname_cannot_forge_an_extra_output_row() {
        // Nicknames are attacker-controlled and printed next to an authority
        // claim. Three separate forgery vectors must all be neutralised.
        let owner = key(1);
        let evil = key(2);

        // 1. A newline starts a second line that reads as a real row.
        let newline = "Eve\n  Admin (AAAAAAAA)  deputy of: Room Owner";
        // 2. An ANSI escape can repaint or erase what follows.
        let ansi = "Eve\u{1b}[2K\u{1b}[31mAdmin";
        // 3. A bidi override reverses the rendering of the REST of the line,
        //    including the member id and the deputy annotation (Trojan Source).
        //    U+202E is category Cf, which `char::is_control()` does NOT cover.
        let bidi = "Eve\u{202e} rewonR mooR :fo ytuped";
        // 4. Zero-width characters can hide text inside an apparent name.
        let zero_width = "Ev\u{200b}e\u{feff}";
        // 5. A carriage return rewrites the line in place; a tab fakes columns.
        let carriage = "Eve\r  Admin (AAAAAAAA)";
        let tabbed = "Eve\tAdmin";
        // 6. The Zl/Zp separators are neither Cc nor Cf, and a terminal or log
        //    viewer may still break a line on them.
        let separators = "Eve\u{2028}Admin\u{2029}Mod";

        for hostile in [
            newline, ansi, bidi, zero_width, carriage, tabbed, separators,
        ] {
            let mut state = room(&owner, &[&evil]);
            push_info(&mut state, &owner, 0, "Room Owner", vec![]);
            push_info(&mut state, &evil, 0, hostile, vec![]);

            let secrets = no_secrets();
            let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);
            let label = party_label(&d.party(id(&evil)));

            for forbidden in [
                '\n', '\r', '\t', '\u{1b}', '\u{202e}', '\u{200b}', '\u{feff}', '\u{2028}',
                '\u{2029}',
            ] {
                assert!(
                    !label.contains(forbidden),
                    "{forbidden:?} must not survive into terminal output: {label:?}"
                );
            }
            assert!(
                label.starts_with('"'),
                "the nickname must be quoted so it cannot be mistaken for \
                 surrounding structure: {label:?}"
            );
            assert!(
                label.ends_with(&format!("({})", id(&evil))),
                "the real member id must still terminate the label: {label:?}"
            );

            // The JSON side keeps the faithful value: JSON escaping already
            // makes it safe, and a bridge relaying nicknames needs the real
            // string rather than a mangled one.
            assert_eq!(
                d.party(id(&evil)).nickname.as_deref(),
                Some(hostile),
                "DeputyParty.nickname must not be lossily rewritten"
            );
        }

        // A nickname of purely PRINTABLE characters can forge a row too, which
        // escaping alone does not stop; the quotes are what close that hole.
        let plausible = "Bob (AAAAAAAA)  deputy of: Room Owner";
        assert_eq!(
            display_nickname(plausible),
            format!("\"{plausible}\""),
            "a printable forgery attempt must be visibly delimited"
        );

        // Ordinary names are untouched apart from the quotes.
        assert_eq!(display_nickname("Ian Clarke"), "\"Ian Clarke\"");
        assert_eq!(display_nickname("O'Brien"), "\"O'Brien\"");
        assert_eq!(display_nickname("emoji \u{1f600}"), "\"emoji \u{1f600}\"");
    }

    /// `escape_nickname_inline` (freenet/river#474) must escape exactly the
    /// same control/format/separator bytes as `display_nickname` — it is
    /// `display_nickname`'s output with the outer quote pair trimmed, not a
    /// second hand-maintained escaping scheme — but must NOT wrap the result
    /// in quotes, since it is meant to sit inline inside other text (an
    /// `@mention` substituted into a message or reply preview) rather than
    /// stand as a labelled column.
    #[test]
    fn escape_nickname_inline_matches_display_nickname_minus_quotes() {
        let hostile = "\u{1b}[2J\rEve\u{7}";
        let quoted = display_nickname(hostile);
        let inline = escape_nickname_inline(hostile);

        assert!(
            quoted.starts_with('"') && quoted.ends_with('"'),
            "sanity: display_nickname always wraps in exactly one quote pair"
        );
        assert_eq!(
            inline,
            quoted[1..quoted.len() - 1],
            "escape_nickname_inline must be display_nickname's escaping with \
             only the surrounding quotes removed"
        );
        assert!(
            !inline.starts_with('"') && !inline.ends_with('"'),
            "escape_nickname_inline must not add its own quoting: {inline:?}"
        );
        for forbidden in ['\u{1b}', '\r', '\u{7}'] {
            assert!(
                !inline.contains(forbidden),
                "{forbidden:?} must not survive into inline terminal output: {inline:?}"
            );
        }

        // Ordinary names round-trip with no visible change at all — this is
        // the property that makes it safe for EVERY ordinary @mention, not
        // just hostile ones.
        assert_eq!(escape_nickname_inline("Alice"), "Alice");

        // The empty name is a genuine edge case (an empty nickname is a
        // degenerate but not impossible resolved value): `display_nickname`
        // produces exactly two quote characters and nothing else, so the
        // inline form must be empty, not panic on an out-of-bounds slice.
        assert_eq!(escape_nickname_inline(""), "");
    }

    #[test]
    fn reach_excludes_subtree_members_who_deputized_this_deputy() {
        // `is_ban_authorized` step 4 denies a ban whose TARGET lists the banner
        // in their own deputies ("you cannot ban the member who deputized
        // you"). Those subtree members are not reachable, so counting them
        // would overstate the authority the grant confers.
        let owner = key(1);
        let alice = key(2);
        let target = key(3);
        let other = key(4);
        let deputy = key(5);

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_member(&mut state, &owner, &alice, &target);
        push_member(&mut state, &owner, &alice, &other);
        push_member(&mut state, &owner, &owner, &deputy);

        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &alice, 0, "Alice", vec![id(&deputy)]);
        // `target` has ALSO deputized `deputy`, so `deputy` cannot ban them.
        push_info(&mut state, &target, 0, "Target", vec![id(&deputy)]);
        push_info(&mut state, &other, 0, "Other", vec![]);
        push_info(&mut state, &deputy, 0, "Deputy", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        let grant = d.grant(id(&alice), id(&deputy));
        assert_eq!(
            grant.members_deputy_can_ban, 1,
            "Alice's subtree is {{target, other}}, but target deputized this \
             deputy so only `other` is reachable"
        );
    }

    #[test]
    fn the_guardrail_does_not_apply_where_an_absolute_grant_already_does() {
        // The guardrail is `is_ban_authorized` step 4, checked AFTER the
        // absolute grants, so it must NOT be applied as a blanket exclusion.
        // Two cases where a target who deputized the deputy is STILL bannable:
        //
        //   (a) the deputy is that target's own invite ancestor (step 2)
        //   (b) the owner has appointed the deputy globally (step 3)
        //
        // Approximating step 4 by hand undercounted both. The reach now routes
        // through the contract's own predicate, so it cannot drift from it.
        let owner = key(1);
        let alice = key(2);
        let deputy = key(3);
        let target = key(4);

        // (a) owner -> alice -> deputy -> target, and alice deputizes deputy.
        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_member(&mut state, &owner, &alice, &deputy);
        push_member(&mut state, &owner, &deputy, &target);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &alice, 0, "Alice", vec![id(&deputy)]);
        push_info(&mut state, &deputy, 0, "Deputy", vec![]);
        // The target deputized the deputy, which would trip a naive guardrail.
        push_info(&mut state, &target, 0, "Target", vec![id(&deputy)]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);
        assert_eq!(
            d.grant(id(&alice), id(&deputy)).members_deputy_can_ban,
            1,
            "Alice's subtree is {{deputy, target}}; the deputy does not count \
             themselves, and the target IS counted because the deputy is their \
             own invite ancestor, so step 2 authorizes the ban despite the \
             target having deputized them"
        );

        // (b) the same shape, but authority comes from the owner appointing the
        //     deputy as a global moderator.
        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_member(&mut state, &owner, &alice, &target);
        push_member(&mut state, &owner, &owner, &deputy);
        push_info(&mut state, &owner, 0, "Room Owner", vec![id(&deputy)]);
        push_info(&mut state, &alice, 0, "Alice", vec![id(&deputy)]);
        push_info(&mut state, &deputy, 0, "Deputy", vec![]);
        push_info(&mut state, &target, 0, "Target", vec![id(&deputy)]);

        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);
        assert_eq!(
            d.grant(id(&alice), id(&deputy)).members_deputy_can_ban,
            1,
            "Alice's subtree is {{target}}; the owner's global appointment is \
             absolute (step 3), so the target's own grant cannot block it"
        );

        // The owner's own grant reaches every member.
        let owner_grant = d.grant(id(&owner), id(&deputy));
        assert_eq!(owner_grant.scope, DeputyScope::RoomWide);
        assert_eq!(
            owner_grant.members_deputy_can_ban, 2,
            "alice + target; the owner is never a valid ban target and the \
             deputy does not count themselves"
        );
    }

    #[test]
    fn an_inactive_grant_reaches_nobody() {
        // A script filtering `members_deputy_can_ban > 0` for real moderation
        // authority must not get a false positive from a grant the contract
        // would refuse outright.
        //
        // The fixture is deliberately the case where the `if active` gate is
        // LOAD-BEARING: the deputizer has left, but still names a child via
        // that child's `invited_by`, so they still have a subtree, and the
        // deputy is a current member the contract would otherwise authorize
        // (step 5, via the departed inviter as a strict ancestor). Without the
        // gate this reports 1. A fixture where the DEPUTY is the absent party
        // would report 0 either way and pin nothing.
        let owner = key(1);
        let gone = key(2);
        let child = key(3);
        let deputy = key(4);

        let mut state = ChatRoomStateV1::default();
        // `gone` is not in `members`, but is `child`'s inviter.
        push_member(&mut state, &owner, &gone, &child);
        push_member(&mut state, &owner, &owner, &deputy);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &gone, 0, "Gone", vec![id(&deputy)]);
        push_info(&mut state, &child, 0, "Child", vec![]);
        push_info(&mut state, &deputy, 0, "Deputy", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        // The departed deputizer does have a subtree, and the raw predicate
        // does authorize the ban, so only the gate keeps this at zero.
        assert_eq!(d.deputies_of(id(&gone)), [id(&deputy)]);
        let grant = d.grant(id(&gone), id(&deputy));
        assert!(!grant.active, "the deputizer is not in the room");
        assert_eq!(
            grant.members_deputy_can_ban, 0,
            "an inactive grant must report no reach even though the departed \
             deputizer still has a subtree the contract would authorize"
        );
    }

    #[test]
    fn inactive_line_names_both_parties_when_both_are_absent() {
        let owner = key(1);
        let gone_deputizer = key(2);
        let gone_deputy = key(3);
        let mut state = ChatRoomStateV1::default();
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(
            &mut state,
            &gone_deputizer,
            0,
            "Gone",
            vec![id(&gone_deputy)],
        );

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        let line = grant_status_line(&d.grant(id(&gone_deputizer), id(&gone_deputy)));
        assert!(line.starts_with("inactive: neither"), "{line}");
        assert!(line.contains(&id(&gone_deputizer).to_string()), "{line}");
        assert!(line.contains(&id(&gone_deputy).to_string()), "{line}");
    }

    #[test]
    fn status_line_uses_singular_for_a_reach_of_one() {
        let owner = key(1);
        let alice = key(2);
        let child = key(3);
        let deputy = key(4);
        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_member(&mut state, &owner, &alice, &child);
        push_member(&mut state, &owner, &owner, &deputy);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &alice, 0, "Alice", vec![id(&deputy)]);
        push_info(&mut state, &child, 0, "Child", vec![]);
        push_info(&mut state, &deputy, 0, "Deputy", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        let line = grant_status_line(&d.grant(id(&alice), id(&deputy)));
        assert!(line.contains("1 other member in"), "{line}");
        assert!(!line.contains("1 other members"), "{line}");
        assert!(!line.contains("member(s)"), "{line}");
    }

    #[test]
    fn dedup_survives_two_distinct_ids_that_print_identically() {
        // A `MemberId` prints as 8 base32 characters, which is only the first 5
        // of its 8 underlying bytes, so distinct ids CAN print the same. Sorting
        // by the printed form alone would not necessarily put equal ids
        // adjacent, and `dedup` would then leave a duplicate behind. Constructed
        // directly here because grinding a real 40-bit collision is not viable
        // in a unit test.
        use freenet_scaffold::util::FastHash;

        // Little-endian: the low 5 bytes drive the printed form, so these two
        // differ only in bytes the printed id never sees.
        let twin_a = MemberId(FastHash(0x0000_0000_0000_0001));
        let twin_b = MemberId(FastHash(0x0100_0000_0000_0001));
        assert_ne!(twin_a, twin_b, "the two ids must be genuinely distinct");
        assert_eq!(
            twin_a.to_string(),
            twin_b.to_string(),
            "...but must print identically, or this test proves nothing"
        );

        // Drive the PRODUCTION path, not an inlined copy of the sort: a signed
        // deputies list carrying [a, b, a]. Sorting by the printed form alone
        // is stable, so it leaves the equal-keyed triple in that order and
        // `dedup` (which compares adjacent MemberIds) removes nothing.
        let owner = key(1);
        let alice = key(2);
        let mut state = room(&owner, &[&alice]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &alice, 0, "Alice", vec![twin_a, twin_b, twin_a]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);
        let deputies = d.deputies_of(id(&alice));
        assert_eq!(
            deputies.len(),
            2,
            "the genuine duplicate must collapse while the twin survives; \
             sorting by the printed id alone leaves three"
        );
        assert!(deputies.contains(&twin_a) && deputies.contains(&twin_b));
    }

    #[test]
    fn room_wide_status_covers_zero_one_and_many_other_members() {
        // Three arms, all reachable now that a deputy does not count themselves:
        // a room where the deputy is the ONLY member reaches nobody; one other
        // member needs a singular form ("may ban any of the 1 member in the
        // room" is not English); two or more takes the plural.
        let owner = key(1);
        let deputy = key(2);
        let other = key(3);
        let third = key(4);
        let secrets = no_secrets();

        // Zero: the deputy is the only member, and cannot count themselves.
        let mut alone = room(&owner, &[&deputy]);
        push_info(&mut alone, &owner, 0, "Room Owner", vec![id(&deputy)]);
        push_info(&mut alone, &deputy, 0, "Deputy", vec![]);
        let d = RoomDeputies::new(&alone, &owner.verifying_key(), &secrets);
        let grant = d.grant(id(&owner), id(&deputy));
        assert_eq!(grant.members_deputy_can_ban, 0);
        let line = grant_status_line(&grant);
        assert!(line.contains("no other member in the room"), "{line}");

        // One.
        let mut pair = room(&owner, &[&deputy, &other]);
        push_info(&mut pair, &owner, 0, "Room Owner", vec![id(&deputy)]);
        push_info(&mut pair, &deputy, 0, "Deputy", vec![]);
        push_info(&mut pair, &other, 0, "Other", vec![]);
        let d = RoomDeputies::new(&pair, &owner.verifying_key(), &secrets);
        let grant = d.grant(id(&owner), id(&deputy));
        assert_eq!(grant.members_deputy_can_ban, 1);
        let line = grant_status_line(&grant);
        assert!(line.contains("the only other member"), "{line}");
        assert!(!line.contains("1 member in the room"), "{line}");

        // Many.
        let mut crowd = room(&owner, &[&deputy, &other, &third]);
        push_info(&mut crowd, &owner, 0, "Room Owner", vec![id(&deputy)]);
        push_info(&mut crowd, &deputy, 0, "Deputy", vec![]);
        push_info(&mut crowd, &other, 0, "Other", vec![]);
        push_info(&mut crowd, &third, 0, "Third", vec![]);
        let d = RoomDeputies::new(&crowd, &owner.verifying_key(), &secrets);
        let grant = d.grant(id(&owner), id(&deputy));
        assert_eq!(grant.members_deputy_can_ban, 2);
        let line = grant_status_line(&grant);
        assert!(
            line.contains("2 other members in the room"),
            "the count excludes the deputy, so the line must not assert a room \
             size one short of the real one: {line}"
        );
        assert!(line.contains("room owner"), "{line}");
    }

    #[test]
    fn a_private_room_nickname_decrypts_through_party() {
        // `party` is the only place the deputy surfaces resolve a nickname, and
        // in a private room that nickname is sealed. Pin the wiring: with the
        // secret it decrypts, without it falls back to the placeholder rather
        // than leaking ciphertext.
        use river_core::ecies::seal_bytes;

        let owner = key(1);
        let alice = key(2);
        let secret = [4u8; 32];

        let mut state = room(&owner, &[&alice]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        let sealed = seal_bytes(b"Alice", &secret, 0);
        let info = MemberInfo {
            member_id: id(&alice),
            version: 0,
            preferred_nickname: sealed,
            deputies: Vec::new(),
        };
        state
            .member_info
            .member_info
            .push(AuthorizedMemberInfo::new_with_member_key(info, &alice));

        let secrets = HashMap::from([(0u32, secret)]);
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);
        assert_eq!(d.party(id(&alice)).nickname.as_deref(), Some("Alice"));
        assert_eq!(
            party_label(&d.party(id(&alice))),
            format!("\"Alice\" ({})", id(&alice))
        );

        // No secret: the placeholder, never raw ciphertext.
        let none = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &none);
        let nickname = d.party(id(&alice)).nickname.expect("record exists");
        assert!(nickname.contains("Encrypted"), "{nickname}");
        assert!(!nickname.contains("Alice"), "{nickname}");
    }

    #[test]
    fn json_shape_is_pinned_so_scripts_do_not_break_silently() {
        // These field names and the kebab-case scope values are the CLI's
        // published contract for `-f json`; renaming one silently breaks every
        // consumer, so pin them here rather than in a reviewer's memory.
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);
        let mut state = room(&owner, &[&alice, &bob]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![id(&bob)]);
        push_info(&mut state, &alice, 0, "Alice", vec![]);
        push_info(&mut state, &bob, 0, "Bob", vec![]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        let grant = d.grant(id(&owner), id(&bob));
        let json = serde_json::to_value(&grant).unwrap();

        assert_eq!(json["scope"], "room-wide");
        assert_eq!(json["active"], true);
        assert_eq!(
            json["members_deputy_can_ban"], 1,
            "alice; bob is the deputy and does not count themselves"
        );
        for side in ["deputizer", "deputy"] {
            let party = &json[side];
            assert!(party["member_id"].is_string(), "{side}.member_id");
            assert!(party["nickname"].is_string(), "{side}.nickname");
            assert!(party["is_owner"].is_boolean(), "{side}.is_owner");
            assert!(party["in_room"].is_boolean(), "{side}.in_room");
        }
        assert_eq!(json["deputizer"]["is_owner"], true);
        assert_eq!(
            json["deputizer"]["in_room"], true,
            "the owner is in the room even though members.members never lists them"
        );

        // The other scope value, and the null nickname for an id with no record.
        let subtree = d.grant(id(&alice), id(&key(9)));
        let json = serde_json::to_value(&subtree).unwrap();
        assert_eq!(json["scope"], "invite-subtree");
        assert_eq!(json["active"], false);
        assert!(json["deputy"]["nickname"].is_null());
    }

    // ------------------------------------------------------------------
    // freenet/river#478: nobody may ban themselves OUT OF THE ROOM.
    //
    // owner -> alpha -> beta -> deputy -> child, plus an unrelated stranger.
    // `deputy` is an OWNER-APPOINTED GLOBAL MODERATOR, which is the only grant
    // in `is_ban_authorized` that reaches a member's own ancestors — and it is
    // granted at step 3, ahead of the step-4 guardrail.
    // ------------------------------------------------------------------

    /// The chain above, with `deputy` deputized by the owner.
    fn ban_chain() -> ChatRoomStateV1 {
        let (owner, alpha, beta, deputy, child, stranger) =
            (key(1), key(2), key(3), key(4), key(5), key(6));
        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alpha);
        push_member(&mut state, &owner, &alpha, &beta);
        push_member(&mut state, &owner, &beta, &deputy);
        push_member(&mut state, &owner, &deputy, &child);
        push_member(&mut state, &owner, &owner, &stranger);
        push_info(&mut state, &owner, 0, "Owner", vec![id(&deputy)]);
        for (sk, name) in [
            (&alpha, "Alpha"),
            (&beta, "Beta"),
            (&deputy, "Deputy"),
            (&child, "Child"),
            (&stranger, "Stranger"),
        ] {
            push_info(&mut state, sk, 0, name, vec![]);
        }
        state
    }

    /// riverctl refuses a self-ban AND a ban of any strict ancestor, and keeps
    /// refusing for the reason the rule states rather than for want of
    /// authority — the precondition assertions are what prove that.
    #[test]
    fn self_removing_bans_are_refused_directly_and_transitively() {
        let state = ban_chain();
        let (owner, alpha, beta, deputy) = (key(1), key(2), key(3), key(4));
        let members_by_id = state.members.members_by_member_id();

        for (label, target) in [
            ("self", id(&deputy)),
            ("direct inviter (parent)", id(&beta)),
            ("grandparent", id(&alpha)),
        ] {
            assert!(
                MembersV1::is_ban_authorized(
                    id(&deputy),
                    target,
                    &members_by_id,
                    &state.member_info,
                    id(&owner)
                ),
                "precondition ({label}): the contract AUTHORIZES this ban, so a \
                 refusal below is the #478 rule and not missing authority"
            );
            assert!(
                ban_removal_set(&state, target).contains(&id(&deputy)),
                "precondition ({label}): the cascade really does remove the banner"
            );
            assert!(
                self_removing_ban_reason(&state, id(&deputy), target).is_some(),
                "riverctl must refuse a ban of {label} (#478)"
            );
        }
    }

    /// The over-broadness case: legitimate bans stay legitimate.
    #[test]
    fn bans_that_do_not_remove_the_banner_are_not_refused() {
        let state = ban_chain();
        let (owner, alpha, deputy, child, stranger) = (key(1), key(2), key(4), key(5), key(6));

        for (label, banner, target) in [
            ("an unrelated member", id(&deputy), id(&stranger)),
            ("the deputy's own downstream", id(&deputy), id(&child)),
            ("the owner banning mid-chain", id(&owner), id(&alpha)),
        ] {
            assert!(
                self_removing_ban_reason(&state, banner, target).is_none(),
                "banning {label} does not remove the banner and must stay \
                 available (#478 must not be over-broad)"
            );
        }
    }

    /// The reported reach must not advertise authority riverctl's write path
    /// refuses. Before #478's transitive case, a global moderator's reach
    /// counted their own ancestors — bans `ban_member` now rejects.
    #[test]
    fn reach_excludes_targets_whose_ban_would_remove_the_deputy() {
        let state = ban_chain();
        let (owner, deputy) = (key(1), key(4));
        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        // Room is alpha, beta, deputy, child, stranger. A global moderator can
        // ban all of them per the contract, but alpha + beta (their ancestors)
        // and the deputy themselves would take the deputy down too, leaving
        // child + stranger.
        assert_eq!(
            d.grant(id(&owner), id(&deputy)).members_deputy_can_ban,
            2,
            "reach must exclude the deputy AND their own invite ancestors"
        );
    }

    /// The write path must go through the shared rule, not re-derive it.
    /// Source-scraped: `ban_member` needs a live node connection, so the guard
    /// itself cannot be unit-tested here — this pins the wiring.
    #[test]
    fn ban_member_is_wired_to_the_self_removal_rule() {
        let source = include_str!("api.rs");
        let prod = source
            .lines()
            .map(|line| line.split_once("//").map(|(code, _)| code).unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");

        let fn_at = prod
            .find("pub async fn ban_member(")
            .expect("api.rs must define ban_member");
        let build_at = prod[fn_at..]
            .find("let user_ban = UserBan {")
            .expect("ban_member must build a UserBan")
            + fn_at;
        assert!(
            prod[fn_at..build_at].contains("self_removing_ban_reason("),
            "`ban_member` must refuse a self-removing ban BEFORE building the \
             UserBan, through the shared rule so riverctl and the UI cannot \
             disagree (#478)"
        );
    }
}
