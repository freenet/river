//! One key per member id (freenet/river#974 follow-up).
//!
//! A `MemberId` is a non-cryptographic hash of the member's key, so two keys
//! can share one. Field applies keep one record per (id, key), admit at most
//! one new key per id per apply (so at most two keys per id are ever held,
//! and no work multiplies with the number of keys a sender names), and accept
//! what an id signed under ANY key a held record names for it, so both sides
//! of a merge see the same records. This cleanup step, which runs at the
//! start of every pass before anything reads a record by id, then decides
//! which key each id keeps.

use crate::room_state::ban_evidence::{evidence_depth_cap, evidence_digest, EvidenceDigest};
use crate::room_state::member::{AuthorizedMember, MemberId};
use crate::room_state::ChatRoomParametersV1;
use crate::ChatRoomStateV1;
use ed25519_dalek::VerifyingKey;
use std::collections::{BTreeMap, HashMap, HashSet};

/// A member key, as bytes so it orders and hashes.
type KeyBytes = [u8; 32];

impl ChatRoomStateV1 {
    /// For every id held under two or more keys (in `members` and ban
    /// evidence together), keep one key and drop what the others brought.
    ///
    /// Only keys whose record is REACHABLE compete: its invite chain reaches
    /// the owner through records whose links verify (a link from a shared id
    /// is checked against the inviter key it names; other links were checked
    /// on arrival), within the evidence depth cap for an evidence record.
    /// Among those, in this order:
    ///
    /// 1. A key that has signed a `member_info` the state holds for the id
    ///    beats one that has not. A `member_info` is the one record a key
    ///    with no known secret can never produce and that no later cleanup
    ///    step removes while its member stays, so the decision does not
    ///    depend on when pruning, a cap or a retention horizon removed
    ///    something (bans, invitees, purges, messages and DMs do not count
    ///    for that reason).
    /// 2. Then the lower record digest. With no `member_info` on either side
    ///    this is a choice the other key's author can steer: a member who has
    ///    never published a `member_info` can lose their id to a record under
    ///    another key on every peer. Clients publish one on join
    ///    (freenet/river#987).
    ///
    /// Reachability through another shared id counts only through the key
    /// kept for it, so the decision is repeated until it no longer changes
    /// (each round can only narrow what is reachable; the rounds are bounded
    /// by the number of shared ids).
    ///
    /// The records naming the other keys go, with the records of members a
    /// dropped key invited (an invitee of a decided id whose signature does
    /// not verify under the kept key) and their subtrees. What the id signed
    /// that does not verify under the kept key goes too: its `member_info`,
    /// messages, direct messages and purge records. Bans are left to step
    /// 0-sig, which reads the records this step leaves.
    ///
    /// Known limit: the decision is taken when the pass runs. If the kept
    /// key's member is then removed in the same merge (inactivity prune, a
    /// ban), the other record has already gone, so a three-way merge can end
    /// with no record for the id in one order and the other record in
    /// another. The two converge after one more exchange, and only for a
    /// member who is leaving anyway.
    ///
    /// Returns the ids it decided, each with the key kept. A room where no id
    /// is shared pays one pass over the records.
    pub(crate) fn resolve_shared_ids(
        &mut self,
        parameters: &ChatRoomParametersV1,
    ) -> HashMap<MemberId, VerifyingKey> {
        let mut keys: BTreeMap<MemberId, Vec<VerifyingKey>> = BTreeMap::new();
        for m in self
            .members
            .members
            .iter()
            .chain(&self.ban_evidence.members)
        {
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
        let depth_cap = evidence_depth_cap(self.configuration.configuration.max_members);

        // Decide, then decide again over what is reachable through the keys
        // kept, until nothing changes.
        let mut chosen: BTreeMap<MemberId, Option<KeyBytes>> = BTreeMap::new();
        let mut sigs: HashMap<(EvidenceDigest, KeyBytes), bool> = HashMap::new();
        for _ in 0..=keys.len() {
            let reached = self.reachable_slots(parameters, &keys, &chosen, depth_cap, &mut sigs);
            let next: BTreeMap<MemberId, Option<KeyBytes>> = keys
                .iter()
                .map(|(id, candidates)| {
                    let best = candidates
                        .iter()
                        .filter_map(|vk| {
                            let digest = reached.get(&(*id, vk.to_bytes()))?;
                            Some(((!self.has_member_info(*id, vk), *digest), vk.to_bytes()))
                        })
                        .min()
                        .map(|(_, k)| k);
                    (*id, best)
                })
                .collect();
            if next == chosen {
                break;
            }
            chosen = next;
        }

        let kept: HashMap<MemberId, VerifyingKey> = chosen
            .iter()
            .filter_map(|(id, k)| {
                k.and_then(|k| VerifyingKey::from_bytes(&k).ok())
                    .map(|vk| (*id, vk))
            })
            .collect();
        let keep = |m: &AuthorizedMember| {
            let id = m.member.id();
            let loses = chosen
                .get(&id)
                .is_some_and(|k| *k != Some(m.member.member_vk.to_bytes()));
            let fails_inviter = kept
                .get(&m.member.invited_by)
                .is_some_and(|vk| m.verify_signature(vk).is_err());
            !(loses || fails_inviter)
        };
        let before: HashSet<MemberId> = self
            .members
            .members
            .iter()
            .chain(&self.ban_evidence.members)
            .map(|m| m.member.id())
            .collect();
        self.members.members.retain(keep);
        self.ban_evidence.members.retain(keep);
        // Records whose inviter's id this step left with no record follow.
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

        // What a decided id signed under a key it no longer resolves to. An
        // id left with no record keeps its content here; the steps that drop
        // content of absent members handle it.
        let verifies =
            |id: MemberId, ok: &dyn Fn(&VerifyingKey) -> bool| kept.get(&id).is_none_or(ok);
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

    /// The reachable (id, key) slots of the shared ids in `keys`, each with
    /// the least record digest among its reachable records. One breadth-first
    /// walk over (id, key) slots from the owner: a record is reached from
    /// every slot of its inviter's id, a link from a shared id needs the
    /// record's signature to verify under that slot's key, and a slot of a
    /// shared id whose key `chosen` rules out is reached but not walked
    /// from. An evidence record deeper than `depth_cap` is not reached. Each
    /// record is visited once per slot of its inviter's id (at most two), so
    /// the walk is linear in the records, with at most two signature checks
    /// per record invited by a shared id, memoized in `sigs`.
    fn reachable_slots(
        &self,
        parameters: &ChatRoomParametersV1,
        keys: &BTreeMap<MemberId, Vec<VerifyingKey>>,
        chosen: &BTreeMap<MemberId, Option<KeyBytes>>,
        depth_cap: usize,
        sigs: &mut HashMap<(EvidenceDigest, KeyBytes), bool>,
    ) -> HashMap<(MemberId, KeyBytes), EvidenceDigest> {
        let owner_id = parameters.owner_id();
        let mut by_inviter: HashMap<MemberId, Vec<(&AuthorizedMember, bool)>> = HashMap::new();
        for m in &self.members.members {
            by_inviter
                .entry(m.member.invited_by)
                .or_default()
                .push((m, false));
        }
        for m in &self.ban_evidence.members {
            by_inviter
                .entry(m.member.invited_by)
                .or_default()
                .push((m, true));
        }
        let mut seen: HashSet<(MemberId, KeyBytes)> = HashSet::new();
        let mut out: HashMap<(MemberId, KeyBytes), EvidenceDigest> = HashMap::new();
        let mut frontier: Vec<(MemberId, VerifyingKey)> = vec![(owner_id, parameters.owner)];
        let mut depth = 0;
        while !frontier.is_empty() {
            depth += 1;
            let mut next = Vec::new();
            for (inviter_id, inviter_vk) in &frontier {
                let shared_inviter = keys.contains_key(inviter_id);
                for (m, evidence) in by_inviter.get(inviter_id).into_iter().flatten() {
                    if *evidence && depth > depth_cap {
                        continue;
                    }
                    if shared_inviter
                        && !*sigs
                            .entry((evidence_digest(m), inviter_vk.to_bytes()))
                            .or_insert_with(|| m.verify_signature(inviter_vk).is_ok())
                    {
                        continue;
                    }
                    let id = m.member.id();
                    let slot = (id, m.member.member_vk.to_bytes());
                    if keys.contains_key(&id) {
                        let d = evidence_digest(m);
                        out.entry(slot)
                            .and_modify(|best| *best = (*best).min(d))
                            .or_insert(d);
                    }
                    let walk_on = chosen
                        .get(&id)
                        .is_none_or(|k| *k == Some(m.member.member_vk.to_bytes()));
                    if walk_on && seen.insert(slot) {
                        next.push((id, m.member.member_vk));
                    }
                }
            }
            frontier = next;
        }
        out
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

    /// Whether the state holds a `member_info` for `id` signed by `vk`.
    fn has_member_info(&self, id: MemberId, vk: &VerifyingKey) -> bool {
        self.member_info
            .member_info
            .iter()
            .any(|i| i.member_info.member_id == id && i.verify_signature_with_key(vk).is_ok())
    }
}
