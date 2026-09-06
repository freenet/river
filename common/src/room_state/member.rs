use crate::room_state::ban::{BanSignatureCache, BansV1};
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
        // Memoized across members so each member's own invite signature is
        // verified ONCE, instead of once per descendant that walks through it
        // (freenet/river#422). See [`InviteChainCache`].
        let mut chains = InviteChainCache::new(parameters, &members_by_id);

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
            chains.validate_chain(member)?;
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
            // Build a combined lookup map that includes both existing members
            // AND members being added in this delta. This is necessary because
            // during merge, a member and their inviter may both be in the delta
            // (e.g., member B invited by member A, both being added from the
            // other state). Without this, verify would fail with "Inviter not found".
            let mut combined_members_by_id = self.members_by_member_id();
            for member in &delta.added {
                combined_members_by_id
                    .entry(member.member.id())
                    .or_insert(member);
            }

            // Verify that all new members have valid invites
            let mut chains = InviteChainCache::new(parameters, &combined_members_by_id);
            for member in &delta.added {
                Self::verify_member_invite_with_lookup(&mut chains, member, parameters)?;
            }

            // Add ALL new members (deduplicated), let remove_excess_members handle trimming.
            // This ensures CRDT convergence: regardless of delta order, the same set of
            // members will be kept based on the deterministic removal criteria.
            for member in &delta.added {
                // Skip if this member already exists
                if self
                    .members
                    .iter()
                    .any(|m| m.member.id() == member.member.id())
                {
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

impl MembersV1 {
    /// Verify a member's invite chain using a pre-built lookup map.
    /// The lookup map should include both existing members AND delta members
    /// when called during apply_delta, so that inviters in the same delta
    /// can be found.
    ///
    /// The owner-invited fast path is DELIBERATELY kept distinct from the
    /// chain walk (and is not memoized): it checks only the signature, where
    /// the walk additionally rejects self-invitation and cycles. Collapsing the
    /// two would change which deltas `apply_delta` accepts.
    fn verify_member_invite_with_lookup(
        chains: &mut InviteChainCache<'_>,
        member: &AuthorizedMember,
        parameters: &ChatRoomParametersV1,
    ) -> Result<(), String> {
        if member.member.invited_by == parameters.owner_id() {
            // Member was invited by the owner, verify signature against owner's key
            member
                .verify_signature(&parameters.owner)
                .map_err(|e| format!("Invalid signature for member invited by owner: {}", e))?;
        } else {
            // Member was invited by another member, verify the invite chain
            chains.validate_chain(member)?;
        }
        Ok(())
    }
}

/// Memoizes invite-chain validation within one `verify` / `apply_delta`.
///
/// [`MembersV1::get_invite_chain_with_lookup`] walks from a member up to the
/// room owner, Ed25519-verifying every link on the way. Running that per member
/// re-verifies each ancestor's invite signature once for every descendant, so
/// validating a room of `M` members whose invite tree is `D` deep costs
/// `O(sum of chain lengths)` — up to `O(M^2)` — verifications where `O(M)`
/// suffices, because a member's own invite signature is the same signature no
/// matter who walks through it.
///
/// This is the `validate_state`-side half of freenet/river#422 (`update_state`
/// never calls `verify`; it pays this only for the members a delta ADDS).
///
/// How much it saves depends entirely on the invite tree, and the honest range
/// is wide. Measured by counting calls into `verify_struct`, for
/// `MembersV1::verify`:
///
/// * The live Freenet Official room is a near-flat star — owner -> invite bot ->
///   everyone — with invite depth max 4 and mean 2.02 over 496 members
///   (2026-07-29, `cli/examples/invite_depth_probe.rs`). 1003 verifications
///   before, 496 after: **2x**, and only ~7% of that room's whole
///   `validate_state` because its 2000 messages dominate.
/// * A synthetic 400-member room at invite depth 50 needs 10,700 before and 900
///   after (**12x**, 4.52 s -> 0.38 s); at depth 200, 40,700 before
///   (**45x**, 17.19 s). Those breach the 5s budget — but no room has been
///   measured with a tree that deep, so treat them as the shape of the curve,
///   not as a characterisation of any real room.
///
/// # Why memoizing is exactly equivalent
///
/// "Member `X`'s chain is valid" is a property of `X`, its ancestors, the room
/// OWNER, and the `members_by_id` the ancestors resolve through. The first
/// three are fixed for a cache (see "Why the context is bound at
/// construction"), and the fourth is `X` itself — so a verdict travels between
/// walks correctly, with ONE exception, handled by the non-canonical-start
/// bypass in [`Self::validate_chain`]: the start's `MemberId` seeds the cycle
/// guard, so a walk starting at a non-canonical duplicate can terminate with a
/// circular-chain error that a memo hit would suppress. Such a start neither
/// reads nor writes the memo. (An earlier version of this comment claimed "the
/// starting member of a walk never enters the checks". That was false, and it
/// is the bug this bypass fixes.)
///
/// For every other start, caching the walk's verdict per member and reusing it
/// for descendants yields the same verdict the full walk would produce. Every
/// node on a completed walk is recorded with that walk's verdict, which is
/// sound in both directions:
///
/// * `Ok` — every node between the start and the owner had its own signature
///   verified and a valid chain above it, which is exactly what a walk started
///   at that node would check.
/// * `Err` — the failure happened at some node `E` on the path, so every node
///   BELOW `E` also has an invalid chain, and its walk would fail at `E` with
///   the very same message (the messages name `E`, not the starting member).
///
/// Errors are surfaced by the FIRST failing member in iteration order and the
/// callers propagate with `?`, so a later member can never observe a memoized
/// error that a fresh walk would have phrased differently — the operation has
/// already returned.
///
/// # Why the key is the whole member, and NOT its `MemberId`
///
/// `MemberId` is a 64-bit `fast_hash` of a verifying key. Keying the memo on it
/// would let an attacker who grinds a `MemberId` collision between two keypairs
/// they generated (a birthday search, ~2^33 work — not 2^64) get a SECOND,
/// forged member entry admitted on the first one's verdict, with its own invite
/// signature never checked. That would let them forge their position in the
/// invite tree — claiming to be invited by the owner, or by whoever's subtree
/// confers ban authority — which is precisely what this check exists to
/// prevent. Ancestor RESOLUTION is still `MemberId`-keyed via `members_by_id`,
/// exactly as it was before; this only keeps the memo from being a new way in.
/// `AuthorizedMember`'s `Hash` covers only `member`, but its `Eq` is derived
/// over every field, so a hash collision costs a bucket probe rather than a
/// wrong answer. The struct is inline data (no heap), so keying on a clone is a
/// flat memcpy — ~272 bytes, since `VerifyingKey` carries a decompressed
/// 160-byte `EdwardsPoint` alongside the 32 compressed ones — against a
/// ~422 us verification.
///
/// # Why the context is bound at construction
///
/// A verdict is only meaningful relative to the room OWNER it was earned under
/// and the `members_by_id` map its ancestors were resolved through. Both are
/// therefore taken by [`InviteChainCache::new`] and held for the cache's whole
/// life, so one instance is structurally incapable of spanning two contexts.
///
/// This is not hypothetical tidiness — the type is `pub`, and there are two
/// distinct ways a shared instance would hand out an unearned `Ok`:
///
/// * **Across rooms.** `Ok` means "this chain reaches THE OWNER". A member
///   validated under owner A would inherit that verdict under owner B, whose
///   key never signed anything in the chain.
/// * **Across `verify` and `apply_delta` within one room.** `apply_delta`
///   deliberately resolves inviters through a map that also contains the
///   delta's not-yet-verified members, so the same member can earn a WEAKER
///   `Ok` there than `verify`'s stricter map would grant. Sharing would leak
///   the weaker verdict into the stronger context.
///
/// A stored-owner + `debug_assert` would not catch either: the room contract
/// ships as a release build, where debug assertions are compiled out. Binding
/// the borrow is checked by the compiler instead, in every build.
#[derive(Debug)]
pub struct InviteChainCache<'a> {
    /// The owner every `Ok` in `verdicts` was earned against.
    parameters: &'a ChatRoomParametersV1,
    /// The map every ancestor in `verdicts` was resolved through.
    members_by_id: &'a HashMap<MemberId, &'a AuthorizedMember>,
    verdicts: HashMap<AuthorizedMember, Result<(), String>>,
    verifications: usize,
}

impl<'a> InviteChainCache<'a> {
    /// Binds a cache to one room owner and one member lookup. Every verdict it
    /// memoizes is earned, and reused, under exactly these.
    pub fn new(
        parameters: &'a ChatRoomParametersV1,
        members_by_id: &'a HashMap<MemberId, &'a AuthorizedMember>,
    ) -> Self {
        Self {
            parameters,
            members_by_id,
            verdicts: HashMap::new(),
            verifications: 0,
        }
    }

    /// How many Ed25519 verifications this cache has actually performed.
    ///
    /// Exposed so the regression tests for freenet/river#422 can assert the
    /// `O(members x depth)` blow-up is gone DETERMINISTICALLY, rather than by
    /// timing (which varies with the machine and would be flaky in CI).
    pub fn verifications_performed(&self) -> usize {
        self.verifications
    }

    /// Memoized equivalent of [`MembersV1::get_invite_chain_with_lookup`]'s
    /// validation (it discards the assembled chain, which no caller of this
    /// path uses).
    pub fn validate_chain(&mut self, member: &AuthorizedMember) -> Result<(), String> {
        let parameters = self.parameters;
        let members_by_id = self.members_by_id;
        let owner_id = parameters.owner_id();

        // The start of a walk is NOT purely an input to its ancestors' checks:
        // its `MemberId` seeds `visited` below, so the original walk can
        // terminate with a circular-chain error that exists only because of
        // where the walk began. The memo is therefore unsound for exactly one
        // class of start — a NON-CANONICAL duplicate, i.e. an entry whose
        // `MemberId` resolves through `members_by_id` to a DIFFERENT
        // `AuthorizedMember`. For such a start a memo hit can short-circuit
        // above the point where the original re-entered the start's own id and
        // errored. Every node reached AFTER the start comes out of
        // `members_by_id` and is canonical by construction, so the start is the
        // only place this can arise.
        //
        // Concretely (freenet/river#422 review): B, invited by A, mints a
        // second entry for A's key claiming B invited it. Walking B memoizes
        // `B -> Ok`; the duplicate's walk then hits that memo and returns `Ok`
        // where the original walked on into A's already-visited id and errored.
        // Note that moving the memo lookup below the cycle guard does NOT fix
        // this — the hit lands on B before the walk ever reaches A again.
        //
        // Such a start bypasses the memo entirely: it neither reads a verdict
        // nor writes one, so it runs as the untouched original walk and cannot
        // leave a start-dependent verdict behind for anyone else. Duplicates
        // are rare, so this costs one hash lookup per walk and does not affect
        // the `O(members)` bound in any ordinary state.
        let start_is_canonical = members_by_id
            .get(&member.member.id())
            .is_none_or(|canonical| *canonical == member);
        // Nodes walked during THIS call, all of which share the walk's verdict.
        let mut walked: Vec<AuthorizedMember> = Vec::new();
        // Cycle guard, scoped to this walk exactly as in the uncached version.
        let mut visited: HashSet<MemberId> = HashSet::new();
        let mut current = member;

        // A cycle verdict is the ONE outcome that depends on where the walk
        // STARTED — it names the first node the walk revisits, which is the
        // point at which this particular walk entered the cycle. Every other
        // outcome names a specific offending node, or is a plain `Ok`, and is
        // therefore identical whoever asks. So cycle verdicts are not memoized.
        // That costs nothing in practice: a cyclic member set is invalid, and
        // both `MembersV1::verify` and `MembersV1::apply_delta` propagate the
        // first error with `?`, so at most one such walk ever runs.
        let mut cyclic = false;

        let verdict = loop {
            let current_id = current.member.id();

            if start_is_canonical {
                if let Some(memoized) = self.verdicts.get(current) {
                    break memoized.clone();
                }
            }

            if !visited.insert(current_id) {
                cyclic = true;
                break Err(format!(
                    "Circular invite chain detected for member {:?}",
                    current_id
                ));
            }

            if current.member.invited_by == current_id {
                break Err(format!(
                    "Self-invitation detected for member {:?}",
                    current_id
                ));
            }

            walked.push(current.clone());

            if current.member.invited_by == owner_id {
                self.verifications += 1;
                break current.verify_signature(&parameters.owner).map_err(|e| {
                    format!(
                        "Invalid signature for member {:?} invited by owner: {}",
                        current_id, e
                    )
                });
            }

            let inviter = match members_by_id.get(&current.member.invited_by) {
                Some(inviter) => *inviter,
                None => {
                    break Err(format!(
                        "Inviter {:?} not found for member {:?}",
                        current.member.invited_by, current_id
                    ))
                }
            };

            self.verifications += 1;
            if let Err(e) = current.verify_signature(&inviter.member.member_vk) {
                break Err(format!(
                    "Invalid signature for member {:?}: {}",
                    current_id, e
                ));
            }

            current = inviter;
        };

        if start_is_canonical && !cyclic {
            for member in walked {
                self.verdicts.insert(member, verdict.clone());
            }
        }
        verdict
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
        self.banned_member_ids_cached(
            &mut BanSignatureCache::new(),
            bans_v1,
            member_info,
            parameters,
        )
    }

    /// [`Self::banned_member_ids`] against a caller-owned
    /// [`BanSignatureCache`]. `post_apply_cleanup` shares one cache across all
    /// of its ban passes so each ban's Ed25519 signature is verified once per
    /// apply instead of once per pass (freenet/river#422).
    pub fn banned_member_ids_cached(
        &self,
        cache: &mut BanSignatureCache,
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
            if !cache.ban_signature_matches_current_key(
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
    /// Grants are checked in priority order. The ABSOLUTE grants come FIRST so
    /// they cannot be stripped by the target "self-immunizing" (a spammer
    /// listing the moderator in their OWN `deputies` to make the mod's ban go
    /// inert):
    /// 1. `banner` is the room owner — absolute.
    /// 2. `banner` is a STRICT ancestor of `target` in the invite tree ("you can
    ///    ban your own subtree") — absolute.
    /// 3. `banner` is an owner-appointed global moderator (`owner`'s `deputies`
    ///    list `banner`) — absolute (the owner's subtree is everyone).
    ///
    /// Only then the deputy-derived branch (authority via a NON-owner ancestor),
    /// which the guardrail applies to:
    /// 4. Guardrail ("cannot ban the member who deputized you"): if `target`
    ///    currently lists `banner` in `target.deputies`, DENY — a deputy cannot
    ///    ban a fellow deputizer. Checked AFTER the absolute grants, so a genuine
    ///    ancestor / owner-appointed mod keeps authority even if the target
    ///    deputizes them.
    /// 5. Some strict NON-owner ancestor `A` of `target` lists `banner` in
    ///    `A.deputies` — deputy authority scoped to `A`'s subtree.
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
        // outright, before any grant. (This guard is load-bearing since the
        // reorder below puts the owner-global-mod grant ahead of the
        // deputizer guardrail that previously masked this case.)
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

        // 2. Genuine strict ancestor — absolute (cannot be self-immunized away).
        //    The chain walk above only reaches PRESENT members, so this grant
        //    already implies the banner is a current member.
        if strict_ancestors.contains(&banner) {
            return true;
        }

        // Deputy authority (steps 3 & 5) is granted to a banner ID only while
        // that banner is a CURRENT, signature-validated member (#411 round 3).
        // Otherwise a deputy who gets pruned leaves a stale non-member ID in
        // some `deputies` list, and any outsider could forge a ban
        // `banner=<stale id>` with a garbage signature (which `verify` skips for
        // non-member banners) and have it honored as authorized — removing an
        // arbitrary member + subtree. Requiring current membership closes that:
        // a stale/forged deputy ID grants nothing, and a present member's ban
        // signature is verified in `verify`.
        let banner_is_member = members_by_id.contains_key(&banner);

        // 3. Owner-appointed global moderator — absolute (among members).
        if banner_is_member && member_info.deputies_of(owner_id).contains(&banner) {
            return true;
        }
        // 4. Guardrail: a deputy cannot ban a member who currently deputizes
        //    them (a fellow deputizer). Only reachable once the absolute grants
        //    above have been ruled out.
        if member_info.deputies_of(target).contains(&banner) {
            return false;
        }
        // 5. Deputy authority via a strict NON-owner ancestor of target.
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
