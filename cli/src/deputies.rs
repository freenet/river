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
use std::collections::{BTreeSet, HashMap, HashSet};

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
    /// Whether the grant is not structurally inert: both parties are currently
    /// in the room. The contract only honours deputy authority when the deputy
    /// is a current member, and a non-owner deputizer must be a genuine invite
    /// ancestor of the ban target — so a grant naming a pruned member on either
    /// side confers nothing. `true` does NOT mean the deputy can ban any given
    /// person: a non-owner deputizer's authority is still limited to their own
    /// invite subtree, which may be empty.
    pub active: bool,
}

/// Why a short member id could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No member id in the room matched.
    NotFound,
    /// The input is a prefix of more than one member id. Carries the matching
    /// ids, sorted, so the caller can list them.
    Ambiguous(Vec<String>),
}

/// Deputy queries over one fetched room state.
///
/// Borrows the state; construct one per fetch. `secrets` is the private-room
/// decryption map from `ApiClient::room_display_secrets` (empty for a public
/// room), used only to render nicknames.
pub struct RoomDeputies<'a> {
    state: &'a ChatRoomStateV1,
    secrets: &'a HashMap<u32, [u8; 32]>,
    owner_id: MemberId,
    current_members: HashSet<MemberId>,
}

impl<'a> RoomDeputies<'a> {
    pub fn new(
        state: &'a ChatRoomStateV1,
        owner_vk: &VerifyingKey,
        secrets: &'a HashMap<u32, [u8; 32]>,
    ) -> Self {
        let current_members = state
            .members
            .members
            .iter()
            .map(|m| m.member.id())
            .collect();
        Self {
            state,
            secrets,
            owner_id: MemberId::from(owner_vk),
            current_members,
        }
    }

    fn member_info(&self) -> &MemberInfoV1 {
        &self.state.member_info
    }

    /// Every member id this room mentions anywhere: the owner, current members,
    /// members with a `member_info` record, and ids appearing in ANY deputies
    /// list. The last source matters — a deputy pruned for inactivity keeps no
    /// `member_info` record but is still named by their deputizer's signed
    /// record, and "is this (now absent) member still someone's deputy?" is a
    /// question worth being able to ask.
    fn known_member_ids(&self) -> BTreeSet<MemberId> {
        let mut ids: BTreeSet<MemberId> = BTreeSet::new();
        ids.insert(self.owner_id);
        ids.extend(self.current_members.iter().copied());
        for info in &self.member_info().member_info {
            ids.insert(info.member_info.member_id);
        }
        for id in self.deputizer_candidates() {
            ids.extend(self.member_info().deputies_of(id).iter().copied());
        }
        ids
    }

    /// Ids that could plausibly hold a `deputies` list: every member with a
    /// `member_info` record (deputies live nowhere else, so a member without a
    /// record has, by definition, deputized no one).
    fn deputizer_candidates(&self) -> BTreeSet<MemberId> {
        self.member_info()
            .member_info
            .iter()
            .map(|info| info.member_info.member_id)
            .collect()
    }

    /// Resolve a user-supplied short id against every id the room mentions.
    ///
    /// Matching mirrors `ban_member` / `member deputize` — a case-sensitive
    /// prefix match, or a case-insensitive match against the first 8 characters
    /// — but, unlike those write paths, an input matching more than one member
    /// is reported as [`ResolveError::Ambiguous`] rather than silently resolving
    /// to whichever record happens to come first in the state vector.
    pub fn resolve_short_id(&self, short: &str) -> Result<MemberId, ResolveError> {
        if short.is_empty() {
            return Err(ResolveError::NotFound);
        }
        let matches: Vec<MemberId> = self
            .known_member_ids()
            .into_iter()
            .filter(|id| {
                let s = id.to_string();
                s.starts_with(short) || s[..8.min(s.len())].eq_ignore_ascii_case(short)
            })
            .collect();
        match matches.len() {
            0 => Err(ResolveError::NotFound),
            1 => Ok(matches[0]),
            _ => Err(ResolveError::Ambiguous(
                matches.iter().map(|id| id.to_string()).collect(),
            )),
        }
    }

    /// Whether `id` is currently in the room (a member, or the owner).
    fn in_room(&self, id: MemberId) -> bool {
        id == self.owner_id || self.current_members.contains(&id)
    }

    /// Resolve one member for display.
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

    /// Build the resolved view of a single `deputizer -> deputy` grant.
    pub fn grant(&self, deputizer: MemberId, deputy: MemberId) -> DeputyGrant {
        DeputyGrant {
            scope: if deputizer == self.owner_id {
                DeputyScope::RoomWide
            } else {
                DeputyScope::InviteSubtree
            },
            active: self.in_room(deputizer) && self.in_room(deputy),
            deputizer: self.party(deputizer),
            deputy: self.party(deputy),
        }
    }

    /// Forward lookup: the members `deputizer` has deputized, sorted by id.
    ///
    /// Reads the CANONICAL record, so a revoked grant lingering in a duplicate
    /// lower-rank record is not reported.
    pub fn deputies_of(&self, deputizer: MemberId) -> Vec<MemberId> {
        let mut ids: Vec<MemberId> = self.member_info().deputies_of(deputizer).to_vec();
        ids.sort_by_key(|id| id.to_string());
        ids.dedup();
        ids
    }

    /// Reverse lookup: every member who has deputized `deputy`, sorted by id.
    pub fn deputizers_of(&self, deputy: MemberId) -> Vec<MemberId> {
        let mut ids: Vec<MemberId> = self
            .deputizer_candidates()
            .into_iter()
            .filter(|deputizer| self.member_info().deputies_of(*deputizer).contains(&deputy))
            .collect();
        ids.sort_by_key(|id| id.to_string());
        ids
    }

