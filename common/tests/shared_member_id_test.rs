//! Two keys sharing one `MemberId`.
//!
//! A `MemberId` is a non-cryptographic hash of a member's key, so two keys
//! can share one. A peer keeps the record for an id that it took first, and
//! checks everything the id signed against that record's key. These tests pin
//! that an exchange between a peer holding one key's record and a peer
//! holding the other's never fails, that every state it produces verifies,
//! and that content signed by the other key is skipped rather than stored.

use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{
    sign_direct_message, sign_recipient_purges, AuthorizedDirectMessage, AuthorizedRecipientPurges,
    DirectMessagesDelta, RecipientPurges,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::time::{Duration, SystemTime};

fn sk(label: &str) -> SigningKey {
    SigningKey::from_bytes(blake3::hash(label.as_bytes()).as_bytes())
}

/// A key other than `victim` with `victim`'s `MemberId`, ordering after
/// `victim`'s bytes when `after` is set and before them otherwise, or `None`.
/// It has no secret key.
fn other_key_with_id_of(victim: &VerifyingKey, after: bool) -> Option<VerifyingKey> {
    let target: MemberId = victim.into();
    let base = victim.to_bytes();
    (0..31).find_map(|i| {
        let mut bytes = base;
        if after {
            if bytes[i] == 255 || bytes[i + 1] < 31 {
                return None;
            }
            bytes[i] += 1;
            bytes[i + 1] -= 31;
        } else {
            if bytes[i] == 0 || bytes[i + 1] > 255 - 31 {
                return None;
            }
            bytes[i] -= 1;
            bytes[i + 1] += 31;
        }
        VerifyingKey::from_bytes(&bytes)
            .ok()
            .filter(|vk| MemberId::from(vk) == target && vk != victim)
    })
}

struct Room {
    owner: SigningKey,
    owner_id: MemberId,
    params: ChatRoomParametersV1,
    /// Invited by the owner.
    a: SigningKey,
    b: SigningKey,
    /// The member whose id is shared, invited by `a`.
    v: SigningKey,
    /// The other key for `v`'s id.
    f: VerifyingKey,
    base: SystemTime,
}

impl Room {
    /// Both orders of the two keys' bytes, so a read of either the first or
    /// the last record stored for an id is caught.
    fn both_orders() -> [Self; 2] {
        [Self::with_order(true), Self::with_order(false)]
    }

    fn with_order(f_after: bool) -> Self {
        let owner = sk("owner");
        let owner_id = MemberId::from(&owner.verifying_key());
        let (v, f) = (0..)
            .find_map(|i| {
                let v = sk(&format!("v{i}"));
                other_key_with_id_of(&v.verifying_key(), f_after).map(|f| (v, f))
            })
            .unwrap();
        assert_eq!(f.as_bytes() > v.verifying_key().as_bytes(), f_after);
        Room {
            params: ChatRoomParametersV1 {
                owner: owner.verifying_key(),
            },
            owner_id,
            owner,
            a: sk("a"),
            b: sk("b"),
            v,
            f,
            base: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        }
    }

    fn id(&self, k: &SigningKey) -> MemberId {
        k.verifying_key().into()
    }

    fn record(&self, vk: VerifyingKey, inviter: &SigningKey) -> AuthorizedMember {
        AuthorizedMember::new(
            Member {
                owner_member_id: self.owner_id,
                invited_by: inviter.verifying_key().into(),
                member_vk: vk,
            },
            inviter,
        )
    }

    fn msg(&self, author: &SigningKey, t: u64) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: self.owner_id,
                author: self.id(author),
                time: self.base + Duration::from_secs(t),
                content: RoomMessageBody::public(format!("m{t}")),
            },
            author,
        )
    }

    fn info(&self, who: &SigningKey) -> AuthorizedMemberInfo {
        AuthorizedMemberInfo::new(MemberInfo::new_public(self.id(who), 1, "nick".into()), who)
    }

    fn ban(&self, by: &SigningKey, target: MemberId, t: u64) -> AuthorizedUserBan {
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: self.owner_id,
                banned_at: self.base + Duration::from_secs(t),
                banned_user: target,
            },
            self.id(by),
            by,
        )
    }

    fn dm(&self, from: &SigningKey, to: &SigningKey, t: u64) -> AuthorizedDirectMessage {
        sign_direct_message(
            from,
            self.id(from),
            self.id(to),
            &self.params.owner,
            t,
            format!("dm{t}").into_bytes(),
        )
        .unwrap()
    }

    fn purge(&self, by: &SigningKey) -> AuthorizedRecipientPurges {
        sign_recipient_purges(
            by,
            self.id(by),
            &self.params.owner,
            RecipientPurges {
                version: 1,
                purged: vec![],
            },
        )
        .unwrap()
    }

    /// `a` and `b` invited by the owner, each with a message.
    fn empty(&self) -> ChatRoomStateV1 {
        let mut s = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: self.owner_id,
                    max_members: 20,
                    max_user_bans: 10,
                    max_recent_messages: 50,
                    max_message_size: 1000,
                    ..Default::default()
                },
                &self.owner,
            ),
            ..Default::default()
        };
        self.apply(
            &mut s,
            ChatRoomStateV1Delta {
                members: Some(MembersDelta::new(vec![
                    self.record(self.a.verifying_key(), &self.owner),
                    self.record(self.b.verifying_key(), &self.owner),
                ])),
                recent_messages: Some(vec![self.msg(&self.a, 1), self.msg(&self.b, 2)]),
                ..Default::default()
            },
        );
        s
    }

    fn apply(&self, s: &mut ChatRoomStateV1, d: ChatRoomStateV1Delta) {
        let parent = s.clone();
        s.apply_delta(&parent, &self.params, &Some(d))
            .expect("delta applies");
        self.valid(s, "apply");
    }

    /// The state verifies and is canonical.
    fn valid(&self, s: &ChatRoomStateV1, what: &str) {
        s.verify(s, &self.params)
            .unwrap_or_else(|e| panic!("{what}: verify: {e}"));
        s.validate_whole_state(&self.params, &cbor(s))
            .unwrap_or_else(|e| panic!("{what}: validate: {e}"));
    }

    /// `v` joins, invited by `a`, with its member_info, a message, a DM, a
    /// purge record, an invitee `w` it signed, and a ban it issued on `t`
    /// (another member it invited).
    fn with_v(&self) -> ChatRoomStateV1 {
        let (w, t) = (sk("w"), sk("t"));
        let mut s = self.empty();
        self.apply(
            &mut s,
            ChatRoomStateV1Delta {
                members: Some(MembersDelta::new(vec![
                    self.record(self.v.verifying_key(), &self.a),
                    self.record(w.verifying_key(), &self.v),
                    self.record(t.verifying_key(), &self.v),
                ])),
                member_info: Some(vec![self.info(&self.v)]),
                recent_messages: Some(vec![self.msg(&self.v, 3), self.msg(&w, 4), self.msg(&t, 5)]),
                direct_messages: Some(DirectMessagesDelta {
                    new_messages: vec![self.dm(&self.v, &self.a, 6)],
                    advanced_purges: vec![self.purge(&self.v)],
                }),
                ..Default::default()
            },
        );
        self.apply(
            &mut s,
            ChatRoomStateV1Delta {
                bans: Some(vec![self.ban(&self.v, self.id(&t), 7)]),
                ..Default::default()
            },
        );
        s
    }

    /// The other key for `v`'s id, invited by `inviter` and kept by a DM
    /// from `inviter`.
    fn with_f(&self, inviter: &SigningKey) -> ChatRoomStateV1 {
        let mut s = self.empty();
        self.apply(
            &mut s,
            ChatRoomStateV1Delta {
                members: Some(MembersDelta::new(vec![self.record(self.f, inviter)])),
                direct_messages: Some(DirectMessagesDelta {
                    new_messages: vec![self.dm(inviter, &self.v, 8)],
                    advanced_purges: vec![],
                }),
                ..Default::default()
            },
        );
        assert!(self.holds(&s, &self.f));
        s
    }

    fn merge(&self, into: &ChatRoomStateV1, from: &ChatRoomStateV1) -> ChatRoomStateV1 {
        let mut out = into.clone();
        out.merge_from(&self.params, from)
            .expect("a merge between two valid states never fails");
        self.valid(&out, "merge");
        out
    }

    /// `receiver` applies the network delta `sender` offers against
    /// `receiver`'s summary, as two peers syncing do.
    fn sync(&self, receiver: &ChatRoomStateV1, sender: &ChatRoomStateV1) -> ChatRoomStateV1 {
        let summary = receiver.summarize(receiver, &self.params);
        let delta = sender.delta(sender, &self.params, &summary);
        let mut out = receiver.clone();
        out.apply_delta(receiver, &self.params, &delta)
            .expect("a network delta between two valid states never fails");
        self.valid(&out, "sync");
        out
    }

    fn holds(&self, s: &ChatRoomStateV1, vk: &VerifyingKey) -> bool {
        s.members.members.iter().any(|m| m.member.member_vk == *vk)
    }

    /// Nothing `v` signed is held.
    fn holds_nothing_of_v(&self, s: &ChatRoomStateV1) -> bool {
        let v = self.id(&self.v);
        !s.member_info
            .member_info
            .iter()
            .any(|i| i.member_info.member_id == v)
            && !s
                .recent_messages
                .messages
                .iter()
                .any(|m| m.message.author == v)
            && !s
                .direct_messages
                .messages
                .iter()
                .any(|m| m.message.sender == v)
            && !s.direct_messages.purges.iter().any(|p| p.recipient_id == v)
            && !s.bans.0.iter().any(|b| b.banned_by == v)
    }
}

