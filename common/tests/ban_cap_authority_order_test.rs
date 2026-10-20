//! The `max_user_bans` cap keeps bans in a fixed order (owner bans first,
//! then by how high in the invite tree the ban's authority sits), judging
//! each ban over the bans already kept. These pin what that order must not
//! give up: a member a ban removes cannot keep itself in the room by adding
//! bans of its own; and which bans count is decided by one depth rule for
//! present and absent issuers alike.
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

/// A ban counts only when its issuer's invite chain reaches the owner
/// within the ban evidence depth cap (4 here), whether or not the issuer is
/// present: issuers are judged by one depth rule, so a ban's standing does
/// not change when the pass that removes its issuer moves the issuer's
/// record out of `members`. A ban by a member four levels down is kept; one
/// by a member six levels down is removed. The result is a fixpoint.
#[test]
fn a_ban_counts_only_when_its_issuer_is_within_the_evidence_depth_cap() {
    let r = Room::new();
    let chain: Vec<SigningKey> = (0..6).map(|i| sk(10 + i)).collect();
    let mut members = Vec::new();
    let mut inviter = &r.o;
    for m in &chain {
        members.push(r.rec(m, inviter));
        inviter = m;
    }
    let (at_cap, deep) = (&chain[3], &chain[5]);
    let base = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(|c| c.max_user_bans = 3),
        members: MembersV1 { members },
        recent_messages: r.messages(vec![r.msg(deep, 1)]),
        ..Default::default()
    });
    assert_eq!(base.members.members.len(), 6, "premise: the chain is kept");
    let kept = r.ban(at_cap, target(0), 100);
    let swept = r.ban(deep, target(1), 101);
    let mut after = base.clone();
    after
        .apply_delta(
            &base,
            &r.p,
            &Some(ChatRoomStateV1Delta {
                bans: Some(vec![kept.clone(), swept.clone()]),
                ..Default::default()
            }),
        )
        .expect("apply");
    after.verify(&after, &r.p).expect("verifies");
    assert!(after.bans.0.contains(&kept), "an issuer at depth 4 counts");
    assert!(
        !after.bans.0.contains(&swept),
        "an issuer at depth 6 does not"
    );
    let mut again = after.clone();
    again.merge_from(&r.p, &after).expect("self-merge");
    assert!(ser(&again) == ser(&after), "a fixpoint");
}

fn apply(r: &Room, base: &ChatRoomStateV1, bans: Vec<AuthorizedUserBan>) -> ChatRoomStateV1 {
    let mut s = base.clone();
    s.apply_delta(
        base,
        &r.p,
        &Some(ChatRoomStateV1Delta {
            bans: Some(bans),
            ..Default::default()
        }),
    )
    .expect("apply");
    s.verify(&s, &r.p).expect("verifies");
    let mut again = s.clone();
    again.merge_from(&r.p, &s).expect("self-merge");
    assert!(ser(&again) == ser(&s), "a fixpoint");
    s
}
fn owner_ban(r: &Room, target: MemberId, t: u64) -> AuthorizedUserBan {
    r.ban(&r.o, target, t)
}
fn has(s: &ChatRoomStateV1, b: &AuthorizedUserBan) -> bool {
    s.bans.0.contains(b)
}
fn is_member(s: &ChatRoomStateV1, k: &SigningKey) -> bool {
    s.members.members.iter().any(|m| m.member.id() == id(k))
}

/// A removed member's newer bans on ids known nowhere take no effect, so
/// they do not take a slot from a live ban lower in the order.
#[test]
fn a_removed_members_bans_do_not_displace_a_live_deeper_ban() {
    let r = Room::new();
    let (a, x, p, q, d) = (sk(2), sk(3), sk(4), sk(5), sk(6));
    let base = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(|c| {
            c.max_user_bans = 10;
            c.max_members = 100;
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
        recent_messages: r.messages(vec![
            r.msg(&a, 1),
            r.msg(&x, 2),
            r.msg(&p, 3),
            r.msg(&q, 4),
            r.msg(&d, 5),
        ]),
        ..Default::default()
    });
    let d_ban = r.ban(&d, target(500), 5);
    let mut bans = vec![r.ban(&a, id(&x), 10), d_ban.clone()];
    bans.extend((0..20).map(|i| r.ban(&x, target(i), 1_000 + i)));
    let s = apply(&r, &base, bans);
    assert!(!is_member(&s, &x), "X removed");
    assert!(
        has(&s, &d_ban),
        "D's live ban is kept ahead of a removed member's flood"
    );
}

