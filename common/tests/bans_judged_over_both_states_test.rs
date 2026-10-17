//! A whole-state merge applies every incoming ban and the merged room's ban
//! cap judges them over the bans and records BOTH states hold, so a ban one
//! side's records make dead is not kept in place of one the merged room keeps
//! (freenet/river#703). One exchange gives the same state in both directions.

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
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
        let (ab, ba) = exchange(&self.p, a, b);
        ab.verify(&ab, &self.p).unwrap();
        assert!(ser(&ab) == ser(&ba), "one exchange differs by direction");
    }
}

/// One exchange between `a` and `b`, as two peers hold it afterwards. A side
/// holding more than one delta may carry in the other's room, under a
/// configuration that does not outrank the other's, is refused first; it
/// takes the other's configuration from the other direction's merge, and
/// the refused side then merges that (freenet/river#703).
fn exchange(
    params: &ChatRoomParametersV1,
    a: &ChatRoomStateV1,
    b: &ChatRoomStateV1,
) -> (ChatRoomStateV1, ChatRoomStateV1) {
    let merged = |x: &ChatRoomStateV1, y: &ChatRoomStateV1| {
        let mut out = x.clone();
        out.merge_from(params, y).map(|()| out)
    };
    match (merged(a, b), merged(b, a)) {
        (Ok(ab), Ok(ba)) => (ab, ba),
        (Err(e), Ok(ba)) => {
            assert!(e.contains("does not outrank"), "{e}");
            (
                merged(a, &ba).expect("the refused side merges the other's result"),
                ba,
            )
        }
        (Ok(ab), Err(e)) => {
            assert!(e.contains("does not outrank"), "{e}");
            let ba = merged(b, &ab).expect("the refused side merges the other's result");
            (ab, ba)
        }
        (Err(e1), Err(e2)) => panic!("both directions refused: {e1} / {e2}"),
    }
}

/// The receiver holds the owner's ban on issuer X; the sender does not, and
/// holds X's newest bans beside W's older ones. In the merged room X's bans
/// take no effect, so they are not kept in place of W's.
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
fn a_ban_the_receiver_makes_live_is_kept_as_live() {
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
/// authority, and take no effect in the merged room in either direction.
#[test]
fn a_ban_judged_by_evidence_only_the_receiver_holds_is_judged_as_the_merged_room_judges_it() {
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

/// A ban whose issuer is known nowhere is swept by the same cleanup that
/// receives it, so it takes no cap slot: a room at its ban cap that receives
/// one keeps every ban it held, in one apply and in both merge directions.
#[test]
fn a_ban_whose_issuer_is_known_nowhere_takes_no_cap_slot() {
    let r = Room::new();
    let (m, n, unknown) = (sk(2), sk(3), sk(4));
    let members = MembersV1 {
        members: vec![r.rec(&m, &r.o), r.rec(&n, &m)],
    };
    let msgs = MessagesV1 {
        messages: vec![r.msg(&m, 1), r.msg(&n, 2)],
        ..Default::default()
    };
    // N bans its own inviter (takes no effect) and the owner bans an id held
    // nowhere: two stored bans, at the cap.
    let held = r.settle(ChatRoomStateV1 {
        configuration: r.cfg(1, 2),
        members: members.clone(),
        recent_messages: msgs.clone(),
        bans: BansV1(vec![r.ban(&n, id(&m), 5), r.ban(&r.o, id(&sk(90)), 6)]),
        ..Default::default()
    });
    assert_eq!(held.bans.0.len(), 2, "premise");
    let arriving = r.ban(&unknown, id(&sk(91)), 20);

    let mut applied = held.clone();
    applied
        .apply_delta(
            &held,
            &r.p,
            &Some(ChatRoomStateV1Delta {
                bans: Some(vec![arriving.clone()]),
                ..Default::default()
            }),
        )
        .expect("applies");
    assert_eq!(applied.bans, held.bans, "nothing the room held is evicted");

    // A peer that holds the same room plus that ban, still on an older
    // configuration with room for it.
    let mut other = held.clone();
    other.configuration = r.cfg(0, 3);
    other.bans.0.push(arriving);
    r.assert_one_exchange_agrees(&held, &other);
}

/// Over the cap, the bans no known key verifies are dropped before the cap
/// counts; an issuer known only through ban evidence is known, so its genuine
/// ban is kept while one a present member's key does not verify is dropped.
#[test]
fn an_evidence_issuers_ban_is_known_when_unverified_bans_are_dropped() {
    use river_core::room_state::ban_evidence::BanEvidenceV1;
    let r = Room::new();
    let (m, e, other) = (sk(2), sk(3), sk(4));
    let mut msgs = MessagesV1 {
        messages: vec![r.msg(&m, 1)],
        ..Default::default()
    };
    msgs.rebuild_actions_state();
    let genuine = r.ban(&e, id(&sk(80)), 5);
    let held = r.settle(ChatRoomStateV1 {
        configuration: r.cfg(1, 2),
        members: MembersV1 {
            members: vec![r.rec(&m, &r.o)],
        },
        ban_evidence: BanEvidenceV1 {
            members: vec![r.rec(&e, &r.o)],
        },
        recent_messages: msgs,
        bans: BansV1(vec![genuine.clone(), r.ban(&r.o, id(&sk(81)), 6)]),
        ..Default::default()
    });
    assert_eq!(held.bans.0.len(), 2, "premise");
    assert_eq!(
        held.ban_evidence.members.len(),
        1,
        "premise: the issuer is evidence"
    );
    // Claims to be the present member M's, signed by another key.
    let unverified = AuthorizedUserBan {
        banned_by: id(&m),
        ..r.ban(&other, id(&sk(82)), 7)
    };
    let mut applied = held.clone();
    applied.bans.0.push(unverified);
    applied.post_apply_cleanup(&r.p).unwrap();
    assert!(
        applied.bans.0.contains(&genuine),
        "the evidence issuer's ban is kept"
    );
    assert_eq!(applied.bans.0.len(), 2);
}
