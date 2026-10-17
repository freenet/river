//! Ban pools larger than one delta may carry, through the public whole-state
//! merge: every incoming ban is applied and the merged room's ban cap keeps
//! what it keeps, so one exchange gives the same ban set in both directions
//! (freenet/river#703).
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
    x[31] = 3;
    SigningKey::from_bytes(&x)
}
fn at(t: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + t)
}
fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = Vec::new();
    ciborium::ser::into_writer(s, &mut v).unwrap();
    v
}

/// owner bans and a present member's bans on ids held nowhere (live, in the
/// other pool), each pool larger than half the per-delta bound; the receiver's
/// newer configuration lowers `max_user_bans` to exactly the bound. Both merge
/// directions must leave each pool its newest `bound / 2` in ONE exchange.
fn run(owner_n: usize, other_n: usize, receiver_cap: usize, owner_newer: bool) {
    let o = sk(1);
    let owner = MemberId::from(&o.verifying_key());
    let params = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let a = sk(2);
    let a_rec = AuthorizedMember::new(
        Member {
            owner_member_id: owner,
            invited_by: owner,
            member_vk: a.verifying_key(),
        },
        &o,
    );
    let a_id = a_rec.member.id();
    let target = |i: u64| MemberId::from(&sk(10_000 + i).verifying_key());
    let ban = |by: MemberId, by_sk: &SigningKey, t: u64, i: u64| {
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner,
                banned_at: at(t),
                banned_user: target(i),
            },
            by,
            by_sk,
        )
    };
    let mut bans: Vec<AuthorizedUserBan> = (0..owner_n as u64)
        .map(|i| ban(owner, &o, if owner_newer { 1_000_000 + i } else { i }, i))
        .collect();
    bans.extend((0..other_n as u64).map(|i| ban(a_id, &a, 1_000 + i, 5_000 + i)));
    let cfg = |version: u32, cap: usize| {
        AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner,
                configuration_version: version,
                max_user_bans: cap,
                ..Default::default()
            },
            &o,
        )
    };
    let mut sender = ChatRoomStateV1 {
        configuration: cfg(1, owner_n + other_n),
        members: MembersV1 {
            members: vec![a_rec],
        },
        bans: BansV1(bans),
        recent_messages: MessagesV1 {
            messages: vec![AuthorizedMessageV1::new(
                MessageV1 {
                    room_owner: owner,
                    author: a_id,
                    time: at(1),
                    content: RoomMessageBody::public("hi".into()),
                },
                &a,
            )],
            ..Default::default()
        },
        ..Default::default()
    };
    sender.recent_messages.rebuild_actions_state();
    sender.post_apply_cleanup(&params).unwrap();
    sender
        .verify(&sender, &params)
        .expect("the sender verifies");
    assert_eq!(
        sender.bans.0.len(),
        owner_n + other_n,
        "premise: nothing was capped"
    );
    let receiver = ChatRoomStateV1 {
        configuration: cfg(2, receiver_cap),
        ..Default::default()
    };

    let mut ab = receiver.clone();
    ab.merge(&receiver, &params, &sender)
        .expect("merge(R, S) must not fail");
    let mut ba = sender.clone();
    ba.merge(&sender, &params, &receiver)
        .expect("merge(S, R) must not fail");
    assert_eq!(ba.bans.0.len(), receiver_cap.min(owner_n + other_n));
    if ser(&ab) != ser(&ba) {
        let mut ab2 = ab.clone();
        ab2.merge(&ab, &params, &ba).unwrap();
        let mut ba2 = ba.clone();
        ba2.merge(&ba, &params, &ab).unwrap();
        let o = |s: &ChatRoomStateV1| s.bans.0.iter().filter(|b| b.banned_by == owner).count();
        panic!("owner_newer={owner_newer} owner_n={owner_n} other_n={other_n} cap={receiver_cap}: one exchange differs: R<-S has {} bans ({} owner), S<-R has {} ({} owner); after a 2nd exchange equal={}", ab.bans.0.len(), o(&ab), ba.bans.0.len(), o(&ba), ser(&ab2)==ser(&ba2));
    }
    ab.verify(&ab, &params).unwrap();
}

#[test]
fn each_pool_keeps_its_newest_half_at_an_even_cap() {
    for newer in [false, true] {
        run(60, 140, 100, newer);
        run(100, 100, 100, newer);
        run(30, 200, 100, newer);
    }
}