/// While the cap binds, an owner ban that would remove the issuer of an
/// already kept ban is refused.
#[test]
fn an_owner_ban_on_a_kept_bans_issuer_is_refused_while_the_cap_binds() {
    let r = Room::new();
    let (d, x) = (sk(2), sk(3));
    let base = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(|c| c.max_user_bans = 3),
        members: MembersV1 {
            members: vec![r.rec(&d, &r.o), r.rec(&x, &d)],
        },
        recent_messages: r.messages(vec![r.msg(&d, 1), r.msg(&x, 2)]),
        ..Default::default()
    });
    let o1 = owner_ban(&r, target(1), 30);
    let o_d = owner_ban(&r, id(&d), 10);
    let o3 = owner_ban(&r, target(3), 5);
    let d_x = r.ban(&d, id(&x), 20);
    let s = apply(
        &r,
        &base,
        vec![o1.clone(), o_d.clone(), o3.clone(), d_x.clone()],
    );
    assert!(has(&s, &o1) && has(&s, &o3) && has(&s, &d_x));
    assert!(!has(&s, &o_d), "the owner ban on D is refused");
    assert!(is_member(&s, &d) && !is_member(&s, &x));
}

/// A second ban for one (issuer, target) pair never takes a pool's share
/// from another pair's live ban.
#[test]
fn a_duplicate_does_not_take_a_pool_share_slot() {
    let r = Room::new();
    let d = sk(2);
    let base = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(|c| c.max_user_bans = 4),
        members: MembersV1 {
            members: vec![r.rec(&d, &r.o)],
        },
        recent_messages: r.messages(vec![r.msg(&d, 1)]),
        ..Default::default()
    });
    let os: Vec<_> = [40, 30, 20]
        .iter()
        .enumerate()
        .map(|(i, t)| owner_ban(&r, target(10 + i as u64), *t))
        .collect();
    let dx_new = r.ban(&d, target(1), 60);
    let dx_old = r.ban(&d, target(1), 50);
    let dy = r.ban(&d, target(2), 10);
    let mut bans = os.clone();
    bans.extend([dx_new.clone(), dx_old.clone(), dy.clone()]);
    let s = apply(&r, &base, bans);
    assert!(has(&s, &dx_new) && has(&s, &dy), "both pairs keep a ban");
    assert!(!has(&s, &dx_old));
}

fn dm_msg(
    r: &Room,
    from: &SigningKey,
    to: &SigningKey,
    ts: u64,
) -> river_core::room_state::direct_messages::AuthorizedDirectMessage {
    river_core::room_state::direct_messages::sign_direct_message(
        from,
        id(from),
        id(to),
        &r.p.owner,
        1_700_000_000 + ts,
        vec![7; 8],
    )
    .unwrap()
}

/// The cap's answer follows every input, including when the last answer is
/// remembered: here only the deputy grant (member_info) differs between two
/// calls.
#[test]
fn the_cap_answer_follows_member_info() {
    let r = Room::new();
    let (d, x) = (sk(2), sk(3));
    let members = MembersV1 {
        members: vec![r.rec(&d, &r.o), r.rec(&x, &r.o)],
    };
    let mut grant = MemberInfo::new_public(r.owner, 1, "o".into());
    grant.deputies = vec![id(&d)];
    let with = MemberInfoV1 {
        member_info: vec![AuthorizedMemberInfo::new(grant, &r.o)],
    };
    let without = MemberInfoV1::default();
    let o1 = r.ban(&r.o, target(1), 30);
    let o2 = r.ban(&r.o, target(2), 20);
    let dx = r.ban(&d, id(&x), 10);
    let run = |info: &MemberInfoV1| {
        let mut b = vec![o1.clone(), o2.clone(), dx.clone()];
        BansV1::enforce_user_ban_cap(&mut b, 2, 4, &members, &Default::default(), info, &r.p);
        b
    };
    let a = run(&with);
    let b = run(&without);
    assert!(a.contains(&dx), "with the grant the deputy's ban is live");
    assert!(
        !b.contains(&dx) && b.contains(&o2),
        "without it the ban is inert"
    );
}

/// The same for the depth cap and max_bans inputs.
#[test]
fn the_cap_answer_follows_depth_cap_and_max() {
    let r = Room::new();
    let (a, b) = (sk(2), sk(3));
    let members = MembersV1 {
        members: vec![r.rec(&a, &r.o), r.rec(&b, &a)],
    };
    let bans: Vec<_> = (0..4).map(|i| r.ban(&b, target(i), 10 + i)).collect();
    let run = |max: usize, cap: usize| {
        let mut v = bans.clone();
        v.push(r.ban(&r.o, target(99), 1));
        BansV1::enforce_user_ban_cap(
            &mut v,
            max,
            cap,
            &members,
            &Default::default(),
            &Default::default(),
            &r.p,
        );
        v
    };
    assert_eq!(run(3, 4).len(), 3);
    assert_eq!(run(2, 4).len(), 2, "a lower max_bans is honoured");
    // Depth cap 1: B (depth 2) is not a known issuer, so its bans count for nothing.
    let shallow = run(3, 1);
    assert!(
        shallow.iter().all(|x| x.banned_by == r.owner),
        "{shallow:?}"
    );
}

