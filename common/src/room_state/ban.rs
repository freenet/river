use crate::room_state::ban_evidence::{BanEvidenceV1, EvidenceDigest};
use crate::room_state::member::{AuthorizedMember, BanContext, BanResolution, MemberId, MembersV1};
use crate::room_state::member_info::MemberInfoV1;
use crate::room_state::ChatRoomParametersV1;
use crate::util::{sign_struct, verify_struct};
use crate::ChatRoomStateV1;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use freenet_scaffold::util::{fast_hash, FastHash};
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::SystemTime;

/// Represents a collection of user bans in a chat room
///
/// This structure maintains a list of authorized bans and provides methods
/// to verify, summarize, and apply changes to the ban list while ensuring
/// all bans are valid according to room rules.
///
/// # Ban validity & the un-ban DoS (#411)
///
/// Which bans take effect is decided from converged state by
/// [`MembersV1::resolve_bans`]: a ban whose issuer is themselves removed does
/// not take effect, except in a mutual ban, where only the cycle's members
/// are removed; and the owner's ban wins over everything (freenet/river#423,
/// #702).
///
/// A ban is only honored while its issuer's record is known: the owner, a
/// current member, or a member held in `ban_evidence` (a removed or pruned
/// member whom a stored ban still references), and its signature must verify
/// against that record's key. [`crate::room_state::ChatRoomStateV1::post_apply_cleanup`]
/// sweeps any ban whose issuer is known nowhere, and any ban whose issuer is
/// deeper in the invite tree than the ban evidence depth cap. The
/// `max_user_bans` cap is enforced there too, in a fixed authority order
/// (owner bans first, up to `max_user_bans / 2`, then by how high in the
/// invite tree each ban's authority sits; see [`Self::enforce_user_ban_cap`]),
/// and [`ComposableState::apply_delta`] bounds a single delta to
/// `max_user_bans` new bans so signature verification stays bounded.
///
/// An owner ban on a member makes all of that member's bans inert, and the
/// cap then keeps them only in slots no ban that takes effect needs.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct BansV1(pub Vec<AuthorizedUserBan>);

impl BansV1 {
    /// Signature checks only: every ban whose banner is the owner, a member
    /// of `parent_state` or held in its ban evidence must verify against that
    /// banner's key. A ban whose banner is known nowhere is skipped, because
    /// the key is not available here (see the loop body). Used by both
    /// `verify` and `apply_delta`.
    ///
    /// There is deliberately NO orphaned-ban rule any more (freenet/river#423,
    /// #702): whether a ban takes effect, including "its issuer is banned", is
    /// decided from converged state by `MembersV1::resolve_bans`.
    fn verify_ban_signatures(
        &self,
        parent_state: &ChatRoomStateV1,
        parameters: &ChatRoomParametersV1,
    ) -> Result<(), String> {
        let members_by_id = parent_state
            .ban_evidence
            .lookup(&parent_state.members, parameters.owner_id());
        let owner_vk = parameters.owner;
        let owner_id = parameters.owner_id();

        // Verify signatures for all bans.
        for ban in &self.0 {
            if ban.banned_by == owner_id {
                ban.verify_signature(&owner_vk)
                    .map_err(|e| format!("Invalid ban signature: {}", e))?;
            } else if let Some(banning_member) = members_by_id.get(&ban.banned_by) {
                ban.verify_signature(&banning_member.member.member_vk)
                    .map_err(|e| format!("Invalid ban signature: {}", e))?;
            } else {
                // Banning member's record is neither in `members` nor in ban
                // evidence. This happens during a merge, when bans apply before
                // the members delta. Skip signature verification: there is no
                // key to check against. post_apply_cleanup step 4 sweeps every
                // ban whose banner's record is known nowhere, and enforcement
                // re-checks the signature (`ban_signature_matches_current_key`).
            }
        }

        Ok(())
    }

    /// Re-verify `ban`'s signature against the banner's CURRENT converged key
    /// (#411 round 4 item A). Returns `true` iff the banner is the owner or a
    /// current member AND the stored signature verifies against that banner's
    /// current verifying key; `false` for a non-member banner (unverifiable) or a
    /// signature that does not match — a forged/replayed ban.
    ///
    /// This closes the SAME-DELTA replay bypass. Field order applies `bans`
    /// before `members`, so `verify` SKIPS the signature check for a banner that
    /// is absent from the parent state at bans-apply time (see
    /// `verify_ban_signatures`). A single delta that re-adds a pruned deputy via
    /// their PUBLIC, replayable `AuthorizedMember` AND carries a garbage-signature
    /// ban attributed to that deputy would otherwise have the ban ENFORCED by
    /// `post_apply_cleanup` once the deputy is a current member (the retained
    /// deputy grant authorizes it) — a ban forged without the deputy's private
    /// key. ENFORCEMENT re-checks the signature against the converged key, so the
    /// apply-time skip no longer matters. It can never false-reject a genuine
    /// ban: a `MemberId` is the hash of its verifying key, so a current member's
    /// key is exactly the one that produced their id, and a ban legitimately
    /// signed by that member always verifies.
    ///
    /// `verify` itself is deliberately NOT tightened (that would strand the
    /// Official room's migration PUT, which carries legitimately sig-skipped
    /// non-member-banner bans); the check lives only in enforcement.
    pub fn ban_signature_matches_current_key(
        ban: &AuthorizedUserBan,
        members_by_id: &HashMap<MemberId, &AuthorizedMember>,
        owner_id: MemberId,
        owner_vk: &VerifyingKey,
    ) -> bool {
        let banner = ban.banned_by;
        let vk = if banner == owner_id {
            *owner_vk
        } else if let Some(member) = members_by_id.get(&banner) {
            member.member.member_vk
        } else {
            // Non-member banner: unverifiable here (key unavailable). Treated as
            // not-matching so callers classify it inert / sweep it.
            return false;
        };
        ban.verify_signature(&vk).is_ok()
    }

    /// Drop every ban that no key known here verifies: one whose issuer is
    /// the owner or a record in `members_by_id` and whose key does not verify
    /// it, and one whose issuer is known nowhere (cleanup step 4 sweeps that
    /// one in the same pass). Both `post_apply_cleanup` (before the cap
    /// counts) and the DM apply's enforced-ban set run this when the bans are
    /// over the cap, so such a ban never takes a cap slot from a stored one
    /// (freenet/river#703). Returns whether anything was dropped.
    pub fn drop_bans_no_known_key_verifies(
        bans: &mut Vec<AuthorizedUserBan>,
        members_by_id: &HashMap<MemberId, &AuthorizedMember>,
        owner_id: MemberId,
        owner_vk: &VerifyingKey,
    ) -> bool {
        let before = bans.len();
        bans.retain(|b| {
            Self::ban_signature_matches_current_key(b, members_by_id, owner_id, owner_vk)
        });
        bans.len() != before
    }

