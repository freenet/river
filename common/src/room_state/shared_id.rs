//! One key per member id (freenet/river#974 follow-up).
//!
//! A `MemberId` is a non-cryptographic hash of the member's key, so two keys
//! can share one. Every field apply keeps one record per (id, key) and
//! accepts what an id signed under ANY key a held record names for it, so
//! both sides of a merge see the same records. This cleanup step, which runs
//! at the start of every pass before anything reads a record by id, then
//! decides which key each id keeps.

use crate::room_state::ban_evidence::{evidence_digest, EvidenceDigest};
use crate::room_state::member::{AuthorizedMember, MemberId};
use crate::room_state::ChatRoomParametersV1;
use crate::ChatRoomStateV1;
use ed25519_dalek::VerifyingKey;
use std::collections::{BTreeMap, HashMap, HashSet};

impl ChatRoomStateV1 {
    /// For every id held under two or more keys (in `members` and ban
    /// evidence together), keep one key and drop what the others brought.
    ///
    /// The key kept is decided from the whole state, so it is the same
    /// whichever order the records arrived in, and in this order:
    ///
    /// 1. A key that has signed something the state holds for the id (its
    ///    `member_info`, a message, a direct message, a ban it issued, or the
    ///    record of a member it invited) beats a key that has not. A key with
    ///    no known secret can never sign anything, so a record naming another
    ///    key for a member who has used the room never displaces them, on any
    ///    peer, whichever record arrived first.
    /// 2. Then the key whose record sits at the least invite depth (through
    ///    member and evidence records, as `normalize_held_records` counts it).
    /// 3. Then the lower record digest.
    ///
    /// The records naming the other keys go, with the records of the members
    /// they invited (an invitee whose signature does not verify under the
    /// kept key) and those invitees' own subtrees. What the id signed that
    /// does not verify under the kept key goes too: its `member_info`,
    /// messages, direct messages and purge records. Bans are left to step
    /// 0-sig, which reads the records this step leaves.
    ///
    /// Returns the ids it decided, each with the key kept. Only those ids are
    /// re-checked, so a room where no id is shared pays one pass over the
    /// records.
    pub(crate) fn resolve_shared_ids(
        &mut self,
        parameters: &ChatRoomParametersV1,
    ) -> HashMap<MemberId, VerifyingKey> {
        let records = || {
            self.members
                .members
                .iter()
                .chain(&self.ban_evidence.members)
        };
        let mut keys: BTreeMap<MemberId, Vec<VerifyingKey>> = BTreeMap::new();
        for m in records() {
            let k = keys.entry(m.member.id()).or_default();
            if !k.contains(&m.member.member_vk) {
                k.push(m.member.member_vk);
            }
        }
        keys.retain(|_, k| k.len() > 1);
        if keys.is_empty() {
            return HashMap::new();
        }
        let owner_id = parameters.owner_id();
        let owner_vk = parameters.owner;

        // Each id's least depth, by a breadth-first walk over ids from the
        // owner through every member and evidence record.
        let mut by_inviter: HashMap<MemberId, Vec<&AuthorizedMember>> = HashMap::new();
        for m in records() {
            by_inviter.entry(m.member.invited_by).or_default().push(m);
        }
        let mut inviter_depth: HashMap<MemberId, usize> = HashMap::from([(owner_id, 0)]);
        let mut frontier = vec![owner_id];
        let mut level = 1;
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for inviter in &frontier {
                for m in by_inviter.get(inviter).into_iter().flatten() {
                    let id = m.member.id();
                    if !inviter_depth.contains_key(&id) {
                        inviter_depth.insert(id, level);
                        next.push(id);
                    }
                }
            }
            frontier = next;
            level += 1;
        }
        let record_depth = |m: &AuthorizedMember| -> usize {
            inviter_depth
                .get(&m.member.invited_by)
                .map_or(usize::MAX, |d| d + 1)
        };

        let mut kept: HashMap<MemberId, VerifyingKey> = HashMap::new();
        for (id, candidates) in &keys {
            let signed = |vk: &VerifyingKey| self.has_signed(*id, vk, &owner_vk);
            let best = candidates
                .iter()
                .map(|vk| {
                    let mine =
                        || records().filter(|m| m.member.id() == *id && m.member.member_vk == *vk);
                    let depth = mine().map(record_depth).min().unwrap_or(usize::MAX);
                    let digest: EvidenceDigest = mine()
                        .map(evidence_digest)
                        .min()
                        .expect("a record names this key");
                    ((!signed(vk), depth, digest), *vk)
                })
                .min_by(|a, b| a.0.cmp(&b.0))
                .map(|(_, vk)| vk)
                .expect("two or more candidates");
            kept.insert(*id, best);
        }

