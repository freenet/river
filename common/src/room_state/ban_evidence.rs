//! Ban evidence (freenet/river#702): the `AuthorizedMember` records of
//! members who are no longer in the room but whom a stored ban still needs.
//!
//! A ban is only verifiable, and its authority only decidable, from its
//! issuer's key and the invite chains of its issuer and target. `MemberId` is
//! a non-cryptographic hash of the key and a ban carries no key, so those
//! records have to be kept somewhere after their members leave. They cannot
//! stay in `members` (that grants membership), and they cannot be dropped
//! (the ban's validity would then change as a side effect of the removal it
//! caused, and `post_apply_cleanup` would stop being idempotent). This field
//! keeps them, and NOTHING treats it as membership: an evidence record
//! confers no posting, DM, invite, secret, slot or listing. It is read only by
//! ban resolution (`MembersV1::resolve_bans`), the ban signature checks, and
//! `member_info` retention.
//!
//! The `member_info` records of evidenced members are kept too, because
//! `MembersV1::is_ban_authorized` reads their `deputies`. They keep merging by
//! rank like any other record, so an evidenced member can still publish a new
//! `deputies` list. Freezing it at removal was considered and does not
//! converge: two peers that removed the member at different moments hold
//! different records, and any rule that lets one replace the other (highest
//! rank, lowest rank, first seen elsewhere) is also a rule the removed member
//! can win by signing a suitable record, while a rule that never replaces
//! leaves the peers disagreeing forever. A list published after removal
//! grants no more than the member's list could while present: authority
//! over their own invite subtree. A target's own `deputies` list does not
//! limit who may ban it (#772), so the list cannot void a ban on its author.
//!
//! `post_apply_cleanup` recomputes the set on every pass: a record is kept
//! while a stored ban references its member (issuer or target) or one of
//! their invite descendants, and while that member is absent from `members`.
//! Every record's invite chain must verify, through `members` and this field,
//! back to the owner, so the state stays self-authorizing
//! (AGENTS.md, State Authorization Rule).

use crate::room_state::ban::BansV1;
use crate::room_state::member::{AuthorizedMember, MemberId, MembersV1};
use crate::room_state::ChatRoomParametersV1;
use crate::ChatRoomStateV1;
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

