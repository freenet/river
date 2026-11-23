//! Two keys sharing one `MemberId`.
//!
//! A `MemberId` is a non-cryptographic hash of a member's key, so two keys
//! can share one. A record for such a key has no secret behind it, so it can
//! sign nothing; these tests pin that it never displaces a member who has
//! used the room, that peers holding different records for the id converge
//! whichever record each got first, and that no exchange between them fails.

use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{sign_direct_message, DirectMessagesDelta};
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

/// Every key other than `victim` with `victim`'s `MemberId` found by the
/// same search, in both byte orders.
fn other_keys_with_id_of(victim: &VerifyingKey) -> Vec<VerifyingKey> {
    let target: MemberId = victim.into();
    let base = victim.to_bytes();
    let mut out = Vec::new();
    for i in 0..31 {
        for after in [true, false] {
            let mut bytes = base;
            if after {
                if bytes[i] == 255 || bytes[i + 1] < 31 {
                    continue;
                }
                bytes[i] += 1;
                bytes[i + 1] -= 31;
            } else {
                if bytes[i] == 0 || bytes[i + 1] > 255 - 31 {
                    continue;
                }
                bytes[i] -= 1;
                bytes[i + 1] += 31;
            }
            if let Ok(vk) = VerifyingKey::from_bytes(&bytes) {
                if MemberId::from(&vk) == target && vk != *victim && !out.contains(&vk) {
                    out.push(vk);
                }
            }
        }
    }
    out
}

fn digest(m: &AuthorizedMember) -> river_core::room_state::ban_evidence::EvidenceDigest {
    river_core::room_state::ban_evidence::evidence_digest(m)
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
    /// The other key orders after `v`'s, so a read of the LAST record per
    /// id returns it.
    fn new() -> Self {
        Self::with_order(true)
    }

    /// Both orders of the two keys' bytes, so a read of either the first or
    /// the last record for the id is caught.
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

    fn info(&self, who: &SigningKey, nick: &str) -> AuthorizedMemberInfo {
        AuthorizedMemberInfo::new(MemberInfo::new_public(self.id(who), 1, nick.into()), who)
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
        s.verify(s, &self.params).expect("applied state verifies");
    }

    /// `v` joins as a member invited by `a`, with its member_info and a
    /// message.
    fn with_v(&self) -> ChatRoomStateV1 {
        let mut s = self.empty();
        self.apply(
            &mut s,
            ChatRoomStateV1Delta {
                members: Some(MembersDelta::new(vec![
                    self.record(self.v.verifying_key(), &self.a)
                ])),
                member_info: Some(vec![self.info(&self.v, "v")]),
                recent_messages: Some(vec![self.msg(&self.v, 3)]),
                direct_messages: Some(DirectMessagesDelta {
                    new_messages: vec![self.dm_from_v()],
                    advanced_purges: vec![],
                }),
                ..Default::default()
            },
        );
        s
    }

    /// A DM from `v` to `a`.
    fn dm_from_v(&self) -> river_core::room_state::direct_messages::AuthorizedDirectMessage {
        sign_direct_message(
            &self.v,
            self.id(&self.v),
            self.id(&self.a),
            &self.params.owner,
            3,
            b"from v".to_vec(),
        )
        .unwrap()
    }

    fn holds_dm_from_v(&self, s: &ChatRoomStateV1) -> bool {
        s.direct_messages.messages.contains(&self.dm_from_v())
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
                    new_messages: vec![sign_direct_message(
                        inviter,
                        self.id(inviter),
                        self.id(&self.v),
                        &self.params.owner,
                        4,
                        b"x".to_vec(),
                    )
                    .unwrap()],
                    advanced_purges: vec![],
                }),
                ..Default::default()
            },
        );
        assert!(s
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == self.f));
        s
    }

    fn merge(&self, into: &ChatRoomStateV1, from: &ChatRoomStateV1) -> ChatRoomStateV1 {
        let mut out = into.clone();
        out.merge_from(&self.params, from)
            .expect("a merge between two valid states never fails");
        out
    }

    /// Both merge directions agree, verify, and pass `validate_whole_state`.
    fn converge(&self, x: &ChatRoomStateV1, y: &ChatRoomStateV1) -> ChatRoomStateV1 {
        let xy = self.merge(x, y);
        let yx = self.merge(y, x);
        assert!(cbor(&xy) == cbor(&yx), "merge is commutative");
        xy.verify(&xy, &self.params).expect("merged state verifies");
        xy.validate_whole_state(&self.params, &cbor(&xy))
            .expect("merged state is canonical");
        xy
    }

    /// `receiver` applies the network delta `sender` offers against
    /// `receiver`'s summary, as two peers syncing do.
    fn sync(&self, receiver: &ChatRoomStateV1, sender: &ChatRoomStateV1) -> ChatRoomStateV1 {
        let summary = receiver.summarize(receiver, &self.params);
        let delta = sender.delta(sender, &self.params, &summary);
        let mut out = receiver.clone();
        out.apply_delta(receiver, &self.params, &delta)
            .expect("a network delta between two valid states never fails");
        out
    }

    fn holds(&self, s: &ChatRoomStateV1, vk: &VerifyingKey) -> bool {
        s.members.members.iter().any(|m| m.member.member_vk == *vk)
    }
}