fn cbor(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(s, &mut out).unwrap();
    out
}

/// A peer holding the other key's record for `v`'s id syncs with a peer
/// holding `v`: `v`'s member_info, messages, DM, purge record, ban and
/// invitees all verify under a key the receiver does not hold. Each is
/// skipped, and the update applies. Before, `v`'s member_info, purge record
/// or an invitee `v` signed failed the whole update, so the receiver stopped
/// taking updates from that peer for good. Each peer keeps the record it took
/// first; every state verifies, in both merge directions and through a
/// summary sync, for both byte orders of the two keys.
#[test]
fn a_peer_holding_another_key_for_an_id_keeps_syncing() {
    for r in Room::both_orders() {
        let x = r.with_v();
        let y = r.with_f(&r.b);
        for s in [r.sync(&y, &x), r.merge(&y, &x)] {
            assert!(r.holds(&s, &r.f) && !r.holds(&s, &r.v.verifying_key()));
            assert!(r.holds_nothing_of_v(&s), "content signed by v is skipped");
        }
        for s in [r.sync(&x, &y), r.merge(&x, &y)] {
            assert!(r.holds(&s, &r.v.verifying_key()) && !r.holds(&s, &r.f));
            assert!(s
                .member_info
                .member_info
                .iter()
                .any(|i| i.member_info.member_id == r.id(&r.v)));
        }
    }
}

