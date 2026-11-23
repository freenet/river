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

/// A key other than `victim` with `victim`'s `MemberId`, or `None`. It has
/// no secret key.
fn other_key_with_id_of(victim: &VerifyingKey) -> Option<VerifyingKey> {
    let target: MemberId = victim.into();
    let base = victim.to_bytes();
    (0..31).find_map(|i| {
        let mut bytes = base;
        if bytes[i] == 255 || bytes[i + 1] < 31 {
            return None;
        }
        bytes[i] += 1;
        bytes[i + 1] -= 31;
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
    fn new() -> Self {
        let owner = sk("owner");
        let owner_id = MemberId::from(&owner.verifying_key());
        let (v, f) = (0..)
            .find_map(|i| {
                let v = sk(&format!("v{i}"));
                other_key_with_id_of(&v.verifying_key()).map(|f| (v, f))
            })
            .unwrap();
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
    let r = Room::new();
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

    let synced = r.sync(&y, &x);
    assert!(
        r.holds(&synced, &r.v.verifying_key()),
        "the sync delivers v"
    );
    assert!(!r.holds(&synced, &r.f), "and the other record goes");
    synced.verify(&synced, &r.params).unwrap();
    // The peer that held `v` does not take the other record either.
    let synced = r.sync(&x, &y);
    assert!(r.holds(&synced, &r.v.verifying_key()));
    assert!(!r.holds(&synced, &r.f));
}

/// Whether `v` has signed something decides before invite depth: the other
/// key's record, invited by the owner, sits higher than `v` (invited by `a`)
/// and still goes.
#[test]
fn a_signed_key_wins_over_a_shallower_record() {
    let r = Room::new();
    let merged = r.converge(&r.with_v(), &r.with_f(&r.owner));
    assert!(r.holds(&merged, &r.v.verifying_key()));
    assert!(!r.holds(&merged, &r.f));
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

/// With nothing signed by either key, the record kept is decided by depth,
/// then digest, the same way on both sides.
#[test]
fn two_unsigned_keys_resolve_the_same_way_in_both_directions() {
    let r = Room::new();
    let mut x = r.empty();
    r.apply(
        &mut x,
        ChatRoomStateV1Delta {
            members: Some(MembersDelta::new(vec![r.record(r.v.verifying_key(), &r.a)])),
            direct_messages: Some(DirectMessagesDelta {
                new_messages: vec![sign_direct_message(
                    &r.a,
                    r.id(&r.a),
                    r.id(&r.v),
                    &r.params.owner,
                    4,
                    b"y".to_vec(),
                )
                .unwrap()],
                advanced_purges: vec![],
            }),
            ..Default::default()
        },
    );
    let merged = r.converge(&x, &r.with_f(&r.owner));
    // The other key's record is invited by the owner (depth 1), `v`'s by `a`
    // (depth 2): with neither signed, depth decides.
    assert!(r.holds(&merged, &r.f));
    assert!(!r.holds(&merged, &r.v.verifying_key()));
}
