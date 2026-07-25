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
//! state.

use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::MemberId;
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
    /// How many members this grant currently reaches: the size of the
    /// deputizer's invite subtree, or the whole member list for an owner grant.
    ///
    /// This is what separates "the grant exists" from "the grant does anything".
    /// Per `MembersV1::is_ban_authorized`, a non-owner deputizer's grant only
    /// fires where that deputizer is a strict invite ancestor of the ban target,
    /// so a deputizer who has invited nobody confers nothing even while
    /// [`Self::active`] is true. Most members have invited nobody, so that is
    /// the common case, not a corner.
    pub reaches_members: usize,
    /// Whether the grant is structurally live: both parties are currently in the
    /// room. The load-bearing half is the DEPUTY: `is_ban_authorized` gates both
    /// deputy branches on the banner being a current member, so a grant naming a
    /// pruned deputy is honoured by nobody. The deputizer half is
    /// defence-in-depth: `MemberInfoV1::verify` rejects a `member_info` record
    /// whose member is neither the owner nor a current member, so a deputizer
    /// absent from the room cannot occur in a state the contract accepted.
    ///
    /// `active` does NOT mean the deputy can ban a given person; see
    /// [`Self::reaches_members`] and [`Self::scope`] for how far it goes.
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
    /// `member -> number of members they invited, directly or indirectly`, for
    /// every member with a non-empty subtree.
    subtree_sizes: HashMap<MemberId, usize>,
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
            ids.sort_by_key(|id| id.to_string());
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

        let subtree_sizes = invite_subtree_sizes(state);

        Self {
            state,
            secrets,
            owner_id,
            current_members,
            deputies_by_deputizer,
            known_ids,
            subtree_sizes,
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
    pub fn party(&self, id: MemberId) -> DeputyParty {
        let nickname = self.member_info().canonical(id).map(|info| {
            sanitize_for_terminal(&crate::api::unseal_nickname_display(
                &info.member_info.preferred_nickname,
                self.secrets,
            ))
        });
        DeputyParty {
            member_id: id.to_string(),
            nickname,
            is_owner: id == self.owner_id,
            in_room: self.in_room(id),
        }
    }

    /// How many members `id`'s grants reach: everyone for the owner, otherwise
    /// the size of `id`'s invite subtree.
    fn reach_of(&self, id: MemberId) -> usize {
        if id == self.owner_id {
            self.current_members.len()
        } else {
            self.subtree_sizes.get(&id).copied().unwrap_or(0)
        }
    }

    /// Build the resolved view of a single `deputizer -> deputy` grant.
    pub fn grant(&self, deputizer: MemberId, deputy: MemberId) -> DeputyGrant {
        DeputyGrant {
            scope: if deputizer == self.owner_id {
                DeputyScope::RoomWide
            } else {
                DeputyScope::InviteSubtree
            },
            active: self.in_room(deputizer) && self.in_room(deputy),
            reaches_members: self.reach_of(deputizer),
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

/// `member -> number of members they invited, directly or indirectly`, for every
/// member with a non-empty invite subtree.
///
/// Mirrors the traversal `MembersV1::get_downstream_members` performs (which is
/// private to the contract crate), including its visited-set cycle guard, so the
/// reach reported here matches the subtree `is_ban_authorized` grants authority
/// over. The owner is excluded: they are never in `members.members`, and their
/// reach is the whole room, handled separately.
fn invite_subtree_sizes(state: &ChatRoomStateV1) -> HashMap<MemberId, usize> {
    let mut children: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
    for member in &state.members.members {
        children
            .entry(member.member.invited_by)
            .or_default()
            .push(member.member.id());
    }

    let mut sizes = HashMap::new();
    for root in children.keys().copied() {
        let mut seen: HashSet<MemberId> = HashSet::new();
        let mut stack = vec![root];
        while let Some(current) = stack.pop() {
            for child in children.get(&current).into_iter().flatten() {
                // A self-invite or an invite cycle would otherwise loop forever;
                // `seen` also stops a descendant from being counted twice.
                if *child != root && seen.insert(*child) {
                    stack.push(*child);
                }
            }
        }
        if !seen.is_empty() {
            sizes.insert(root, seen.len());
        }
    }
    sizes
}

/// Strip C0 control characters from a nickname before it is printed.
///
/// Nicknames are attacker-controlled and `unseal_nickname_display` is a lossy
/// UTF-8 decode with no filtering, so an embedded newline or ANSI escape could
/// forge an extra output row. That was cosmetic before deputy status was
/// printed; now a forged row would claim moderation authority, so filter it.
fn sanitize_for_terminal(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_control() && c != '\t' {
                char::REPLACEMENT_CHARACTER
            } else {
                c
            }
        })
        .collect()
}

/// Render a party as `Nickname (SHORTID)`, or `(unknown) (SHORTID)` when they
/// have no `member_info` record in this room.
pub fn party_label(party: &DeputyParty) -> String {
    match &party.nickname {
        Some(nickname) => format!("{} ({})", nickname, party.member_id),
        None => format!("(unknown) ({})", party.member_id),
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
        let absent = if !grant.deputy.in_room {
            &grant.deputy
        } else {
            &grant.deputizer
        };
        return format!(
            "inactive: {} is not currently in this room",
            party_label(absent)
        );
    }
    match grant.scope {
        // `is_ban_authorized` refuses `target == owner` outright, so even an
        // owner-appointed deputy cannot ban the owner.
        DeputyScope::RoomWide => format!(
            "active: may ban any of the {} member(s) in the room (granted by the room owner)",
            grant.reaches_members
        ),
        DeputyScope::InviteSubtree if grant.reaches_members == 0 => format!(
            "active but reaches no one: {} has invited nobody, so this grant currently \
             confers no ban authority",
            party_label(&grant.deputizer)
        ),
        DeputyScope::InviteSubtree => format!(
            "active: may ban the {} member(s) {} invited, directly or indirectly",
            grant.reaches_members,
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
            owner_grant.reaches_members, 2,
            "an owner grant reaches every member"
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
        // Grant @ v1, then revoke @ v2, pushed in "wrong" order on purpose.
        push_info(&mut state, &alice, 2, "Alice", vec![]);
        push_info(&mut state, &alice, 1, "Alice", vec![id(&bob)]);

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
        // The whole point of `reaches_members`: a deputy of someone who invited
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
        assert_eq!(from_alice.reaches_members, 2);
        let line = grant_status_line(&from_alice);
        assert!(line.contains("2 member(s)"), "{line}");
        assert!(line.contains("Alice"), "must name the deputizer: {line}");

        // Bob invited nobody, so his grant is live but reaches no one. This
        // must NOT read as though the deputy can ban within a real subtree.
        let from_bob = d.grant(id(&bob), id(&deputy));
        assert!(from_bob.active);
        assert_eq!(from_bob.reaches_members, 0);
        let bob_line = grant_status_line(&from_bob);
        assert!(
            bob_line.contains("reaches no one"),
            "an empty subtree must be stated plainly: {bob_line}"
        );
        assert!(bob_line.contains("Bob"), "{bob_line}");
    }

    #[test]
    fn invite_subtree_sizes_survives_a_cycle_and_a_self_invite() {
        // `get_downstream_members` carries a visited-set cycle guard; the CLI
        // mirror must too, or a malformed invite graph hangs the command.
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);

        let mut state = ChatRoomStateV1::default();
        push_member(&mut state, &owner, &owner, &alice);
        push_member(&mut state, &owner, &alice, &bob);
        // Hand-forged cycle: alice claims to have been invited by bob (her own
        // descendant). `AuthorizedMember::new` would assert, so build it raw.
        state.members.members[0].member.invited_by = id(&bob);

        let sizes = invite_subtree_sizes(&state);
        assert_eq!(sizes.get(&id(&alice)).copied().unwrap_or(0), 1);
        assert_eq!(sizes.get(&id(&bob)).copied().unwrap_or(0), 1);

        // A member who invited themselves must not be counted as their own
        // descendant, and must not loop.
        let mut selfie = ChatRoomStateV1::default();
        push_member(&mut selfie, &owner, &owner, &alice);
        selfie.members.members[0].member.invited_by = id(&alice);
        assert_eq!(
            invite_subtree_sizes(&selfie).get(&id(&alice)).copied(),
            None,
            "self-invite is not a subtree"
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
        assert!(line.contains("invited"), "must scope it: {line}");

        let owner_line = grant_status_line(&d.grant(id(&owner), id(&bob)));
        assert!(
            owner_line.contains("room owner"),
            "an owner grant must be attributed to the owner: {owner_line}"
        );

        assert_eq!(
            party_label(&d.party(id(&alice))),
            format!("Alice ({})", id(&alice))
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
        // claim, so an embedded newline must not produce a second line that
        // looks like a real grant.
        let owner = key(1);
        let evil = key(2);
        let mut state = room(&owner, &[&evil]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(
            &mut state,
            &evil,
            0,
            "Eve\n  Admin (AAAAAAAA)  deputy of: Room Owner",
            vec![],
        );

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        let label = party_label(&d.party(id(&evil)));
        assert!(!label.contains('\n'), "newline must be stripped: {label:?}");
        assert!(!label.contains('\r'));
        assert!(!label.contains('\u{1b}'), "ANSI escapes must be stripped");
        assert!(label.starts_with("Eve"), "the readable part survives");

        assert_eq!(sanitize_for_terminal("plain"), "plain");
        assert_eq!(sanitize_for_terminal("keep\ttab"), "keep\ttab");
        assert_eq!(sanitize_for_terminal("emoji \u{1f600}"), "emoji \u{1f600}");
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
        assert_eq!(json["reaches_members"], 2);
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
}
