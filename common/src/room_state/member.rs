use crate::room_state::ban::BansV1;
use crate::room_state::member_info::MemberInfoV1;
use crate::room_state::ChatRoomParametersV1;
use crate::util::{sign_struct, truncated_base32, verify_struct};
use crate::ChatRoomStateV1;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use freenet_scaffold::util::{fast_hash, FastHash};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
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

            // Verify the full invite chain with Ed25519 signature checks
            self.get_invite_chain_with_lookup(member, parameters, &members_by_id)?;
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
            let work_bound = max_members.max(MIN_MEMBERS_DELTA_BOUND);
            if delta.added.len() > work_bound {
                return Err(format!(
                    "Members delta of {} exceeds {}",
                    delta.added.len(),
                    work_bound
                ));
            }

            // The owner is never in the members list (`verify` rejects a
            // record with the owner's id). Such a record in a delta is ignored
            // here rather than failing the delta, so the rest of the delta
            // applies the same way whatever else it carries.
            let owner_id = parameters.owner_id();
            let added: Vec<&AuthorizedMember> = delta
                .added
                .iter()
                .filter(|m| m.member.id() != owner_id)
                .collect();

            // Build a combined lookup map that includes both existing members
            // AND members being added in this delta. This is necessary because
            // during merge, a member and their inviter may both be in the delta
            // (e.g., member B invited by member A, both being added from the
            // other state). Without this, verify would fail with "Inviter not found".
            let mut combined_members_by_id = self.members_by_member_id();
            for member in &added {
                combined_members_by_id
                    .entry(member.member.id())
                    .or_insert(member);
            }

            // Verify that all new members have valid invites
            Self::verify_invite_chains(&added, parameters, &combined_members_by_id)?;

            // Add ALL new members (deduplicated), let remove_excess_members handle trimming.
            // This ensures CRDT convergence: regardless of delta order, the same set of
            // members will be kept based on the deterministic removal criteria.
            let mut held: HashSet<MemberId> = self.members.iter().map(|m| m.member.id()).collect();
            for member in added {
                // Skip if this member already exists
                if !held.insert(member.member.id()) {
                    continue;
                }
                self.members.push(member.clone());
            }
        }

        // Always check for and remove banned members. During apply_delta the
        // sibling `member_info` field (which carries deputy grants) has not
        // been applied yet — field ordering applies `members` before
        // `member_info` — so deputy authority cannot be evaluated correctly
        // here. We therefore pass an EMPTY `member_info`, enforcing only the
        // stable owner/ancestor authority. The full deputy-aware enforcement
        // (which needs the converged deputy state) runs afterwards in
        // `ChatRoomStateV1::post_apply_cleanup`. See #410.
        self.remove_banned_members(&parent_state.bans, &MemberInfoV1::default(), parameters);

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