    /// The records a ban's issuer, target and depth are looked up in by one
    /// cleanup pass, fixed at the start of the pass so nothing the pass
    /// later decides about membership changes them: every record of the pool
    /// (present members plus ban evidence) whose invite chain reaches the
    /// owner through the pool within `depth_cap`. A ban whose issuer sits
    /// deeper than that, present or not, counts for nothing: the cap does not
    /// keep it and cleanup step 4 sweeps it. Judging present and absent
    /// issuers by one depth rule is what keeps a ban's standing from changing
    /// when the pass that removes its issuer moves the issuer's record out of
    /// `members`.
    pub(crate) fn stable_known<'a>(
        members: &'a MembersV1,
        evidence: &'a BanEvidenceV1,
        owner_id: MemberId,
        depth_cap: usize,
    ) -> HashMap<MemberId, &'a AuthorizedMember> {
        let pool = evidence.lookup(members, owner_id);
        pool.iter()
            .filter(|(_, m)| super::ban_evidence::within_depth(m, &pool, owner_id, depth_cap))
            .map(|(id, m)| (*id, *m))
            .collect()
    }

    /// Which of `bans` the room enforces: THE single decision cleanup step
    /// 0 makes and the DM apply's enforced-ban set reuses (freenet/river#675).
    /// Drops every ban no key in [`Self::stable_known`] verifies (step
    /// 0-sig, on every pass), keeps at most `max_bans` of the rest in the
    /// authority order ([`Self::enforce_user_ban_cap`], step 0-cap), and
    /// resolves what is kept ([`MembersV1::resolve_bans`], step 0). `bans`
    /// is left as the kept set.
    pub(crate) fn decide_enforced(
        bans: &mut Vec<AuthorizedUserBan>,
        max_bans: usize,
        depth_cap: usize,
        members: &MembersV1,
        evidence: &BanEvidenceV1,
        member_info: &MemberInfoV1,
        parameters: &ChatRoomParametersV1,
    ) -> BanResolution {
        let owner_id = parameters.owner_id();
        Self::drop_bans_no_known_key_verifies(
            bans,
            &Self::stable_known(members, evidence, owner_id, depth_cap),
            owner_id,
            &parameters.owner,
        );
        Self::enforce_user_ban_cap(
            bans,
            max_bans,
            depth_cap,
            members,
            evidence,
            member_info,
            parameters,
        );
        let kept = BansV1(std::mem::take(bans));
        let resolution = members.resolve_bans(evidence, &kept, member_info, parameters);
        *bans = kept.0;
        resolution
    }

    /// Whether a ban signed by `issuer` can take effect in a room with
    /// `max_members`: the issuer is the owner, or a member whose invite chain
    /// reaches the owner through `members` within the ban evidence depth cap
    /// ([`super::ban_evidence::evidence_depth_cap`]). Cleanup drops a ban by
    /// a deeper issuer before ban resolution sees it (step 0-sig reads
    /// [`Self::stable_known`]), so clients use this to refuse to sign one.
    pub fn issuer_within_depth_cap(
        issuer: MemberId,
        members: &MembersV1,
        owner_id: MemberId,
        max_members: usize,
    ) -> bool {
        if issuer == owner_id {
            return true;
        }
        let by_id = members.members_by_member_id();
        by_id.get(&issuer).is_some_and(|m| {
            super::ban_evidence::within_depth(
                m,
                &by_id,
                owner_id,
                super::ban_evidence::evidence_depth_cap(max_members),
            )
        })
    }

    /// THE single definition of the `max_user_bans` cap. Two sites need to
    /// know which bans survive it: `post_apply_cleanup` step 0-cap, which
    /// stores the result, and `DirectMessagesV1::apply_delta`, which sweeps
    /// DMs against the resulting enforced-ban set (freenet/river#675). One
    /// function is what makes them agree by construction. The network-delta
    /// trim uses it too, at the delta's bound.
    ///
    /// A no-op when the bans fit the cap, so a room under its cap keeps
    /// every ban. Over the cap, the kept set is chosen greedily over ONE
    /// fixed order, so it does not depend on any ban the cap rejects:
    ///
    /// - The order (the "authority order"): owner bans first, newest first;
    ///   then every other ban by how high in the invite tree its authority
    ///   sits. A ban on a target whose record is known ranks at the target's
    ///   depth minus one (its authority, an ancestor or a deputy's granter,
    ///   sits strictly above the target); a ban on an id known nowhere ranks
    ///   at its issuer's depth. Owner bans rank at level 0 ahead of every
    ///   other ban there; otherwise the newer ban comes first within a level.
    ///   Ties in time are broken by `(banned_at, id, record digest)`, the
    ///   canonical stored order.
    /// - A ban is judged over the bans accepted so far plus itself, by the
    ///   full [`MembersV1::resolve_bans`]. It is LIVE if that gives it a tier
    ///   above [`BanResolution::INERT`], or if its target is known nowhere,
    ///   its issuer is a known member (not the owner) and no ban removes the
    ///   issuer (such a ban removes nobody until the target returns). It is
    ///   SAFE if no accepted ban's issuer, nor its own, becomes owner-removed,
    ///   so step 4 never sweeps a ban the cap kept.
    /// - Phase 1: owner bans, live and safe, up to `max_bans / 2`; then the
    ///   other bans, live and safe, up to `max_bans / 2`. Phase 2: the rest
    ///   in order, live and safe, up to `max_bans`. In phases 1 and 2 a
    ///   second ban for an already accepted `(issuer, target)` pair is
    ///   refused. Phase 3 fills the slots left with the remaining bans in
    ///   order whose signature verifies and that are safe, duplicates of an
    ///   accepted pair included.
    ///
    /// So each pool is guaranteed `max_bans / 2` slots for bans that take
    /// effect, a flood of bans that take no effect given the kept ones only
    /// fills free slots, and a mutual-ban cycle's tier is decided over the
    /// bans accepted before it. The issuer, target and depth lookups read
    /// [`Self::stable_known`] with `depth_cap`; signatures are checked against
    /// it. Within one cleanup every ban signature is verified at most once
    /// (see [`AuthorizedUserBan::verify_signature`]).
    ///
    /// The result is in the canonical `(banned_at, id, record digest)` stored
    /// order and does not depend on the order of `bans` as passed in.
    pub fn enforce_user_ban_cap(
        bans: &mut Vec<AuthorizedUserBan>,
        max_bans: usize,
        depth_cap: usize,
        members: &MembersV1,
        evidence: &BanEvidenceV1,
        member_info: &MemberInfoV1,
        parameters: &ChatRoomParametersV1,
    ) {
        if bans.len() <= max_bans {
            return;
        }
        bans.sort_by_cached_key(|b| (b.ban.banned_at, b.id(), crate::util::record_digest(b)));
        // The DM apply's enforced-ban set and cleanup step 0-cap run this on
        // the same inputs within one apply; the second gets the first's
        // answer (`signature_memo::cap_result`, the last cap computed).
        let inputs = signature_memo::cap_inputs_digest(
            bans,
            max_bans,
            depth_cap,
            members,
            evidence,
            member_info,
            parameters,
        );
        if let Some(kept) = signature_memo::cap_result(&inputs) {
            *bans = kept;
            return;
        }
        Self::keep_in_authority_order(
            bans,
            max_bans,
            depth_cap,
            members,
            evidence,
            member_info,
            parameters,
        );
        signature_memo::record_cap_result(inputs, bans);
    }

    /// The body of [`Self::enforce_user_ban_cap`] over the canonically
    /// ordered, over-cap `bans`.
    fn keep_in_authority_order(
        bans: &mut Vec<AuthorizedUserBan>,
        max_bans: usize,
        depth_cap: usize,
        members: &MembersV1,
        evidence: &BanEvidenceV1,
        member_info: &MemberInfoV1,
        parameters: &ChatRoomParametersV1,
    ) {
        let owner_id = parameters.owner_id();
        let known = evidence.lookup(members, owner_id);
        let stable = Self::stable_known(members, evidence, owner_id, depth_cap);
        let sig_ok: Vec<bool> = bans
            .iter()
            .map(|b| {
                Self::ban_signature_matches_current_key(b, &stable, owner_id, &parameters.owner)
            })
            .collect();
        let n = bans.len();
        // Invite depth over the known records, the owner at 0, by one walk
        // down from the owner; a record whose chain does not reach the owner
        // ranks last.
        let depth: HashMap<MemberId, usize> = {
            let mut children: HashMap<MemberId, Vec<MemberId>> = HashMap::new();
            for (id, m) in &known {
                children.entry(m.member.invited_by).or_default().push(*id);
            }
            let mut depth = HashMap::from([(owner_id, 0usize)]);
            let mut frontier = vec![owner_id];
            let mut level = 0;
            while !frontier.is_empty() {
                level += 1;
                let mut next = Vec::new();
                for inviter in &frontier {
                    for child in children.get(inviter).into_iter().flatten() {
                        if !depth.contains_key(child) {
                            depth.insert(*child, level);
                            next.push(*child);
                        }
                    }
                }
                frontier = next;
            }
            depth
        };
        let depth_of = |id: &MemberId| depth.get(id).copied().unwrap_or(usize::MAX);
        // The authority order, best first: indices into the canonical order,
        // newest first within a level.
        let mut order: Vec<usize> = (0..n).rev().collect();
        order.sort_by_key(|&i| {
            let ban = &bans[i];
            let level = if ban.banned_by == owner_id {
                0
            } else if stable.contains_key(&ban.ban.banned_user) {
                depth_of(&ban.ban.banned_user).saturating_sub(1)
            } else {
                depth_of(&ban.banned_by)
            };
            // An owner ban comes before any other ban at its level.
            (level, ban.banned_by != owner_id, std::cmp::Reverse(i))
        });

        let mut accepted = vec![false; n];
        let mut accepted_pairs: HashSet<(MemberId, MemberId)> = HashSet::new();
        // The records every judgment resolves over, built once.
        let context = BanContext::new(members, evidence, parameters);
        // (live, safe) of ban `i` judged over the accepted bans plus itself,
        // by the full resolution of that set.
        let judge = |i: usize, accepted: &[bool]| -> (bool, bool) {
            let ban = &bans[i];
            let set: Vec<&AuthorizedUserBan> = (0..n)
                .filter(|j| *j == i || accepted[*j])
                .map(|j| &bans[j])
                .collect();
            let res = context.resolve(set.iter().copied(), member_info, parameters);
            let safe = !res.owner_removed.contains(&ban.banned_by)
                && (0..n).all(|j| !accepted[j] || !res.owner_removed.contains(&bans[j].banned_by));
            if res.tier(&ban.id()) > BanResolution::INERT {
                return (true, safe);
            }
            let live = ban.banned_by != owner_id
                && ban.ban.banned_user != owner_id
                && stable.contains_key(&ban.banned_by)
                && !known.contains_key(&ban.ban.banned_user)
                && !res.removed.contains(&ban.banned_by);
            (live, safe)
        };
        let pair = |i: usize| (bans[i].banned_by, bans[i].ban.banned_user);

        let share = max_bans / 2;
        let mut total = 0usize;
        // Phase 1: each pool up to its share.
        for owner_pool in [true, false] {
            let mut count = 0;
            for &i in &order {
                if count >= share {
                    break;
                }
                if (bans[i].banned_by == owner_id) != owner_pool
                    || !sig_ok[i]
                    || accepted_pairs.contains(&pair(i))
                {
                    continue;
                }
                let (live, safe) = judge(i, &accepted);
                if live && safe {
                    accepted[i] = true;
                    accepted_pairs.insert(pair(i));
                    count += 1;
                    total += 1;
                }
            }
        }
        // Phase 2: the rest in order.
        for &i in &order {
            if total >= max_bans {
                break;
            }
            if accepted[i] || !sig_ok[i] || accepted_pairs.contains(&pair(i)) {
                continue;
            }
            let (live, safe) = judge(i, &accepted);
            if live && safe {
                accepted[i] = true;
                accepted_pairs.insert(pair(i));
                total += 1;
            }
        }
        // Phase 3: fill the slots left with any ban whose signature verifies
        // and that is safe, duplicates of an accepted pair included.
        for &i in &order {
            if total >= max_bans {
                break;
            }
            if accepted[i] || !sig_ok[i] {
                continue;
            }
            if judge(i, &accepted).1 {
                accepted[i] = true;
                total += 1;
            }
        }
        let mut i = 0;
        bans.retain(|_| {
            let keep = accepted[i];
            i += 1;
            keep
        });
    }
}

