//! Which member record a room keeps for a member id (freenet/river#703).
//!
//! A member record is signed by its inviter alone, so a second, validly
//! signed record for a member's id can be issued by someone other than the
//! inviter the room already holds. The room keeps the record it holds:
//!
//! - a record for an id already held is skipped, never an error, whatever
//!   its inviter; a record naming a different key for a held id is skipped;
//! - a state that itself holds two records for one id (`verify` allows it)
//!   is normalized to one, the same way whatever order they are stored in.
//!
//! Two peers that each hold a different record for one member each keep
//! their own when they merge (see `convergence_tests`).

use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta, MembersV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::time::{Duration, SystemTime};

fn at(t: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + t)
}

fn sk(b: u8) -> SigningKey {
    SigningKey::from_bytes(&[b; 32])
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

fn msg(owner: MemberId, author: MemberId, s: &SigningKey, t: u64) -> AuthorizedMessageV1 {
    AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner,
            author,
            time: at(t),
            content: RoomMessageBody::public(format!("m{t}")),
        },
        s,
    )
}

fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(s, &mut out).unwrap();
    out
}

/// owner -> A -> M -> N, and owner -> X. N and X have posted.
struct Fx {
    params: ChatRoomParametersV1,
    owner: MemberId,
    x_sk: SigningKey,
    x: MemberId,
    n_sk: SigningKey,
    n: MemberId,
    n_rec: AuthorizedMember,
    base: ChatRoomStateV1,
}

fn fx() -> Fx {
    let o_sk = sk(1);
    let owner = MemberId::from(&o_sk.verifying_key());
    let params = ChatRoomParametersV1 {
        owner: o_sk.verifying_key(),
    };
    let (a_sk, x_sk, m_sk, n_sk) = (sk(2), sk(3), sk(4), sk(5));
    let a = rec(owner, owner, a_sk.verifying_key(), &o_sk);
    let x = rec(owner, owner, x_sk.verifying_key(), &o_sk);
    let m = rec(owner, a.member.id(), m_sk.verifying_key(), &a_sk);
    let n = rec(owner, m.member.id(), n_sk.verifying_key(), &m_sk);
    let mut members = vec![a, x.clone(), m, n.clone()];
    members.sort_by_key(|r| r.member.id());
    let mut base = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner,
                max_members: 50,
                ..Default::default()
            },
            &o_sk,
        ),
        members: MembersV1 { members },
        recent_messages: MessagesV1 {
            messages: vec![
                msg(owner, n.member.id(), &n_sk, 1),
                msg(owner, x.member.id(), &x_sk, 2),
            ],
            ..Default::default()
        },
        ..Default::default()
    };
    base.recent_messages.rebuild_actions_state();
    base.post_apply_cleanup(&params).unwrap();
    base.verify(&base, &params).unwrap();
    assert_eq!(base.members.members.len(), 4, "premise: no one is pruned");
    Fx {
        params,
        owner,
        x: x.member.id(),
        x_sk,
        n: n.member.id(),
        n_sk,
        n_rec: n,
        base,
    }
}

fn apply(f: &Fx, s: &ChatRoomStateV1, delta: ChatRoomStateV1Delta) -> ChatRoomStateV1 {
    let mut next = s.clone();
    next.apply_delta(s, &f.params, &Some(delta))
        .expect("the delta applies");
    next.verify(&next, &f.params).expect("the result verifies");
    next
}

fn members_delta(added: Vec<AuthorizedMember>) -> ChatRoomStateV1Delta {
    ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(added)),
        ..Default::default()
    }
}

/// A member nearer the owner issues a second record for a deeper member. The
/// room keeps the record it holds, so that member's later ban on the deeper
/// one has no authority behind it and removes nobody.
#[test]
fn a_held_record_is_kept_over_one_issued_by_another_inviter() {
    let f = fx();
    let second = rec(f.owner, f.x, f.n_sk.verifying_key(), &f.x_sk);
    let s = apply(&f, &f.base, members_delta(vec![second]));
    let held = s
        .members
        .members
        .iter()
        .find(|m| m.member.id() == f.n)
        .unwrap();
    assert_eq!(held, &f.n_rec, "the held record is kept");

    let ban = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: f.owner,
            banned_at: at(10),
            banned_user: f.n,
        },
        f.x,
        &f.x_sk,
    );
    let s = apply(
        &f,
        &s,
        ChatRoomStateV1Delta {
            bans: Some(vec![ban]),
            ..Default::default()
        },
    );
    assert!(
        s.members.members.iter().any(|m| m.member.id() == f.n),
        "a ban from a member who is not an ancestor removes nobody"
    );
}

/// A state holding two records for one id (which `verify` accepts) is
/// normalized to one on any apply, and the stored order of the two does not
/// change the result or the summary.
#[test]
fn two_held_records_for_one_id_normalize_whatever_their_order() {
    let f = fx();
    let second = rec(f.owner, f.x, f.n_sk.verifying_key(), &f.x_sk);
    let mut one = f.base.clone();
    one.members.members.push(second.clone());
    let mut two = f.base.clone();
    two.members.members.insert(0, second);
    for s in [&one, &two] {
        s.verify(s, &f.params).expect("premise: verify accepts it");
    }
    let summary = |s: &ChatRoomStateV1| {
        let mut out = Vec::new();
        ciborium::ser::into_writer(&s.members.summarize(s, &f.params), &mut out).unwrap();
        out
    };
    let normal = |s: &ChatRoomStateV1| apply(&f, s, ChatRoomStateV1Delta::default());
    let (one, two) = (normal(&one), normal(&two));
    assert_eq!(ser(&one), ser(&two));
    assert_eq!(summary(&one), summary(&two));
    assert_eq!(
        one.members
            .members
            .iter()
            .filter(|m| m.member.id() == f.n)
            .count(),
        1
    );
    // And merging either with the base agrees.
    let merged = |a: &ChatRoomStateV1, b: &ChatRoomStateV1| {
        let mut m = a.clone();
        m.merge(a, &f.params, b).unwrap();
        m
    };
    assert_eq!(ser(&merged(&one, &f.base)), ser(&merged(&two, &f.base)));
}
