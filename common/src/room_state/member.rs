use crate::room_state::ban::{BanId, BansV1};
use crate::room_state::ban_evidence::{evidence_digest, BanEvidenceV1, EvidenceDigest};
use crate::room_state::member_info::MemberInfoV1;
use crate::room_state::ChatRoomParametersV1;
use crate::util::{sign_struct, truncated_base32, verify_struct};
use crate::ChatRoomStateV1;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use freenet_scaffold::util::{fast_hash, FastHash};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fmt::{Debug, Display};
use std::hash::{Hash, Hasher};

/*
Note that the owner should not be in the members list but for most purposes (eg. sending messages)
they should be treated as if they are in the list. The reason is to avoid storing the owner's
VerifyingKey twice because it's already stored in the ChatRoomParametersV1.
*/

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug, Default)]
pub struct MembersV1 {
    pub members: Vec<AuthorizedMember>,
}

impl ComposableState for MembersV1 {
    type ParentState = ChatRoomStateV1;
    // BTreeSet (not HashSet) so the ciborium-serialized summary bytes are
    // deterministic: freenet-core byte-compares `summarize_state` output for
    // staleness, and a HashSet iterates in a per-process-random order, making
    // two identical member sets summarize to different bytes → spurious
    // anti-entropy heals. See `.claude/rules/contract-summary-determinism.md`
    // and freenet/freenet-core#4857.
    //
    // Ids only: a peer keeps the record it holds for a member (see
    // `MembersV1::resolve_records`), so a different record for a held id is
    // never offered or taken, and the summary need not tell two apart.
    type Summary = BTreeSet<MemberId>;
    type Delta = MembersDelta;
    type Parameters = ChatRoomParametersV1;

    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        if self.members.is_empty() {
            return Ok(());
        }

        if self.members.len() > parent_state.configuration.configuration.max_members {
            return Err(format!(
                "Too many members: {} > {}",
                self.members.len(),
                parent_state.configuration.configuration.max_members
            ));
        }

        let owner_id = parameters.owner_id();
        let members_by_id = self.members_by_member_id();
        let mut verified = HashSet::new();

        for member in &self.members {
            if member.member.id() == owner_id {
                return Err("Owner should not be included in the members list".to_string());
            }
            if member.member.member_vk == parameters.owner {
                return Err(
                    "Member cannot have the same verifying key as the room owner".to_string(),
                );
            }
            if member.member.invited_by == member.member.id() {
                return Err("Self-invitation detected".to_string());
            }

            // Verify the full invite chain with Ed25519 signature checks;
            // see `verify_invite_chain` for which walks stop early.
            Self::verify_invite_chain(member, parameters, &members_by_id, &mut verified)?;
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
        let added = self
            .members
            .iter()
            .filter(|m| !old_state_summary.contains(&m.member.id()))
            .cloned()
            .collect::<Vec<_>>();
        if added.is_empty() {
            None
        } else {
            Some(MembersDelta { added })
        }
    }

    fn apply_delta(
        &mut self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        let max_members = parent_state.configuration.configuration.max_members;

        // One record per id among those held, on every apply (see
        // `normalize_held_records`).
        self.normalize_held_records(parameters);

        if let Some(delta) = delta {
            // A bound on the compute work one delta can cause, not a room
            // rule: each added member costs a signature check, and a delta
            // much past `MIN_MEMBERS_DELTA_BOUND` members would exceed the
            // contract's compute limit anyway. Deltas larger than
            // `max_members` within the bound are accepted and trimmed below.
            // A peer still holding a larger set from before a big
            // `max_members` reduction can produce a delta over the bound; it
            // is rejected only until that peer receives the new
            // configuration and trims its own set.
            //
            // Only NEW records count: a record for an id this peer already
            // holds is skipped before anything else (see `resolve_records`),
            // so a delta computed against a stale summary of this peer, which
            // re-delivers held members, costs nothing and is not refused over
            // them (freenet/river#703 defect 2). A raw length past held +
            // bound is refused first.
            //
            // The owner is never in the members list (`verify` rejects a
            // record with the owner's id). Such a record in a delta is ignored
            // here rather than failing the delta, so the rest of the delta
            // applies the same way whatever else it carries.
            let work_bound = members_work_bound(max_members);
            if !crate::room_state::work_bounds::lifted()
                && delta.added.len() > self.members.len().saturating_add(work_bound)
            {
                return Err(format!(
                    "Members delta of {} exceeds {}",
                    delta.added.len(),
                    work_bound
                ));
            }
            let owner_id = parameters.owner_id();
            let held: HashSet<MemberId> = self.members.iter().map(|m| m.member.id()).collect();
            let mut in_delta = HashSet::new();
            let added: Vec<&AuthorizedMember> = delta
                .added
                .iter()
                .filter(|m| m.member.id() != owner_id)
                .filter(|m| !held.contains(&m.member.id()) && in_delta.insert(evidence_digest(m)))
                .collect();
            if !crate::room_state::work_bounds::lifted() && added.len() > work_bound {
                return Err(format!(
                    "Members delta of {} new members exceeds {}",
                    added.len(),
                    work_bound
                ));
            }
            if !added.is_empty() {
                self.members = Self::resolve_records(&self.members, &added, parameters)?;
            }
        }

        // Remove members the owner has banned now, so the `max_members` trim
        // below does not count them. Everything else waits for
        // `post_apply_cleanup` step 0, which has the converged deputy grants
        // (`member_info` applies after this field). See
        // `remove_owner_banned_members` for why this cannot change step 0's
        // answer (#410, freenet/river#423, #702).
        self.remove_owner_banned_members(&parent_state.bans, parameters);

        // Always enforce max members limit
        self.remove_excess_members(parameters, max_members);

        // Sort for deterministic ordering (CRDT convergence requirement)
        self.members.sort_by_key(|m| m.member.id());

        Ok(())
    }
}

/// One delta may add `max(max_members, MIN_MEMBERS_DELTA_BOUND)` members.
/// See `MembersV1::apply_delta`.
const MIN_MEMBERS_DELTA_BOUND: usize = 1_000;

/// How many new members one delta may add in a room with `max_members`.
pub fn members_work_bound(max_members: usize) -> usize {
    max_members.max(MIN_MEMBERS_DELTA_BOUND)
}

impl MembersV1 {
    /// The member records to keep: every `held` one, plus the `added` ones
    /// this room admits. `added` holds no record for a held id: the caller
    /// skips those: a held record is kept, and a record naming a different
    /// inviter or a different key for a held id is skipped. Two peers that
    /// each hold a different record for one member therefore each keep their
    /// own when they merge.
    ///
    /// The added records are placed by a breadth-first walk from the owner
    /// through the held and added records: each new member keeps a record at
    /// its least invite depth, and of several at that depth the one with the
    /// lower record digest, so the result does not depend on the order the
    /// records arrived in. A record that is never reached (its inviter is
    /// unknown here, or it only loops back on itself) is skipped rather than
    /// failing the delta: a delta computed against a stale summary can omit
    /// an inviter this peer has since pruned, and the next delta re-sends the
    /// record with its inviter (freenet/river#703 defect 2). Each record is
    /// visited once, so the work is linear in the records. Every reached
    /// added record has its signature checked against its inviter's key, and
    /// a bad one fails the delta. Held records are kept as they are, reached
    /// or not.
    fn resolve_records(
        held: &[AuthorizedMember],
        added: &[&AuthorizedMember],
        parameters: &ChatRoomParametersV1,
    ) -> Result<Vec<AuthorizedMember>, String> {
        let owner_id = parameters.owner_id();
        let mut held_by_inviter: HashMap<MemberId, Vec<&AuthorizedMember>> = HashMap::new();
        for m in held {
            held_by_inviter
                .entry(m.member.invited_by)
                .or_default()
                .push(m);
        }
        // Of new records for one id that name different keys, only those
        // naming the key of the LAST one in the delta compete: a delta lists
        // a sender's records in the order it stores them, and that last
        // record is the one the sender's `verify` reads and its
        // normalization keeps (see `normalize_held_records`), so this peer
        // keeps the record the sender keeps.
        let mut sender_key: HashMap<MemberId, VerifyingKey> = HashMap::new();
        for m in added {
            sender_key.insert(m.member.id(), m.member.member_vk);
        }
        let mut added_by_inviter: HashMap<MemberId, Vec<&AuthorizedMember>> = HashMap::new();
        for m in added
            .iter()
            .filter(|m| sender_key.get(&m.member.id()) == Some(&m.member.member_vk))
        {
            added_by_inviter
                .entry(m.member.invited_by)
                .or_default()
                .push(m);
        }
        let held_ids: HashSet<MemberId> = held.iter().map(|m| m.member.id()).collect();
        // Every key some held or added record names for each id.
        let mut by_id_keys: HashMap<MemberId, Vec<VerifyingKey>> = HashMap::new();
        for m in held.iter().chain(added.iter().copied()) {
            let keys = by_id_keys.entry(m.member.id()).or_default();
            if !keys.contains(&m.member.member_vk) {
                keys.push(m.member.member_vk);
            }
        }
        let mut reached: HashSet<MemberId> = HashSet::from([owner_id]);
        let mut kept: BTreeMap<MemberId, &AuthorizedMember> = BTreeMap::new();
        let mut frontier: Vec<(MemberId, VerifyingKey)> = vec![(owner_id, parameters.owner)];
        while !frontier.is_empty() {
            let mut next: Vec<(MemberId, VerifyingKey)> = Vec::new();
            let mut level: BTreeMap<MemberId, (EvidenceDigest, &AuthorizedMember)> =
                BTreeMap::new();
            for (inviter_id, inviter_vk) in &frontier {
                for record in held_by_inviter.get(inviter_id).into_iter().flatten() {
                    #[cfg(test)]
                    tests::count_chain_step();
                    let id = record.member.id();
                    if reached.insert(id) {
                        next.push((id, record.member.member_vk));
                    }
                }
                for record in added_by_inviter.get(inviter_id).into_iter().flatten() {
                    #[cfg(test)]
                    tests::count_chain_step();
                    let id = record.member.id();
                    if id == owner_id || held_ids.contains(&id) {
                        continue;
                    }
                    // Checked even when the member already has a record kept
                    // at a lesser depth, so whether a delta fails does not
                    // depend on which records it competes with.
                    #[cfg(test)]
                    tests::count_invite_signature_check();
                    if let Err(e) = record.verify_signature(inviter_vk) {
                        // Signed by the key another record for its inviter's
                        // id names: skipped, as a record whose inviter record
                        // this peer does not keep. Otherwise the delta
                        // fails.
                        let other_key_signed = by_id_keys
                            .get(inviter_id)
                            .into_iter()
                            .flatten()
                            .any(|vk| vk != inviter_vk && record.verify_signature(vk).is_ok());
                        if other_key_signed {
                            continue;
                        }
                        return Err(format!("Invalid signature for member {:?}: {}", id, e));
                    }
                    if reached.contains(&id) {
                        continue;
                    }
                    let digest = evidence_digest(record);
                    level
                        .entry(id)
                        .and_modify(|best| {
                            if digest < best.0 {
                                *best = (digest, record);
                            }
                        })
                        .or_insert((digest, record));
                }
            }
            for (id, (_, r)) in level {
                reached.insert(id);
                next.push((id, r.member.member_vk));
                kept.insert(id, r);
            }
            frontier = next;
        }
        let mut out = held.to_vec();
        out.extend(kept.into_values().cloned());
        Ok(out)
    }