    /// Every grant in the room, sorted by `(deputizer, deputy)` id.
    pub fn all_grants(&self) -> Vec<DeputyGrant> {
        let mut grants = Vec::new();
        for deputizer in self.deputizer_candidates() {
            for deputy in self.deputies_of(deputizer) {
                grants.push(self.grant(deputizer, deputy));
            }
        }
        grants.sort_by(|a, b| {
            (&a.deputizer.member_id, &a.deputy.member_id)
                .cmp(&(&b.deputizer.member_id, &b.deputy.member_id))
        });
        grants
    }

    /// `deputy -> [deputizer, …]` for every grant in the room, for annotating a
    /// full member listing without an O(members²) rescan.
    pub fn deputizers_by_deputy(&self) -> HashMap<MemberId, Vec<MemberId>> {
        let mut map: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
        for deputizer in self.deputizer_candidates() {
            for deputy in self.deputies_of(deputizer) {
                map.entry(deputy).or_default().push(deputizer);
            }
        }
        for deputizers in map.values_mut() {
            deputizers.sort_by_key(|id| id.to_string());
        }
        map
    }
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
        DeputyScope::RoomWide => {
            "active: may ban room-wide (granted by the room owner)".to_string()
        }
        DeputyScope::InviteSubtree => format!(
            "active: may ban within {}'s invite subtree",
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
            let member = Member {
                owner_member_id: id(owner),
                invited_by: id(owner),
                member_vk: sk.verifying_key(),
            };
            state
                .members
                .members
                .push(AuthorizedMember::new(member, owner));
        }
        state
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

        assert_eq!(d.deputies_of(id(&alice)), vec![id(&bob)]);
        assert_eq!(d.deputizers_of(id(&bob)), vec![id(&alice)]);

        // The reverse direction is NOT symmetric: Bob deputized nobody.
        assert!(d.deputies_of(id(&bob)).is_empty());
        assert!(d.deputizers_of(id(&alice)).is_empty());
    }

    #[test]
    fn reverse_lookup_finds_every_deputizer_of_one_member() {
        // Ian's use case: "is this member a deputy of anyone — the owner, the
        // invite bot, …?" Two independent members deputize the same target.
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
        assert_eq!(party.nickname, None, "no member_info → no nickname");
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
        // A one-character prefix will match several of the room's ids; the read
        // path must refuse rather than pick whichever comes first in the vector.
        let owner = key(1);
        let members: Vec<SigningKey> = (2u8..12).map(key).collect();
        let refs: Vec<&SigningKey> = members.iter().collect();
        let mut state = room(&owner, &refs);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        for (n, sk) in members.iter().enumerate() {
            push_info(&mut state, sk, 0, &format!("M{n}"), vec![]);
        }

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        // Find a first character shared by at least two ids.
        let mut by_first: HashMap<char, usize> = HashMap::new();
        for sk in &members {
            *by_first
                .entry(id(sk).to_string().chars().next().unwrap())
                .or_default() += 1;
        }
        let (shared, _) = by_first
            .iter()
            .find(|(_, count)| **count > 1)
            .expect("10 ids over a 32-symbol alphabet must share a first character");

        match d.resolve_short_id(&shared.to_string()) {
            Err(ResolveError::Ambiguous(ids)) => assert!(ids.len() > 1),
            other => panic!("expected an ambiguity error, got {other:?}"),
        }
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
        // Grant @ v1, then revoke @ v2 — pushed in "wrong" order on purpose.
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
    fn a_grant_naming_a_pruned_member_is_reported_but_marked_inactive() {
        // A deputy pruned for inactivity keeps no `member_info` record but is
        // still named by their deputizer's signed record. The contract honours
        // deputy authority only for CURRENT members, so the grant is inert —
        // surface it, but do not claim it confers anything.
        let owner = key(1);
        let alice = key(2);
        let pruned = key(3);
        let mut state = room(&owner, &[&alice]);
        push_info(&mut state, &owner, 0, "Room Owner", vec![]);
        push_info(&mut state, &alice, 0, "Alice", vec![id(&pruned)]);

        let secrets = no_secrets();
        let d = RoomDeputies::new(&state, &owner.verifying_key(), &secrets);

        assert_eq!(d.deputies_of(id(&alice)), vec![id(&pruned)]);
        let grant = d.grant(id(&alice), id(&pruned));
        assert!(!grant.active, "the deputy is no longer a member");
        assert_eq!(grant.deputy.nickname, None);
        assert!(!grant.deputy.in_room);

        // The pruned id is still resolvable, so `deputized-by` can answer for
        // it (mirroring the revoke path's own-deputies fallback).
        assert_eq!(
            d.resolve_short_id(&id(&pruned).to_string()),
            Ok(id(&pruned))
        );
        assert_eq!(d.deputizers_of(id(&pruned)), vec![id(&alice)]);
    }

    #[test]
    fn a_grant_from_a_pruned_deputizer_is_inactive() {
        // Mirror image: the DEPUTIZER left the room. A non-owner deputizer must
        // be a genuine invite ancestor of the ban target, which a non-member
        // never is, so the grant confers nothing.
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
    fn human_status_line_names_the_deputizer_not_a_bare_badge() {
        // The UI's viewer-scoped shield badge is the confusion this command
        // exists to avoid: every line must say WHOSE authority is in play.
        let owner = key(1);
        let alice = key(2);
        let bob = key(3);
        let mut state = room(&owner, &[&alice, &bob]);
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
            owner_line.contains("room-wide"),
            "an owner grant is room-wide: {owner_line}"
        );

        assert_eq!(
            party_label(&d.party(id(&alice))),
            format!("Alice ({})", id(&alice))
        );
    }
}