/// The case the #974 fork fuzz found: `v` is held only as ban evidence (a
/// stored ban needs its record) with its member_info, and another peer holds
/// a member record under the other key for `v`'s id. The merge used to keep
/// the member record and `v`'s member_info, which then failed `verify`. Now
/// the evidence-holding peer skips the other member record, the member-holding
/// peer skips `v`'s evidence and member_info, and both merges verify.
#[test]
fn evidence_with_member_info_against_a_member_record_under_another_key_verifies() {
    for r in Room::both_orders() {
        let mut x = r.with_v();
        // `a` (v's inviter) bans `v`: `v` leaves `members`, and its record
        // stays as ban evidence for the ban, with its member_info.
        r.apply(
            &mut x,
            ChatRoomStateV1Delta {
                bans: Some(vec![r.ban(&r.a, r.id(&r.v), 9)]),
                ..Default::default()
            },
        );
        assert!(!r.holds(&x, &r.v.verifying_key()));
        assert!(x
            .ban_evidence
            .members
            .iter()
            .any(|m| m.member.member_vk == r.v.verifying_key()));
        let y = r.with_f(&r.b);
        let xy = r.merge(&x, &y);
        assert!(!r.holds(&xy, &r.f), "the evidence-holding peer skips f");
        let yx = r.merge(&y, &x);
        assert!(yx.member_info.member_info.iter().all(|i| {
            i.member_info.member_id != r.id(&r.v) || i.verify_signature_with_key(&r.f).is_ok()
        }));
        r.sync(&y, &x);
        r.sync(&x, &y);
    }
}

