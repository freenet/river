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
use std::collections::{BTreeMap, HashMap, HashSet};

/// A 128-bit digest of one ban-evidence record: the first 16 bytes of the
/// BLAKE3 hash of the record's CBOR encoding (`crate::util::record_digest`).
/// It is the value of the evidence summary, so anti-entropy sends a record
/// whenever two peers hold different records for one id, and it orders the
/// canonical choice between two valid records for one id (the lower digest
/// wins). 128 bits because a party can choose both records it compares.
/// It serializes as a CBOR byte string (17 bytes), not a 16-element array.
/// The hash, width, kept bytes and encoding are wire format: pinned by
/// `evidence_digest_golden_vector`; changing any of them re-keys the contract.
/// See `.claude/rules/contract-summary-determinism.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EvidenceDigest(pub [u8; 16]);

impl Serialize for EvidenceDigest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for EvidenceDigest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = <Vec<u8>>::deserialize(deserializer)?;
        let arr: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
            serde::de::Error::custom(format!(
                "expected 16-byte EvidenceDigest, got {} bytes",
                bytes.len()
            ))
        })?;
        Ok(EvidenceDigest(arr))
    }
}

/// The [`EvidenceDigest`] of `record`.
pub fn evidence_digest(record: &AuthorizedMember) -> EvidenceDigest {
    let full = crate::util::record_digest(record);
    let mut out = [0u8; 16];
    out.copy_from_slice(&full[..16]);
    EvidenceDigest(out)
}

/// The deepest invite chain a ban-evidence record may have, counted in
/// records from the owner (a member the owner invited is at depth 1). A work
/// bound, not a room rule: every evidence record costs a chain check in each
/// apply and verify, and a stored ban references at most two chains, so a
/// state holds at most `2 x max_user_bans x depth` evidence records. Cleanup
/// keeps `member_info` for at most `depth - 1` evidence ancestors per ban,
/// but verify accepts a `member_info` for any evidence id, so a crafted state
/// at 200 bans and depth 4 holds 1600 evidence and 1600 `member_info`
/// records: about 3400 signature checks with the bans. In the node's WASM
/// runtime 2000 such checks validated in 0.64-1.0 s, so that worst case is
/// about 1.1-1.7 s, inside the 5 s compute limit. Native clients built with
/// `opt-level = "z"` (riverctl) are about 5x slower: 5.5 s measured at 2400
/// checks, so about 7.5 s at 3400. The deepest chain in the Official room's
/// states is 3.
pub const MAX_EVIDENCE_DEPTH: usize = 4;

/// The evidence depth cap for a room: `min(max_members, MAX_EVIDENCE_DEPTH)`.
pub fn evidence_depth_cap(max_members: usize) -> usize {
    max_members.min(MAX_EVIDENCE_DEPTH)
}

/// The most ban evidence a room's configuration allows:
/// `2 x max_user_bans x cap`, saturating so a huge `max_user_bans` cannot wrap
/// (a wrapped bound would reject every evidence record on 32-bit WASM).
pub fn evidence_bound(max_user_bans: usize, cap: usize) -> usize {
    max_user_bans.saturating_mul(2).saturating_mul(cap)
}

/// Whether `record`'s invite chain, read through `known`, reaches the owner
/// within `cap` records. A chain that leaves `known` before reaching the
/// owner does not. Walks at most `cap` links.
pub(crate) fn within_depth(
    record: &AuthorizedMember,
    known: &HashMap<MemberId, &AuthorizedMember>,
    owner_id: MemberId,
    cap: usize,
) -> bool {
    let mut depth = 1;
    let mut inviter = record.member.invited_by;
    while inviter != owner_id {
        if depth >= cap {
            return false;
        }
        match known.get(&inviter) {
            Some(m) => inviter = m.member.invited_by,
            None => return false,
        }
        depth += 1;
    }
    depth <= cap
}