impl ComposableState for BansV1 {
    type ParentState = ChatRoomStateV1;
    // BTreeSet (not HashSet) so the ciborium-serialized summary bytes are
    // deterministic: freenet-core byte-compares `summarize_state` output for
    // staleness, and a HashSet iterates in a per-process-random order, making
    // two identical ban sets summarize to different bytes → spurious
    // anti-entropy heals. See `.claude/rules/contract-summary-determinism.md`
    // and freenet/freenet-core#4857.
    //
    // Keyed by each ban's whole-record digest, not its `BanId` (which hashes
    // the signature alone): two records that share a signature but differ
    // elsewhere are different bans, and a summary that cannot tell them apart
    // leaves peers holding different ones unreconciled (freenet/river#703
    // review). Changing this type re-keys the contract.
    type Summary = BTreeSet<EvidenceDigest>;
    type Delta = Vec<AuthorizedUserBan>;
    type Parameters = ChatRoomParametersV1;

    /// Verifies that all bans in the collection are valid:
    /// - every signature whose issuer is known verifies
    ///   (`verify_ban_signatures`), AND
    /// - the number of bans does not exceed `max_user_bans` (the hard ceiling
    ///   on stored state; a legitimately-produced state is already ≤ the cap
    ///   because `post_apply_cleanup` caps it).
    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String> {
        // The count first: it needs no signature check.
        if self.0.len() > parent_state.configuration.configuration.max_user_bans {
            return Err(format!(
                "Number of bans ({}) exceeds the maximum allowed ({})",
                self.0.len(),
                parent_state.configuration.configuration.max_user_bans
            ));
        }

        self.verify_ban_signatures(parent_state, parameters)
    }

