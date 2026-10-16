//! Ban pools at the work bound, through the public whole-state merge: the
//! merge trims what it offers the way the ban cap keeps bans, so one
//! exchange gives the same ban set in both directions (freenet/river#703).
use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
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
/// other pool), each pool larger than half the work bound; the receiver's newer
/// configuration lowers `max_user_bans` to exactly the bound. The sender's trim
/// must leave each pool its newest `bound / 2`, or the two merge directions end
/// in different ban sets in ONE exchange.
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
fn a_bans_trim_leaves_each_pool_its_newest_half_at_an_even_bound() {
    for newer in [false, true] {
        run(60, 140, 100, newer);
        run(100, 100, 100, newer);
        run(30, 200, 100, newer);
    }
}

/// An odd bound above the floor: the spare slot must go where the cap puts it.
#[test]
fn a_bans_trim_agrees_with_the_cap_at_an_odd_bound() {
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
