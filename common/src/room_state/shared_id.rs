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

        self.apply_shared_id_decision(&chosen, parameters)
    }

    /// Drop what `chosen` rules out: for each decided id, the records under
    /// another key (all of them for `None`), the records of members a
    /// dropped key invited and their subtrees, and the id's `member_info`,
    /// messages, DMs and purge records that do not verify under the kept
    /// key. Split from the decision so a test can force one.
    pub(crate) fn apply_shared_id_decision(
        &mut self,
        chosen: &BTreeMap<MemberId, Option<KeyBytes>>,
        parameters: &ChatRoomParametersV1,
    ) -> HashMap<MemberId, VerifyingKey> {
        let owner_id = parameters.owner_id();
        let owner_vk = parameters.owner;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
    use crate::room_state::ban_evidence::BanEvidenceV1;
    use crate::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
    use crate::room_state::member::tests::invite_signature_checks;
    use crate::room_state::member::{Member, MembersDelta};
    use crate::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
    use crate::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
    use ed25519_dalek::SigningKey;
    use freenet_scaffold::ComposableState;
    use std::time::{Duration, SystemTime};

    fn sk(label: &str) -> SigningKey {
        SigningKey::from_bytes(blake3::hash(label.as_bytes()).as_bytes())
    }

    /// Keys other than `victim` with its `MemberId` (no secret keys).
    fn other_keys_with_id_of(victim: &VerifyingKey) -> Vec<VerifyingKey> {
        let target = MemberId::from(victim);
        let base = victim.to_bytes();
        let mut out = Vec::new();
        for i in 0..31 {
            for after in [true, false] {
                let mut b = base;
                if after {
                    if b[i] == 255 || b[i + 1] < 31 {
                        continue;
                    }
                    b[i] += 1;
                    b[i + 1] -= 31;
                } else {
                    if b[i] == 0 || b[i + 1] > 255 - 31 {
                        continue;
                    }
                    b[i] -= 1;
                    b[i + 1] += 31;
                }
                if let Ok(vk) = VerifyingKey::from_bytes(&b) {
                    if MemberId::from(&vk) == target && vk != *victim && !out.contains(&vk) {
                        out.push(vk);
                    }
                }
            }
        }
        out
    }

    fn record(owner: MemberId, vk: VerifyingKey, inviter: &SigningKey) -> AuthorizedMember {
        AuthorizedMember::new(
            Member {
                owner_member_id: owner,
                invited_by: inviter.verifying_key().into(),
                member_vk: vk,
            },
            inviter,
        )
    }

    fn room(owner: &SigningKey) -> (ChatRoomStateV1, ChatRoomParametersV1) {
        let owner_id = MemberId::from(&owner.verifying_key());
        let state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    max_members: 1000,
                    ..Default::default()
                },
                owner,
            ),
            ..Default::default()
        };
        (
            state,
            ChatRoomParametersV1 {
                owner: owner.verifying_key(),
            },
        )
    }

    /// A decision that rules out a key which has signed things (reachable
    /// only with a second keypair for the id) drops that key's record, the
    /// members it invited and their subtree, and its member_info and
    /// messages, leaving a state that verifies.
    #[test]
    fn a_dropped_key_takes_its_invitees_and_content_with_it() {
        let (o, a, v, w, x) = (sk("o"), sk("a"), sk("v"), sk("w"), sk("x"));
        let (mut s, p) = room(&o);
        let owner = p.owner_id();
        s.members.members = vec![
            record(owner, a.verifying_key(), &o),
            record(owner, v.verifying_key(), &a),
            record(owner, w.verifying_key(), &v),
            record(owner, x.verifying_key(), &w),
        ];
        let v_id = MemberId::from(&v.verifying_key());
        s.member_info.member_info = vec![AuthorizedMemberInfo::new(
            MemberInfo::new_public(v_id, 1, "v".into()),
            &v,
        )];
        s.recent_messages.messages = vec![AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner,
                author: v_id,
                time: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
                content: RoomMessageBody::public("hi".into()),
            },
            &v,
        )];
        let other = sk("other").verifying_key().to_bytes();
        let chosen = BTreeMap::from([(v_id, Some(other))]);
        s.apply_shared_id_decision(&chosen, &p);
        assert_eq!(s.members.members.len(), 1, "only a stays");
        assert!(s.member_info.member_info.is_empty());
        assert!(s.recent_messages.messages.is_empty());
        s.verify(&s, &p).unwrap();
    }

    /// One delta naming many keys for one id, each signed into the room by a
    /// member, plus many records naming that id as inviter with junk
    /// signatures: the apply admits one new key for the id, so it checks
    /// about one signature per record, not keys times records.
    #[test]
    fn many_keys_for_one_id_cost_one_signature_per_record() {
        let (o, a, v) = (sk("o"), sk("a"), sk("v"));
        let (mut s, p) = room(&o);
        let owner = p.owner_id();
        s.members.members = vec![record(owner, a.verifying_key(), &o)];
        let v_id = MemberId::from(&v.verifying_key());
        let forged = other_keys_with_id_of(&v.verifying_key());
        assert!(forged.len() >= 20, "{} forged keys", forged.len());
        let mut added: Vec<AuthorizedMember> =
            forged.iter().map(|k| record(owner, *k, &a)).collect();
        let junk_signature = added[0].signature;
        let m = 200;
        for i in 0..m {
            added.push(AuthorizedMember {
                member: Member {
                    owner_member_id: owner,
                    invited_by: v_id,
                    member_vk: sk(&format!("junk{i}")).verifying_key(),
                },
                signature: junk_signature,
            });
        }
        let parent = s.clone();
        let before = invite_signature_checks();
        s.members
            .apply_delta(&parent, &p, &Some(MembersDelta::new(added)))
            .unwrap();
        let checks = invite_signature_checks() - before;
        assert!(
            checks <= m + 2,
            "{checks} signature checks for {m} invitees of one id named under {} keys",
            forged.len()
        );
        let keys_for_v = s
            .members
            .members
            .iter()
            .filter(|r| r.member.id() == v_id)
            .count();
        assert_eq!(keys_for_v, 1, "one new key admitted");
    }

    /// Evidence records for two ids under many keys each, invited by each
    /// other (a cycle that never reaches the owner), named by a ban: the
    /// apply admits one new key per id and walks each chain once.
    #[test]
    fn many_keys_in_an_evidence_cycle_are_walked_once() {
        let (o, xk, yk) = (sk("o"), sk("x"), sk("y"));
        let (mut s, p) = room(&o);
        let owner = p.owner_id();
        let (x_id, y_id) = (
            MemberId::from(&xk.verifying_key()),
            MemberId::from(&yk.verifying_key()),
        );
        // An unverified ban by y on x makes both ids referenced.
        s.bans = BansV1(vec![AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner,
                banned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
                banned_user: x_id,
            },
            y_id,
            &yk,
        )]);
        let mut delta = Vec::new();
        for k in other_keys_with_id_of(&xk.verifying_key()) {
            delta.push(record(owner, k, &yk));
        }
        for k in other_keys_with_id_of(&yk.verifying_key()) {
            delta.push(record(owner, k, &xk));
        }
        assert!(delta.len() >= 40);
        let parent = s.clone();
        let mut evidence = BanEvidenceV1::default();
        let before = crate::room_state::ban_evidence::tests::chain_walks();
        evidence
            .apply_delta(&parent, &p, &Some(delta))
            .expect("never refused");
        let walks = crate::room_state::ban_evidence::tests::chain_walks() - before;
        assert!(evidence.members.is_empty(), "nothing reaches the owner");
        assert!(walks <= 40, "{walks} chain walks");
    }
}