/// Every member id a stored non-owner ban references: its issuer and target,
/// and their invite ancestors, as far as `inviters` knows them. The set
/// `post_apply_cleanup` step 5a keeps evidence for, and the only evidence
/// `verify` accepts. `inviters(id)` gives the inviter of every known record
/// with that id; with more than one, every one is followed.
pub(crate) fn referenced_by_bans<I: IntoIterator<Item = MemberId>>(
    bans: &BansV1,
    owner_id: MemberId,
    inviters: impl Fn(MemberId) -> I,
) -> HashSet<MemberId> {
    let mut referenced = HashSet::new();
    let mut stack = Vec::new();
    for ban in bans.0.iter().filter(|b| b.banned_by != owner_id) {
        for start in [ban.banned_by, ban.ban.banned_user] {
            if start != owner_id && referenced.insert(start) {
                stack.push(start);
            }
            while let Some(current) = stack.pop() {
                for inviter in inviters(current) {
                    if inviter != owner_id && referenced.insert(inviter) {
                        stack.push(inviter);
                    }
                }
            }
        }
    }
    referenced
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct BanEvidenceV1 {
    /// Sorted by member id, at most one record per id, none of them in
    /// `members`.
    pub members: Vec<AuthorizedMember>,
}

impl BanEvidenceV1 {
    /// No records: the field is then left out of the serialized state.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Every record ban resolution may read: `members`, then each evidence
    /// record whose inviter is the owner or is itself in the map. A record
    /// whose chain no longer reaches the owner (an ancestor was lost, for
    /// example to the `max_members` trim, which runs before cleanup) is left
    /// out for every reader at once, and `post_apply_cleanup` drops it.
    pub fn lookup<'a>(
        &'a self,
        members: &'a MembersV1,
        owner_id: MemberId,
    ) -> HashMap<MemberId, &'a AuthorizedMember> {
        let mut map = members.members_by_member_id();
        let mut pending: Vec<&AuthorizedMember> = self
            .members
            .iter()
            .filter(|m| !map.contains_key(&m.member.id()))
            .collect();
        loop {
            let before = pending.len();
            pending.retain(|m| {
                let inviter = m.member.invited_by;
                if inviter == owner_id || map.contains_key(&inviter) {
                    map.insert(m.member.id(), m);
                    false
                } else {
                    true
                }
            });
            if pending.len() == before {
                break;
            }
        }
        map
    }

    /// The per-record checks of [`ComposableState::verify`]: no duplicate
    /// ids, not the owner, not a current member, and an invite chain that
    /// verifies through `members` and this field. Each record's signature is
    /// checked at most once.
    fn verify_records(
        &self,
        parent_state: &ChatRoomStateV1,
        parameters: &ChatRoomParametersV1,
    ) -> Result<(), String> {
        let owner_id = parameters.owner_id();
        let present = parent_state.members.members_by_member_id();
        let lookup = self.lookup(&parent_state.members, owner_id);
        let mut seen = HashSet::new();
        let mut verified = HashSet::new();
        for m in &self.members {
            let id = m.member.id();
            if !seen.insert(id) {
                return Err(format!("Duplicate ban evidence record for {id:?}"));
            }
            if id == owner_id || m.member.member_vk == parameters.owner {
                return Err("Ban evidence must not hold the owner".to_string());
            }
            if present.contains_key(&id) {
                return Err(format!("Ban evidence record {id:?} is a current member"));
            }
            MembersV1::verify_invite_chain(m, parameters, &lookup, &mut verified)
                .map_err(|e| format!("Ban evidence record {id:?}: {e}"))?;
        }
        Ok(())
    }
}