fn cbor(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(s, &mut out).unwrap();
    out
}

/// A peer that took the other key's record first gets `v`'s record, and
/// `v`'s member_info and messages, from the next sync, and the other record
/// goes; merges agree in both directions. Before, the summary listed ids
/// only, so `v`'s record was never offered to that peer, and `v`'s
/// member_info failed the whole update there.
#[test]
fn a_record_under_another_key_never_displaces_a_member_who_has_signed_something() {
    for r in Room::both_orders() {
        shared_id_converges_to_the_signed_key(&r);
    }
}

fn shared_id_converges_to_the_signed_key(r: &Room) {
    let x = r.with_v();
    let y = r.with_f(&r.b);
    let merged = r.converge(&x, &y);
    assert!(r.holds(&merged, &r.v.verifying_key()));
    assert!(!r.holds(&merged, &r.f));
    assert!(merged
        .recent_messages
        .messages
        .iter()
        .any(|m| m.message.author == r.id(&r.v)));
    assert!(merged
        .member_info
        .member_info
        .iter()
        .any(|i| i.member_info.member_id == r.id(&r.v)));
    assert!(
        r.holds_dm_from_v(&merged),
        "v's DM survives both directions"
    );

    let synced = r.sync(&y, &x);
    assert!(
        r.holds(&synced, &r.v.verifying_key()),
        "the sync delivers v"
    );
    assert!(!r.holds(&synced, &r.f), "and the other record goes");
    assert!(r.holds_dm_from_v(&synced), "with v's DM");
    synced.verify(&synced, &r.params).unwrap();
    // The peer that held `v` does not take the other record either.
    let synced = r.sync(&x, &y);
    assert!(r.holds(&synced, &r.v.verifying_key()));
    assert!(!r.holds(&synced, &r.f));
}

/// A `member_info` decides before the record digest and invite depth: with
/// the other key's record invited by the owner (shallower than `v`) and by
/// each member, so its digest falls on both sides of `v`'s, `v` is kept.
#[test]
fn a_signed_key_wins_over_a_shallower_record() {
    let mut digests_below = 0;
    for r in Room::both_orders() {
        let v_rec = r.record(r.v.verifying_key(), &r.a);
        for inviter in [&r.owner, &r.a, &r.b] {
            if digest(&r.record(r.f, inviter)) < digest(&v_rec) {
                digests_below += 1;
            }
            let merged = r.converge(&r.with_v(), &r.with_f(inviter));
            assert!(r.holds(&merged, &r.v.verifying_key()));
            assert!(!r.holds(&merged, &r.f));
        }
    }
    assert!(
        digests_below > 0,
        "some case has the other key's digest below v's"
    );
}

/// `v`'s invitee, signed by `v`, arrives at a peer holding the other key's
/// record for `v`'s id. It verifies under none of the keys held there, and
/// is skipped rather than failing the update; with `v`'s record beside it,
/// it is kept.
#[test]
fn a_record_signed_by_another_key_for_its_inviter_is_skipped_not_refused() {
    let r = Room::new();
    let w = sk("w");
    let y = r.with_f(&r.b);
    let w_rec = r.record(w.verifying_key(), &r.v);
    let mut alone = y.clone();
    alone
        .apply_delta(
            &y,
            &r.params,
            &Some(ChatRoomStateV1Delta {
                members: Some(MembersDelta::new(vec![w_rec.clone()])),
                recent_messages: Some(vec![r.msg(&w, 9)]),
                ..Default::default()
            }),
        )
        .expect("skipped, not refused");
    assert!(!alone.members.members.contains(&w_rec));

    let mut with_v = y.clone();
    with_v
        .apply_delta(
            &y,
            &r.params,
            &Some(ChatRoomStateV1Delta {
                members: Some(MembersDelta::new(vec![
                    r.record(r.v.verifying_key(), &r.a),
                    w_rec.clone(),
                ])),
                member_info: Some(vec![r.info(&r.v, "v")]),
                recent_messages: Some(vec![r.msg(&r.v, 8), r.msg(&w, 9)]),
                ..Default::default()
            }),
        )
        .unwrap();
    with_v.verify(&with_v, &r.params).unwrap();
    assert!(with_v.members.members.contains(&w_rec));
    assert!(!r.holds(&with_v, &r.f));
}