    /// Creates a summary of the current ban state
    ///
    /// Returns a set of all ban IDs currently in the collection
    fn summarize(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
    ) -> Self::Summary {
        self.0.iter().map(ban_digest).collect()
    }

    /// Computes the difference between current ban state and old state
    ///
    /// Returns a vector of bans that exist in the current state but not in the old state,
    /// or None if there are no differences
    fn delta(
        &self,
        _parent_state: &Self::ParentState,
        _parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta> {
        // Identify bans in self.0 that are not in old_state_summary
        let delta = self
            .0
            .iter()
            .filter(|ban| !old_state_summary.contains(&ban_digest(ban)))
            .cloned()
            .collect::<Vec<_>>();
        if delta.is_empty() {
            None
        } else {
            Some(delta)
        }
    }

    /// Applies changes from a delta to the current ban state.
    ///
    /// This method:
    /// - Skips bans already held (a re-delivery is a no-op)
    /// - Verifies the signatures of the bans whose banner is known here,
    ///   EXCLUDING the `max_user_bans` ceiling. Whether a ban takes effect is
    ///   decided later, from converged state (freenet/river#423).
    /// - Adds the new bans to the collection
    ///
    /// It deliberately does NOT enforce `max_user_bans` here. The cap is
    /// enforced in `ChatRoomStateV1::post_apply_cleanup`, which keeps bans in
    /// its fixed authority order judged over the converged state (the
    /// `member_info` deputy grants are not yet applied at this point in the
    /// field-apply order). Capping here would judge bans before the grants
    /// that decide whether they take effect (#410 review round 1). The
    /// transient over-cap set is capped by post_apply_cleanup at the end of
    /// the same apply; `verify` still rejects any stored state over the cap.
    ///
    /// Returns an error if the delta carries more new bans than the work bound
    /// allows. A new ban whose known issuer's key does not verify it is
    /// skipped.
    fn apply_delta(
        &mut self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String> {
        if let Some(delta) = delta {
            // A ban this peer already holds (the same record, which is what
            // the summary and `delta` compare) is skipped. A delta computed
            // against a stale summary of this peer re-delivers such bans, and
            // freenet-core sends exactly those; rejecting the whole delta over
            // them cost a full-state resync each time and broke the
            // `delta_idempotence` merge law (freenet/river#703 defect 2). A
            // ban repeated identically inside one delta is stored once;
            // records that share an id but differ are all kept, so the checks
            // here and at step 0 judge each of them.
            let max_bans = parent_state.configuration.configuration.max_user_bans;
            let bound = max_bans.max(MIN_BANS_DELTA_BOUND);
            // A delta longer than everything held plus the bound cannot be
            // within the bound however many it re-delivers: refused before
            // any record is hashed.
            if !crate::room_state::work_bounds::lifted()
                && delta.len() > self.0.len().saturating_add(bound)
            {
                return Err(format!(
                    "Ban delta of {} exceeds {}; refusing to process a flood",
                    delta.len(),
                    bound
                ));
            }
            let held: std::collections::HashSet<EvidenceDigest> =
                self.0.iter().map(ban_digest).collect();
            let mut in_delta = std::collections::HashSet::new();
            let new_bans: Vec<&AuthorizedUserBan> = delta
                .iter()
                .filter(|ban| {
                    let d = ban_digest(ban);
                    !held.contains(&d) && in_delta.insert(d)
                })
                .collect();

            // Bound the number of NEW bans a single delta may carry (#411 round
            // 3 item C). Since post_apply_cleanup caps stored state at
            // `max_user_bans`, a legitimate peer's ban list holds at most its
            // own `max_user_bans` bans, so a far larger delta can only be a
            // forged flood; rejecting it bounds the O(N) signature-verification
            // work below (round 2 removed the pre-verify cap that used to do
            // this). Deterministic across peers.
            //
            // The bound is `max(max_user_bans, MIN_BANS_DELTA_BOUND)`, not
            // `max_user_bans`: config applies before bans in field order, so a
            // peer that has already applied a LOWERED cap would otherwise
            // reject a ban delta from a peer still on the old, higher cap
            // (freenet/river#703 defect 2). The floor heals any reduction from
            // a cap at or below it in one exchange; cleanup trims the excess.
            if !crate::room_state::work_bounds::lifted() && new_bans.len() > bound {
                return Err(format!(
                    "Ban delta of {} new bans exceeds {}; refusing to process a flood",
                    new_bans.len(),
                    bound
                ));
            }

            // Create a temporary BansV1 with the new bans, WITHOUT the max-cap
            // ceiling (deferred to post_apply_cleanup).
            //
            // There is deliberately NO orphaned-ban rule here any more
            // (freenet/river#423, #702). It was judged against
            // `parent_state.members`, which at this point in field order is
            // the receiver's PRE-update member set, so the verdict depended on
            // whom this peer happened to have pruned. Rejecting the whole
            // delta on it forked rooms permanently. Each apply-time
            // replacement tried in #702 review (skip it; drop the ban;
            // drop it only when a verified ban names its issuer) had its own
            // authorization or divergence hole.
            //
            // Whether a ban takes effect, including "its issuer is banned", is
            // now decided once, from converged state, by
            // `MembersV1::resolve_bans` at `post_apply_cleanup` step 0. A ban
            // stored here whose issuer is absent is unverifiable and takes no
            // effect there, and step 4 sweeps it. A ban whose issuer the same
            // delta re-adds is verified at step 0 against that issuer's key,
            // and takes no effect if the issuer is banned by a ban that does.
            //
            // Signatures are still checked here for every NEW ban whose issuer
            // this peer knows, and one that does not verify is skipped, not
            // an error: a peer that does not know its issuer holds it validly
            // (`verify` has no key to check it against there), and an error
            // would fail every merge with that peer in one direction only.
            // Held bans are not re-checked here: post_apply_cleanup step 4
            // sweeps a held ban whose issuer's key does not verify it
            // (freenet/river#703).
            let known = parent_state
                .ban_evidence
                .lookup(&parent_state.members, parameters.owner_id());
            let owner_id = parameters.owner_id();
            self.0.extend(
                new_bans
                    .into_iter()
                    .filter(|ban| {
                        let key = if ban.banned_by == owner_id {
                            Some(parameters.owner)
                        } else {
                            known.get(&ban.banned_by).map(|m| m.member.member_vk)
                        };
                        key.is_none_or(|vk| ban.verify_signature(&vk).is_ok())
                    })
                    .cloned(),
            );
        }

        // A ban held twice (a state `verify` accepts but no apply produces)
        // is kept once, so peers holding the copy and not converge
        // (freenet/river#703).
        let mut seen = std::collections::HashSet::new();
        self.0.retain(|ban| seen.insert(ban_digest(ban)));

        // Sort for deterministic ordering (CRDT convergence requirement)
        self.0.sort_by(|a, b| {
            a.ban
                .banned_at
                .cmp(&b.ban.banned_at)
                .then_with(|| a.id().cmp(&b.id()))
        });

        Ok(())
    }
}

/// One ban delta may carry `max(max_user_bans, MIN_BANS_DELTA_BOUND)` NEW
/// bans (see `BansV1::apply_delta`); ban evidence takes the same floor. A
/// work bound, not a room rule: each new ban costs a signature check here and
/// up to `2 x MAX_EVIDENCE_DEPTH` evidence chain checks. Kept well below the
/// members and messages bounds (1000) because of that evidence fan-out: at 100
/// the evidence floor is 800 records.
///
/// The cost this admits, for a room on the default `max_user_bans` (10, so
/// 10 bans / 80 evidence records before the floor): a 100-ban delta,
/// including the cap eviction and cleanup it triggers, measured 15-18 ms
/// natively (opt-level 3, 200 members, 2026-10-16); with the node's WASM
/// runtime at 0.64-1.0 s per 2000 signature checks (see
/// `ban_evidence::MAX_EVIDENCE_DEPTH`) that is about 0.03-0.05 s for the
/// 100 ban checks, and at most 0.26-0.4 s for 800 evidence chain checks:
/// under 0.5 s together, inside the 5 s compute limit. In exchange, a peer
/// that has applied a `max_user_bans` reduction from any cap up to 100 no
/// longer refuses a lagging peer's ban delta (freenet/river#703).
pub const MIN_BANS_DELTA_BOUND: usize = 100;

/// The digest of a whole ban record: what the bans summary, `delta` and the
/// held-ban checks compare (16 bytes of BLAKE3 over its CBOR, serialized as a
/// byte string like [`EvidenceDigest`]).
pub fn ban_digest(ban: &AuthorizedUserBan) -> EvidenceDigest {
    let full = crate::util::record_digest(ban);
    let mut out = [0u8; 16];
    out.copy_from_slice(&full[..16]);
    EvidenceDigest(out)
}

/// A user ban with authorization proof
///
/// Contains the ban details, the ID of the member who created the ban,
/// and a cryptographic signature proving the ban's authenticity
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AuthorizedUserBan {
    pub ban: UserBan,
    pub banned_by: MemberId,
    pub signature: Signature,
}

impl Eq for AuthorizedUserBan {}

impl Hash for AuthorizedUserBan {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.signature.to_bytes().hash(state);
    }
}

impl AuthorizedUserBan {
    /// Creates a new authorized ban
    ///
    /// Signs the ban with the provided signing key and verifies that the
    /// banned_by ID matches the public key derived from the signing key
    pub fn new(ban: UserBan, banned_by: MemberId, banner_signing_key: &SigningKey) -> Self {
        assert_eq!(
            MemberId::from(banner_signing_key.verifying_key()),
            banned_by
        );

        let signature = sign_struct(&ban, banner_signing_key);

        Self {
            ban,
            banned_by,
            signature,
        }
    }

