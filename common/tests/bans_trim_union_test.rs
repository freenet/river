//! The whole-state merge trims an incoming ban list to the work bound by the
//! ban cap's own rule, judged over the bans and records BOTH states hold, so
//! a ban one side's records make dead is not kept in place of one the merged
//! room keeps (freenet/river#703). One exchange then gives the same state in
//! both directions.

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::time::{Duration, SystemTime};

fn sk(b: u64) -> SigningKey {
    let mut x = [0u8; 32];
    x[..8].copy_from_slice(&b.to_le_bytes());
    x[31] = 9;
    SigningKey::from_bytes(&x)
}
fn id(s: &SigningKey) -> MemberId {
    s.verifying_key().into()
}
fn at(t: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + t)
}
fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = Vec::new();
    ciborium::ser::into_writer(s, &mut v).unwrap();
    v
}

struct Room {
    o: SigningKey,
    owner: MemberId,
    p: ChatRoomParametersV1,
}

impl Room {
    fn new() -> Self {
        let o = sk(1);
        Room {
            owner: id(&o),
            p: ChatRoomParametersV1 {
                owner: o.verifying_key(),
            },
            o,
        }
    }
    fn cfg(&self, version: u32, max_user_bans: usize) -> AuthorizedConfigurationV1 {
        AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: self.owner,
                configuration_version: version,
                max_user_bans,
                max_members: 300,
                ..Default::default()
            },
            &self.o,
        )
    }
    fn rec(&self, who: &SigningKey, inviter: &SigningKey) -> AuthorizedMember {
        AuthorizedMember::new(
            Member {
                owner_member_id: self.owner,
                invited_by: id(inviter),
                member_vk: who.verifying_key(),
            },
            inviter,
        )
    }
    fn msg(&self, who: &SigningKey, t: u64) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: self.owner,
                author: id(who),
                time: at(t),
                content: RoomMessageBody::public(format!("m{t}")),
            },
            who,
        )
    }
    fn ban(&self, by: &SigningKey, target: MemberId, t: u64) -> AuthorizedUserBan {
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: self.owner,
                banned_at: at(t),
                banned_user: target,
            },
            id(by),
            by,
        )
    }
    /// The state with its records normalized: a fixpoint that verifies.
    fn settle(&self, mut s: ChatRoomStateV1) -> ChatRoomStateV1 {
        s.recent_messages.rebuild_actions_state();
        let s0 = s.clone();
        s.merge_from(&self.p, &s0).unwrap();
        s.verify(&s, &self.p).expect("the state verifies");
        s
    }
    fn assert_one_exchange_agrees(&self, a: &ChatRoomStateV1, b: &ChatRoomStateV1) {
        let mut ab = a.clone();
        ab.merge_from(&self.p, b).expect("merge(A, B)");
        let mut ba = b.clone();
        ba.merge_from(&self.p, a).expect("merge(B, A)");
        ab.verify(&ab, &self.p).unwrap();
        assert!(ser(&ab) == ser(&ba), "one exchange differs by direction");
    }
}