    /// Reduce the held records to one per member id. `verify` accepts a state
    /// holding two records for one id (a full-state PUT can carry one), and
    /// without this the record such a state used, and its summary, would
    /// depend on the order the records happen to be stored in.
    ///
    /// Only records naming the key `verify` checks the state's other records
    /// against compete (the key of the last record stored for the id, as
    /// [`Self::members_by_member_id`] reads it), so normalization keeps the
    /// record `verify` checks the member's invitees against and a valid state
    /// stays valid. Among those, the record kept is the one at the least invite
    /// depth through the held records (a record whose chain does not reach
    /// the owner counts as deepest), then the lower record digest. Depth
    /// first, because the shallowest record depends on the fewest others to
    /// stay valid; the digest breaks a tie. Records for one id that name one
    /// key are kept the same way whatever order they are stored in. Applied
    /// only among duplicates one state already holds: an arriving record
    /// never replaces a held one (see `resolve_records`). A no-op, after one
    /// pass over the ids, when no id is held twice.
    fn normalize_held_records(&mut self, parameters: &ChatRoomParametersV1) {
        let mut seen = HashSet::new();
        if self.members.iter().all(|m| seen.insert(m.member.id())) {
            return;
        }
        let owner_id = parameters.owner_id();
        let mut by_inviter: HashMap<MemberId, Vec<usize>> = HashMap::new();
        for (i, m) in self.members.iter().enumerate() {
            by_inviter.entry(m.member.invited_by).or_default().push(i);
        }
        // Each record's depth: one more than the least depth of its inviter's
        // id, by a breadth-first walk over ids from the owner.
        let mut depth = vec![usize::MAX; self.members.len()];
        let mut reached: HashSet<MemberId> = HashSet::from([owner_id]);
        let mut frontier = vec![owner_id];
        let mut level = 1;
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for inviter in &frontier {
                for &i in by_inviter.get(inviter).into_iter().flatten() {
                    depth[i] = depth[i].min(level);
                    let id = self.members[i].member.id();
                    if reached.insert(id) {
                        next.push(id);
                    }
                }
            }
            frontier = next;
            level += 1;
        }
        let verify_key: HashMap<MemberId, VerifyingKey> = self
            .members
            .iter()
            .map(|m| (m.member.id(), m.member.member_vk))
            .collect();
        let mut best: BTreeMap<MemberId, (usize, EvidenceDigest, usize)> = BTreeMap::new();
        for (i, m) in self.members.iter().enumerate() {
            if verify_key.get(&m.member.id()) != Some(&m.member.member_vk) {
                continue;
            }
            let rank = (depth[i], evidence_digest(m), i);
            best.entry(m.member.id())
                .and_modify(|b| {
                    if (rank.0, rank.1) < (b.0, b.1) {
                        *b = rank;
                    }
                })
                .or_insert(rank);
        }
        let keep: HashSet<usize> = best.values().map(|b| b.2).collect();
        let mut i = 0;
        self.members.retain(|_| {
            let k = keep.contains(&i);
            i += 1;
            k
        });
    }

    /// Verify `member`'s invite chain up to the owner, with the same checks
    /// and errors as [`Self::get_invite_chain_with_lookup`].
    ///
    /// When `member` is the record `members_by_id` holds for its id, the walk
    /// stops at a record in `verified` (compared in full, signature included)
    /// whose chain an earlier call already verified against the same lookup
    /// map, and on success adds every record on the walk to `verified`. Every
    /// record past the first is a lookup result, so the walk and the earlier
    /// one meet on the same records, and stopping there gives the result the
    /// full walk would. Another record with the same id is not the lookup
    /// result: it is walked in full, as before, since the full walk also fails
    /// when its own id reappears further up the chain. Across calls sharing
    /// `verified`, each lookup record's signature is checked once.
    pub(crate) fn verify_invite_chain<'a>(
        member: &'a AuthorizedMember,
        parameters: &ChatRoomParametersV1,
        members_by_id: &HashMap<MemberId, &'a AuthorizedMember>,
        verified: &mut HashSet<&'a AuthorizedMember>,
    ) -> Result<(), String> {
        Self::verify_invite_chain_memo(member, parameters, members_by_id, verified, None)
    }

    /// [`Self::verify_invite_chain`], also remembering failures in `failed`
    /// when given: a walk that fails marks every record on it (each one's chain
    /// runs through the failing link), and a later walk reaching one of them
    /// fails at once. Only walks starting from the lookup record use either
    /// memo, so a record whose chain returns to its own id marks nothing.
    pub(crate) fn verify_invite_chain_memo<'a>(
        member: &'a AuthorizedMember,
        parameters: &ChatRoomParametersV1,
        members_by_id: &HashMap<MemberId, &'a AuthorizedMember>,
        verified: &mut HashSet<&'a AuthorizedMember>,
        mut failed: Option<&mut HashSet<&'a AuthorizedMember>>,
    ) -> Result<(), String> {
        let use_memo = members_by_id
            .get(&member.member.id())
            .is_some_and(|held| *held == member);
        let mut path_ids = HashSet::new();
        let mut path: Vec<&'a AuthorizedMember> = Vec::new();
        let mut current: &'a AuthorizedMember = member;
        let result = loop {
            if use_memo && verified.contains(current) {
                break Ok(());
            }
            if use_memo
                && failed
                    .as_ref()
                    .is_some_and(|failed| failed.contains(current))
            {
                break Err(format!(
                    "Invite chain of member {:?} failed verification",
                    current.member.id()
                ));
            }
            match Self::verify_invite_link(current, parameters, members_by_id, &mut path_ids) {
                Err(e) => {
                    path.push(current);
                    break Err(e);
                }
                Ok(None) => {
                    path.push(current);
                    break Ok(());
                }
                Ok(Some(inviter)) => {
                    path.push(current);
                    current = inviter;
                }
            }
        };
        if use_memo {
            match &result {
                Ok(()) => verified.extend(path),
                Err(_) => {
                    if let Some(failed) = failed.as_mut() {
                        failed.extend(path);
                    }
                }
            }
        }
        result
    }

    /// One link of an invite-chain walk: `current`'s checks and signature.
    /// `Ok(None)` when `current` was invited by the owner, `Ok(Some(inviter))`
    /// to continue with the inviter.
    fn verify_invite_link<'a>(
        current: &'a AuthorizedMember,
        parameters: &ChatRoomParametersV1,
        members_by_id: &HashMap<MemberId, &'a AuthorizedMember>,
        path_ids: &mut HashSet<MemberId>,
    ) -> Result<Option<&'a AuthorizedMember>, String> {
        let owner_id = parameters.owner_id();
        let id = current.member.id();
        if !path_ids.insert(id) {
            return Err(format!(
                "Circular invite chain detected for member {:?}",
                id
            ));
        }
        if current.member.invited_by == id {
            return Err(format!("Self-invitation detected for member {:?}", id));
        }
        #[cfg(test)]
        tests::count_invite_signature_check();
        if current.member.invited_by == owner_id {
            current.verify_signature(&parameters.owner).map_err(|e| {
                format!(
                    "Invalid signature for member {:?} invited by owner: {}",
                    id, e
                )
            })?;
            return Ok(None);
        }
        let inviter: &'a AuthorizedMember = members_by_id
            .get(&current.member.invited_by)
            .ok_or_else(|| {
                format!(
                    "Inviter {:?} not found for member {:?}",
                    current.member.invited_by, id
                )
            })?;
        current
            .verify_signature(&inviter.member.member_vk)
            .map_err(|e| format!("Invalid signature for member {:?}: {}", id, e))?;
        Ok(Some(inviter))
    }
}

impl MembersV1 {
    /// Returns true if the given member_id invited the target_id, properly handling both
    /// regular members and the room owner. Use this instead of checking the members list directly.
    pub fn is_inviter_of(
        &self,
        member_id: MemberId,
        target_id: MemberId,
        params: &ChatRoomParametersV1,
    ) -> bool {
        if member_id == params.owner_id() {
            // Check if target was invited by owner
            self.members
                .iter()
                .find(|m| m.member.id() == target_id)
                .map(|m| m.member.invited_by == member_id)
                .unwrap_or(false)
        } else {
            // Check regular members
            self.members
                .iter()
                .find(|m| m.member.id() == target_id)
                .map(|m| m.member.invited_by == member_id)
                .unwrap_or(false)
        }
    }

    /// Note: doesn't include owner
    pub fn members_by_member_id(&self) -> HashMap<MemberId, &AuthorizedMember> {
        self.members.iter().map(|m| (m.member.id(), m)).collect()
    }

    /// Every member id removed by a verifiable OWNER ban, over `members`
    /// alone: each target plus its invite subtree. Used by the early removal
    /// in [`ComposableState::apply_delta`]; see [`Self::resolve_bans`] step 0.
    pub fn owner_removed_ids(
        &self,
        bans_v1: &BansV1,
        parameters: &ChatRoomParametersV1,
    ) -> HashSet<MemberId> {
        owner_removed_in(
            &children_by_inviter(self.members.iter()),
            bans_v1,
            parameters,
        )
    }

    /// The early ban removal run from `MembersV1::apply_delta`, before
    /// `member_info` (deputy grants) has been applied: it removes exactly the
    /// members an owner ban removes ([`Self::owner_removed_ids`]).
    ///
    /// This cannot change what `post_apply_cleanup` step 0 decides: an
    /// owner-removed member contributes no edge to the ban graph, and step 0
    /// removes the same set from the same bans. Bans by ancestors or deputies
    /// are left to step 0, which has the converged grants.
    fn remove_owner_banned_members(&mut self, bans_v1: &BansV1, parameters: &ChatRoomParametersV1) {
        let removed = self.owner_removed_ids(bans_v1, parameters);
        if !removed.is_empty() {
            self.members.retain(|m| !removed.contains(&m.member.id()));
        }
    }