    /// Create an AuthorizedUserBan with a pre-computed signature.
    /// Use this when signing is done externally (e.g., via delegate).
    pub fn with_signature(ban: UserBan, banned_by: MemberId, signature: Signature) -> Self {
        Self {
            ban,
            banned_by,
            signature,
        }
    }

    /// Verifies that the ban's signature is valid
    ///
    /// Checks that the signature was created by the key corresponding to the provided verifying key
    pub fn verify_signature(&self, banner_verifying_key: &VerifyingKey) -> Result<(), String> {
        #[cfg(test)]
        tests::BAN_SIGNATURE_CHECKS.with(|c| c.set(c.get() + 1));
        let key = (
            self.signature.to_bytes(),
            banner_verifying_key.to_bytes(),
            self.ban.owner_member_id,
            self.ban.banned_at,
            self.ban.banned_user,
        );
        if let Some(ok) = signature_memo::lookup(&key) {
            return if ok {
                Ok(())
            } else {
                Err("Invalid ban signature".to_string())
            };
        }
        #[cfg(any(test, river_cleanup_counters))]
        crate::room_state::cleanup_counters::count_ban_signature_verification();
        let result = verify_struct(&self.ban, &self.signature, banner_verifying_key)
            .map_err(|e| format!("Invalid ban signature: {}", e));
        signature_memo::record(key, result.is_ok());
        result
    }

    /// Generates a unique identifier for this ban based on its signature
    pub fn id(&self) -> BanId {
        BanId(fast_hash(&self.signature.to_bytes()))
    }
}

/// Contains the core information about a user ban
///
/// Includes the room owner's ID, the time of the ban, and the ID of the banned user
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UserBan {
    pub owner_member_id: MemberId,
    pub banned_at: SystemTime,
    pub banned_user: MemberId,
}

/// A unique identifier for a ban
///
/// Created from a hash of the ban's signature to ensure uniqueness
#[derive(Serialize, Deserialize, PartialEq, Eq, Clone, Hash, Debug, Ord, PartialOrd)]
pub struct BanId(pub FastHash);

#[cfg(test)]
mod ban_digest_golden {
    use super::*;

    /// A fixed ban record: fixed fields and fixed signature bytes (a
    /// `Signature` is not validated on construction), so the digest input is
    /// exactly this record's CBOR.
    fn fixture() -> AuthorizedUserBan {
        let mut raw = [0u8; 64];
        for (i, b) in raw.iter_mut().enumerate() {
            *b = i as u8;
        }
        AuthorizedUserBan::with_signature(
            UserBan {
                owner_member_id: MemberId(freenet_scaffold::util::FastHash(1)),
                banned_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
                banned_user: MemberId(freenet_scaffold::util::FastHash(2)),
            },
            MemberId(freenet_scaffold::util::FastHash(3)),
            Signature::from_bytes(&raw),
        )
    }

    /// `ban_digest` is wire format: the bans summary and `delta` compare it.
    /// One fixed record, one expected digest, one expected encoding. If this
    /// changes deliberately it re-keys the room contract.
    #[test]
    fn ban_digest_golden_vector() {
        const EXPECTED: [u8; 16] = [
            0x98, 0x22, 0xae, 0x47, 0x04, 0x56, 0xd4, 0xc9, 0xb2, 0xd1, 0x39, 0x6e, 0xce, 0xf6,
            0x64, 0xcc,
        ];
        let digest = ban_digest(&fixture());
        assert_eq!(
            digest.0, EXPECTED,
            "ban_digest changed: a wire-format change"
        );
        let mut reversed = EXPECTED;
        reversed.reverse();
        assert_ne!(digest.0, reversed, "the vector must distinguish byte order");
        let mut cbor = Vec::new();
        ciborium::ser::into_writer(&digest, &mut cbor).unwrap();
        assert_eq!(cbor.len(), 17, "a CBOR byte string of 16 bytes");
        assert_eq!(cbor[0], 0x50);
        assert_eq!(&cbor[1..], &EXPECTED);
    }
}

/// Two caches for the ban cap's cost, both exact.
///
/// - The signature memo: the ban signature checks of one cleanup or
///   whole-state merge, remembered so each ban is verified at most once
///   however many times the cap, the cleanup steps and the fixpoint passes
///   ask. Keyed by the signature, the verifying key and every field of the
///   signed record, the whole input of the check (two bans can share
///   signature bytes), so a remembered answer is the answer the check would
///   give. Active only inside [`active_while`]; outside it nothing is
///   remembered.
/// - The last cap: the inputs digest and choice of the last ban cap computed
///   on this thread, kept outside any scope (one entry, at most
///   `max_user_bans` bans), so the DM apply's enforced-ban set and cleanup
///   step 0-cap, which judge the same inputs, run the cap once on every
///   apply path. The digest is BLAKE3 over the CBOR of every input the cap
///   reads; a hit returns the choice those inputs determine.
pub(crate) mod signature_memo {
    use crate::room_state::member::MemberId;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::time::SystemTime;

