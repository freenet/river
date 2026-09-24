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
//! leaves the peers disagreeing forever. The consequence is the pre-existing
//! "cannot ban your deputizer" escape (`is_ban_authorized` rule 4), which a
//! removed member already had on `main` by re-adding themselves; absolute
//! grants (owner, owner-appointed moderator, ancestor) are unaffected.
//!
//! `post_apply_cleanup` recomputes the set on every pass: a record is kept
//! while a stored ban references its member (issuer or target) or one of
//! their invite descendants, and while that member is absent from `members`.
//! Every record's invite chain must verify, through `members` and this field,
//! back to the owner, so the state stays self-authorizing
//! (AGENTS.md, State Authorization Rule).

use crate::room_state::member::{AuthorizedMember, MemberId, MembersV1};
use crate::room_state::ChatRoomParametersV1;
use crate::ChatRoomStateV1;
use freenet_scaffold::ComposableState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct BanEvidenceV1 {
    /// Sorted by member id, at most one record per id, none of them in
    /// `members`.
    pub members: Vec<AuthorizedMember>,
}

impl BanEvidenceV1 {
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
        let present = parent_state.members.members_by_member_id();
        let lookup = self.lookup(&parent_state.members, parameters.owner_id());
        let mut seen = BTreeSet::new();
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
            parent_state
                .members
                .get_invite_chain_with_lookup(m, parameters, &lookup)
                .map_err(|e| format!("Ban evidence record {id:?}: {e}"))?;
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
            let mut combined = self.lookup(&parent_state.members, parameters.owner_id());
            for m in delta {
                combined.entry(m.member.id()).or_insert(m);
            }
            let mut added = Vec::new();
            for m in delta {
                let id = m.member.id();
                if id == owner_id
                    || present.contains_key(&id)
                    || self.members.iter().any(|e| e.member.id() == id)
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
            self.members.extend(added);
        }
        self.members
            .retain(|m| !present.contains_key(&m.member.id()));
        self.members.sort_by_key(|m| m.member.id());
        Ok(())
    }
}