    /// The set of member ids that must be removed by the room's bans: see
    /// [`Self::resolve_bans`] for the rule. Shared by `post_apply_cleanup`
    /// step 0, the step 0-cap eviction (through `enforced_ban_set_of`) and the
    /// DM sweep, so all three read one definition.
    pub fn banned_member_ids(
        &self,
        evidence: &BanEvidenceV1,
        bans_v1: &BansV1,
        member_info: &MemberInfoV1,
        parameters: &ChatRoomParametersV1,
    ) -> HashSet<MemberId> {
        self.resolve_bans(evidence, bans_v1, member_info, parameters)
            .removed
    }

    /// Decide which bans take effect, as a pure function of the converged
    /// `(members + ban evidence + member_info + bans)` state
    /// (freenet/river#423, #702). Every peer computes the same result
    /// regardless of the order in which deltas arrived, which is why this runs
    /// from `ChatRoomStateV1::post_apply_cleanup` and NOT from `verify` (#410).
    ///
    /// # The rules (Ian, 2026-09-23 and 2026-09-24)
    ///
    /// - A ban does not take effect if its issuer is themselves removed by a
    ///   ban that does, EXCEPT in a mutual ban: every member of a mutual-ban
    ///   cycle is removed, and only they are. A moderator facing a ban cannot
    ///   escape it by counter-banning.
    /// - The owner's ban wins over everything: a member the owner's ban
    ///   removes has no ban authority, even inside a mutual ban.
    ///
    /// # Formalization
    ///
    /// Every lookup below (keys, invite chains, subtrees) reads `members`
    /// together with the ban evidence ([`BanEvidenceV1`]), so a ban's validity
    /// does not change when its issuer or target leaves `members`. That is
    /// what keeps `post_apply_cleanup` idempotent (#702 review round 5).
    ///
    /// 0. **Owner bans.** Every verifiable owner ban takes effect; the owner
    ///    can never be a target. The members it removes (target and subtree)
    ///    contribute no edge below. Their `deputies` grants authorize nothing
    ///    by construction: a member's grant only covers their own invite
    ///    subtree, which the owner's ban removes with them.
    /// 1. **Valid bans.** Any other ban counts only if its signature verifies
    ///    against its issuer's key ([`BansV1::ban_signature_matches_current_key`])
    ///    and the issuer is authorized to ban the target
    ///    ([`Self::is_ban_authorized`]). A ban whose issuer's record is known
    ///    nowhere takes no effect, and `post_apply_cleanup` step 4 sweeps it.
    /// 2. **Reach.** Removing a member removes their whole invite subtree (a
    ///    member cannot stay without their inviter). A ban on `t` reaches `t`
    ///    and `t`'s subtree, and "the issuer is removed" uses the same notion.
    /// 3. **Self-removing bans are void.** A ban whose reach contains its own
    ///    issuer (a self-ban, or a ban on one's own invite ancestor) takes no
    ///    effect. The UI already refused to offer it (`self_removing_ban_reason`).
    /// 4. **Cycles.** Draw an edge `I -> Y` for every valid ban from `I` and
    ///    every `Y` in its reach. Every member of a strongly connected
    ///    component of two or more members is removed, with their own
    ///    subtrees, and NOTHING ELSE (Ian, 2026-09-24): a cycle member's ban
    ///    removes no one outside the cycle, not even its target when the
    ///    cycle closes through the target's subtree. So a member of a banned
    ///    member's subtree can shield the banned member by counter-banning
    ///    the moderator, at the cost of being removed together with that
    ///    moderator. The owner's ban, or another moderator's, still reaches
    ///    the shielded member.
    /// 5. **Everything else**, in topological order of the component graph: a
    ///    ban from `I` takes effect iff `I` has not been removed by a ban that
    ///    took effect earlier, and then removes its whole reach.
    ///
    /// Edges only point from an issuer to the members its ban would remove, so
    /// whether `I` is removed is settled before `I`'s own component is
    /// processed, and the result does not depend on ban order.
    ///
    /// # A consequence of the rule
    ///
    /// A mutual ban counts whenever its two halves were issued, and nothing
    /// in converged state can tell a counter-ban from a later ban (`banned_at`
    /// is signed by the issuer). So a moderator removed by another moderator's
    /// ban, while their moderator grant stands, can counter-ban the moderator
    /// who banned them at any later time (their record is in the ban
    /// evidence, so the counter-ban verifies), and both are then removed. The
    /// owner ends this either by revoking that moderator's grant or by
    /// banning them (the owner's ban wins). Pinned by
    /// `a_removed_moderator_can_later_counter_ban_until_the_owner_acts`.
    pub fn resolve_bans(
        &self,
        evidence: &BanEvidenceV1,
        bans_v1: &BansV1,
        member_info: &MemberInfoV1,
        parameters: &ChatRoomParametersV1,
    ) -> BanResolution {
        let owner_id = parameters.owner_id();
        let lookup = evidence.lookup(self, owner_id);
        let children = children_by_inviter(lookup.values().copied());

        // Step 0: the owner's bans.
        let owner_removed = owner_removed_in(&children, bans_v1, parameters);
        let mut ban_tiers: HashMap<BanId, u8> = HashMap::new();
        let mut verified: HashSet<BanId> = HashSet::new();
        for ban in &bans_v1.0 {
            if ban.banned_by == owner_id
                && ban.ban.banned_user != owner_id
                && ban.verify_signature(&parameters.owner).is_ok()
            {
                ban_tiers.insert(ban.id(), BanResolution::OWNER);
                verified.insert(ban.id());
            }
        }

        // Steps 1-3: the valid bans, grouped by issuer, each with its reach.
        let mut valid: BTreeMap<MemberId, Vec<(BanId, BTreeSet<MemberId>)>> = BTreeMap::new();
        for ban in &bans_v1.0 {
            let issuer = ban.banned_by;
            if issuer == owner_id || owner_removed.contains(&issuer) {
                continue;
            }
            if !BansV1::ban_signature_matches_current_key(ban, &lookup, owner_id, &parameters.owner)
            {
                continue;
            }
            verified.insert(ban.id());
            if !Self::is_ban_authorized(issuer, ban.ban.banned_user, &lookup, member_info, owner_id)
            {
                continue;
            }
            let reach = reach_in(&children, ban.ban.banned_user);
            if reach.contains(&issuer) {
                continue; // step 3: self-removing, void
            }
            valid.entry(issuer).or_default().push((ban.id(), reach));
        }

        // Step 4: the graph. Nodes are issuers and the members they reach.
        let mut adjacency: BTreeMap<MemberId, BTreeSet<MemberId>> = BTreeMap::new();
        for (issuer, bans) in &valid {
            let out = adjacency.entry(*issuer).or_default();
            for (_, reach) in bans {
                out.extend(reach.iter().copied());
            }
        }
        let targets: Vec<MemberId> = adjacency.values().flatten().copied().collect();
        for y in targets {
            adjacency.entry(y).or_default();
        }

        // Steps 4-5, sources first. Owner-removed members start removed and
        // have no outgoing edges.
        let mut removed: HashSet<MemberId> = owner_removed.clone();
        for component in strongly_connected_components(&adjacency).into_iter().rev() {
            let in_cycle = component.len() >= 2;
            for issuer in &component {
                let Some(bans) = valid.get(issuer) else {
                    continue;
                };
                let removed_earlier = removed.contains(issuer);
                for (ban_id, reach) in bans {
                    if in_cycle {
                        // A cycle ban holds the cycle together whether or
                        // not an earlier ban already removed its issuer.
                        if reach.iter().any(|y| component.contains(y)) {
                            ban_tiers.insert(ban_id.clone(), BanResolution::MUTUAL);
                        }
                    } else if !removed_earlier {
                        removed.extend(reach.iter().copied());
                        ban_tiers.insert(ban_id.clone(), BanResolution::EFFECTIVE);
                    }
                }
            }
            if in_cycle {
                for member in &component {
                    removed.extend(reach_in(&children, *member));
                }
            }
        }
        // The owner is never a target, so this cannot fire; kept so a future
        // authority change cannot silently turn a ban of the owner into a ban
        // of the whole room.
        removed.remove(&owner_id);
        BanResolution {
            removed,
            owner_removed,
            ban_tiers,
            verified,
        }
    }

    /// Whether `banner` is currently authorized to ban `target` (#410).
    ///
    /// `banner` is authorized if any of these holds:
    /// 1. `banner` is the room owner.
    /// 2. `banner` is a STRICT ancestor of `target` in the invite tree ("you can
    ///    ban your own subtree").
    /// 3. `banner` is an owner-appointed global moderator (`owner`'s `deputies`
    ///    list `banner`); the owner's subtree is everyone.
    /// 4. Some strict NON-owner ancestor `A` of `target` lists `banner` in
    ///    `A.deputies`: deputy authority scoped to `A`'s subtree.
    ///
    /// There is no transitive re-deputization: a deputy's own `deputies` only
    /// grant authority over the deputy's OWN subtree (where the deputy is a
    /// genuine ancestor), never over a subtree they merely hold as a deputy.
    pub fn is_ban_authorized(
        banner: MemberId,
        target: MemberId,
        members_by_id: &HashMap<MemberId, &AuthorizedMember>,
        member_info: &MemberInfoV1,
        owner_id: MemberId,
    ) -> bool {
        // The owner is NEVER a valid ban target: they are not in the members
        // list, and the owner's reach (`reach_in`) is the entire room, so an
        // "authorized" ban of the owner would cascade-remove everyone. Deny
        // outright, before any grant: the owner-global-mod grant below would
        // otherwise authorize it.
        if target == owner_id {
            return false;
        }

        // 1. Owner — absolute.
        if banner == owner_id {
            return true;
        }

        // Collect target's STRICT ancestors: the invite chain strictly above
        // target, up to and including the owner (the root ancestor of everyone).
        // Excludes target itself. Retains a visited-set cycle guard.
        let mut strict_ancestors: HashSet<MemberId> = HashSet::new();
        strict_ancestors.insert(owner_id);
        let mut visited = HashSet::new();
        visited.insert(target);
        let mut current = members_by_id.get(&target).map(|m| m.member.invited_by);
        while let Some(c) = current {
            if !visited.insert(c) {
                break; // cycle guard
            }
            strict_ancestors.insert(c);
            if c == owner_id {
                break;
            }
            current = members_by_id.get(&c).map(|m| m.member.invited_by);
        }

        // 2. Genuine strict ancestor. The chain walk above only reaches
        //    records in `members_by_id`, so this grant already implies the
        //    banner's record is known.
        if strict_ancestors.contains(&banner) {
            return true;
        }

        // Deputy authority (steps 3 & 4) is granted to a banner ID only while
        // the banner's record is known (#411 round 3): `members_by_id` holds
        // the members, plus the ban evidence when called from `resolve_bans`
        // (freenet/river#702). A stale deputy ID with no record grants
        // nothing, so an outsider cannot forge `banner=<stale id>`; a known
        // banner's ban must verify against that record's key, which
        // `resolve_bans` checks before asking about authority. A member held
        // as evidence keeps the authority its grants give, the same authority
        // it would have by re-adding its own public record.
        let banner_is_member = members_by_id.contains_key(&banner);

        // 3. Owner-appointed global moderator (among members).
        if banner_is_member && member_info.deputies_of(owner_id).contains(&banner) {
            return true;
        }
        // 4. Deputy authority via a strict NON-owner ancestor of target.
        if banner_is_member {
            for a in &strict_ancestors {
                if *a == owner_id {
                    continue; // the owner's grant is handled absolutely in (3)
                }
                if member_info.deputies_of(*a).contains(&banner) {
                    return true;
                }
            }
        }
        false
    }