    type Key = ([u8; 64], [u8; 32], MemberId, SystemTime, MemberId);

    thread_local! {
        static MEMO: RefCell<Option<HashMap<Key, bool>>> = const { RefCell::new(None) };
        static LAST_CAP: RefCell<Option<([u8; 32], Vec<super::AuthorizedUserBan>)>> =
            const { RefCell::new(None) };
    }

    /// A digest of everything the ban cap's choice depends on.
    pub(crate) fn cap_inputs_digest(
        bans: &[super::AuthorizedUserBan],
        max_bans: usize,
        depth_cap: usize,
        members: &super::MembersV1,
        evidence: &super::BanEvidenceV1,
        member_info: &super::MemberInfoV1,
        parameters: &super::ChatRoomParametersV1,
    ) -> [u8; 32] {
        crate::util::record_digest(&(
            bans,
            max_bans as u64,
            depth_cap as u64,
            members,
            evidence,
            member_info,
            parameters,
        ))
    }

    /// The bans the cap kept for these inputs, if they are the inputs of
    /// the last cap computed on this thread. The cap's choice is a function
    /// of its inputs alone, so the remembered answer is the answer; it is
    /// kept whatever scope is active, so a delta's DM apply and the cleanup
    /// after it share it on every path (the contract, the UI, a merge).
    pub(crate) fn cap_result(inputs: &[u8; 32]) -> Option<Vec<super::AuthorizedUserBan>> {
        LAST_CAP.with(|c| {
            c.borrow()
                .as_ref()
                .filter(|(held, _)| held == inputs)
                .map(|(_, kept)| kept.clone())
        })
    }

    pub(crate) fn record_cap_result(inputs: [u8; 32], kept: &[super::AuthorizedUserBan]) {
        LAST_CAP.with(|c| *c.borrow_mut() = Some((inputs, kept.to_vec())));
    }

    pub(crate) fn lookup(key: &Key) -> Option<bool> {
        MEMO.with(|m| m.borrow().as_ref().and_then(|m| m.get(key).copied()))
    }

    pub(crate) fn record(key: Key, verified: bool) {
        MEMO.with(|m| {
            if let Some(m) = m.borrow_mut().as_mut() {
                m.insert(key, verified);
            }
        });
    }

    /// Run `f` with the memo set aside (neither read nor written), restoring
    /// it afterwards: for the debug-build cross-check in cleanup, so debug
    /// builds verify the same signatures release builds do.
    #[cfg(debug_assertions)]
    pub(crate) fn set_aside<R>(f: impl FnOnce() -> R) -> R {
        let held = MEMO.with(|m| m.borrow_mut().take());
        let held_cap = LAST_CAP.with(|c| c.borrow_mut().take());
        let r = f();
        MEMO.with(|m| *m.borrow_mut() = held);
        LAST_CAP.with(|c| *c.borrow_mut() = held_cap);
        r
    }

    /// Run `f` with the memo active. A nested call (a cleanup inside a
    /// merge, or one cleanup inside another) shares the outer memo and leaves
    /// it in place; the outermost call clears it when it returns, also if `f`
    /// panics.
    pub(crate) fn active_while<R>(f: impl FnOnce() -> R) -> R {
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                MEMO.with(|m| *m.borrow_mut() = None);
            }
        }
        let outermost = MEMO.with(|m| {
            let mut m = m.borrow_mut();
            if m.is_none() {
                *m = Some(HashMap::new());
                true
            } else {
                false
            }
        });
        let _clear = outermost.then(|| Clear);
        f()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::configuration::AuthorizedConfigurationV1;

    thread_local! {
        /// Ban signature checks (`AuthorizedUserBan::verify_signature`).
        pub(super) static BAN_SIGNATURE_CHECKS: std::cell::Cell<usize> =
            const { std::cell::Cell::new(0) };
    }

    /// A nested memo scope (the DM apply's enforced-ban set inside a merge, a
    /// cleanup inside a merge) shares the outer memo and leaves it in place;
    /// only the outermost scope clears it.
    #[test]
    fn a_nested_memo_scope_keeps_the_outer_memo() {
        let id = MemberId::from(&SigningKey::from_bytes(&[3u8; 32]).verifying_key());
        let key = ([1u8; 64], [2u8; 32], id, SystemTime::UNIX_EPOCH, id);
        signature_memo::active_while(|| {
            signature_memo::record(key, true);
            signature_memo::active_while(|| {
                assert_eq!(signature_memo::lookup(&key), Some(true), "shared inside");
            });
            assert_eq!(
                signature_memo::lookup(&key),
                Some(true),
                "still held after the nested scope returns"
            );
        });
        assert_eq!(
            signature_memo::lookup(&key),
            None,
            "cleared by the outermost scope"
        );
    }