/// Every member id a stored non-owner ban references: its issuer and target,
/// and their invite ancestors, as far as `inviters` knows them. The set
/// `post_apply_cleanup` step 4a keeps evidence for, and the only evidence
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

    /// This evidence without the records deeper than `cap`
    /// ([`within_depth`], read through members and this field), or `None` if
    /// every record is within it. Only a lowered `max_members` can leave a
    /// stored record too deep: apply, cleanup and `verify` never keep one.
    pub fn without_too_deep(
        &self,
        members: &MembersV1,
        owner_id: MemberId,
        cap: usize,
    ) -> Option<BanEvidenceV1> {
        let known = self.lookup(members, owner_id);
        let keep = |m: &&AuthorizedMember| {
            !known.contains_key(&m.member.id()) || within_depth(m, &known, owner_id, cap)
        };
        if self.members.iter().all(|m| keep(&m)) {
            return None;
        }
        Some(BanEvidenceV1 {
            members: self.members.iter().filter(keep).cloned().collect(),
        })
    }

    /// Every record ban resolution may read: `members`, then each evidence
    /// record whose inviter is the owner or is itself in the map. A record
    /// whose chain no longer reaches the owner (an ancestor was lost, for
    /// example to the `max_members` trim, which runs before cleanup) is left
    /// out for every reader at once, and `post_apply_cleanup` drops it.
    ///
    /// Linear in the records: a walk down from the owner and the members
    /// through the evidence records each one invited. Evidence holds at most
    /// one record per id (`verify`).
    pub fn lookup<'a>(
        &'a self,
        members: &'a MembersV1,
        owner_id: MemberId,
    ) -> HashMap<MemberId, &'a AuthorizedMember> {
        #[cfg(test)]
        tests::LOOKUP_CALLS.with(|c| c.set(c.get() + 1));
        let mut map = members.members_by_member_id();
        let mut invited: HashMap<MemberId, Vec<&'a AuthorizedMember>> = HashMap::new();
        for m in self
            .members
            .iter()
            .filter(|m| !map.contains_key(&m.member.id()))
        {
            invited.entry(m.member.invited_by).or_default().push(m);
        }
        // Seeded in stored order, not map order, so the walk (and with it the
        // record kept when two share an id) is the same on every call.
        let mut reached: Vec<MemberId> = std::iter::once(owner_id)
            .chain(members.members.iter().map(|m| m.member.id()))
            .collect();
        while let Some(inviter) = reached.pop() {
            for m in invited.remove(&inviter).unwrap_or_default() {
                #[cfg(test)]
                tests::LOOKUP_STEPS.with(|c| c.set(c.get() + 1));
                let id = m.member.id();
                if let std::collections::hash_map::Entry::Vacant(slot) = map.entry(id) {
                    slot.insert(m);
                    reached.push(id);
                }
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
    /// Id -> record digest. BTreeMap so the summary bytes are deterministic.
    type Summary = BTreeMap<MemberId, EvidenceDigest>;
    type Delta = Vec<AuthorizedMember>;
    type Parameters = ChatRoomParametersV1;

    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        let owner_id = parameters.owner_id();
        let config = &parent_state.configuration.configuration;
        let cap = evidence_depth_cap(config.max_members);
        // Every check that needs no signature runs first, so the signature
        // work below is bounded by the room's configuration whatever a full
        // state carries. The count bound is honest-safe by construction:
        // every record must be referenced by a stored non-owner ban (its
        // issuer's or target's invite chain) and lie within `cap` invites of
        // the owner, so each ban accounts for at most `2 x cap` records, and
        // `BansV1::verify` holds the bans to `max_user_bans`.
        let bound = evidence_bound(config.max_user_bans, cap);
        if self.members.len() > bound {
            return Err(format!(
                "Ban evidence holds {} records, more than {bound}",
                self.members.len()
            ));
        }
        let present = parent_state.members.members_by_member_id();
        let mut seen = HashSet::new();
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
        }
        let lookup = self.lookup(&parent_state.members, owner_id);
        if let Some(m) = self
            .members
            .iter()
            .find(|m| !within_depth(m, &lookup, owner_id, cap))
        {
            return Err(format!(
                "Ban evidence record {:?} is deeper than {cap} invites",
                m.member.id()
            ));
        }
        // Every record is one a stored ban needs, as `post_apply_cleanup`
        // keeps them, so the field is bounded by the stored bans.
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
        self.verify_records(parent_state, parameters)
    }

    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        self.members
            .iter()
            .map(|m| (m.member.id(), evidence_digest(m)))
            .collect()
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
            .filter(|m| old_state_summary.get(&m.member.id()) != Some(&evidence_digest(m)))
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
            // A bound on the work one delta can cause: no state this room's
            // configuration allows holds more evidence (see `verify`), so a
            // delta derived from one carries no more. Only records this peer
            // does not already hold count, so a re-delivery is free, and the
            // bound takes the bans floor (`MIN_BANS_DELTA_BOUND`), so a peer
            // still on a higher `max_user_bans` is not refused once this one
            // has applied a lowered cap (freenet/river#703 defect 2). A peer
            // holding more than even the floor allows is refused until it
            // takes the new configuration, as `BansV1::apply_delta` refuses
            // its ban delta.
            let config = &parent_state.configuration.configuration;
            let bound = evidence_bound(
                config
                    .max_user_bans
                    .max(crate::room_state::ban::MIN_BANS_DELTA_BOUND),
                evidence_depth_cap(config.max_members),
            );
            // Refused before any record is hashed when it cannot be within
            // the bound however many records it re-delivers.
            if delta.len() > self.members.len().saturating_add(bound) {
                return Err(format!(
                    "Ban evidence delta of {} exceeds {bound}",
                    delta.len()
                ));
            }
            let held_records: HashSet<(MemberId, EvidenceDigest)> = self
                .members
                .iter()
                .map(|m| (m.member.id(), evidence_digest(m)))
                .collect();
            let new_records = delta
                .iter()
                .filter(|m| !held_records.contains(&(m.member.id(), evidence_digest(m))))
                .count();
            if new_records > bound {
                return Err(format!(
                    "Ban evidence delta of {new_records} new records exceeds {bound}"
                ));
            }
            // Only records a stored ban references can be kept (see `verify`;
            // cleanup step 4a drops the rest), so the others are dropped here
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
            let depth_cap =
                evidence_depth_cap(parent_state.configuration.configuration.max_members);
            let added = {
                let mut combined = self.lookup(&parent_state.members, owner_id);
                for m in delta {
                    combined.entry(m.member.id()).or_insert(m);
                }
                // A held record competes only while its own chain still
                // reaches the owner within the cap through the records known
                // here; one that no longer does (its inviter was removed by
                // this merge) is replaced by any incoming record that does,
                // as cleanup would drop it anyway. Otherwise a peer holding
                // the stale record and a peer holding the resolvable one
                // ended a merge differently (freenet/river#703 review).
                let held: HashMap<MemberId, EvidenceDigest> = self
                    .members
                    .iter()
                    .filter(|m| within_depth(m, &combined, owner_id, depth_cap))
                    .map(|m| (m.member.id(), evidence_digest(m)))
                    .collect();
                // Two valid records for one id resolve the same way on every
                // peer, whatever order they arrive in: of those whose chains
                // resolve, the lower digest wins. Per id only the
                // lowest-digest record that passes the cheap checks is
                // considered, and only if it would replace what is held, so an
                // id costs at most one chain check. If it fails, a peer holding
                // a valid record offers it on the next sync.
                let mut best: BTreeMap<MemberId, (EvidenceDigest, &AuthorizedMember)> =
                    BTreeMap::new();
                for m in delta {
                    let id = m.member.id();
                    if id == owner_id
                        || !referenced.contains(&id)
                        || present.contains_key(&id)
                        || !within_depth(m, &combined, owner_id, depth_cap)
                    {
                        continue;
                    }
                    let digest = evidence_digest(m);
                    if held.get(&id).is_some_and(|h| *h <= digest) {
                        continue;
                    }
                    if best.get(&id).is_none_or(|(d, _)| digest < *d) {
                        best.insert(id, (digest, m));
                    }
                }
                let mut verified = HashSet::new();
                let mut failed = HashSet::new();
                let mut added = Vec::new();
                for (_, (_, m)) in best {
                    if MembersV1::verify_invite_chain_memo(
                        m,
                        parameters,
                        &combined,
                        &mut verified,
                        Some(&mut failed),
                    )
                    .is_ok()
                    {
                        added.push(m.clone());
                    }
                }
                added
            };
            let replaced: HashSet<MemberId> = added.iter().map(|m| m.member.id()).collect();
            self.members.retain(|m| !replaced.contains(&m.member.id()));
            self.members.extend(added);
        }
        self.members
            .retain(|m| !present.contains_key(&m.member.id()));
        self.members.sort_by_key(|m| m.member.id());
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeSet;

    thread_local! {
        /// Calls to `BanEvidenceV1::lookup`, and evidence records it examined.
        pub(crate) static LOOKUP_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        pub(crate) static LOOKUP_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
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

    /// `apply_delta` with every record's full chain walked (no memo): the
    /// same filters and the same canonical choice per id.
    fn apply_walking_every_chain(
        evidence: &mut BanEvidenceV1,
        parent_state: &ChatRoomStateV1,
        parameters: &ChatRoomParametersV1,
        delta: &[AuthorizedMember],
        eligible: impl Fn(&AuthorizedMember, &HashMap<MemberId, &AuthorizedMember>) -> bool,
    ) {
        let owner_id = parameters.owner_id();
        let present = parent_state.members.members_by_member_id();
        let mut combined = evidence.lookup(&parent_state.members, parameters.owner_id());
        for m in delta {
            combined.entry(m.member.id()).or_insert(m);
        }
        // Per id, the lowest-digest eligible record, if it is lower than the
        // held one (a held record whose chain no longer resolves within the
        // cap does not count); then a full walk of its chain.
        let cap = evidence_depth_cap(parent_state.configuration.configuration.max_members);
        let held: HashMap<MemberId, EvidenceDigest> = evidence
            .members
            .iter()
            .filter(|m| within_depth(m, &combined, owner_id, cap))
            .map(|m| (m.member.id(), evidence_digest(m)))
            .collect();
        let mut best: BTreeMap<MemberId, (EvidenceDigest, &AuthorizedMember)> = BTreeMap::new();
        for m in delta {
            let id = m.member.id();
            if id == owner_id || !eligible(m, &combined) || present.contains_key(&id) {
                continue;
            }
            let digest = evidence_digest(m);
            if held.get(&id).is_some_and(|h| *h <= digest) {
                continue;
            }
            if best.get(&id).is_none_or(|(d, _)| digest < *d) {
                best.insert(id, (digest, m));
            }
        }
        let mut added = Vec::new();
        for (_, (_, m)) in best {
            if parent_state
                .members
                .get_invite_chain_with_lookup(m, parameters, &combined)
                .is_ok()
            {
                added.push(m.clone());
            }
        }
        let replaced: HashSet<MemberId> = added.iter().map(|m| m.member.id()).collect();
        evidence
            .members
            .retain(|m| !replaced.contains(&m.member.id()));
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
                let mut expected = BanEvidenceV1 {
                    members: held.clone(),
                };
                // The full walk, filtered as the apply filters: records a ban
                // references, within the depth cap.
                let cap = evidence_depth_cap(parent_state.configuration.configuration.max_members);
                apply_walking_every_chain(
                    &mut expected,
                    &parent_state,
                    &parameters,
                    &delta,
                    |m, combined| {
                        referenced.contains(&m.member.id())
                            && within_depth(m, combined, owner_id, cap)
                    },
                );
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

    /// `lookup` as it was before it became a single walk: rescan the pending
    /// records until a pass admits none.
    fn lookup_by_rescanning<'a>(
        evidence: &'a BanEvidenceV1,
        members: &'a MembersV1,
        owner_id: MemberId,
    ) -> HashMap<MemberId, &'a AuthorizedMember> {
        let mut map = members.members_by_member_id();
        let mut pending: Vec<&AuthorizedMember> = evidence
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

    /// `evidence_digest` pinned to one fixed record: the hash, the width, the
    /// kept bytes and the CBOR byte-string encoding are wire format.
    #[test]
    fn evidence_digest_golden_vector() {
        let owner = SigningKey::from_bytes(&[1u8; 32]);
        let member = SigningKey::from_bytes(&[2u8; 32]);
        let owner_id: MemberId = owner.verifying_key().into();
        let mut raw = [0u8; 64];
        for (i, b) in raw.iter_mut().enumerate() {
            *b = i as u8;
        }
        let record = AuthorizedMember::with_signature(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: member.verifying_key(),
            },
            ed25519_dalek::Signature::from_bytes(&raw),
        );
        let digest = evidence_digest(&record);
        let mut encoded = Vec::new();
        ciborium::ser::into_writer(&digest, &mut encoded).unwrap();
        const EXPECTED: [u8; 16] = [
            0x51, 0x56, 0xe2, 0x67, 0xa0, 0x30, 0xa8, 0x26, 0x99, 0x76, 0xdc, 0x87, 0x02, 0x01,
            0x2a, 0xa6,
        ];
        assert_eq!(
            digest.0, EXPECTED,
            "evidence_digest changed: a wire-format change"
        );
        let mut expected_encoding = vec![0x50];
        expected_encoding.extend_from_slice(&EXPECTED);
        assert_eq!(
            encoded, expected_encoding,
            "encoded as a 16-byte CBOR byte string"
        );
        let mut reversed = EXPECTED;
        reversed.reverse();
        assert_ne!(reversed, EXPECTED);
    }

    /// The single-walk `lookup` returns the same map as rescanning, on random
    /// trees split between `members` and the evidence, with a record removed
    /// (so its subtree is unreachable) or its inviter pointed at itself.
    #[test]
    fn lookup_matches_rescanning() {
        use rand::{Rng, SeedableRng};
        for seed in 0..200u64 {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let owner_sk = SigningKey::generate(&mut rng);
            let owner_id: MemberId = owner_sk.verifying_key().into();
            let n = rng.gen_range(1..30);
            let (mut records, _) = random_tree(&mut rng, &owner_sk, n);
            let i = rng.gen_range(0..records.len());
            match seed % 3 {
                0 => {}
                1 => {
                    records.remove(i);
                }
                _ => records[i].member.invited_by = records[i].member.id(),
            }
            let (present, held): (Vec<_>, Vec<_>) =
                records.into_iter().partition(|_| rng.gen_bool(0.3));
            let members = MembersV1 { members: present };
            let mut evidence = BanEvidenceV1 { members: held };
            evidence.members.sort_by_key(|m| m.member.id());
            assert_eq!(
                evidence.lookup(&members, owner_id),
                lookup_by_rescanning(&evidence, &members, owner_id),
                "seed {seed}"
            );
        }
    }

    /// A chain held in id order, which rescanning admitted one record per pass
    /// when the ids descend down the chain (quadratic), is admitted in one
    /// walk that examines each record once, in either id order.
    #[test]
    fn lookup_admits_a_deep_chain_in_one_walk() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let n = 2_000;
        let mut records = chain(&owner_sk, n);
        for descending in [false, true] {
            records.sort_by_key(|m| m.member.id());
            if descending {
                records.reverse();
            }
            let evidence = BanEvidenceV1 {
                members: records.clone(),
            };
            let members = MembersV1::default();
            let before = LOOKUP_STEPS.with(|c| c.get());
            let map = evidence.lookup(&members, owner_id);
            assert_eq!(map.len(), n);
            assert_eq!(LOOKUP_STEPS.with(|c| c.get()) - before, n);
        }
    }

    /// `MemberInfoV1::apply_delta` builds the lookup a fixed number of times
    /// per delta, not once per record.
    #[test]
    fn member_info_apply_builds_the_lookup_once() {
        use crate::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let mut members = Vec::new();
        let mut infos = Vec::new();
        for _ in 0..20 {
            let (member, sk) = create_test_member(owner_id, owner_id);
            infos.push(AuthorizedMemberInfo::new_with_member_key(
                MemberInfo::new_public(member.id(), 1, "n".into()),
                &sk,
            ));
            members.push(AuthorizedMember::new(member, &owner_sk));
        }
        let parent_state = state_with(&owner_sk, 10, members);
        let calls = |delta: Vec<AuthorizedMemberInfo>| {
            let mut info = MemberInfoV1::default();
            let before = LOOKUP_CALLS.with(|c| c.get());
            info.apply_delta(&parent_state, &parameters, &Some(delta))
                .unwrap();
            LOOKUP_CALLS.with(|c| c.get()) - before
        };
        assert_eq!(calls(infos[..1].to_vec()), calls(infos.clone()));
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

    /// Each link of a chain held as evidence is checked once by
    /// `verify_records`, and each link of a chain at the depth cap added in
    /// one delta, in either order, once by `apply_delta`.
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

        // `apply_delta` keeps records within the depth cap only.
        let n = MAX_EVIDENCE_DEPTH;
        let records = chain(&owner_sk, n);
        parent_state.bans = bans_naming([records[n - 1].member.id()], parameters.owner_id());
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
    /// referenced chain in it (at the depth cap) costs one check per link.
    #[test]
    fn unreferenced_records_cost_no_chain_checks() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let mut parent_state = state_with(&owner_sk, 200, Vec::new());
        let mut delta: Vec<AuthorizedMember> = chain(&owner_sk, 500);
        let referenced_chain = chain(&owner_sk, MAX_EVIDENCE_DEPTH);
        parent_state.bans = bans_naming(
            [referenced_chain[MAX_EVIDENCE_DEPTH - 1].member.id()],
            owner_id,
        );

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
        assert_eq!(invite_signature_checks() - before, MAX_EVIDENCE_DEPTH);
        let mut expected = referenced_chain;
        expected.sort_by_key(|m| m.member.id());
        assert_eq!(evidence.members, expected);
    }

    /// The work bound `MAX_EVIDENCE_DEPTH` documents: 200 bans, each
    /// referencing two disjoint chains at the depth cap, is 1600 evidence
    /// records, and `verify` checks each record's signature once.
    #[test]
    fn evidence_at_the_work_bound_costs_one_check_per_record() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let bans = 200;
        let mut state = state_with(&owner_sk, bans, Vec::new());
        let mut records = Vec::new();
        let mut stored = Vec::new();
        for _ in 0..bans {
            let issuer = chain(&owner_sk, MAX_EVIDENCE_DEPTH);
            let target = chain(&owner_sk, MAX_EVIDENCE_DEPTH);
            let mut ban = bans_naming([target[MAX_EVIDENCE_DEPTH - 1].member.id()], owner_id)
                .0
                .remove(0);
            ban.banned_by = issuer[MAX_EVIDENCE_DEPTH - 1].member.id();
            stored.push(ban);
            records.extend(issuer);
            records.extend(target);
        }
        records.sort_by_key(|m| m.member.id());
        state.bans = BansV1(stored);
        state.ban_evidence = BanEvidenceV1 { members: records };
        assert_eq!(
            state.ban_evidence.members.len(),
            2 * bans * MAX_EVIDENCE_DEPTH
        );
        let before = invite_signature_checks();
        state.ban_evidence.verify(&state, &parameters).unwrap();
        assert_eq!(
            invite_signature_checks() - before,
            2 * bans * MAX_EVIDENCE_DEPTH
        );
    }

    /// `verify` rejects evidence over its count bound, or holding a record no
    /// stored ban references, before checking any signature.
    #[test]
    fn verify_rejects_unreferenced_or_excess_evidence_before_any_signature() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let mut state = state_with(&owner_sk, 10, Vec::new());
        let record = || AuthorizedMember::new(create_test_member(owner_id, owner_id).0, &owner_sk);
        // 10 bans x cap 4 x 2 = 80.
        for (count, reason) in [(2_000, "more than 80"), (50, "not referenced")] {
            let mut members: Vec<AuthorizedMember> = (0..count).map(|_| record()).collect();
            members.sort_by_key(|m| m.member.id());
            state.ban_evidence = BanEvidenceV1 { members };
            let before = invite_signature_checks();
            let err = state.ban_evidence.verify(&state, &parameters).unwrap_err();
            assert!(err.contains(reason), "{count}: {err}");
            assert_eq!(invite_signature_checks() - before, 0, "{count}");
        }
    }

    /// Many distinct records for one referenced id cost one chain check,
    /// whether or not it verifies.
    #[test]
    fn repeated_records_for_one_id_cost_one_check() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let mut parent_state = state_with(&owner_sk, 200, Vec::new());
        let (member, _) = create_test_member(owner_id, owner_id);
        parent_state.bans = bans_naming([member.id()], owner_id);
        // 100 distinct records for one id, each signed by a different wrong
        // key.
        let bad: Vec<AuthorizedMember> = (0..100)
            .map(|_| {
                let other = SigningKey::generate(&mut OsRng);
                AuthorizedMember::with_signature(
                    member.clone(),
                    crate::util::sign_struct(&member, &other),
                )
            })
            .collect();
        assert_eq!(
            bad.iter().collect::<HashSet<_>>().len(),
            100,
            "precondition: the records are distinct"
        );
        let mut evidence = BanEvidenceV1::default();
        let before = invite_signature_checks();
        evidence
            .apply_delta(&parent_state, &parameters, &Some(bad))
            .unwrap();
        assert_eq!(invite_signature_checks() - before, 1);
        assert!(evidence.members.is_empty());
    }

    /// The bound saturates rather than wrapping at a huge `max_user_bans`, so
    /// evidence is still accepted.
    #[test]
    fn the_evidence_bound_saturates() {
        assert_eq!(evidence_bound(usize::MAX, 4), usize::MAX);
        assert_eq!(evidence_bound(usize::MAX / 4, 4), usize::MAX);
        assert_eq!(evidence_bound(200, 4), 1_600);
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let records = chain(&owner_sk, 2);
        let mut state = state_with(&owner_sk, usize::MAX, Vec::new());
        state.bans = bans_naming([records[1].member.id()], owner_id);
        let mut evidence = BanEvidenceV1::default();
        evidence
            .apply_delta(&state, &parameters, &Some(records))
            .unwrap();
        assert_eq!(evidence.members.len(), 2);
        state.ban_evidence = evidence;
        state.ban_evidence.verify(&state, &parameters).unwrap();
    }

    /// An evidence delta over `2 x max(max_user_bans, MIN_BANS_DELTA_BOUND) x
    /// depth cap` NEW records is refused before any check. Records already
    /// held do not count (freenet/river#703).
    #[test]
    fn an_evidence_delta_over_the_bound_is_refused() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let parent_state = state_with(&owner_sk, 10, Vec::new());
        let bound = evidence_bound(crate::room_state::ban::MIN_BANS_DELTA_BOUND, 4);
        assert_eq!(bound, 800, "premise: the floor, not max_user_bans = 10");
        let record = || AuthorizedMember::new(create_test_member(owner_id, owner_id).0, &owner_sk);
        let mut evidence = BanEvidenceV1::default();
        let at_bound: Vec<AuthorizedMember> = (0..bound).map(|_| record()).collect();
        evidence
            .apply_delta(&parent_state, &parameters, &Some(at_bound))
            .unwrap();
        let over: Vec<AuthorizedMember> = (0..bound + 1).map(|_| record()).collect();
        let before = invite_signature_checks();
        let err = evidence
            .apply_delta(&parent_state, &parameters, &Some(over))
            .unwrap_err();
        assert!(err.contains(&format!("exceeds {bound}")), "{err}");
        assert_eq!(invite_signature_checks() - before, 0);

        // Held records do not count toward the bound.
        let mut held = BanEvidenceV1 {
            members: (0..bound).map(|_| record()).collect(),
        };
        let mut redelivery = held.members.clone();
        redelivery.push(record());
        held.apply_delta(&parent_state, &parameters, &Some(redelivery))
            .expect("re-delivered records must not count toward the bound");
    }

    /// Records whose chains all run through one record that fails are
    /// checked once each, and the failing record once: a failed walk is
    /// remembered.
    #[test]
    fn a_failing_shared_ancestor_is_checked_once() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let mut parent_state = state_with(&owner_sk, 200, Vec::new());
        let (a, a_sk) = create_test_member(owner_id, owner_id);
        let other = SigningKey::generate(&mut OsRng);
        let bad_a =
            AuthorizedMember::with_signature(a.clone(), crate::util::sign_struct(&a, &other));
        let n = 50;
        let invitees: Vec<AuthorizedMember> = (0..n)
            .map(|_| AuthorizedMember::new(create_test_member(owner_id, a.id()).0, &a_sk))
            .collect();
        parent_state.bans = bans_naming(invitees.iter().map(|m| m.member.id()), owner_id);
        let mut delta = vec![bad_a];
        delta.extend(invitees);
        let mut evidence = BanEvidenceV1::default();
        let before = invite_signature_checks();
        evidence
            .apply_delta(&parent_state, &parameters, &Some(delta))
            .unwrap();
        assert_eq!(invite_signature_checks() - before, n + 1);
        assert!(evidence.members.is_empty());
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