    /// If the number of members exceeds the specified limit, remove the members with the longest invite chains
    /// until the limit is satisfied. When chain lengths are equal, remove the member with the highest MemberId
    /// for deterministic ordering (CRDT convergence requirement).
    fn remove_excess_members(&mut self, parameters: &ChatRoomParametersV1, max_members: usize) {
        if self.members.len() <= max_members {
            return;
        }

        let members_by_id = self.members_by_member_id();
        let owner_id = parameters.owner_id();

        // Pre-compute chain lengths once for all members (no Ed25519 verification needed)
        let mut chain_lengths: Vec<(MemberId, usize)> = self
            .members
            .iter()
            .map(|m| {
                let len = Self::invite_chain_length(m, owner_id, &members_by_id);
                (m.member.id(), len)
            })
            .collect();

        // Sort by chain length descending, then by MemberId descending for deterministic tie-breaking
        chain_lengths.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));

        // Collect IDs to remove
        let excess = self.members.len() - max_members;
        let ids_to_remove: HashSet<MemberId> = chain_lengths
            .iter()
            .take(excess)
            .map(|(id, _)| *id)
            .collect();

        self.members
            .retain(|m| !ids_to_remove.contains(&m.member.id()));
    }

    /// Get the full invite chain with Ed25519 signature verification at each link.
    /// This is the authoritative verification used by `verify()`.
    pub fn get_invite_chain(
        &self,
        member: &AuthorizedMember,
        parameters: &ChatRoomParametersV1,
    ) -> Result<Vec<AuthorizedMember>, String> {
        let members_by_id = self.members_by_member_id();
        self.get_invite_chain_with_lookup(member, parameters, &members_by_id)
    }

    /// Get the full invite chain with Ed25519 signature verification, using a pre-built
    /// HashMap for O(1) member lookups instead of linear scans.
    pub(crate) fn get_invite_chain_with_lookup(
        &self,
        member: &AuthorizedMember,
        parameters: &ChatRoomParametersV1,
        members_by_id: &HashMap<MemberId, &AuthorizedMember>,
    ) -> Result<Vec<AuthorizedMember>, String> {
        let mut invite_chain = Vec::new();
        let mut current_member = member;
        let owner_id = parameters.owner_id();
        let mut visited_members = HashSet::new();

        loop {
            if !visited_members.insert(current_member.member.id()) {
                return Err(format!(
                    "Circular invite chain detected for member {:?}",
                    current_member.member.id()
                ));
            }

            if current_member.member.invited_by == current_member.member.id() {
                return Err(format!(
                    "Self-invitation detected for member {:?}",
                    current_member.member.id()
                ));
            }

            if current_member.member.invited_by == owner_id {
                current_member
                    .verify_signature(&parameters.owner)
                    .map_err(|e| {
                        format!(
                            "Invalid signature for member {:?} invited by owner: {}",
                            current_member.member.id(),
                            e
                        )
                    })?;
                break;
            } else {
                let inviter = members_by_id
                    .get(&current_member.member.invited_by)
                    .ok_or_else(|| {
                        format!(
                            "Inviter {:?} not found for member {:?}",
                            current_member.member.invited_by,
                            current_member.member.id()
                        )
                    })?;

                current_member
                    .verify_signature(&inviter.member.member_vk)
                    .map_err(|e| {
                        format!(
                            "Invalid signature for member {:?}: {}",
                            current_member.member.id(),
                            e
                        )
                    })?;

                invite_chain.push((*inviter).clone());
                current_member = inviter;
            }
        }

        Ok(invite_chain)
    }

    /// Walk the invite chain and return the length WITHOUT Ed25519 signature verification.
    /// Used by `remove_excess_members` where we only need chain length for comparison.
    fn invite_chain_length(
        member: &AuthorizedMember,
        owner_id: MemberId,
        members_by_id: &HashMap<MemberId, &AuthorizedMember>,
    ) -> usize {
        let mut length = 0;
        let mut current_id = member.member.invited_by;
        let mut visited = HashSet::new();
        visited.insert(member.member.id());

        while current_id != owner_id {
            if !visited.insert(current_id) {
                break; // Circular chain — will be caught by verify()
            }
            length += 1;
            match members_by_id.get(&current_id) {
                Some(inviter) => current_id = inviter.member.invited_by,
                None => break, // Missing inviter — will be caught by verify()
            }
        }
        length
    }
}

/// The outcome of [`MembersV1::resolve_bans`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BanResolution {
    /// Every member id a ban removes: targets, their invite subtrees, and the
    /// members of mutual-ban cycles with theirs. May name ids that are
    /// already absent.
    pub removed: HashSet<MemberId>,
    /// The members a verifiable owner ban removes: each target and its
    /// invite subtree. Their bans never take effect, and `post_apply_cleanup`
    /// sweeps them.
    pub owner_removed: HashSet<MemberId>,
    /// Why each ban takes effect ([`Self::OWNER`], [`Self::EFFECTIVE`],
    /// [`Self::MUTUAL`]). A ban absent here takes no effect ([`Self::INERT`]);
    /// the `max_user_bans` eviction drops those first, then evicts by age
    /// within an owner pool and an everyone-else pool (freenet/river#796).
    pub ban_tiers: HashMap<BanId, u8>,
    /// The bans whose signature was checked against their issuer's key and
    /// verified. Bans by owner-removed issuers are not checked.
    pub verified: HashSet<BanId>,
}

impl BanResolution {
    /// A ban that takes no effect.
    pub const INERT: u8 = 0;
    /// A ban that closes a mutual-ban cycle.
    pub const MUTUAL: u8 = 1;
    /// A ban that takes effect because its issuer is not removed.
    pub const EFFECTIVE: u8 = 2;
    /// A verifiable owner ban.
    pub const OWNER: u8 = 3;

    /// The tier of `ban` ([`Self::INERT`] if it has none).
    pub fn tier(&self, ban: &BanId) -> u8 {
        self.ban_tiers.get(ban).copied().unwrap_or(Self::INERT)
    }
}

/// Parent -> invitees over the given records.
fn children_by_inviter<'a>(
    records: impl Iterator<Item = &'a AuthorizedMember>,
) -> HashMap<MemberId, Vec<MemberId>> {
    let mut children: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
    for m in records {
        children
            .entry(m.member.invited_by)
            .or_default()
            .push(m.member.id());
    }
    children
}

/// Every id a verifiable owner ban removes: target plus invite subtree.
fn owner_removed_in(
    children: &HashMap<MemberId, Vec<MemberId>>,
    bans_v1: &BansV1,
    parameters: &ChatRoomParametersV1,
) -> HashSet<MemberId> {
    let owner_id = parameters.owner_id();
    let mut removed = HashSet::new();
    for ban in &bans_v1.0 {
        if ban.banned_by != owner_id
            || ban.ban.banned_user == owner_id
            || ban.verify_signature(&parameters.owner).is_err()
        {
            continue;
        }
        removed.extend(reach_in(children, ban.ban.banned_user));
    }
    removed
}

/// `target` and its whole invite subtree.
fn reach_in(children: &HashMap<MemberId, Vec<MemberId>>, target: MemberId) -> BTreeSet<MemberId> {
    let mut reach = BTreeSet::new();
    reach.insert(target);
    let mut stack = vec![target];
    while let Some(current) = stack.pop() {
        if let Some(kids) = children.get(&current) {
            for kid in kids {
                if reach.insert(*kid) {
                    stack.push(*kid);
                }
            }
        }
    }
    reach
}