/// A ban by a present member deeper than the evidence depth cap takes no
/// effect, so the DM apply does not sweep its target's DMs either
/// (freenet/river#671, #675).
#[test]
fn a_deep_issuers_ban_does_not_sweep_its_targets_dms() {
    let r = Room::new();
    let chain: Vec<SigningKey> = (0..6).map(|i| sk(10 + i)).collect(); // depths 1..=6
    let c = sk(50);
    let mut members = vec![r.rec(&c, &r.o)];
    let mut inviter = &r.o;
    for m in &chain {
        members.push(r.rec(m, inviter));
        inviter = m;
    }
    let (a, b) = (&chain[4], &chain[5]); // A depth 5, B depth 6 invited by A
    let mut base = ChatRoomStateV1 {
        configuration: r.cfg(|cf| cf.max_members = 100),
        members: MembersV1 { members },
        recent_messages: r.messages(vec![r.msg(b, 1), r.msg(&c, 2)]),
        ..Default::default()
    };
    base.direct_messages.messages.push(dm_msg(&r, &c, b, 5));
    let base = r.settled(base);
    assert_eq!(base.direct_messages.messages.len(), 1, "premise");
    let mut s = base.clone();
    s.apply_delta(
        &base,
        &r.p,
        &Some(ChatRoomStateV1Delta {
            bans: Some(vec![r.ban(a, id(b), 100)]),
            ..Default::default()
        }),
    )
    .expect("apply");
    assert!(
        s.members.members.iter().any(|m| m.member.id() == id(b)),
        "B stays"
    );
    assert_eq!(s.direct_messages.messages.len(), 1, "B's DM survives");
}

/// Step 6 sweeps DMs of removed members BEFORE the DM caps, so a banned
/// member's newer DM never takes the last global slot from a kept one.
#[test]
fn a_banned_members_dm_does_not_take_a_dm_cap_slot() {
    let r = Room::new();
    let (a, b, x) = (sk(2), sk(3), sk(4));
    let mut base = ChatRoomStateV1 {
        configuration: r.cfg(|cf| cf.max_direct_messages = Some(1)),
        members: MembersV1 {
            members: vec![r.rec(&a, &r.o), r.rec(&b, &r.o), r.rec(&x, &r.o)],
        },
        recent_messages: r.messages(vec![r.msg(&a, 1), r.msg(&b, 2), r.msg(&x, 3)]),
        ..Default::default()
    };
    base.direct_messages.messages.push(dm_msg(&r, &a, &b, 5));
    let base = r.settled(base);
    assert_eq!(base.direct_messages.messages.len(), 1, "premise");
    let newer = dm_msg(&r, &x, &b, 9);
    let mut s = base.clone();
    s.apply_delta(
        &base,
        &r.p,
        &Some(ChatRoomStateV1Delta {
            bans: Some(vec![r.ban(&r.o, id(&x), 100)]),
            direct_messages: Some(
                river_core::room_state::direct_messages::DirectMessagesDelta {
                    new_messages: vec![newer],
                    advanced_purges: vec![],
                },
            ),
            ..Default::default()
        }),
    )
    .expect("apply");
    assert_eq!(s.direct_messages.messages.len(), 1, "A->B survives the cap");
    assert_eq!(s.direct_messages.messages[0].message.sender, id(&a));
}

/// A DM of a member the member cap removes never takes a DM cap slot at
/// apply time (the field apply must not cap before cleanup).
#[test]
fn a_member_the_member_cap_removes_does_not_take_a_dm_cap_slot() {
    let r = Room::new();
    let (a, b, x) = (sk(2), sk(3), sk(4));
    let mut base = ChatRoomStateV1 {
        configuration: r.cfg(|cf| {
            cf.max_direct_messages = Some(1);
            cf.max_members = 2;
        }),
        members: MembersV1 {
            members: vec![r.rec(&a, &r.o), r.rec(&b, &r.o)],
        },
        recent_messages: r.messages(vec![r.msg(&a, 1), r.msg(&b, 2)]),
        ..Default::default()
    };
    base.direct_messages.messages.push(dm_msg(&r, &a, &b, 5));
    let base = r.settled(base);
    assert_eq!(base.direct_messages.messages.len(), 1, "premise");
    let mut s = base.clone();
    s.apply_delta(
        &base,
        &r.p,
        &Some(ChatRoomStateV1Delta {
            members: Some(river_core::room_state::member::MembersDelta::new(vec![
                r.rec(&x, &a)
            ])),
            recent_messages: Some(vec![r.msg(&x, 3)]),
            direct_messages: Some(
                river_core::room_state::direct_messages::DirectMessagesDelta {
                    new_messages: vec![dm_msg(&r, &x, &b, 9)],
                    advanced_purges: vec![],
                },
            ),
            ..Default::default()
        }),
    )
    .expect("apply");
    assert!(
        !s.members.members.iter().any(|m| m.member.id() == id(&x)),
        "X (deepest) is capped out"
    );
    assert_eq!(s.direct_messages.messages.len(), 1, "A->B survives");
    assert_eq!(s.direct_messages.messages[0].message.sender, id(&a));
}

