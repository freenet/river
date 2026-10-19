//! The `max_user_bans` cap keeps bans in a fixed order (owner bans first,
//! then by how high in the invite tree the ban's authority sits), judging
//! each ban over the bans already kept. These pin what that order must not
//! give up: a member a ban removes cannot keep itself in the room by adding
//! bans of its own, and a ban by a present member deep in the invite tree
//! is never dropped for its issuer's depth.
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
    x[31] = 17;
    SigningKey::from_bytes(&x)
}
fn id(s: &SigningKey) -> MemberId {
    s.verifying_key().into()
}
fn at(t: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + t)
}
fn target(i: u64) -> MemberId {
    id(&sk(100_000 + i))
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
    fn cfg(&self, f: impl FnOnce(&mut Configuration)) -> AuthorizedConfigurationV1 {
        let mut c = Configuration {
            owner_member_id: self.owner,
            configuration_version: 1,
            ..Default::default()
        };
        f(&mut c);
        AuthorizedConfigurationV1::new(c, &self.o)
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
    fn messages(&self, msgs: Vec<AuthorizedMessageV1>) -> MessagesV1 {
        let mut m = MessagesV1 {
            messages: msgs,
            ..Default::default()
        };
        m.rebuild_actions_state();
        m
    }
    fn settled(&self, s: ChatRoomStateV1) -> ChatRoomStateV1 {
        let mut m = s.clone();
        m.merge_from(&self.p, &s).expect("self-merge");
        m.verify(&m, &self.p).expect("the state verifies");
        m
    }
}

/// A member banned by its inviter adds, in the same delta, more bans of its
/// own (newer, against ids known nowhere) than the cap holds. Its own bans
/// take no effect once its inviter's ban is kept, so it is removed.
#[test]
fn a_member_removed_by_its_inviters_ban_cannot_keep_itself_with_bans_of_its_own() {
    let r = Room::new();
    let (a, x) = (sk(2), sk(3));
    let base = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(|c| c.max_user_bans = 10),
        members: MembersV1 {
            members: vec![r.rec(&a, &r.o), r.rec(&x, &a)],
        },
        recent_messages: r.messages(vec![r.msg(&a, 1), r.msg(&x, 2)]),
        ..Default::default()
    });
    let mut bans = vec![r.ban(&a, id(&x), 10)];
    bans.extend((0..20).map(|i| r.ban(&x, target(i), 1_000 + i)));
    let delta = ChatRoomStateV1Delta {
        bans: Some(bans),
        ..Default::default()
    };
    let mut s = base.clone();
    s.apply_delta(&base, &r.p, &Some(delta)).expect("apply");
    s.verify(&s, &r.p).expect("verifies");
    assert!(
        !s.members.members.iter().any(|m| m.member.id() == id(&x)),
        "X is removed by A's ban"
    );
    assert!(
        s.bans.0.iter().any(|b| b.banned_by == id(&a)),
        "A's ban is kept"
    );
}

/// The same with a deputy: a deputy of X's inviter, deeper in the invite
/// tree than X, bans X, and X adds more bans of its own than the cap holds.
#[test]
fn a_member_removed_by_a_deputys_ban_cannot_keep_itself_with_bans_of_its_own() {
    let r = Room::new();
    let (a, x, p, q, d) = (sk(2), sk(3), sk(4), sk(5), sk(6));
    let mut deputy_grant = MemberInfo::new_public(id(&a), 1, "a".to_string());
    deputy_grant.deputies = vec![id(&d)];
    let mut bans = vec![r.ban(&d, id(&x), 10)];
    bans.extend((0..20).map(|i| r.ban(&x, target(i), 1_000 + i)));
    let mut s = ChatRoomStateV1 {
        configuration: r.cfg(|c| {
            c.max_members = 100;
            c.max_user_bans = 10;
            c.max_recent_messages = 1_000;
        }),
        members: MembersV1 {
            members: vec![
                r.rec(&a, &r.o),
                r.rec(&x, &a),
                r.rec(&p, &r.o),
                r.rec(&q, &p),
                r.rec(&d, &q),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![AuthorizedMemberInfo::new_with_member_key(deputy_grant, &a)],
        },
        recent_messages: r.messages(vec![
            r.msg(&a, 1),
            r.msg(&x, 2),
            r.msg(&p, 3),
            r.msg(&q, 4),
            r.msg(&d, 5),
        ]),
        bans: BansV1(bans),
        ..Default::default()
    };
    s.post_apply_cleanup(&r.p).unwrap();
    assert!(
        !s.members.members.iter().any(|m| m.member.id() == id(&x)),
        "X is removed by D's deputy ban"
    );
    assert!(
        s.bans.0.iter().any(|b| b.banned_by == id(&d)),
        "D's ban is kept"
    );
}

/// A present member six levels down the invite tree (deeper than the ban
/// evidence depth cap) issues more bans than the cap holds, against ids
/// known nowhere. Its bans are judged like any other member's: the cap keeps
/// `max_user_bans` of them, and the result is a fixpoint.
#[test]
fn a_present_issuer_deeper_than_the_evidence_cap_keeps_its_bans() {
    let r = Room::new();
    let chain: Vec<SigningKey> = (0..6).map(|i| sk(10 + i)).collect();
    let mut members = Vec::new();
    let mut inviter = &r.o;
    for m in &chain {
        members.push(r.rec(m, inviter));
        inviter = m;
    }
    let deep = chain.last().unwrap();
    let base = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(|c| c.max_user_bans = 3),
        members: MembersV1 { members },
        recent_messages: r.messages(vec![r.msg(deep, 1)]),
        ..Default::default()
    });
    assert_eq!(base.members.members.len(), 6, "premise: the chain is kept");

    // Under the cap: every ban stays.
    let mut under = base.clone();
    under
        .apply_delta(
            &base,
            &r.p,
            &Some(ChatRoomStateV1Delta {
                bans: Some(vec![r.ban(deep, target(0), 100)]),
                ..Default::default()
            }),
        )
        .expect("apply");
    assert_eq!(under.bans.0.len(), 1, "a deep issuer's ban stays");

    // Over the cap: the cap keeps three of the deep issuer's bans.
    let mut over = base.clone();
    over.apply_delta(
        &base,
        &r.p,
        &Some(ChatRoomStateV1Delta {
            bans: Some((0..5).map(|i| r.ban(deep, target(i), 100 + i)).collect()),
            ..Default::default()
        }),
    )
    .expect("apply");
    over.verify(&over, &r.p).expect("verifies");
    assert_eq!(over.bans.0.len(), 3, "the cap keeps a deep issuer's bans");
    assert!(over.bans.0.iter().all(|b| b.banned_by == id(deep)));
    let mut again = over.clone();
    again.merge_from(&r.p, &over).expect("self-merge");
    assert!(ser(&again) == ser(&over), "a fixpoint");
}