/// `v`'s ban rides in the same delta as `v`'s record to a peer holding the
/// other key's record. Bans apply before members, so the ban is checked
/// against `v`'s key only in cleanup; both directions keep it.
#[test]
fn a_ban_by_the_signed_key_survives_either_merge_direction() {
    let r = Room::new();
    let t = sk("t");
    let mut x = r.with_v();
    r.apply(
        &mut x,
        ChatRoomStateV1Delta {
            members: Some(MembersDelta::new(vec![r.record(t.verifying_key(), &r.v)])),
            recent_messages: Some(vec![r.msg(&t, 5)]),
            ..Default::default()
        },
    );
    let ban = r.ban(&r.v, r.id(&t), 6);
    r.apply(
        &mut x,
        ChatRoomStateV1Delta {
            bans: Some(vec![ban.clone()]),
            ..Default::default()
        },
    );
    assert!(x.bans.0.contains(&ban));
    let merged = r.converge(&x, &r.with_f(&r.b));
    assert!(merged.bans.0.contains(&ban));
    assert!(!r.holds(&merged, &r.f));
}

/// The case the #974 fork fuzz found: `v` is held only as ban evidence (a
/// stored ban needs its record) with its member_info, and another peer holds
/// a member record for `v`'s id under the other key. The merge used to keep
/// the member record and `v`'s member_info, which then failed `verify`.
#[test]
fn evidence_with_a_signed_member_info_beats_a_member_record_under_another_key() {
    let r = Room::new();
    let mut x = r.with_v();
    // `a` (v's inviter) bans `v`: `v` leaves `members`, and its record stays
    // as ban evidence for the ban, with its member_info.
    r.apply(
        &mut x,
        ChatRoomStateV1Delta {
            bans: Some(vec![r.ban(&r.a, r.id(&r.v), 7)]),
            ..Default::default()
        },
    );
    assert!(!r.holds(&x, &r.v.verifying_key()));
    assert!(x
        .ban_evidence
        .members
        .iter()
        .any(|m| m.member.member_vk == r.v.verifying_key()));
    let merged = r.converge(&x, &r.with_f(&r.b));
    assert!(!r.holds(&merged, &r.f));
    for info in &merged.member_info.member_info {
        if info.member_info.member_id == r.id(&r.v) {
            info.verify_signature_with_key(&r.v.verifying_key())
                .expect("the member_info kept verifies under v's key");
        }
    }
}

/// `v` with a record, a message and a DM only (no `member_info`), against the
/// other key's record. Neither has signed a `member_info`, so the lower
/// record digest decides, whatever the invite depth. When the other key wins,
/// `v`'s message and DM go with `v`'s record and the merged state verifies.
/// The same key is kept in both merge directions, through a summary sync, and
/// when the exchange withholds `v`'s message behind a retention horizon:
/// messages and DMs are never part of the decision.
#[test]
fn without_member_info_the_lower_record_digest_decides_and_the_loser_s_content_goes() {
    for r in Room::both_orders() {
        let v_rec = r.record(r.v.verifying_key(), &r.a);
        // An inviter for the other key whose record digest is lower than
        // v's, so v loses and its content must go.
        let inviter = [&r.owner, &r.a, &r.b]
            .into_iter()
            .find(|i| digest(&r.record(r.f, i)) < digest(&v_rec))
            .expect("one of three inviters gives a lower digest");
        let mut x = r.empty();
        r.apply(
            &mut x,
            ChatRoomStateV1Delta {
                members: Some(MembersDelta::new(vec![v_rec.clone()])),
                recent_messages: Some(vec![r.msg(&r.v, 3)]),
                direct_messages: Some(DirectMessagesDelta {
                    new_messages: vec![r.dm_from_v()],
                    advanced_purges: vec![],
                }),
                ..Default::default()
            },
        );
        let y = r.with_f(inviter);
        let check = |s: &ChatRoomStateV1, what: &str| {
            assert!(r.holds(s, &r.f), "{what}: the lower digest is kept");
            assert!(!r.holds(s, &r.v.verifying_key()), "{what}");
            assert!(
                !s.recent_messages
                    .messages
                    .iter()
                    .any(|m| m.message.author == r.id(&r.v)),
                "{what}: v's message goes"
            );
            assert!(!r.holds_dm_from_v(s), "{what}: v's DM goes");
            s.verify(s, &r.params).expect(what);
        };
        check(&r.converge(&x, &y), "merge");
        check(&r.sync(&y, &x), "sync into the other key's peer");
        check(&r.sync(&x, &y), "sync into v's peer");
        // The exchange withholds v's message (a horizon below it): the same
        // key is kept.
        let mut x_without_history = x.clone();
        x_without_history.recent_messages.messages.clear();
        check(
            &r.converge(&x_without_history, &y),
            "merge without v's message",
        );
    }
}