        // The records naming another key go, and the members a dropped key
        // invited: a record naming a decided id as its inviter whose
        // signature fails under the kept key. Their own invitees follow by
        // id, through ids left with no record. A dropped key has invitees,
        // and below content, only if it signed something, so this and the
        // content check below act only when two keys for the id have each
        // signed something (a second keypair with the same id). No test
        // reaches them; they keep such a state valid.
        let keep = |m: &AuthorizedMember| {
            let loses = kept
                .get(&m.member.id())
                .is_some_and(|vk| *vk != m.member.member_vk);
            let fails_inviter = kept
                .get(&m.member.invited_by)
                .is_some_and(|vk| m.verify_signature(vk).is_err());
            !(loses || fails_inviter)
        };
        let before: HashSet<MemberId> = records().map(|m| m.member.id()).collect();
        self.members.members.retain(keep);
        self.ban_evidence.members.retain(keep);
        loop {
            let present: HashSet<MemberId> = self
                .members
                .members
                .iter()
                .chain(&self.ban_evidence.members)
                .map(|m| m.member.id())
                .collect();
            let gone: HashSet<MemberId> = before.difference(&present).copied().collect();
            let orphan = |m: &AuthorizedMember| gone.contains(&m.member.invited_by);
            let count = self.members.members.len() + self.ban_evidence.members.len();
            self.members.members.retain(|m| !orphan(m));
            self.ban_evidence.members.retain(|m| !orphan(m));
            if self.members.members.len() + self.ban_evidence.members.len() == count {
                break;
            }
        }

        // What a decided id signed under a key it no longer resolves to.
        let verifies = |id: MemberId, ok: &dyn Fn(&VerifyingKey) -> bool| {
            kept.get(&id).is_none_or(|vk| ok(vk))
        };
        self.member_info.member_info.retain(|info| {
            let id = info.member_info.member_id;
            id == owner_id || verifies(id, &|vk| info.verify_signature_with_key(vk).is_ok())
        });
        self.recent_messages.messages.retain(|m| {
            let id = m.message.author;
            id == owner_id || verifies(id, &|vk| m.validate(vk).is_ok())
        });
        self.direct_messages.messages.retain(|m| {
            let id = m.message.sender;
            id == owner_id || verifies(id, &|vk| m.verify_signature(vk, &owner_vk).is_ok())
        });
        self.direct_messages.purges.retain(|p| {
            let id = p.recipient_id;
            id == owner_id || verifies(id, &|vk| p.verify_signature(vk, &owner_vk).is_ok())
        });
        kept
    }

    /// Whether some id is held under two or more keys in `members` and ban
    /// evidence together: what `resolve_shared_ids` decides.
    pub(crate) fn has_shared_id(&self) -> bool {
        let mut seen: HashMap<MemberId, &VerifyingKey> = HashMap::new();
        self.members
            .members
            .iter()
            .chain(&self.ban_evidence.members)
            .any(|m| {
                *seen.entry(m.member.id()).or_insert(&m.member.member_vk) != &m.member.member_vk
            })
    }

    /// Whether `vk` has signed anything this state holds for `id`.
    fn has_signed(&self, id: MemberId, vk: &VerifyingKey, owner_vk: &VerifyingKey) -> bool {
        self.member_info
            .member_info
            .iter()
            .any(|i| i.member_info.member_id == id && i.verify_signature_with_key(vk).is_ok())
            || self
                .recent_messages
                .messages
                .iter()
                .any(|m| m.message.author == id && m.validate(vk).is_ok())
            || self
                .members
                .members
                .iter()
                .chain(&self.ban_evidence.members)
                .any(|m| m.member.invited_by == id && m.verify_signature(vk).is_ok())
            || self
                .bans
                .0
                .iter()
                .any(|b| b.banned_by == id && b.verify_signature(vk).is_ok())
            || self
                .direct_messages
                .messages
                .iter()
                .any(|m| m.message.sender == id && m.verify_signature(vk, owner_vk).is_ok())
            || self
                .direct_messages
                .purges
                .iter()
                .any(|p| p.recipient_id == id && p.verify_signature(vk, owner_vk).is_ok())
    }
}