/// The receiver holds the owner's ban on issuer X; the sender does not, and
/// holds X's newest bans beside W's older ones. In the merged room X's bans
/// take no effect, so the trim must not keep them in place of W's.
#[test]
fn a_ban_the_receiver_makes_dead_does_not_displace_a_live_one() {
    let r = Room::new();
    let (x, w) = (sk(2), sk(3));
    let target = |i: u64| id(&sk(100_000 + i));
    let mut bans: Vec<AuthorizedUserBan> =
        (0..120).map(|i| r.ban(&x, target(i), 10_000 + i)).collect();
    bans.extend((0..60).map(|i| r.ban(&w, target(1_000 + i), 100 + i)));
    let sender = r.settle(ChatRoomStateV1 {
        configuration: r.cfg(1, 300),
        members: MembersV1 {
            members: vec![r.rec(&x, &r.o), r.rec(&w, &r.o)],
        },
        bans: BansV1(bans),
        recent_messages: MessagesV1 {
            messages: vec![r.msg(&w, 1), r.msg(&x, 2)],
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(sender.bans.0.len(), 180, "premise");
    let receiver = r.settle(ChatRoomStateV1 {
        configuration: r.cfg(2, 10),
        bans: BansV1(vec![r.ban(&r.o, id(&x), 50)]),
        ..Default::default()
    });
    r.assert_one_exchange_agrees(&receiver, &sender);
}

/// The receiver holds the deputy grant that makes the sender's newest bans
/// (by M6 on M0's invitees) take effect; in the sender they take none.
#[test]
fn a_ban_the_receiver_makes_live_is_not_trimmed_as_dead() {
    let r = Room::new();
    let (m0, m6) = (sk(2), sk(3));
    let invitees: Vec<SigningKey> = (0..10).map(|i| sk(700 + i)).collect();
    let mut members = vec![r.rec(&m0, &r.o), r.rec(&m6, &r.o)];
    members.extend(invitees.iter().map(|t| r.rec(t, &m0)));
    let mut msgs = vec![r.msg(&m0, 1), r.msg(&m6, 2)];
    msgs.extend(
        invitees
            .iter()
            .enumerate()
            .map(|(i, t)| r.msg(t, 3 + i as u64)),
    );
    let mut bans = Vec::new();
    for k in 0..100 {
        bans.push(r.ban(&r.o, id(&sk(9_000 + k)), 1_000 + k));
    }
    for k in 0..10 {
        bans.push(r.ban(&m0, id(&sk(8_000 + k)), 1_500 + k));
    }
    for (i, t) in invitees.iter().enumerate() {
        bans.push(r.ban(&m6, id(t), 2_000 + i as u64));
    }
    let sender = r.settle(ChatRoomStateV1 {
        configuration: r.cfg(2, 150),
        members: MembersV1 { members },
        bans: BansV1(bans),
        recent_messages: MessagesV1 {
            messages: msgs,
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(sender.bans.0.len(), 120, "premise");
    let mut grant = MemberInfo::new_public(id(&m0), 1, "m0".into());
    grant.deputies = vec![id(&m6)];
    let receiver = r.settle(ChatRoomStateV1 {
        configuration: r.cfg(3, 10),
        members: MembersV1 {
            members: vec![r.rec(&m0, &r.o), r.rec(&m6, &r.o)],
        },
        member_info: MemberInfoV1 {
            member_info: vec![AuthorizedMemberInfo::new_with_member_key(grant, &m0)],
        },
        recent_messages: MessagesV1 {
            messages: vec![r.msg(&m0, 1), r.msg(&m6, 2)],
            ..Default::default()
        },
        ..Default::default()
    });
    r.assert_one_exchange_agrees(&receiver, &sender);
}

/// Only the receiver holds the records (as ban evidence) of the targets of
/// the sender's newest bans, which are then judged by their issuer's
/// authority, and take no effect in the merged room.
#[test]
fn a_ban_judged_by_evidence_only_the_receiver_holds_is_trimmed_as_the_merged_room_judges_it() {
    let r = Room::new();
    let (z, m, w) = (sk(2), sk(3), sk(4));
    let targets: Vec<SigningKey> = (0..60).map(|i| sk(500 + i)).collect();
    // The receiver: Z invited every target and banned each of them, so
    // their records are kept as ban evidence.
    let mut rmembers = vec![r.rec(&z, &r.o), r.rec(&m, &r.o), r.rec(&w, &r.o)];
    rmembers.extend(targets.iter().map(|t| r.rec(t, &z)));
    let receiver = r.settle(ChatRoomStateV1 {
        configuration: r.cfg(2, 70),
        members: MembersV1 { members: rmembers },
        bans: BansV1(
            targets
                .iter()
                .enumerate()
                .map(|(i, t)| r.ban(&z, id(t), i as u64))
                .collect(),
        ),
        recent_messages: MessagesV1 {
            messages: vec![r.msg(&z, 1), r.msg(&m, 2), r.msg(&w, 3)],
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(receiver.bans.0.len(), 60, "premise");
    assert!(!receiver.ban_evidence.is_empty(), "premise: evidence held");
    // The sender: no record of any target; M's bans on them (newest) count
    // live there, beside W's older bans on ids known nowhere.
    let mut bans: Vec<AuthorizedUserBan> = targets
        .iter()
        .enumerate()
        .map(|(i, t)| r.ban(&m, id(t), 1_000 + i as u64))
        .collect();
    bans.extend((0..60).map(|i| r.ban(&w, id(&sk(9_000 + i)), 100 + i)));
    let sender = r.settle(ChatRoomStateV1 {
        configuration: r.cfg(1, 300),
        members: MembersV1 {
            members: vec![r.rec(&z, &r.o), r.rec(&m, &r.o), r.rec(&w, &r.o)],
        },
        bans: BansV1(bans),
        recent_messages: MessagesV1 {
            messages: vec![r.msg(&z, 1), r.msg(&m, 2), r.msg(&w, 3)],
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(sender.bans.0.len(), 120, "premise");
    r.assert_one_exchange_agrees(&receiver, &sender);
}

/// A held ban attributed to a known member, whose key does not verify it
/// (only a peer that did not know the member could have stored it), counts
/// for nothing at the cap: it is dropped before the cap counts, as the same
/// ban arriving at a peer that knows the member is skipped. So it cannot
/// take a cap slot and evict a real ban one direction only (freenet/river#703
/// review Q2).
#[test]
fn a_held_ban_its_issuers_key_does_not_verify_takes_no_cap_slot() {
    let r = Room::new();
    let (m0, m1, m2, m4, m5) = (sk(2), sk(3), sk(4), sk(5), sk(6));
    let cfg = r.cfg(2, 3);
    // A does not know M4, so it holds F, attributed to M4 but signed by M5.
    let d = r.ban(&m2, id(&m0), 15);
    let mut f = r.ban(&m5, id(&m5), 33);
    f.banned_by = id(&m4);
    let a = ChatRoomStateV1 {
        configuration: cfg.clone(),
        members: MembersV1 {
            members: vec![r.rec(&m0, &r.o), r.rec(&m2, &m0)],
        },
        bans: BansV1(vec![d.clone(), f]),
        recent_messages: MessagesV1 {
            messages: vec![r.msg(&m0, 1), r.msg(&m2, 2)],
            ..Default::default()
        },
        ..Default::default()
    };
    a.verify(&a, &r.p)
        .expect("premise: F's issuer is unknown in A");
    let b = r.settle(ChatRoomStateV1 {
        configuration: cfg,
        members: MembersV1 {
            members: vec![
                r.rec(&m0, &r.o),
                r.rec(&m1, &r.o),
                r.rec(&m2, &m0),
                r.rec(&m4, &m1),
                r.rec(&m5, &m4),
            ],
        },
        bans: BansV1(vec![r.ban(&m4, id(&m5), 13), r.ban(&m0, id(&sk(4242)), 18)]),
        recent_messages: MessagesV1 {
            messages: vec![
                r.msg(&m0, 1),
                r.msg(&m2, 2),
                r.msg(&m1, 3),
                r.msg(&m4, 4),
                r.msg(&m5, 5),
            ],
            ..Default::default()
        },
        ..Default::default()
    });
    r.assert_one_exchange_agrees(&a, &b);
}