/// An odd cap above the floor: the spare slot goes where the cap puts it.
#[test]
fn each_pool_keeps_its_share_at_an_odd_cap() {
    let mut failures = vec![];
    for newer in [false, true] {
        for (o, x) in [(60usize, 160usize), (130, 90), (51, 51), (200, 60)] {
            if let Err(e) = std::panic::catch_unwind(|| run(o, x, 101, newer)) {
                failures.push(format!("{:?}", e.downcast_ref::<String>()));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Bans whose liveness depends on a deputy grant: D (a deputy of A, not an
/// ancestor of the targets) bans A's invitees. All 120 bans are live, so the
/// merged cap leaves each pool its newest half in both directions; judging
/// without the grant would call D's bans dead and evict them first.
#[test]
fn deputy_bans_are_judged_live_in_both_directions() {
    let o = sk(1);
    let owner = MemberId::from(&o.verifying_key());
    let params = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let (a, d) = (sk(2), sk(3));
    let rec = |inviter: &SigningKey, inviter_id: MemberId, who: &SigningKey| {
        AuthorizedMember::new(
            Member {
                owner_member_id: owner,
                invited_by: inviter_id,
                member_vk: who.verifying_key(),
            },
            inviter,
        )
    };
    let a_rec = rec(&o, owner, &a);
    let d_rec = rec(&o, owner, &d);
    let a_id = a_rec.member.id();
    let d_id = d_rec.member.id();
    let targets: Vec<SigningKey> = (0..60u64).map(|i| sk(100 + i)).collect();
    let mut members = vec![a_rec, d_rec];
    members.extend(targets.iter().map(|t| rec(&a, a_id, t)));
    let msg = |s: &SigningKey, t: u64| {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner,
                author: MemberId::from(&s.verifying_key()),
                time: at(t),
                content: RoomMessageBody::public(format!("m{t}")),
            },
            s,
        )
    };
    let mut msgs = vec![msg(&a, 1), msg(&d, 2)];
    msgs.extend(
        targets
            .iter()
            .enumerate()
            .map(|(i, t)| msg(t, 10 + i as u64)),
    );
    let mut grant = MemberInfo::new_public(a_id, 1, "a".into());
    grant.deputies = vec![d_id];
    let member_info = MemberInfoV1 {
        member_info: vec![AuthorizedMemberInfo::new_with_member_key(grant, &a)],
    };
    let ban = |by: MemberId, by_sk: &SigningKey, target: MemberId, t: u64| {
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: owner,
                banned_at: at(t),
                banned_user: target,
            },
            by,
            by_sk,
        )
    };
    let mut bans: Vec<AuthorizedUserBan> = (0..60u64)
        .map(|i| {
            ban(
                owner,
                &o,
                MemberId::from(&sk(10_000 + i).verifying_key()),
                1_000 + i,
            )
        })
        .collect();
    bans.extend(targets.iter().enumerate().map(|(i, t)| {
        ban(
            d_id,
            &d,
            MemberId::from(&t.verifying_key()),
            2_000 + i as u64,
        )
    }));
    let cfg = |version: u32, cap: usize| {
        AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner,
                configuration_version: version,
                max_user_bans: cap,
                max_members: 300,
                ..Default::default()
            },
            &o,
        )
    };
    let mut sender = ChatRoomStateV1 {
        configuration: cfg(1, 120),
        members: MembersV1 { members },
        member_info,
        bans: BansV1(bans),
        recent_messages: MessagesV1 {
            messages: msgs,
            ..Default::default()
        },
        ..Default::default()
    };
    sender.recent_messages.rebuild_actions_state();
    sender.post_apply_cleanup(&params).unwrap();
    sender
        .verify(&sender, &params)
        .expect("the sender verifies");
    assert_eq!(sender.bans.0.len(), 120, "premise: nothing was capped");
    let receiver = ChatRoomStateV1 {
        configuration: cfg(2, 100),
        ..Default::default()
    };
    let mut ab = receiver.clone();
    ab.merge(&receiver, &params, &sender).expect("merge(R, S)");
    let mut ba = sender.clone();
    ba.merge(&sender, &params, &receiver).expect("merge(S, R)");
    let by_d = |s: &ChatRoomStateV1| s.bans.0.iter().filter(|b| b.banned_by == d_id).count();
    assert_eq!(
        by_d(&ba),
        50,
        "premise: S<-R keeps each pool its newest half"
    );
    assert_eq!(by_d(&ab), by_d(&ba), "R<-S keeps the same split");
    assert_eq!(ser(&ab), ser(&ba));
}