/// At or under `max_user_bans` the cap changes nothing: a room holding bans
/// the over-cap order would drop or reorder (a ban that takes no effect, a
/// second ban for one pair, an older owner ban) keeps every one of them, byte
/// for byte, through an apply and a merge.
#[test]
fn under_the_cap_every_ban_is_kept() {
    let r = Room::new();
    let (a, b, d) = (sk(2), sk(3), sk(4));
    let base = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(|c| c.max_user_bans = 5),
        members: MembersV1 {
            members: vec![r.rec(&a, &r.o), r.rec(&b, &r.o), r.rec(&d, &a)],
        },
        recent_messages: r.messages(vec![r.msg(&a, 1), r.msg(&b, 2), r.msg(&d, 3)]),
        ..Default::default()
    });
    let bans = vec![
        r.ban(&b, id(&d), 10),       // B has no authority over D: no effect
        r.ban(&a, target(1), 20),    // a pair...
        r.ban(&a, target(1), 30),    // ...and its second ban
        owner_ban(&r, target(2), 5), // an older owner ban
        r.ban(&a, target(3), 40),
    ];
    let s = apply(&r, &base, bans.clone());
    assert_eq!(s.bans.0.len(), 5, "every ban is kept at the cap");
    for b in &bans {
        assert!(has(&s, b));
    }
    let mut merged = base.clone();
    merged.merge_from(&r.p, &s).expect("merge");
    assert!(ser(&merged) == ser(&s), "a merge keeps them too");
}

/// With a cap of one, an owner ban is kept ahead of a newer ban by a deputy
/// on a member the owner invited, which ranks at the owner's level.
#[test]
fn an_owner_ban_is_kept_ahead_of_a_newer_ban_at_the_owners_level() {
    let r = Room::new();
    let (d, x) = (sk(2), sk(3));
    let mut grant = MemberInfo::new_public(r.owner, 1, "o".into());
    grant.deputies = vec![id(&d)];
    let base = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(|c| c.max_user_bans = 1),
        members: MembersV1 {
            members: vec![r.rec(&d, &r.o), r.rec(&x, &r.o)],
        },
        member_info: MemberInfoV1 {
            member_info: vec![AuthorizedMemberInfo::new(grant, &r.o)],
        },
        recent_messages: r.messages(vec![r.msg(&d, 1), r.msg(&x, 2)]),
        ..Default::default()
    });
    let o = owner_ban(&r, target(1), 10);
    let dx = r.ban(&d, id(&x), 20);
    for bans in [vec![o.clone(), dx.clone()], vec![dx.clone(), o.clone()]] {
        let s = apply(&r, &base, bans);
        assert!(has(&s, &o) && !has(&s, &dx), "the owner ban is kept");
    }
}

/// Under `max_user_bans` the cap keeps even an owner ban it would refuse
/// while it binds: here the oldest of three owner bans removes D, the issuer
/// of a ban already in the room (over the cap, the newer owner bans take the
/// owner pool and D's ban the other, so this one would come last and be
/// refused). Under the cap it is kept, D goes, and cleanup sweeps D's ban.
#[test]
fn under_the_cap_an_owner_ban_on_a_bans_issuer_is_kept() {
    let r = Room::new();
    let (a, b, d) = (sk(2), sk(3), sk(4));
    let base = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(|c| c.max_user_bans = 5),
        members: MembersV1 {
            members: vec![r.rec(&a, &r.o), r.rec(&b, &r.o), r.rec(&d, &a)],
        },
        recent_messages: r.messages(vec![r.msg(&a, 1), r.msg(&b, 2), r.msg(&d, 3)]),
        ..Default::default()
    });
    let on_d = owner_ban(&r, id(&d), 5);
    let newer = [owner_ban(&r, target(2), 50), owner_ban(&r, target(4), 40)];
    let by_d = r.ban(&d, target(5), 30);
    let mut bans = newer.to_vec();
    bans.extend([on_d.clone(), by_d.clone()]);
    let s = apply(&r, &base, bans);
    assert!(has(&s, &on_d), "the owner ban on D is kept");
    assert!(newer.iter().all(|b| has(&s, b)));
    assert!(!is_member(&s, &d), "D is removed");
    assert!(!has(&s, &by_d), "D's ban is swept");
}