/// A crafted delta can carry `v`'s record together with another forged key
/// for `v`'s id whose record digest is lower: an apply admits one new key per
/// id, so `v` is skipped there. That only delays `v`: the next honest delta
/// carries `v` alone, and `v`, who has a `member_info`, ends kept on every
/// peer, whichever delta arrives first.
#[test]
fn a_crafted_delta_with_a_lower_digest_key_only_delays_the_signed_key() {
    let r = Room::new();
    let v_rec = r.record(r.v.verifying_key(), &r.a);
    let others = other_keys_with_id_of(&r.v.verifying_key());
    // A second forged key and inviter whose record digest is below v's.
    let f2_rec = others
        .iter()
        .filter(|k| **k != r.f)
        .flat_map(|k| [&r.owner, &r.a, &r.b].map(|i| r.record(*k, i)))
        .find(|rec| digest(rec) < digest(&v_rec))
        .expect("a forged record with a lower digest");
    let held = r.with_f(&r.b);
    let crafted = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![v_rec.clone(), f2_rec.clone()])),
        member_info: Some(vec![r.info(&r.v, "v")]),
        ..Default::default()
    };
    let honest = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![v_rec.clone()])),
        member_info: Some(vec![r.info(&r.v, "v")]),
        recent_messages: Some(vec![r.msg(&r.v, 3)]),
        ..Default::default()
    };
    let apply = |s: &ChatRoomStateV1, d: &ChatRoomStateV1Delta| {
        let mut out = s.clone();
        out.apply_delta(s, &r.params, &Some(d.clone()))
            .expect("never refused");
        out.verify(&out, &r.params).expect("verifies");
        out
    };
    let after_crafted = apply(&held, &crafted);
    assert!(
        !r.holds(&after_crafted, &r.v.verifying_key()),
        "the crafted delta admits the lower-digest key, not v"
    );
    for end in [
        apply(&after_crafted, &honest),
        apply(&apply(&held, &honest), &crafted),
    ] {
        assert!(r.holds(&end, &r.v.verifying_key()), "v is kept");
        assert_eq!(
            end.members
                .members
                .iter()
                .filter(|m| m.member.id() == r.id(&r.v))
                .count(),
            1
        );
    }
}

/// A state naming three keys for one id is refused before any signature is
/// checked.
#[test]
fn a_state_with_three_keys_for_one_id_is_refused() {
    let r = Room::new();
    let mut s = r.with_v();
    let others = other_keys_with_id_of(&r.v.verifying_key());
    assert!(others.len() >= 2, "two other keys for v's id");
    for k in &others[..2] {
        s.members.members.push(r.record(*k, &r.b));
    }
    let err = s.verify(&s, &r.params).unwrap_err();
    assert!(err.contains("more than 2 keys"), "{err}");
}

/// One delta brings the other key's record, invited by `a`, and `a`'s ban on
/// the shared id to a peer holding `v` (invited by `b`, with a DM). Over the
/// other record the ban would be by the target's inviter and take effect;
/// over `v`'s it does not. The DM apply judges bans over the records cleanup
/// keeps, so `v` and `v`'s DM both stay.
#[test]
fn the_dm_apply_judges_bans_over_the_key_cleanup_keeps() {
    let r = Room::new();
    let mut x = r.empty();
    r.apply(
        &mut x,
        ChatRoomStateV1Delta {
            members: Some(MembersDelta::new(vec![r.record(r.v.verifying_key(), &r.b)])),
            member_info: Some(vec![r.info(&r.v, "v")]),
            recent_messages: Some(vec![r.msg(&r.v, 3)]),
            direct_messages: Some(DirectMessagesDelta {
                new_messages: vec![r.dm_from_v()],
                advanced_purges: vec![],
            }),
            ..Default::default()
        },
    );
    assert!(r.holds_dm_from_v(&x));
    let parent = x.clone();
    x.apply_delta(
        &parent,
        &r.params,
        &Some(ChatRoomStateV1Delta {
            members: Some(MembersDelta::new(vec![r.record(r.f, &r.a)])),
            bans: Some(vec![r.ban(&r.a, r.id(&r.v), 9)]),
            ..Default::default()
        }),
    )
    .unwrap();
    x.verify(&x, &r.params).unwrap();
    assert!(r.holds(&x, &r.v.verifying_key()));
    assert!(!r.holds(&x, &r.f));
    assert!(
        r.holds_dm_from_v(&x),
        "v's DM is not swept by a ban that does not apply to v"
    );
}