impl ComposableState for BanEvidenceV1 {
    type ParentState = ChatRoomStateV1;
    type Summary = BTreeSet<MemberId>;
    type Delta = Vec<AuthorizedMember>;
    type Parameters = ChatRoomParametersV1;

    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        let owner_id = parameters.owner_id();
        self.verify_records(parent_state, parameters)?;
        // Every record is one a stored ban needs, as `post_apply_cleanup`
        // keeps them, so the field is bounded by the stored bans.
        let lookup = self.lookup(&parent_state.members, owner_id);
        let referenced = referenced_by_bans(&parent_state.bans, owner_id, |id| {
            lookup.get(&id).map(|m| m.member.invited_by)
        });
        if let Some(m) = self
            .members
            .iter()
            .find(|m| !referenced.contains(&m.member.id()))
        {
            return Err(format!(
                "Ban evidence record {:?} is not referenced by a stored ban",
                m.member.id()
            ));
        }
        Ok(())
    }

    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        self.members.iter().map(|m| m.member.id()).collect()
    }

    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta> {
        let missing: Vec<AuthorizedMember> = self
            .members
            .iter()
            .filter(|m| !old_state_summary.contains(&m.member.id()))
            .cloned()
            .collect();
        if missing.is_empty() {
            None
        } else {
            Some(missing)
        }
    }

    /// Adds every incoming record whose invite chain verifies through
    /// `members`, this field and the delta itself. A record that does not
    /// verify is skipped, not an error: nothing unverified enters state, and a
    /// sender's evidence can never make the receiver reject the rest of the
    /// merge. Records whose member is (again) in `members` are dropped: a
    /// present member's record lives there. Whether a record is still
    /// NEEDED is decided by `post_apply_cleanup`, from the converged bans.
    fn apply_delta(
        &mut self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        let owner_id = parameters.owner_id();
        let present = parent_state.members.members_by_member_id();
        if let Some(delta) = delta {
            // Only records a stored ban references can be kept (see `verify`;
            // cleanup step 5a drops the rest), so the others are dropped here
            // before any chain is checked, by id alone. `bans` is final by
            // field order, before the `max_user_bans` cap, which only removes
            // bans; every record that could be known by cleanup is followed,
            // so the set is never smaller than the one cleanup keeps.
            let mut inviters: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
            for m in parent_state
                .members
                .members
                .iter()
                .chain(&self.members)
                .chain(delta)
            {
                inviters
                    .entry(m.member.id())
                    .or_default()
                    .push(m.member.invited_by);
            }
            let referenced = referenced_by_bans(&parent_state.bans, owner_id, |id| {
                inviters.get(&id).cloned().unwrap_or_default()
            });
            let added = {
                let mut combined = self.lookup(&parent_state.members, owner_id);
                for m in delta {
                    combined.entry(m.member.id()).or_insert(m);
                }
                let held: HashSet<MemberId> = self.members.iter().map(|m| m.member.id()).collect();
                let mut added_ids = HashSet::new();
                let mut verified = HashSet::new();
                let mut added = Vec::new();
                for m in delta {
                    let id = m.member.id();
                    if id == owner_id
                        || !referenced.contains(&id)
                        || present.contains_key(&id)
                        || held.contains(&id)
                        || added_ids.contains(&id)
                    {
                        continue;
                    }
                    if MembersV1::verify_invite_chain(m, parameters, &combined, &mut verified)
                        .is_ok()
                    {
                        added_ids.insert(id);
                        added.push(m.clone());
                    }
                }
                added
            };
            self.members.extend(added);
        }
        self.members
            .retain(|m| !present.contains_key(&m.member.id()));
        self.members.sort_by_key(|m| m.member.id());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::ban::{AuthorizedUserBan, UserBan};
    use crate::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use crate::room_state::member::tests::{
        create_test_member, invite_signature_checks, random_tree,
    };
    use crate::room_state::member::Member;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    /// `verify_records` as it was before chain checks were memoized: every
    /// record's full chain is walked.
    fn verify_walking_every_chain(
        evidence: &BanEvidenceV1,
        parent_state: &ChatRoomStateV1,
        parameters: &ChatRoomParametersV1,
    ) -> Result<(), String> {
        let owner_id = parameters.owner_id();
        let present = parent_state.members.members_by_member_id();
        let lookup = evidence.lookup(&parent_state.members, parameters.owner_id());
        let mut seen = BTreeSet::new();
        for m in &evidence.members {
            let id = m.member.id();
            if !seen.insert(id) {
                return Err(format!("Duplicate ban evidence record for {id:?}"));
            }
            if id == owner_id || m.member.member_vk == parameters.owner {
                return Err("Ban evidence must not hold the owner".to_string());
            }
            if present.contains_key(&id) {
                return Err(format!("Ban evidence record {id:?} is a current member"));
            }
            parent_state
                .members
                .get_invite_chain_with_lookup(m, parameters, &lookup)
                .map_err(|e| format!("Ban evidence record {id:?}: {e}"))?;
        }
        Ok(())
    }

    /// `apply_delta` as it was before chain checks were memoized and the
    /// delta was bounded.
    fn apply_walking_every_chain(
        evidence: &mut BanEvidenceV1,
        parent_state: &ChatRoomStateV1,
        parameters: &ChatRoomParametersV1,
        delta: &[AuthorizedMember],
    ) {
        let owner_id = parameters.owner_id();
        let present = parent_state.members.members_by_member_id();
        let mut combined = evidence.lookup(&parent_state.members, parameters.owner_id());
        for m in delta {
            combined.entry(m.member.id()).or_insert(m);
        }
        let mut added = Vec::new();
        for m in delta {
            let id = m.member.id();
            if id == owner_id
                || present.contains_key(&id)
                || evidence.members.iter().any(|e| e.member.id() == id)
                || added.iter().any(|e: &AuthorizedMember| e.member.id() == id)
            {
                continue;
            }
            if parent_state
                .members
                .get_invite_chain_with_lookup(m, parameters, &combined)
                .is_ok()
            {
                added.push(m.clone());
            }
        }
        evidence.members.extend(added);
        evidence
            .members
            .retain(|m| !present.contains_key(&m.member.id()));
        evidence.members.sort_by_key(|m| m.member.id());
    }

    fn state_with(
        owner_sk: &SigningKey,
        max_user_bans: usize,
        members: Vec<AuthorizedMember>,
    ) -> ChatRoomStateV1 {
        let owner_id: MemberId = owner_sk.verifying_key().into();
        ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    max_user_bans,
                    max_members: 1_000,
                    ..Default::default()
                },
                owner_sk,
            ),
            members: MembersV1 { members },
            ..Default::default()
        }
    }

    /// Non-owner bans naming each id as target, by an issuer id that is not
    /// a member. Only the ids matter to which records are referenced.
    fn bans_naming(ids: impl IntoIterator<Item = MemberId>, owner_id: MemberId) -> BansV1 {
        let issuer_sk = SigningKey::generate(&mut OsRng);
        let issuer: MemberId = issuer_sk.verifying_key().into();
        BansV1(
            ids.into_iter()
                .map(|target| {
                    AuthorizedUserBan::new(
                        UserBan {
                            owner_member_id: owner_id,
                            banned_at: std::time::SystemTime::UNIX_EPOCH,
                            banned_user: target,
                        },
                        issuer,
                        &issuer_sk,
                    )
                })
                .collect(),
        )
    }

    /// A random invite tree with one defect (as in `MembersV1`'s
    /// `verify_matches_walking_every_chain`), its records shuffled. Records
    /// are then split by the caller.
    fn defective_tree(
        rng: &mut rand::rngs::StdRng,
        seed: u64,
        owner_sk: &SigningKey,
    ) -> Vec<AuthorizedMember> {
        use rand::seq::SliceRandom;
        use rand::Rng;
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let n = rng.gen_range(1..16);
        let (mut records, keys) = random_tree(rng, owner_sk, n);
        let i = rng.gen_range(0..records.len());
        let j = rng.gen_range(0..records.len());
        match seed % 9 {
            0 => {}
            1 => records[i].signature = records[j].signature,
            2 => {
                records.remove(i);
            }
            3 => records[i].member.invited_by = records[i].member.id(),
            4 => {
                let target = records[j].member.id();
                records[i].member.invited_by = target;
                records[i] = AuthorizedMember::new(records[i].member.clone(), &keys[j]);
            }
            5 => records.push(AuthorizedMember::new(
                Member {
                    owner_member_id: owner_id,
                    invited_by: records[i].member.id(),
                    member_vk: owner_sk.verifying_key(),
                },
                &keys[i],
            )),
            6 | 7 => {
                let (inviter_id, inviter_sk) = if j == i {
                    (owner_id, owner_sk.clone())
                } else {
                    (records[j].member.id(), keys[j].clone())
                };
                let member = Member {
                    invited_by: inviter_id,
                    ..records[i].member.clone()
                };
                let signature = if seed % 9 == 6 {
                    records[i].signature
                } else {
                    crate::util::sign_struct(&member, &inviter_sk)
                };
                records.push(AuthorizedMember { member, signature });
            }
            _ => {
                let other = SigningKey::generate(rng);
                records[i].signature = crate::util::sign_struct(&records[i].member, &other);
            }
        }
        records.shuffle(rng);
        records
    }

    /// The memoized per-record checks give the same result, error text
    /// included, as walking every record's full chain, on valid trees and on
    /// trees with one defect, split at random between `members` and the
    /// evidence, in shuffled and reversed order.
    #[test]
    fn verify_records_matches_walking_every_chain() {
        use rand::{Rng, SeedableRng};
        let mut rejected = 0;
        for seed in 0..180u64 {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let owner_sk = SigningKey::generate(&mut rng);
            let parameters = ChatRoomParametersV1 {
                owner: owner_sk.verifying_key(),
            };
            let records = defective_tree(&mut rng, seed, &owner_sk);
            let (present, held): (Vec<_>, Vec<_>) =
                records.into_iter().partition(|_| rng.gen_bool(0.3));
            let parent_state = state_with(&owner_sk, 10, present);
            let mut reversed = held.clone();
            reversed.reverse();
            for members in [held, reversed] {
                let evidence = BanEvidenceV1 { members };
                let expected = verify_walking_every_chain(&evidence, &parent_state, &parameters);
                let actual = evidence.verify_records(&parent_state, &parameters);
                assert_eq!(actual, expected, "seed {seed}");
                if expected.is_err() {
                    rejected += 1;
                }
            }
        }
        assert!(rejected > 100, "only {rejected} rejected");
    }

    /// The memoized `apply_delta` keeps exactly the records the full walk
    /// kept that a stored ban references, with records split at random
    /// between `members`, held evidence and the delta, the delta in shuffled
    /// and reversed order.
    #[test]
    fn apply_delta_matches_walking_every_chain() {
        use rand::{Rng, SeedableRng};
        let mut kept_some = 0;
        let mut dropped_some = 0;
        for seed in 0..180u64 {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let owner_sk = SigningKey::generate(&mut rng);
            let parameters = ChatRoomParametersV1 {
                owner: owner_sk.verifying_key(),
            };
            let records = defective_tree(&mut rng, seed, &owner_sk);
            let (mut present, mut held, mut delta) = (Vec::new(), Vec::new(), Vec::new());
            for r in records {
                match rng.gen_range(0..4) {
                    0 => present.push(r),
                    1 => held.push(r),
                    _ => delta.push(r),
                }
            }
            let mut parent_state = state_with(&owner_sk, 10, present);
            // Bans name a random subset of the delta (and so their ancestors).
            let owner_id = parameters.owner_id();
            parent_state.bans = bans_naming(
                delta
                    .iter()
                    .filter(|_| rng.gen_bool(0.7))
                    .map(|m| m.member.id()),
                owner_id,
            );
            let mut known: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
            for m in parent_state
                .members
                .members
                .iter()
                .chain(&held)
                .chain(&delta)
            {
                known
                    .entry(m.member.id())
                    .or_default()
                    .push(m.member.invited_by);
            }
            let referenced = referenced_by_bans(&parent_state.bans, owner_id, |id| {
                known.get(&id).cloned().unwrap_or_default()
            });
            let mut reversed = delta.clone();
            reversed.reverse();
            for delta in [delta, reversed] {
                // The full walk, then only the records a ban references: an
                // unreferenced id is dropped whichever record carries it.
                let mut expected = BanEvidenceV1 {
                    members: held.clone(),
                };
                apply_walking_every_chain(&mut expected, &parent_state, &parameters, &delta);
                expected
                    .members
                    .retain(|m| referenced.contains(&m.member.id()) || held.contains(m));
                let mut actual = BanEvidenceV1 {
                    members: held.clone(),
                };
                actual
                    .apply_delta(&parent_state, &parameters, &Some(delta.clone()))
                    .unwrap();
                assert_eq!(actual, expected, "seed {seed}");
                let added = expected.members.len()
                    - held.iter().filter(|m| expected.members.contains(m)).count();
                if added > 0 {
                    kept_some += 1;
                }
                if added < delta.len() {
                    dropped_some += 1;
                }
            }
        }
        assert!(
            kept_some > 100 && dropped_some > 100,
            "{kept_some} {dropped_some}"
        );
    }

    /// A chain owner -> m0 -> ... -> m{n-1}: the records and the last key.
    fn chain(owner_sk: &SigningKey, n: usize) -> Vec<AuthorizedMember> {
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let mut records = Vec::with_capacity(n);
        let mut inviter_sk = owner_sk.clone();
        let mut inviter_id = owner_id;
        for _ in 0..n {
            let (member, sk) = create_test_member(owner_id, inviter_id);
            inviter_id = member.id();
            records.push(AuthorizedMember::new(member, &inviter_sk));
            inviter_sk = sk;
        }
        records
    }

    /// Each link of a chain held as evidence, or added in one delta in
    /// either order, is checked once.
    #[test]
    fn each_link_of_a_deep_chain_is_checked_once() {
        let n = 200;
        let owner_sk = SigningKey::generate(&mut OsRng);
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let records = chain(&owner_sk, n);
        let mut parent_state = state_with(&owner_sk, 10, Vec::new());
        parent_state.bans = bans_naming([records[n - 1].member.id()], parameters.owner_id());

        let evidence = BanEvidenceV1 {
            members: records.clone(),
        };
        let before = invite_signature_checks();
        evidence.verify_records(&parent_state, &parameters).unwrap();
        assert_eq!(invite_signature_checks() - before, n);

        let mut deepest_first = records.clone();
        deepest_first.reverse();
        for delta in [records, deepest_first] {
            let mut evidence = BanEvidenceV1::default();
            let before = invite_signature_checks();
            evidence
                .apply_delta(&parent_state, &parameters, &Some(delta))
                .unwrap();
            assert_eq!(invite_signature_checks() - before, n);
            assert_eq!(evidence.members.len(), n);
        }
    }

    /// Records no stored ban references are dropped before any chain is
    /// checked: a delta of 500 of them costs no signature checks, and the one
    /// referenced chain in it costs one check per link.
    #[test]
    fn unreferenced_records_cost_no_chain_checks() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let mut parent_state = state_with(&owner_sk, 10, Vec::new());
        let mut delta: Vec<AuthorizedMember> = chain(&owner_sk, 500);
        let referenced_chain = chain(&owner_sk, 8);
        parent_state.bans = bans_naming([referenced_chain[7].member.id()], owner_id);

        let mut evidence = BanEvidenceV1::default();
        let before = invite_signature_checks();
        evidence
            .apply_delta(&parent_state, &parameters, &Some(delta.clone()))
            .unwrap();
        assert_eq!(invite_signature_checks() - before, 0);
        assert!(evidence.members.is_empty());

        delta.extend(referenced_chain.iter().cloned());
        let before = invite_signature_checks();
        evidence
            .apply_delta(&parent_state, &parameters, &Some(delta))
            .unwrap();
        assert_eq!(invite_signature_checks() - before, 8);
        let mut expected = referenced_chain;
        expected.sort_by_key(|m| m.member.id());
        assert_eq!(evidence.members, expected);
    }

    /// `verify` rejects an evidence record no stored ban references, and
    /// accepts it once a ban names it or a member it invited.
    #[test]
    fn verify_requires_every_record_to_be_referenced_by_a_ban() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        // owner -> a (present) ; owner -> x -> y, both absent.
        let (a, a_sk) = create_test_member(owner_id, owner_id);
        let records = chain(&owner_sk, 2);
        let (x, y) = (records[0].member.id(), records[1].member.id());
        let mut state = state_with(
            &owner_sk,
            10,
            vec![AuthorizedMember::new(a.clone(), &owner_sk)],
        );
        state.ban_evidence = BanEvidenceV1 {
            members: records.clone(),
        };
        state.ban_evidence.members.sort_by_key(|m| m.member.id());
        let err = state.ban_evidence.verify(&state, &parameters).unwrap_err();
        assert!(err.contains("not referenced"), "{err}");

        let ban = |target: MemberId| {
            AuthorizedUserBan::new(
                UserBan {
                    owner_member_id: owner_id,
                    banned_at: std::time::SystemTime::UNIX_EPOCH,
                    banned_user: target,
                },
                a.id(),
                &a_sk,
            )
        };
        // A ban on x references x but not y.
        state.bans = BansV1(vec![ban(x)]);
        assert!(state.ban_evidence.verify(&state, &parameters).is_err());
        // A ban on y references y and, through its chain, x.
        state.bans = BansV1(vec![ban(y)]);
        state.ban_evidence.verify(&state, &parameters).unwrap();
        // An owner ban references nothing: it needs no record.
        state.bans = BansV1(vec![AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: std::time::SystemTime::UNIX_EPOCH,
                banned_user: y,
            },
            owner_id,
            &owner_sk,
        )]);
        assert!(state.ban_evidence.verify(&state, &parameters).is_err());
    }
}