impl MembersV1 {
    /// Verify the invite chain of every member in `added` against a lookup
    /// map that includes both existing members AND the delta's members, so
    /// inviters in the same delta can be found.
    ///
    /// A walk starting from the record `members_by_id` holds for its id stops
    /// at a record (compared in full, signature included) whose chain an
    /// earlier walk already verified: every record past the first is a lookup
    /// result, so the walks meet on the same records and stopping there gives
    /// the result the full walk would. Each such record's signature is
    /// checked at most once per call. A walk starting from another record
    /// with the same id is done in full, as before, since the full walk also
    /// fails when its own id reappears further up the chain.
    fn verify_invite_chains<'a>(
        added: &[&'a AuthorizedMember],
        parameters: &ChatRoomParametersV1,
        members_by_id: &HashMap<MemberId, &'a AuthorizedMember>,
    ) -> Result<(), String> {
        let owner_id = parameters.owner_id();
        let mut verified: HashSet<&'a AuthorizedMember> = HashSet::new();
        for member in added {
            let use_memo = members_by_id
                .get(&member.member.id())
                .is_some_and(|held| *held == *member);
            let mut path_ids = HashSet::new();
            let mut path: Vec<&'a AuthorizedMember> = Vec::new();
            let mut current: &'a AuthorizedMember = member;
            loop {
                if use_memo && verified.contains(current) {
                    break;
                }
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
                path.push(current);
                #[cfg(test)]
                tests::count_invite_signature_check();
                if current.member.invited_by == owner_id {
                    current.verify_signature(&parameters.owner).map_err(|e| {
                        format!(
                            "Invalid signature for member {:?} invited by owner: {}",
                            id, e
                        )
                    })?;
                    break;
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
                current = inviter;
            }
            if use_memo {
                verified.extend(path);
            }
        }
        Ok(())
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

    /// Checks if there are any banned members or members downstream of banned members in the invite chain
    pub fn has_banned_members(&self, bans_v1: &BansV1, parameters: &ChatRoomParametersV1) -> bool {
        self.check_banned_members(bans_v1, parameters).is_some()
    }

    /// Removes banned members or members downstream of banned members in the
    /// invite chain, for every currently-AUTHORIZED ban (see
    /// [`Self::banned_member_ids`]). A ban whose banner has no current
    /// authority (e.g. a revoked deputy, #410) is inert and removes nobody.
    fn remove_banned_members(
        &mut self,
        bans_v1: &BansV1,
        member_info: &MemberInfoV1,
        parameters: &ChatRoomParametersV1,
    ) {
        let banned_ids = self.banned_member_ids(bans_v1, member_info, parameters);
        self.members
            .retain(|m| !banned_ids.contains(&m.member.id()));
    }

    /// The set of member ids that must be removed because they are the target
    /// of (or downstream of the target of) a currently-authorized ban.
    ///
    /// This is the deputy-aware member cascade (#410). It is a **pure function
    /// of the converged `(members + member_info deputies + bans)` state**, so
    /// every peer computes the same removal set regardless of the order deltas
    /// arrived in — which is why enforcement lives here / in
    /// `ChatRoomStateV1::post_apply_cleanup`, NOT in `verify` (flipping ban
    /// validity inside `verify` would make it non-stable across deputy-state
    /// changes and break convergence). Bans themselves are never pruned by this
    /// (they remain an add-only CRDT tombstone set); revoking a deputy simply
    /// makes their bans stop being authorized, so the previously-removed
    /// members are no longer in this set and can rejoin.
    pub fn banned_member_ids(
        &self,
        bans_v1: &BansV1,
        member_info: &MemberInfoV1,
        parameters: &ChatRoomParametersV1,
    ) -> HashSet<MemberId> {
        let owner_id = parameters.owner_id();
        let members_by_id = self.members_by_member_id();
        let mut banned_ids = HashSet::new();
        for ban in &bans_v1.0 {
            // A ban only enforces if its signature verifies against the banner's
            // CURRENT converged key (#411 round 4 A). `verify` skips the signature
            // for a banner absent at bans-apply time (bans apply before members),
            // so a delta that re-adds a pruned deputy via their public
            // `AuthorizedMember` AND carries a garbage-signature ban attributed to
            // them must be re-checked here — otherwise the retained deputy grant
            // would remove members via a ban forged without the deputy's key. This
            // never rejects a genuine ban (a member's id is the hash of their key).
            if !BansV1::ban_signature_matches_current_key(
                ban,
                &members_by_id,
                owner_id,
                &parameters.owner,
            ) {
                continue;
            }
            if Self::is_ban_authorized(
                ban.banned_by,
                ban.ban.banned_user,
                &members_by_id,
                member_info,
                owner_id,
            ) {
                banned_ids.insert(ban.ban.banned_user);
                banned_ids.extend(self.get_downstream_members(ban.ban.banned_user));
            }
        }
        banned_ids
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
        // list, and `get_downstream_members(owner)` is the entire room, so an
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
        //    PRESENT members, so this grant
        //    already implies the banner is a current member.
        if strict_ancestors.contains(&banner) {
            return true;
        }

        // Deputy authority (steps 3 & 4) is granted to a banner ID only while
        // that banner is a CURRENT, signature-validated member (#411 round 3).
        // Otherwise a deputy who gets pruned leaves a stale non-member ID in
        // some `deputies` list, and any outsider could forge a ban
        // `banner=<stale id>` with a garbage signature (which `verify` skips for
        // non-member banners) and have it honored as authorized — removing an
        // arbitrary member + subtree. Requiring current membership closes that:
        // a stale/forged deputy ID grants nothing, and a present member's ban
        // signature is verified in `verify`.
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

    /// Helper function to get all downstream members of a given member
    fn get_downstream_members(&self, member_id: MemberId) -> HashSet<MemberId> {
        let mut downstream = HashSet::new();
        let mut to_check = vec![member_id];
        while let Some(current) = to_check.pop() {
            for member in &self.members {
                if member.member.invited_by == current {
                    downstream.insert(member.member.id());
                    to_check.push(member.member.id());
                }
            }
        }
        downstream
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

    /// Checks for banned members and returns a set of member IDs to be removed if any are found.
    /// Uses chain walking without Ed25519 verification since we only need to check membership,
    /// not cryptographic validity.
    fn check_banned_members(
        &self,
        bans_v1: &BansV1,
        parameters: &ChatRoomParametersV1,
    ) -> Option<HashSet<MemberId>> {
        let banned_user_ids: HashSet<MemberId> =
            bans_v1.0.iter().map(|b| b.ban.banned_user).collect();
        if banned_user_ids.is_empty() {
            return None;
        }

        let members_by_id = self.members_by_member_id();
        let owner_id = parameters.owner_id();
        let mut result = HashSet::new();

        for m in &self.members {
            // Walk the invite chain without Ed25519 verification
            let chain_ids = Self::invite_chain_ids(m, owner_id, &members_by_id);
            if chain_ids.iter().any(|id| banned_user_ids.contains(id)) {
                result.insert(m.member.id());
            }
        }

        if result.is_empty() {
            None
        } else {
            Some(result)
        }
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
    fn get_invite_chain_with_lookup(
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

    /// Walk the invite chain and return all member IDs in the chain WITHOUT Ed25519 verification.
    /// Used by `check_banned_members` where we only need to check if any chain member is banned.
    fn invite_chain_ids(
        member: &AuthorizedMember,
        owner_id: MemberId,
        members_by_id: &HashMap<MemberId, &AuthorizedMember>,
    ) -> Vec<MemberId> {
        let mut chain_ids = vec![member.member.id()];
        let mut current_id = member.member.invited_by;
        let mut visited = HashSet::new();
        visited.insert(member.member.id());

        while current_id != owner_id {
            if !visited.insert(current_id) {
                break;
            }
            chain_ids.push(current_id);
            match members_by_id.get(&current_id) {
                Some(inviter) => current_id = inviter.member.invited_by,
                None => break,
            }
        }
        chain_ids
    }
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
mod tests {
    use super::*;
    use crate::room_state::ban::{AuthorizedUserBan, UserBan};
    use crate::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use std::time::SystemTime;

    fn create_test_member(owner_id: MemberId, invited_by: MemberId) -> (Member, SigningKey) {
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
    }

    pub(super) fn count_invite_signature_check() {
        INVITE_SIGNATURE_CHECKS.with(|c| c.set(c.get() + 1));
    }

    fn invite_signature_checks() -> usize {
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
    /// whatever else in the delta shares its id, in either order.
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
        assert!(apply(held.clone(), vec![r.y.clone(), r.other.clone()]).is_err());
        assert!(apply(held.clone(), vec![r.other.clone(), r.y.clone()]).is_err());
        assert!(apply(held, vec![r.y.clone()]).is_ok());
    }

    /// A second record for a held member's key, validly signed by a member
    /// further down the held record's own subtree, makes the chain return to
    /// its own id. The delta is rejected in either order, also when an earlier
    /// walk in the same delta already verified the held record.
    #[test]
    fn a_record_whose_chain_returns_to_its_own_id_is_rejected() {
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
            vec![s],
        ] {
            let mut members = MembersV1 {
                members: held.clone(),
            };
            let result =
                members.apply_delta(&parent_state, &parameters, &Some(MembersDelta::new(added)));
            assert!(result.is_err(), "{result:?}");
        }
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
    fn test_has_banned_members() {
        let owner_signing_key = SigningKey::generate(&mut OsRng);
        let owner_verifying_key = VerifyingKey::from(&owner_signing_key);
        let owner_id = owner_verifying_key.into();

        let (member1, member1_signing_key) = create_test_member(owner_id, owner_id);
        let (member2, member2_signing_key) = create_test_member(owner_id, member1.id());
        let (member3, _) = create_test_member(owner_id, member2.id());

        let authorized_member1 = AuthorizedMember::new(member1.clone(), &owner_signing_key);
        let authorized_member2 = AuthorizedMember::new(member2.clone(), &member1_signing_key);
        let authorized_member3 = AuthorizedMember::new(member3.clone(), &member2_signing_key);

        let members = MembersV1 {
            members: vec![authorized_member1, authorized_member2, authorized_member3],
        };

        let parameters = ChatRoomParametersV1 {
            owner: owner_verifying_key,
        };

        // Test case 1: No banned members
        let empty_bans = BansV1(vec![]);
        assert!(!members.has_banned_members(&empty_bans, &parameters));

        // Test case 2: One banned member
        let banned_member = UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::now(),
            banned_user: member2.id(),
        };
        let authorized_ban = AuthorizedUserBan::new(banned_member, owner_id, &owner_signing_key);
        let bans = BansV1(vec![authorized_ban]);
        assert!(members.has_banned_members(&bans, &parameters));
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
        members.remove_banned_members(&empty_bans, &MemberInfoV1::default(), &parameters);
        assert_eq!(members.members.len(), 4);

        // Test case 2: One banned member
        let banned_member = UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::now(),
            banned_user: member2.id(),
        };
        let authorized_ban = AuthorizedUserBan::new(banned_member, owner_id, &owner_signing_key);
        let bans = BansV1(vec![authorized_ban]);
        members.remove_banned_members(&bans, &MemberInfoV1::default(), &parameters);
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
        members.remove_banned_members(&bans, &MemberInfoV1::default(), &parameters);
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
    fn test_invite_chain_ids() {
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

        // m1 (depth 0): chain is just [m1] (no ancestors other than owner)
        let ids = MembersV1::invite_chain_ids(&auth_m1, owner_id, &members_by_id);
        assert_eq!(ids, vec![member1.id()]);

        // m2 (depth 1): chain is [m2, m1]
        let ids = MembersV1::invite_chain_ids(&auth_m2, owner_id, &members_by_id);
        assert_eq!(ids, vec![member2.id(), member1.id()]);

        // m3 (depth 2): chain is [m3, m2, m1]
        let ids = MembersV1::invite_chain_ids(&auth_m3, owner_id, &members_by_id);
        assert_eq!(ids, vec![member3.id(), member2.id(), member1.id()]);
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
