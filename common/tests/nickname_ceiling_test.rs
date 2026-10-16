//! The contract holds every member_info record to one fixed nickname ceiling
//! (`NICKNAME_CEILING`), held or arriving, whatever the room's
//! `max_nickname_size`, which is a client rule checked before signing
//! (freenet/river#703). The summary and the readers (`canonical`,
//! `deputies_of`) see the same records.

use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::member_info::{
    AuthorizedMemberInfo, MemberInfo, MemberInfoV1, NICKNAME_CEILING,
};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::time::{Duration, SystemTime};

fn at(t: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + t)
}
fn sk(b: u8) -> SigningKey {
    SigningKey::from_bytes(&[b; 32])
}
fn id(s: &SigningKey) -> MemberId {
    s.verifying_key().into()
}
fn rec(
    owner: MemberId,
    inviter: MemberId,
    vk: VerifyingKey,
    inviter_sk: &SigningKey,
) -> AuthorizedMember {
    AuthorizedMember::new(
        Member {
            owner_member_id: owner,
            invited_by: inviter,
            member_vk: vk,
        },
        inviter_sk,
    )
}
fn msg(owner: MemberId, s: &SigningKey, t: u64) -> AuthorizedMessageV1 {
    AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner,
            author: id(s),
            time: at(t),
            content: RoomMessageBody::public(format!("m{t}")),
        },
        s,
    )
}
fn merge(p: &ChatRoomParametersV1, a: &ChatRoomStateV1, b: &ChatRoomStateV1) -> ChatRoomStateV1 {
    let mut x = a.clone();
    x.merge_from(p, b).unwrap();
    x
}
fn info(s: &SigningKey, version: u32, nick: &str, deputies: Vec<MemberId>) -> AuthorizedMemberInfo {
    let mut i = MemberInfo::new_public(id(s), version, nick.to_string());
    i.deputies = deputies;
    AuthorizedMemberInfo::new_with_member_key(i, s)
}

/// Owner O; X invited by O; D invited by X. Both posted.
struct Room {
    o: SigningKey,
    x: SigningKey,
    d: SigningKey,
    p: ChatRoomParametersV1,
    base: ChatRoomStateV1,
}

fn room(max_nickname_size: usize) -> Room {
    let (o, x, d) = (sk(1), sk(2), sk(3));
    let owner = id(&o);
    let p = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let base = ChatRoomStateV1 {
        configuration: config(&o, 1, max_nickname_size),
        members: MembersV1 {
            members: vec![
                rec(owner, owner, x.verifying_key(), &o),
                rec(owner, id(&x), d.verifying_key(), &x),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![msg(owner, &x, 1), msg(owner, &d, 2)],
            ..Default::default()
        },
        ..Default::default()
    };
    Room { o, x, d, p, base }
}

fn config(o: &SigningKey, version: u32, max_nickname_size: usize) -> AuthorizedConfigurationV1 {
    AuthorizedConfigurationV1::new(
        Configuration {
            owner_member_id: id(o),
            configuration_version: version,
            max_nickname_size,
            ..Default::default()
        },
        o,
    )
}

fn holding(r: &Room, records: Vec<AuthorizedMemberInfo>) -> ChatRoomStateV1 {
    let mut s = r.base.clone();
    s.member_info = MemberInfoV1 {
        member_info: records,
    };
    s.verify(&s, &r.p).expect("premise: verify accepts it");
    s
}

/// At the ceiling a record is kept; one byte over, it is dropped, whichever
/// side of the merge holds it, and both directions agree.
#[test]
fn the_nickname_ceiling_is_one_rule_for_held_and_arriving_records() {
    let r = room(50);
    let at_ceiling = info(&r.x, 1, &"a".repeat(NICKNAME_CEILING), vec![]);
    let over = info(&r.x, 2, &"b".repeat(NICKNAME_CEILING + 1), vec![]);
    let a = holding(&r, vec![at_ceiling.clone()]);
    let b = holding(&r, vec![over]);
    let ab = merge(&r.p, &a, &b);
    let ba = merge(&r.p, &b, &a);
    assert_eq!(ab, ba);
    assert_eq!(ab.member_info.canonical(id(&r.x)), Some(&at_ceiling));
    assert_eq!(merge(&r.p, &ab, &ab), ab);
}

/// A state stored with an over-ceiling record (only a full-state PUT carries
/// one): the readers ignore it exactly as the summary does, so ban authority
/// and anti-entropy agree before any apply runs.
#[test]
fn readers_and_the_summary_see_the_same_records() {
    let r = room(50);
    let over = info(&r.x, 1, &"c".repeat(NICKNAME_CEILING + 1), vec![id(&r.d)]);
    let s = holding(&r, vec![over]);
    let summary = s.member_info.summarize(&s, &r.p);
    assert!(!summary.contains_key(&id(&r.x)));
    assert!(s.member_info.canonical(id(&r.x)).is_none());
    assert!(s.member_info.deputies_of(id(&r.x)).is_empty());
}

/// A room whose owner set `max_nickname_size` below a nickname already held
/// (any room from before this rule) keeps that nickname through merges.
#[test]
fn a_nickname_over_the_room_limit_is_kept() {
    let r = room(5);
    let long = info(&r.x, 1, "longer than five", vec![]);
    let s = holding(&r, vec![long.clone()]);
    let m = merge(&r.p, &s, &s);
    assert_eq!(m.member_info.canonical(id(&r.x)), Some(&long));
    let other = merge(&r.p, &r.base, &s);
    assert_eq!(other.member_info.canonical(id(&r.x)), Some(&long));
}

/// X grants D deputy rights (v1), then revokes them with a longer nickname
/// (v2). Lowering the room's limit below v2's nickname afterwards does not
/// bring the grant back on a peer that still held v1.
#[test]
fn a_lowered_limit_does_not_revive_a_revoked_grant() {
    let r = room(50);
    let stale = holding(&r, vec![info(&r.x, 1, "x", vec![id(&r.d)])]);
    let stale = merge(&r.p, &stale, &stale);
    let cur = holding(&r, vec![info(&r.x, 2, &"x".repeat(30), vec![])]);
    let cur = merge(&r.p, &cur, &cur);
    let mut low = cur.clone();
    low.apply_delta(
        &cur,
        &r.p,
        &Some(ChatRoomStateV1Delta {
            configuration: Some(config(&r.o, 2, 20)),
            ..Default::default()
        }),
    )
    .unwrap();
    let m1 = merge(&r.p, &low, &stale);
    let m2 = merge(&r.p, &stale, &low);
    assert_eq!(m1, m2);
    assert!(m1.member_info.deputies_of(id(&r.x)).is_empty());
}