/// One delta brings `v`'s record and the other key's record together to a
/// peer holding neither. One is taken and everything the other key signed is
/// skipped; the result verifies whichever order the delta lists them in.
#[test]
fn one_delta_naming_both_keys_applies_and_verifies() {
    for r in Room::both_orders() {
        let base = r.empty();
        let v_rec = r.record(r.v.verifying_key(), &r.a);
        let f_rec = r.record(r.f, &r.b);
        for order in [vec![v_rec.clone(), f_rec.clone()], vec![f_rec, v_rec]] {
            let mut s = base.clone();
            s.apply_delta(
                &base,
                &r.params,
                &Some(ChatRoomStateV1Delta {
                    members: Some(MembersDelta::new(order)),
                    member_info: Some(vec![r.info(&r.v)]),
                    recent_messages: Some(vec![r.msg(&r.v, 3)]),
                    direct_messages: Some(DirectMessagesDelta {
                        new_messages: vec![r.dm(&r.v, &r.a, 4)],
                        advanced_purges: vec![r.purge(&r.v)],
                    }),
                    ..Default::default()
                }),
            )
            .expect("applies");
            r.valid(&s, "one delta");
            // At most one record for the id (the other key's record, with
            // nothing it signed, is pruned for inactivity when it is the one
            // taken).
            assert!(
                s.members
                    .members
                    .iter()
                    .filter(|m| m.member.id() == r.id(&r.v))
                    .count()
                    <= 1
            );
        }
    }
}

/// A peer holding `v` as ban evidence, with its member_info, receives an
/// evidence record under the other key for `v`'s id, even one with a lower
/// record digest. It keeps `v`'s record: replacing it would leave `v`'s
/// member_info checked against a key that never signed it.
#[test]
fn evidence_under_another_key_never_replaces_held_evidence() {
    for r in Room::both_orders() {
        let mut x = r.with_v();
        r.apply(
            &mut x,
            ChatRoomStateV1Delta {
                bans: Some(vec![r.ban(&r.a, r.id(&r.v), 9)]),
                ..Default::default()
            },
        );
        let held = x
            .ban_evidence
            .members
            .iter()
            .find(|m| m.member.id() == r.id(&r.v))
            .expect("v held as evidence")
            .clone();
        let digest = river_core::room_state::ban_evidence::evidence_digest;
        let others = [&r.owner, &r.a, &r.b].map(|i| r.record(r.f, i));
        let lower = others
            .iter()
            .find(|o| digest(o) < digest(&held))
            .or(others.first())
            .unwrap()
            .clone();
        let parent = x.clone();
        let mut s = x.clone();
        s.apply_delta(
            &parent,
            &r.params,
            &Some(ChatRoomStateV1Delta {
                ban_evidence: Some(vec![lower.clone()]),
                ..Default::default()
            }),
        )
        .expect("applies");
        r.valid(&s, "evidence under another key");
        assert!(s.ban_evidence.members.contains(&held));
        assert!(!s.ban_evidence.members.contains(&lower));
    }
}