/// Strongly connected components of `graph`, in reverse topological order
/// (a component comes before every component that has an edge into it).
/// Iterative Tarjan, so a long ban chain cannot overflow the WASM stack.
/// Deterministic: `BTreeMap`/`BTreeSet` iteration fixes the visit order.
fn strongly_connected_components(
    graph: &BTreeMap<MemberId, BTreeSet<MemberId>>,
) -> Vec<BTreeSet<MemberId>> {
    let mut index_of: HashMap<MemberId, usize> = HashMap::new();
    let mut lowlink: HashMap<MemberId, usize> = HashMap::new();
    let mut on_stack: HashSet<MemberId> = HashSet::new();
    let mut stack: Vec<MemberId> = Vec::new();
    let mut components = Vec::new();
    let mut next_index = 0usize;
    let empty = BTreeSet::new();

    for &root in graph.keys() {
        if index_of.contains_key(&root) {
            continue;
        }
        // Each frame: the node and an iterator over its successors.
        let mut frames: Vec<(MemberId, std::collections::btree_set::Iter<'_, MemberId>)> =
            Vec::new();
        index_of.insert(root, next_index);
        lowlink.insert(root, next_index);
        next_index += 1;
        stack.push(root);
        on_stack.insert(root);
        frames.push((root, graph.get(&root).unwrap_or(&empty).iter()));

        while let Some((node, successors)) = frames.last_mut() {
            let node = *node;
            if let Some(&next) = successors.next() {
                if let std::collections::hash_map::Entry::Vacant(slot) = index_of.entry(next) {
                    slot.insert(next_index);
                    lowlink.insert(next, next_index);
                    next_index += 1;
                    stack.push(next);
                    on_stack.insert(next);
                    frames.push((next, graph.get(&next).unwrap_or(&empty).iter()));
                } else if on_stack.contains(&next) {
                    let low = lowlink[&node].min(index_of[&next]);
                    lowlink.insert(node, low);
                }
                continue;
            }
            frames.pop();
            if let Some((parent, _)) = frames.last() {
                let low = lowlink[parent].min(lowlink[&node]);
                lowlink.insert(*parent, low);
            }
            if lowlink[&node] == index_of[&node] {
                let mut component = BTreeSet::new();
                while let Some(member) = stack.pop() {
                    on_stack.remove(&member);
                    component.insert(member);
                    if member == node {
                        break;
                    }
                }
                components.push(component);
            }
        }
    }
    components
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub struct MembersDelta {
    added: Vec<AuthorizedMember>,
}

impl MembersDelta {
    pub fn new(added: Vec<AuthorizedMember>) -> Self {
        MembersDelta { added }
    }

    pub fn added(&self) -> &[AuthorizedMember] {
        &self.added
    }

    pub fn into_added(self) -> Vec<AuthorizedMember> {
        self.added
    }
}

// TODO: need to generalize to support multiple authorization mechanisms such as ghost keys

#[derive(Serialize, Deserialize, Eq, PartialEq, Clone, Debug)]
pub struct AuthorizedMember {
    pub member: Member,
    pub signature: Signature,
}

impl AuthorizedMember {
    pub fn new(member: Member, inviter_signing_key: &SigningKey) -> Self {
        assert_eq!(
            member.invited_by,
            VerifyingKey::from(inviter_signing_key).into(),
            "The member's invited_by must match the inviter's signing key"
        );
        Self {
            member: member.clone(),
            signature: sign_struct(&member, inviter_signing_key),
        }
    }

    /// Create an AuthorizedMember with a pre-computed signature.
    /// Use this when signing is done externally (e.g., via delegate).
    pub fn with_signature(member: Member, signature: Signature) -> Self {
        Self { member, signature }
    }

    pub fn verify_signature(&self, inviter_vk: &VerifyingKey) -> Result<(), String> {
        verify_struct(&self.member, &self.signature, inviter_vk)
            .map_err(|e| format!("Invalid signature: {}", e))
    }
}

impl Hash for AuthorizedMember {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.member.hash(state);
    }
}

#[derive(Serialize, Deserialize, Eq, PartialEq, Hash, Clone)]
pub struct Member {
    pub owner_member_id: MemberId,
    pub invited_by: MemberId,
    pub member_vk: VerifyingKey,
}

impl fmt::Debug for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Member")
            .field(
                "public_key",
                &format_args!("{}", truncated_base32(self.member_vk.as_bytes())),
            )
            .finish()
    }
}

/*
`MemberId` is a 64-bit *non-cryptographic* `FastHash` (a Java-style `h*31 + byte`
polynomial hash) of the member's `VerifyingKey`, NOT a cryptographic digest. The
8-char base32 `Display` label is lossier still — only 40 of those 64 bits.

Collision cost is therefore modest, not astronomical (an earlier version of this
comment claimed ~3 * 10^59 years, which was wrong):
  - 40-bit `Display` label, targeted second-preimage: ~2^40 keypair generations
    (hours on a GPU / small cluster). Any-collision (birthday): ~2^20, seconds.
  - Full 64-bit id, targeted: ~2^64 (large but finite brute force over keypairs,
    not 10^59 years). Any-collision (birthday): ~2^32, ~hours single-core.
The one thing that keeps this from being trivial is that a usable member needs a
real keypair (to sign), so the weak hash can't be inverted algebraically — an
attacker must brute-force by generating ed25519 keypairs and hashing the pubkey.

Why a collision is not a full compromise: authorization is anchored on the full
ed25519 keys and signatures. `member_vk` carries the full key, and the invite
chain verifies signatures against the looked-up member's real `member_vk`; a
member colliding with the owner's id (or full key) is rejected in `verify`. A
`MemberId` collision buys identity confusion / member-map shadowing (bans,
invite refs and the member map are keyed by the 64-bit id), NOT signature
forgery. The cheap 40-bit label is UI/mention-token only and never keys these
maps. See `mention.rs` for the label's lossy round-trip handling.
*/
#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Ord, PartialOrd, Copy)]
pub struct MemberId(pub FastHash);

impl Display for MemberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", truncated_base32(&self.0 .0.to_le_bytes()))
    }
}

impl Debug for MemberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "MemberId({})",
            truncated_base32(&self.0 .0.to_le_bytes())
        )
    }
}

impl From<&VerifyingKey> for MemberId {
    fn from(vk: &VerifyingKey) -> Self {
        MemberId(fast_hash(&vk.to_bytes()))
    }
}

impl From<VerifyingKey> for MemberId {
    fn from(vk: VerifyingKey) -> Self {
        MemberId(fast_hash(&vk.to_bytes()))
    }
}