    /// `BansV1::verify` rejects more bans than `max_user_bans` before
    /// checking any signature.
    #[test]
    fn verify_checks_the_ban_count_before_any_signature() {
        use crate::room_state::configuration::Configuration;
        let owner_sk = SigningKey::generate(&mut rand::thread_rng());
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let params = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        let mut state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    max_user_bans: 3,
                    ..Default::default()
                },
                &owner_sk,
            ),
            ..Default::default()
        };
        let ban = |i: u64| {
            AuthorizedUserBan::new(
                UserBan {
                    owner_member_id: owner_id,
                    banned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(i),
                    banned_user: MemberId::from(
                        &SigningKey::generate(&mut rand::thread_rng()).verifying_key(),
                    ),
                },
                owner_id,
                &owner_sk,
            )
        };
        state.bans = BansV1((0..500).map(ban).collect());
        let before = BAN_SIGNATURE_CHECKS.with(|c| c.get());
        let err = state.bans.verify(&state, &params).unwrap_err();
        assert!(err.contains("exceeds the maximum"), "{err}");
        assert_eq!(BAN_SIGNATURE_CHECKS.with(|c| c.get()) - before, 0);
        state.bans.0.truncate(3);
        let before = BAN_SIGNATURE_CHECKS.with(|c| c.get());
        state.bans.verify(&state, &params).unwrap();
        assert_eq!(BAN_SIGNATURE_CHECKS.with(|c| c.get()) - before, 3);
    }
    use crate::room_state::member::{AuthorizedMember, Member, MembersV1};
    use ed25519_dalek::SigningKey;
    use std::time::Duration;

    fn create_test_chat_room_state() -> ChatRoomStateV1 {
        // Create a minimal ChatRoomStateV1 for testing
        ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::default(),
            bans: Default::default(),
            members: MembersV1::default(),
            member_info: Default::default(),
            secrets: Default::default(),
            recent_messages: Default::default(),
            upgrade: Default::default(),
            ..Default::default()
        }
    }

    fn create_test_parameters() -> ChatRoomParametersV1 {
        // Create minimal ChatRoomParametersV1 for testing
        let owner_key = SigningKey::generate(&mut rand::thread_rng());
        ChatRoomParametersV1 {
            owner: owner_key.verifying_key(),
        }
    }

    #[test]
    fn test_bans_verify() {
        let mut state = create_test_chat_room_state();
        let params = create_test_parameters();

        // Create some test members
        let owner_key = SigningKey::generate(&mut rand::thread_rng());
        let owner_id: MemberId = owner_key.verifying_key().into();
        let member1_key = SigningKey::generate(&mut rand::thread_rng());
        let member1_id: MemberId = member1_key.verifying_key().into();
        let member2_key = SigningKey::generate(&mut rand::thread_rng());
        let member2_id: MemberId = member2_key.verifying_key().into();

        // Add members to the room_state
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: owner_key.verifying_key(),
            },
            &owner_key,
        ));
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: member1_key.verifying_key(),
            },
            &owner_key,
        ));
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: member1_id,
                member_vk: member2_key.verifying_key(),
            },
            &member1_key,
        ));

        // Update the configuration to allow bans
        state.configuration.configuration.max_user_bans = 5;

        // Test 1: Valid ban by owner
        let ban1 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: member1_id,
            },
            owner_id,
            &owner_key,
        );

        let bans = BansV1(vec![ban1]);
        assert!(
            bans.verify(&state, &params).is_ok(),
            "Valid ban should be verified successfully: {:?}",
            bans.verify(&state, &params).err()
        );

        // Test 2: Exceeding max_user_bans
        let mut many_bans = Vec::new();
        for _ in 0..6 {
            many_bans.push(AuthorizedUserBan::new(
                UserBan {
                    owner_member_id: owner_id,
                    banned_at: SystemTime::now(),
                    banned_user: member1_id,
                },
                owner_id,
                &owner_key,
            ));
        }
        let too_many_bans = BansV1(many_bans);
        assert!(
            too_many_bans.verify(&state, &params).is_err(),
            "Exceeding max_user_bans should fail verification"
        );

        // Test 3: Ban by pruned member (not in member list, not banned) — valid
        // With message-based lifecycle, banners not in the members list who
        // are not themselves banned are considered pruned for inactivity.
        let pruned_key = SigningKey::generate(&mut rand::thread_rng());
        let pruned_id: MemberId = pruned_key.verifying_key().into();
        let pruned_ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: member2_id,
            },
            pruned_id,
            &pruned_key,
        );

        let pruned_bans = BansV1(vec![pruned_ban]);
        assert!(
            pruned_bans.verify(&state, &params).is_ok(),
            "Ban by pruned (non-banned) member should pass verification: {:?}",
            pruned_bans.verify(&state, &params).err()
        );

        // Test 3b: an "orphaned" ban (banner absent and banned) is ACCEPTED by
        // `verify` since freenet/river#423 / #702: whether it takes effect is
        // decided from converged state by `MembersV1::resolve_bans`, and
        // rejecting it here forked rooms permanently.
        let orphaned_key = SigningKey::generate(&mut rand::thread_rng());
        let orphaned_id: MemberId = orphaned_key.verifying_key().into();
        let orphaned_ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: member2_id,
            },
            orphaned_id,
            &orphaned_key,
        );
        // A ban targeting the orphaned member makes them "banned" (not just pruned)
        let ban_of_orphaned = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: orphaned_id,
            },
            owner_id,
            &owner_key,
        );
        let orphaned_bans = BansV1(vec![orphaned_ban, ban_of_orphaned]);
        assert!(
            orphaned_bans.verify(&state, &params).is_ok(),
            "an orphaned ban passes verify; resolution decides its effect"
        );

        // Test 4: Valid ban by non-owner member
        let ban_by_member = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: member2_id,
            },
            member1_id,
            &member1_key,
        );

        let member_bans = BansV1(vec![ban_by_member]);
        assert!(
            member_bans.verify(&state, &params).is_ok(),
            "Valid ban by non-owner member should pass verification"
        );
    }

    #[test]
    fn test_bans_summarize() {
        let state = create_test_chat_room_state();
        let params = create_test_parameters();

        let key = SigningKey::generate(&mut rand::thread_rng());
        let id: MemberId = key.verifying_key().into();

        let ban1 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: id,
                banned_at: SystemTime::now(),
                banned_user: id,
            },
            id,
            &key,
        );

        let ban2 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: id,
                banned_at: SystemTime::now() + Duration::from_secs(1),
                banned_user: id,
            },
            id,
            &key,
        );

        let bans = BansV1(vec![ban1.clone(), ban2.clone()]);
        let summary = bans.summarize(&state, &params);

        assert_eq!(summary.len(), 2);
        assert!(summary.contains(&ban_digest(&ban1)));
        assert!(summary.contains(&ban_digest(&ban2)));
    }

    #[test]
    fn test_bans_delta() {
        let state = create_test_chat_room_state();
        let params = create_test_parameters();

        let key = SigningKey::generate(&mut rand::thread_rng());
        let id: MemberId = key.verifying_key().into();

        let ban1 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: id,
                banned_at: SystemTime::now(),
                banned_user: id,
            },
            id,
            &key,
        );

        let ban2 = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: id,
                banned_at: SystemTime::now() + Duration::from_secs(1),
                banned_user: id,
            },
            id,
            &key,
        );

        let bans = BansV1(vec![ban1.clone(), ban2.clone()]);

        // Test 1: Empty old summary
        let empty_summary = BTreeSet::new();
        let delta = bans.delta(&state, &params, &empty_summary);
        assert_eq!(delta, Some(vec![ban1.clone(), ban2.clone()]));

        // Test 2: Partial old summary
        let partial_summary: BTreeSet<EvidenceDigest> =
            vec![ban_digest(&ban1)].into_iter().collect();
        let delta = bans.delta(&state, &params, &partial_summary);
        assert_eq!(delta, Some(vec![ban2.clone()]));

        // Test 3: Full old summary
        let full_summary: BTreeSet<EvidenceDigest> = vec![ban_digest(&ban1), ban_digest(&ban2)]
            .into_iter()
            .collect();
        let delta = bans.delta(&state, &params, &full_summary);
        assert_eq!(delta, None);
    }

    #[test]
    fn test_bans_apply_delta() {
        let mut state = create_test_chat_room_state();
        let params = create_test_parameters();

        let owner_key = SigningKey::generate(&mut rand::thread_rng());
        let owner_id: MemberId = owner_key.verifying_key().into();
        let member_key = SigningKey::generate(&mut rand::thread_rng());
        let member_id: MemberId = member_key.verifying_key().into();

        // Add members to the room_state
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: owner_key.verifying_key(),
            },
            &owner_key,
        ));
        state.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: member_key.verifying_key(),
            },
            &owner_key,
        ));

        // Update the configuration to allow bans
        state.configuration.configuration.max_user_bans = 5;

        let mut bans = BansV1::default();

        let new_ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: member_id,
            },
            owner_id,
            &owner_key,
        );

        // Test 1: Apply valid delta
        let delta = vec![new_ban.clone()];
        assert!(
            bans.apply_delta(&state, &params, &Some(delta.clone()))
                .is_ok(),
            "Valid delta should be applied successfully: {:?}",
            bans.apply_delta(&state, &params, &Some(delta)).err()
        );
        assert_eq!(
            bans.0.len(),
            1,
            "Bans should contain one ban after applying delta"
        );
        assert_eq!(bans.0[0], new_ban, "Applied ban should match the new ban");

        // Test 2: A delta that pushes past max_user_bans NO LONGER caps here.
        // The cap moved to `ChatRoomStateV1::post_apply_cleanup` so it can evict
        // INERT-before-enforcing bans using the converged member_info (#410
        // review round 1). `apply_delta` accumulates all valid bans; the
        // whole-state cleanup enforces the ceiling. (State-level capping +
        // inert-first eviction are covered by the DoS test in
        // `common/tests/deputy_ban_test.rs`.)
        let mut many_bans = Vec::new();
        for i in 0..5 {
            many_bans.push(AuthorizedUserBan::new(
                UserBan {
                    owner_member_id: owner_id,
                    banned_at: SystemTime::now() + Duration::from_secs(i as u64 + 10),
                    banned_user: member_id,
                },
                owner_id,
                &owner_key,
            ));
        }
        let delta_exceeding_max = Some(many_bans.clone());
        assert!(
            bans.apply_delta(&state, &params, &delta_exceeding_max)
                .is_ok(),
            "Applying more bans should succeed: {:?}",
            bans.apply_delta(&state, &params, &delta_exceeding_max)
                .err()
        );
        assert_eq!(
            bans.0.len(),
            6,
            "apply_delta accumulates without capping (cap is enforced in post_apply_cleanup)"
        );
        assert!(
            bans.0.contains(&new_ban),
            "apply_delta must NOT drop the oldest ban — evicting is post_apply_cleanup's job"
        );

        // Test 3: re-delivering a held ban is a no-op, not an error: a delta
        // computed against a stale summary carries it (freenet/river#703).
        let existing_ban = many_bans.last().unwrap().clone();
        let redelivery = Some(vec![existing_ban]);
        let before = bans.clone();
        bans.apply_delta(&state, &params, &redelivery)
            .expect("re-delivering a held ban must not fail");
        assert_eq!(bans, before, "a re-delivered ban changes nothing");

        // Test 4: More valid bans keep accumulating (still no cap at this level).
        let mut additional_bans = Vec::new();
        for i in 0..2 {
            additional_bans.push(AuthorizedUserBan::new(
                UserBan {
                    owner_member_id: owner_id,
                    banned_at: SystemTime::now() + Duration::from_secs(i as u64 + 100),
                    banned_user: member_id,
                },
                owner_id,
                &owner_key,
            ));
        }
        assert!(
            bans.apply_delta(&state, &params, &Some(additional_bans))
                .is_ok(),
            "Applying more bans should succeed: {:?}",
            bans.apply_delta(&state, &params, &Some(Vec::new())).err()
        );
        assert_eq!(
            bans.0.len(),
            8,
            "apply_delta keeps accumulating; the cap is enforced by post_apply_cleanup"
        );
    }

    #[test]
    fn test_authorized_user_ban() {
        let owner_key = SigningKey::generate(&mut rand::thread_rng());
        let owner_id: MemberId = owner_key.verifying_key().into();
        let member_key = SigningKey::generate(&mut rand::thread_rng());
        let member_id: MemberId = member_key.verifying_key().into();

        let ban = UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::now(),
            banned_user: member_id,
        };

        let authorized_ban = AuthorizedUserBan::new(ban.clone(), owner_id, &owner_key);

        // Test 1: Verify signature
        assert!(authorized_ban
            .verify_signature(&owner_key.verifying_key())
            .is_ok());

        // Test 2: Verify signature with wrong key
        let wrong_key = SigningKey::generate(&mut rand::thread_rng());
        assert!(authorized_ban
            .verify_signature(&wrong_key.verifying_key())
            .is_err());

        // Test 3: Check ban ID
        let id1 = authorized_ban.id();
        let id2 = authorized_ban.id();
        assert_eq!(id1, id2);

        // Test 4: Different bans should have different IDs
        let another_ban = AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now() + Duration::from_secs(1),
                banned_user: member_id,
            },
            owner_id,
            &owner_key,
        );
        assert_ne!(authorized_ban.id(), another_ban.id());
    }

    /// The flood bound counts NEW bans only, against
    /// `max(max_user_bans, MIN_BANS_DELTA_BOUND)` (freenet/river#703).
    #[test]
    fn ban_flood_bound_counts_new_bans_against_the_floor() {
        let owner_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let owner_id = MemberId::from(&owner_key.verifying_key());
        let params = ChatRoomParametersV1 {
            owner: owner_key.verifying_key(),
        };
        let state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                crate::room_state::configuration::Configuration {
                    owner_member_id: owner_id,
                    max_user_bans: 2,
                    ..Default::default()
                },
                &owner_key,
            ),
            ..Default::default()
        };
        let ban = |i: u64| {
            AuthorizedUserBan::new(
                UserBan {
                    owner_member_id: owner_id,
                    banned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(i),
                    banned_user: MemberId(FastHash(i as i64)),
                },
                owner_id,
                &owner_key,
            )
        };
        let at_floor: Vec<_> = (0..MIN_BANS_DELTA_BOUND as u64).map(ban).collect();
        let mut bans = BansV1::default();
        bans.apply_delta(&state, &params, &Some(at_floor.clone()))
            .expect("a delta at the floor is accepted under a lower cap");

        let mut redelivery = at_floor.clone();
        redelivery.push(ban(MIN_BANS_DELTA_BOUND as u64));
        bans.apply_delta(&state, &params, &Some(redelivery))
            .expect("held bans do not count toward the bound");

        let over: Vec<_> = (1_000..1_000 + MIN_BANS_DELTA_BOUND as u64 + 1)
            .map(ban)
            .collect();
        let err = BansV1::default()
            .apply_delta(&state, &params, &Some(over))
            .unwrap_err();
        assert!(err.contains("refusing to process a flood"), "{err}");
    }
}