impl Member {
    pub fn id(&self) -> MemberId {
        self.member_vk.into()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::room_state::ban::{AuthorizedUserBan, UserBan};
    use crate::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use std::time::SystemTime;

    pub(crate) fn create_test_member(
        owner_id: MemberId,
        invited_by: MemberId,
    ) -> (Member, SigningKey) {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let member = Member {
            owner_member_id: owner_id,
            invited_by,
            member_vk: verifying_key,
        };
        (member, signing_key)
    }

    thread_local! {
        static INVITE_SIGNATURE_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        static CHAIN_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    pub(super) fn count_chain_step() {
        CHAIN_STEPS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn chain_steps() -> usize {
        CHAIN_STEPS.with(|c| c.get())
    }

    pub(super) fn count_invite_signature_check() {
        INVITE_SIGNATURE_CHECKS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn invite_signature_checks() -> usize {
        INVITE_SIGNATURE_CHECKS.with(|c| c.get())
    }

    /// A chain owner -> m0 -> m1 -> ... -> m{n-1}, each record signed by its
    /// inviter, the signing key of the last member, and a state allowing
    /// `max_members` members.
    fn invite_chain(
        n: usize,
        max_members: usize,
    ) -> (
        ChatRoomStateV1,
        ChatRoomParametersV1,
        Vec<AuthorizedMember>,
        SigningKey,
    ) {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let mut chain = Vec::with_capacity(n);
        let mut inviter_sk = owner_sk.clone();
        let mut inviter_id = owner_id;
        for _ in 0..n {
            let (member, sk) = create_test_member(owner_id, inviter_id);
            inviter_id = member.id();
            chain.push(AuthorizedMember::new(member, &inviter_sk));
            inviter_sk = sk;
        }
        let parent_state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    max_members,
                    ..Default::default()
                },
                &owner_sk,
            ),
            ..Default::default()
        };
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        (parent_state, parameters, chain, inviter_sk)
    }

    /// Each link of a chain added in one delta is verified once, whatever
    /// order the delta lists the members in.
    #[test]
    fn a_chain_in_one_delta_verifies_each_link_once() {
        let n = 64;
        let (parent_state, parameters, chain, _) = invite_chain(n, n);
        let mut deepest_first = chain.clone();
        deepest_first.reverse();
        for added in [chain, deepest_first] {
            let mut members = MembersV1::default();
            let before = invite_signature_checks();
            members
                .apply_delta(&parent_state, &parameters, &Some(MembersDelta::new(added)))
                .unwrap();
            assert_eq!(invite_signature_checks() - before, n);
            assert_eq!(members.members.len(), n);
        }
    }

    /// `MembersV1::verify` as it was before invite links were memoized: every
    /// member's full chain is walked. Kept to check the two accept and reject
    /// the same member lists with the same errors.
    fn verify_walking_every_chain(
        members: &MembersV1,
        parent_state: &ChatRoomStateV1,
        parameters: &ChatRoomParametersV1,
    ) -> Result<(), String> {
        if members.members.is_empty() {
            return Ok(());
        }
        if members.members.len() > parent_state.configuration.configuration.max_members {
            return Err(format!(
                "Too many members: {} > {}",
                members.members.len(),
                parent_state.configuration.configuration.max_members
            ));
        }
        let owner_id = parameters.owner_id();
        let members_by_id = members.members_by_member_id();
        for member in &members.members {
            if member.member.id() == owner_id {
                return Err("Owner should not be included in the members list".to_string());
            }
            if member.member.member_vk == parameters.owner {
                return Err(
                    "Member cannot have the same verifying key as the room owner".to_string(),
                );
            }
            if member.member.invited_by == member.member.id() {
                return Err("Self-invitation detected".to_string());
            }
            members.get_invite_chain_with_lookup(member, parameters, &members_by_id)?;
        }
        Ok(())
    }

    /// A random invite tree of `n` members under the owner, with signing keys.
    pub(crate) fn random_tree(
        rng: &mut rand::rngs::StdRng,
        owner_sk: &SigningKey,
        n: usize,
    ) -> (Vec<AuthorizedMember>, Vec<SigningKey>) {
        use rand::Rng;
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let mut records: Vec<AuthorizedMember> = Vec::new();
        let mut keys: Vec<SigningKey> = Vec::new();
        for _ in 0..n {
            let sk = SigningKey::generate(rng);
            // Invited by the owner or by an earlier member, biased deep.
            let pick = rng.gen_range(0..=records.len());
            // `gen_bool` is drawn only when `pick == records.len()`, as before
            // these two identical branches were merged, so the RNG stream is
            // unchanged.
            let (inviter_id, inviter_sk) =
                if (pick == records.len() && rng.gen_bool(0.2)) || records.is_empty() {
                    (owner_id, owner_sk.clone())
                } else {
                    let i = rng.gen_range(0..records.len());
                    (records[i].member.id(), keys[i].clone())
                };
            let member = Member {
                owner_member_id: owner_id,
                invited_by: inviter_id,
                member_vk: sk.verifying_key(),
            };
            records.push(AuthorizedMember::new(member, &inviter_sk));
            keys.push(sk);
        }
        (records, keys)
    }

    /// The memoized `verify` gives the same result, error text included, as
    /// walking every member's full chain, on valid trees and on trees with one
    /// defect: a bad signature, a missing inviter, a self-invitation, a cycle,
    /// a record with the owner's id or key, too many members, or a second
    /// record for one member's key naming another inviter (with a signature
    /// that does or does not verify). Each list is checked shuffled and
    /// reversed, since memoization depends on which chains were walked first.
    /// (Two different keys with the same id cannot be built here: the id is
    /// derived from the key.)
    #[test]
    fn verify_matches_walking_every_chain() {
        use rand::seq::SliceRandom;
        use rand::{Rng, SeedableRng};
        let mut rejected = 0;
        for seed in 0..160u64 {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            let owner_sk = SigningKey::generate(&mut rng);
            let owner_id: MemberId = owner_sk.verifying_key().into();
            let parameters = ChatRoomParametersV1 {
                owner: owner_sk.verifying_key(),
            };
            let n = rng.gen_range(1..16);
            let (mut records, keys) = random_tree(&mut rng, &owner_sk, n);
            let mut max_members = 100;
            let i = rng.gen_range(0..records.len());
            let j = rng.gen_range(0..records.len());
            match seed % 10 {
                0 => {}
                1 => records[i].signature = records[j].signature,
                2 => {
                    let removed = records.remove(i);
                    let _ = removed;
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
                        member_vk: parameters.owner,
                    },
                    &keys[i],
                )),
                6 => max_members = records.len() - 1,
                // A second record for member i's key naming another inviter.
                // 8: it carries member i's signature, which does not verify
                // for it. 9: it is signed by that inviter, so it verifies.
                8 | 9 => {
                    let (inviter_id, inviter_sk) = if j == i {
                        (owner_id, owner_sk.clone())
                    } else {
                        (records[j].member.id(), keys[j].clone())
                    };
                    let member = Member {
                        invited_by: inviter_id,
                        ..records[i].member.clone()
                    };
                    let signature = if seed % 10 == 8 {
                        records[i].signature
                    } else {
                        crate::util::sign_struct(&member, &inviter_sk)
                    };
                    records.push(AuthorizedMember { member, signature });
                }
                _ => {
                    let other = SigningKey::generate(&mut rng);
                    records[i].signature = crate::util::sign_struct(&records[i].member, &other);
                }
            }
            records.shuffle(&mut rng);
            let mut reversed = records.clone();
            reversed.reverse();
            for records in [records, reversed] {
                let members = MembersV1 { members: records };
                let parent_state = ChatRoomStateV1 {
                    configuration: AuthorizedConfigurationV1::new(
                        Configuration {
                            owner_member_id: owner_id,
                            max_members,
                            ..Default::default()
                        },
                        &owner_sk,
                    ),
                    ..Default::default()
                };
                let expected = verify_walking_every_chain(&members, &parent_state, &parameters);
                let actual = members.verify(&parent_state, &parameters);
                assert_eq!(actual, expected, "seed {seed}");
                if expected.is_err() {
                    rejected += 1;
                }
            }
        }
        // Most defective trees are rejected; the comparison covers both outcomes.
        assert!(rejected > 140, "only {rejected} rejected");
    }

    /// `verify` on an N-deep chain checks N signatures.
    #[test]
    fn verify_checks_each_link_of_a_deep_chain_once() {
        let n = 200;
        let (mut parent_state, parameters, chain, _) = invite_chain(n, n);
        parent_state.members.members = chain;
        let before = invite_signature_checks();
        parent_state
            .members
            .verify(&parent_state, &parameters)
            .unwrap();
        assert_eq!(invite_signature_checks() - before, n);
    }

    /// A record carrying the owner's id is ignored wherever it sits in the
    /// delta, and the rest of the delta applies as it would without it.
    #[test]
    fn a_member_record_carrying_the_owner_id_is_ignored() {
        let (mut parent_state, parameters, chain, last_sk) = invite_chain(2, 10);
        let owner_id: MemberId = parameters.owner.into();
        parent_state.members.members = chain;
        let inviter_id = parent_state.members.members[1].member.id();
        let (y, y_sk) = create_test_member(owner_id, inviter_id);
        let y_record = AuthorizedMember::new(y.clone(), &last_sk);
        let owner_record = AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: y.id(),
                member_vk: parameters.owner,
            },
            &y_sk,
        );

        let results: Vec<MembersV1> = [
            vec![owner_record.clone(), y_record.clone()],
            vec![y_record.clone(), owner_record],
            vec![y_record],
        ]
        .into_iter()
        .map(|added| {
            let mut members = parent_state.members.clone();
            members
                .apply_delta(&parent_state, &parameters, &Some(MembersDelta::new(added)))
                .unwrap();
            members
        })
        .collect();
        assert_eq!(results[0], results[2]);
        assert_eq!(results[1], results[2]);
        assert!(results[2].members.iter().any(|m| m.member.id() == y.id()));
        assert!(results[0].members.iter().all(|m| m.member.id() != owner_id));
    }

    /// Records for a room where `a` and `b` are invited by the owner:
    /// `genuine` is `a`'s record; `other` carries `a`'s key, names `b` as
    /// inviter and has a signature that does not verify for it; `y` is
    /// invited by `a`.
    struct SameIdRecords {
        parent_state: ChatRoomStateV1,
        parameters: ChatRoomParametersV1,
        genuine: AuthorizedMember,
        other: AuthorizedMember,
        b: AuthorizedMember,
        y: AuthorizedMember,
    }

    fn same_id_records() -> SameIdRecords {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let (a, a_sk) = create_test_member(owner_id, owner_id);
        let (b, _) = create_test_member(owner_id, owner_id);
        let (y, _) = create_test_member(owner_id, a.id());
        let genuine = AuthorizedMember::new(a.clone(), &owner_sk);
        let other = AuthorizedMember {
            member: Member {
                invited_by: b.id(),
                ..a.clone()
            },
            signature: genuine.signature,
        };
        assert_eq!(other.member.id(), genuine.member.id());
        let parent_state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    max_members: 10,
                    ..Default::default()
                },
                &owner_sk,
            ),
            ..Default::default()
        };
        SameIdRecords {
            parent_state,
            parameters: ChatRoomParametersV1 {
                owner: owner_sk.verifying_key(),
            },
            genuine,
            other,
            b: AuthorizedMember::new(b, &owner_sk),
            y: AuthorizedMember::new(y, &a_sk),
        }
    }

    /// A record whose own signature does not verify rejects the delta,
    /// whatever else in the delta shares its id, in either order, unless its
    /// id is already held: a record for a held id is skipped unchecked, since
    /// the held record is kept whatever arrives (freenet/river#703).
    #[test]
    fn a_record_sharing_an_id_with_a_verified_one_is_still_checked() {
        let r = same_id_records();
        let apply = |held: Vec<AuthorizedMember>, added: Vec<AuthorizedMember>| {
            let mut members = MembersV1 { members: held };
            members.apply_delta(
                &r.parent_state,
                &r.parameters,
                &Some(MembersDelta::new(added)),
            )
        };
        // Neither record held.
        assert!(apply(vec![r.b.clone()], vec![r.genuine.clone(), r.other.clone()]).is_err());
        assert!(apply(vec![r.b.clone()], vec![r.other.clone(), r.genuine.clone()]).is_err());
        // The genuine record held, with a member it invited in the delta.
        let held = vec![r.genuine.clone(), r.b.clone()];
        for added in [
            vec![r.y.clone(), r.other.clone()],
            vec![r.other.clone(), r.y.clone()],
            vec![r.y.clone()],
        ] {
            let mut members = MembersV1 {
                members: held.clone(),
            };
            members
                .apply_delta(
                    &r.parent_state,
                    &r.parameters,
                    &Some(MembersDelta::new(added)),
                )
                .expect("a record for a held id is skipped");
            assert!(members.members.contains(&r.genuine));
            assert!(!members.members.contains(&r.other));
        }
    }

    /// `members.apply_delta(added)` on a separate thread, failing the test if
    /// it does not finish within 30 seconds: a looping invite chain must fail
    /// a test with a message, not hang the suite.
    fn apply_within_seconds(
        mut members: MembersV1,
        parent_state: &ChatRoomStateV1,
        parameters: &ChatRoomParametersV1,
        added: Vec<AuthorizedMember>,
    ) -> MembersV1 {
        let (parent_state, parameters) = (parent_state.clone(), parameters.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = members
                .apply_delta(&parent_state, &parameters, &Some(MembersDelta::new(added)))
                .map(|()| members);
            let _ = tx.send(result);
        });
        rx.recv_timeout(std::time::Duration::from_secs(30))
            .expect("apply_delta did not finish: a looping invite chain was followed forever")
            .unwrap()
    }

    /// A second record for a member's key, validly signed by a member
    /// further down that member's own subtree, would make the chain return to
    /// its own id. It is never stored: the member keeps the record nearest
    /// the owner, and a record reachable only through itself is never
    /// reached. Until freenet/river#703 a delta carrying it without the
    /// member held was rejected outright.
    #[test]
    fn a_record_whose_chain_returns_to_its_own_id_is_never_stored() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let (x, x_sk) = create_test_member(owner_id, owner_id);
        let (e, e_sk) = create_test_member(owner_id, x.id());
        let (y, _) = create_test_member(owner_id, x.id());
        let held = vec![
            AuthorizedMember::new(x.clone(), &owner_sk),
            AuthorizedMember::new(e.clone(), &x_sk),
        ];
        let y = AuthorizedMember::new(y, &x_sk);
        let s = AuthorizedMember::new(
            Member {
                invited_by: e.id(),
                ..x.clone()
            },
            &e_sk,
        );
        let parent_state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    max_members: 10,
                    ..Default::default()
                },
                &owner_sk,
            ),
            ..Default::default()
        };
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        for added in [
            vec![y.clone(), s.clone()],
            vec![s.clone(), y.clone()],
            vec![s.clone()],
        ] {
            let members = apply_within_seconds(
                MembersV1 {
                    members: held.clone(),
                },
                &parent_state,
                &parameters,
                added,
            );
            assert!(!members.members.contains(&s), "never stored while held");
        }
        // Not held: x's genuine record and the looping one arrive together,
        // in either order, or the looping one with no way to the owner.
        let x_rec = held[0].clone();
        let e_rec = held[1].clone();
        for (added, kept) in [
            (vec![x_rec.clone(), e_rec.clone(), s.clone()], 2),
            (vec![s.clone(), e_rec.clone(), x_rec.clone()], 2),
            (vec![e_rec.clone(), s.clone()], 0),
        ] {
            let members =
                apply_within_seconds(MembersV1::default(), &parent_state, &parameters, added);
            assert!(!members.members.contains(&s));
            assert_eq!(members.members.len(), kept);
        }
    }

    /// freenet/river#703 defect 2: a stale delta can omit an inviter its
    /// sender believed this peer held. The member, and everyone invited
    /// through them, is skipped rather than rejecting the delta; the rest
    /// applies, and a later delta carrying the inviter adds them all.
    #[test]
    fn a_member_whose_inviter_is_unknown_is_skipped_with_its_subtree() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let parameters = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let parent_state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    max_members: 10,
                    ..Default::default()
                },
                &owner_sk,
            ),
            ..Default::default()
        };
        let (a, a_sk) = create_test_member(owner_id, owner_id);
        let (b, b_sk) = create_test_member(owner_id, a.id());
        let (c, _) = create_test_member(owner_id, b.id());
        let (d, _) = create_test_member(owner_id, owner_id);
        let a = AuthorizedMember::new(a, &owner_sk);
        let b = AuthorizedMember::new(b, &a_sk);
        let c = AuthorizedMember::new(c, &b_sk);
        let d = AuthorizedMember::new(d, &owner_sk);
        let d_signature = d.signature;

        let mut members = MembersV1::default();
        members
            .apply_delta(
                &parent_state,
                &parameters,
                &Some(MembersDelta::new(vec![c.clone(), b.clone(), d.clone()])),
            )
            .expect("an unknown inviter must not reject the delta");
        assert_eq!(members.members, vec![d.clone()]);

        members
            .apply_delta(
                &parent_state,
                &parameters,
                &Some(MembersDelta::new(vec![a.clone(), b.clone(), c.clone()])),
            )
            .unwrap();
        let mut want = vec![a, b, c, d];
        want.sort_by_key(|m| m.member.id());
        assert_eq!(members.members, want);

        // A bad signature is still a rejection, inviter known or not.
        let (e, _) = create_test_member(owner_id, owner_id);
        let forged = AuthorizedMember {
            member: e,
            signature: d_signature,
        };
        assert!(members
            .apply_delta(
                &parent_state,
                &parameters,
                &Some(MembersDelta::new(vec![forged])),
            )
            .is_err());
    }

    /// freenet/river#703: deciding whether new members' invite
    /// chains are known must cost work linear in the delta, not quadratic:
    /// each record's chain is resolved once, however many records chain
    /// through it. Here a long chain of records ends at an inviter nobody
    /// holds, so all of it is skipped; the walk must stay within a small
    /// multiple of its length. A delta longer than held + bound is refused
    /// before any per-record work.
    #[test]
    fn unknown_chains_are_resolved_in_linear_work() {
        let (parent_state, parameters, _, _) = invite_chain(1, 5_000);
        let owner_id = parameters.owner_id();
        let unknown = MemberId::from(&SigningKey::generate(&mut OsRng).verifying_key());
        let any_signature = ed25519_dalek::Signature::from_bytes(&[0; 64]);
        let n = 2_000;
        let mut records = Vec::with_capacity(n);
        let mut inviter = unknown;
        for _ in 0..n {
            let (m, _) = create_test_member(owner_id, inviter);
            inviter = m.id();
            records.push(AuthorizedMember {
                member: m,
                signature: any_signature,
            });
        }
        records.reverse();
        let mut members = MembersV1::default();
        let before = chain_steps();
        members
            .apply_delta(
                &parent_state,
                &parameters,
                &Some(MembersDelta::new(records.clone())),
            )
            .expect("members with an unknown inviter are skipped");
        assert!(members.members.is_empty());
        let steps = chain_steps() - before;
        assert!(steps <= 3 * n, "{steps} chain steps for {n} records");

        // Past held + bound, refused before any chain is walked.
        let mut big = records.clone();
        big.extend(records.iter().cloned());
        big.extend(records.iter().cloned());
        let before = chain_steps();
        assert!(MembersV1::default()
            .apply_delta(&parent_state, &parameters, &Some(MembersDelta::new(big)))
            .is_err());
        assert_eq!(chain_steps() - before, 0);
    }

    /// The bound is `max(max_members, MIN_MEMBERS_DELTA_BOUND)`. It is checked
    /// before any record is verified, so repeated copies of one record suffice.
    #[test]
    fn a_members_delta_over_the_work_bound_is_rejected() {
        for max_members in [10, MIN_MEMBERS_DELTA_BOUND + 500] {
            let bound = max_members.max(MIN_MEMBERS_DELTA_BOUND);
            let (parent_state, parameters, chain, _) = invite_chain(1, max_members);

            let mut members = MembersV1::default();
            let at_bound = vec![chain[0].clone(); bound];
            members
                .apply_delta(
                    &parent_state,
                    &parameters,
                    &Some(MembersDelta::new(at_bound)),
                )
                .unwrap();
            assert_eq!(members.members.len(), 1);

            let mut members = MembersV1::default();
            let over = vec![chain[0].clone(); bound + 1];
            let result =
                members.apply_delta(&parent_state, &parameters, &Some(MembersDelta::new(over)));
            assert!(result.is_err(), "max_members {max_members}");
            assert!(members.members.is_empty());
        }
    }

    #[test]
    fn reach_of_an_acyclic_tree() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        // owner -> a -> {b, c}, b -> d; e is invited by the owner.
        let (a, a_sk) = create_test_member(owner_id, owner_id);
        let (b, b_sk) = create_test_member(owner_id, a.id());
        let (c, _) = create_test_member(owner_id, a.id());
        let (d, _) = create_test_member(owner_id, b.id());
        let (e, _) = create_test_member(owner_id, owner_id);
        let members = MembersV1 {
            members: vec![
                AuthorizedMember::new(a.clone(), &owner_sk),
                AuthorizedMember::new(b.clone(), &a_sk),
                AuthorizedMember::new(c.clone(), &a_sk),
                AuthorizedMember::new(d.clone(), &b_sk),
                AuthorizedMember::new(e.clone(), &owner_sk),
            ],
        };
        let children = children_by_inviter(members.members.iter());
        assert_eq!(
            reach_in(&children, a.id()),
            BTreeSet::from([a.id(), b.id(), c.id(), d.id()])
        );
        assert_eq!(
            reach_in(&children, b.id()),
            BTreeSet::from([b.id(), d.id()])
        );
        assert_eq!(reach_in(&children, e.id()), BTreeSet::from([e.id()]));
        assert_eq!(
            reach_in(&children, owner_id),
            BTreeSet::from([owner_id, a.id(), b.id(), c.id(), d.id(), e.id()])
        );
    }

    #[test]
    fn reach_of_an_invite_cycle_terminates() {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let (a, a_sk) = create_test_member(owner_id, owner_id);
        let (b, _) = create_test_member(owner_id, a.id());
        let mut a_record = AuthorizedMember::new(a.clone(), &owner_sk);
        let b_record = AuthorizedMember::new(b.clone(), &a_sk);
        // a and b each list the other as inviter. `verify` rejects this, so
        // the record is edited after signing.
        a_record.member.invited_by = b.id();
        let members = MembersV1 {
            members: vec![a_record, b_record],
        };
        assert_eq!(
            reach_in(&children_by_inviter(members.members.iter()), a.id()),
            BTreeSet::from([a.id(), b.id()])
        );
    }

    #[test]
    fn test_members_verify() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member2.clone()],
        };

        println!("Member1 ID: {:?}", member1.id());
        println!("Member2 ID: {:?}", member2.id());
        println!("Owner ID: {:?}", owner_id);

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration.configuration.max_members = 3;
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = members.verify(&parent_state, &parameters);
        println!("Verification result: {:?}", result);
        assert!(result.is_ok(), "Verification failed: {:?}", result);

        // Test that including the owner in the members list fails verification
        let owner_member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: owner_verifying_key,
        };
        let authorized_owner = AuthorizedMember::new(owner_member, &owner_signing_key);
        let members_with_owner = MembersV1 {
            members: vec![authorized_owner, authorized_member1, authorized_member2],
        };
        let result_with_owner = members_with_owner.verify(&parent_state, &parameters);
        println!("Verification result with owner: {:?}", result_with_owner);
        assert!(
            result_with_owner.is_err(),
            "Verification should fail when owner is included: {:?}",
            result_with_owner
        );
    }

    #[test]
    fn test_members_summarize() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member1, authorized_member2],
        };

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let summary = members.summarize(&parent_state, &parameters);
        assert_eq!(summary.len(), 2);
        assert!(summary.contains(&member1.id()));
        assert!(summary.contains(&member2.id()));
    }

    #[test]
    fn test_members_delta() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3.clone(), &member1_signing_key);

        let old_members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member2.clone()],
        };

        let new_members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member3.clone()],
        };

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let old_summary = old_members.summarize(&parent_state, &parameters);
        let delta = new_members
            .delta(&parent_state, &parameters, &old_summary)
            .unwrap();

        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.added[0].member.id(), member3.id());
    }

    #[test]
    fn test_members_apply_delta_simple() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3.clone(), &member1_signing_key);

        let original_members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member2.clone()],
        };

        let delta = MembersDelta {
            added: vec![authorized_member3.clone()],
        };

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration.configuration.max_members = 3;

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let mut modified_members = original_members.clone();

        assert!(modified_members
            .apply_delta(&parent_state, &parameters, &Some(delta))
            .is_ok());

        assert_eq!(modified_members.members.len(), 3);
        assert!(modified_members
            .members
            .iter()
            .any(|m| m.member.id() == member1.id()));
        assert!(modified_members
            .members
            .iter()
            .any(|m| m.member.id() == member3.id()));
        assert!(modified_members
            .members
            .iter()
            .any(|m| m.member.id() == member2.id()));
    }

    #[test]
    fn test_authorized_member_validate() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);

        assert!(authorized_member1
            .verify_signature(&owner_verifying_key)
            .is_ok());
        assert!(authorized_member2
            .verify_signature(&member1.member_vk)
            .is_ok());

        // Test with invalid signature
        let invalid_member2 = AuthorizedMember {
            member: member2.clone(),
            signature: Signature::from_bytes(&[0; 64]),
        };
        assert!(invalid_member2
            .verify_signature(&member1.member_vk)
            .is_err());
    }

    #[test]
    fn test_member_id() {
        let owner_id = MemberId(FastHash(0));
        let (member, _) = create_test_member(owner_id, owner_id);
        let member_id = member.id();

        assert_eq!(member_id, member.member_vk.into());
    }

    #[test]
    fn test_verify_self_invited_member() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (mut member, member_signing_key) = create_test_member(owner_id, owner_id);
        member.invited_by = member.id(); // Self-invite

        let authorized_member = AuthorizedMember::new(member, &member_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member],
        };

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = members.verify(&parent_state, &parameters);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Self-invitation detected"));
    }

    #[test]
    fn test_verify_circular_invite_chain() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (mut member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, member2_signing_key) = create_test_member(owner_id, member1.id());
        let (member3, member3_signing_key) = create_test_member(owner_id, member2.id());
        member1.invited_by = member3.id(); // Create a circular chain

        let authorized_member1 = AuthorizedMember::new(member1, &member3_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2, &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3, &member2_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member1, authorized_member2, authorized_member3],
        };

        let parent_state = ChatRoomStateV1::default();
        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = members.verify(&parent_state, &parameters);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("Circular invite chain detected"));
    }

    #[test]
    fn test_check_invite_chain() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        // Test case 1: Valid invite chain
        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, member2_signing_key) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member2.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2, &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3, &member2_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member1, authorized_member2.clone()],
        };

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = members.get_invite_chain(&authorized_member3, &parameters);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 2);

        // Test case 2: Circular invite chain
        let (mut circular_member1, circular_member1_signing_key) =
            create_test_member(owner_id, owner_id);
        let (circular_member2, circular_member2_signing_key) =
            create_test_member(owner_id, circular_member1.id());
        circular_member1.invited_by = circular_member2.id();

        let circular_authorized_member1 =
            AuthorizedMember::new(circular_member1, &circular_member2_signing_key);
        let circular_authorized_member2 =
            AuthorizedMember::new(circular_member2, &circular_member1_signing_key);

        let circular_members = MembersV1 {
            members: vec![
                circular_authorized_member1.clone(),
                circular_authorized_member2,
            ],
        };

        let result = circular_members.get_invite_chain(&circular_authorized_member1, &parameters);
        assert!(result.is_err());
        assert!(result
            .clone()
            .unwrap_err()
            .contains("Circular invite chain detected"));

        // Test case 3: Missing inviter
        let non_existent_inviter_id = MemberId(FastHash(999));
        let (orphan_member, _) = create_test_member(owner_id, non_existent_inviter_id);
        let orphan_authorized_member = AuthorizedMember {
            member: orphan_member,
            signature: Signature::from_bytes(&[0; 64]), // Use a dummy signature
        };

        let result = members.get_invite_chain(&orphan_authorized_member, &parameters);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("Inviter"), "Error message: {}", err);
        assert!(err.contains("not found"), "Error message: {}", err);

        // Test case 4: Invalid signature
        let (invalid_member, _) = create_test_member(owner_id, member1.id());
        let invalid_authorized_member = AuthorizedMember {
            member: invalid_member,
            signature: Signature::from_bytes(&[0; 64]),
        };

        let result = members.get_invite_chain(&invalid_authorized_member, &parameters);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid signature"));
    }

    #[test]
    fn test_remove_banned_members() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, member2_signing_key) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member2.id());
        let (member4, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3.clone(), &member2_signing_key);
        let authorized_member4 = AuthorizedMember::new(member4.clone(), &member1_signing_key);

        let mut members = MembersV1 {
            members: vec![
                authorized_member1.clone(),
                authorized_member2.clone(),
                authorized_member3.clone(),
                authorized_member4.clone(),
            ],
        };

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test case 1: No banned members
        let empty_bans = BansV1(vec![]);
        members.remove_owner_banned_members(&empty_bans, &parameters);
        assert_eq!(members.members.len(), 4);

        // Test case 2: One banned member
        let banned_member = UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::now(),
            banned_user: member2.id(),
        };
        let authorized_ban = AuthorizedUserBan::new(banned_member, owner_id, &owner_signing_key);
        let bans = BansV1(vec![authorized_ban]);
        members.remove_owner_banned_members(&bans, &parameters);
        assert_eq!(members.members.len(), 2);
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member1.id()));
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member4.id()));
        assert!(!members
            .members
            .iter()
            .any(|m| m.member.id() == member2.id()));
        assert!(!members
            .members
            .iter()
            .any(|m| m.member.id() == member3.id()));

        // Test case 3: Banning a member with no downstream members
        members = MembersV1 {
            members: vec![
                authorized_member1,
                authorized_member2,
                authorized_member3,
                authorized_member4,
            ],
        };
        let banned_member = UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::now(),
            banned_user: member4.id(),
        };
        let authorized_ban = AuthorizedUserBan::new(banned_member, owner_id, &owner_signing_key);
        let bans = BansV1(vec![authorized_ban]);
        members.remove_owner_banned_members(&bans, &parameters);
        assert_eq!(members.members.len(), 3);
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member1.id()));
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member2.id()));
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member3.id()));
        assert!(!members
            .members
            .iter()
            .any(|m| m.member.id() == member4.id()));
    }

    #[test]
    fn test_remove_excess_members() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, member2_signing_key) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member2.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3.clone(), &member2_signing_key);

        let mut members = MembersV1 {
            members: vec![authorized_member1, authorized_member2, authorized_member3],
        };

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test case 1: No excess members
        members.remove_excess_members(&parameters, 3);
        assert_eq!(members.members.len(), 3);

        // Test case 2: One excess member
        members.remove_excess_members(&parameters, 2);
        assert_eq!(members.members.len(), 2);
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member1.id()));
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member2.id()));
        assert!(!members
            .members
            .iter()
            .any(|m| m.member.id() == member3.id()));
    }

    #[test]
    fn test_members_by_member_id() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member2.clone()],
        };

        let members_map = members.members_by_member_id();

        assert_eq!(members_map.len(), 2);
        assert_eq!(
            members_map.get(&member1.id()).unwrap().member.id(),
            member1.id()
        );
        assert_eq!(
            members_map.get(&member2.id()).unwrap().member.id(),
            member2.id()
        );
    }

    #[test]
    fn test_invite_chain_length() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id: MemberId = owner_verifying_key.into();

        // Build a chain: owner -> m1 -> m2 -> m3
        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, member2_signing_key) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member2.id());

        let auth_m1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let auth_m2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let auth_m3 = AuthorizedMember::new(member3.clone(), &member2_signing_key);

        let members = MembersV1 {
            members: vec![auth_m1.clone(), auth_m2.clone(), auth_m3.clone()],
        };
        let members_by_id = members.members_by_member_id();

        // Depth 0: member directly invited by owner
        assert_eq!(
            MembersV1::invite_chain_length(&auth_m1, owner_id, &members_by_id),
            0
        );
        // Depth 1: one hop from owner
        assert_eq!(
            MembersV1::invite_chain_length(&auth_m2, owner_id, &members_by_id),
            1
        );
        // Depth 2: two hops from owner
        assert_eq!(
            MembersV1::invite_chain_length(&auth_m3, owner_id, &members_by_id),
            2
        );
    }

    #[test]
    fn test_members_apply_delta_complex() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member1.id());
        let (member4, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3.clone(), &member1_signing_key);
        let authorized_member4 = AuthorizedMember::new(member4.clone(), &member1_signing_key);

        let mut members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member2.clone()],
        };

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration.configuration.max_members = 3;

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test applying delta that would exceed max_members
        // Now ALL members are added first, then excess removed deterministically:
        // - member1 has shortest invite chain (invited by owner)
        // - member2, member3, member4 all have same chain length (invited by member1)
        // - One of member2/3/4 is removed based on highest MemberId (deterministic tie-breaker)
        let delta = MembersDelta {
            added: vec![authorized_member3.clone(), authorized_member4.clone()],
        };

        let result = members.apply_delta(&parent_state, &parameters, &Some(delta));
        assert!(result.is_ok());
        assert_eq!(members.members.len(), 3);
        // member1 is always kept (shortest invite chain)
        assert!(members
            .members
            .iter()
            .any(|m| m.member.id() == member1.id()));
        // Exactly 2 of [member2, member3, member4] are kept
        let kept_count = [member2.id(), member3.id(), member4.id()]
            .iter()
            .filter(|id| members.members.iter().any(|m| m.member.id() == **id))
            .count();
        assert_eq!(
            kept_count, 2,
            "Exactly 2 of the 3 equal-chain-length members should be kept"
        );

        // Test applying delta with already existing member
        let delta = MembersDelta {
            added: vec![authorized_member2.clone()],
        };

        let result = members.apply_delta(&parent_state, &parameters, &Some(delta));
        assert!(result.is_ok());
        assert_eq!(members.members.len(), 3);
    }

    #[test]
    fn test_remove_excess_members_edge_cases() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);

        let mut members = MembersV1 {
            members: vec![authorized_member1.clone(), authorized_member2.clone()],
        };

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test with max_members set to 0
        members.remove_excess_members(&parameters, 0);
        assert_eq!(members.members.len(), 0);

        // Reset members
        members.members = vec![authorized_member1.clone(), authorized_member2.clone()];

        // Test with max_members greater than current number of members
        members.remove_excess_members(&parameters, 3);
        assert_eq!(members.members.len(), 2);
    }

    #[test]
    #[should_panic(expected = "The member's invited_by must match the inviter's signing key")]
    fn test_authorized_member_new_mismatch() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member, _) = create_test_member(owner_id, owner_id);
        let wrong_signing_key = SigningKey::generate(&mut OsRng);

        AuthorizedMember::new(member, &wrong_signing_key);
    }

    #[test]
    fn test_members_verify_edge_cases() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration.configuration.max_members = 2;

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test with empty member list
        let empty_members = MembersV1 { members: vec![] };
        assert!(empty_members.verify(&parent_state, &parameters).is_ok());

        // Test with maximum allowed number of members
        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, _) = create_test_member(owner_id, member1.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);

        let max_members = MembersV1 {
            members: vec![authorized_member1, authorized_member2],
        };
        assert!(max_members.verify(&parent_state, &parameters).is_ok());

        // Test with members invited by non-existent members
        let non_existent_signing_key = SigningKey::generate(&mut OsRng);
        let non_existent_verifying_key = VerifyingKey::from(&non_existent_signing_key);
        let non_existent_id = non_existent_verifying_key.into();
        let (invalid_member, _) = create_test_member(owner_id, non_existent_id);
        let invalid_authorized_member =
            AuthorizedMember::new(invalid_member, &non_existent_signing_key);

        let invalid_members = MembersV1 {
            members: vec![invalid_authorized_member],
        };
        assert!(invalid_members.verify(&parent_state, &parameters).is_err());
    }

    #[test]
    fn test_room_owner_key_not_allowed_in_members() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let owner_member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: owner_verifying_key,
        };

        let authorized_owner_member = AuthorizedMember::new(owner_member, &owner_signing_key);

        let members = MembersV1 {
            members: vec![authorized_owner_member],
        };

        let mut parent_state = ChatRoomStateV1::default();
        parent_state.configuration.configuration.max_members = 2;

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        let result = members.verify(&parent_state, &parameters);
        assert!(
            result.is_err(),
            "Room owner should not be allowed in the members list"
        );
        assert!(result
            .unwrap_err()
            .contains("Owner should not be included in the members list"));
    }
}
