//! Which member record a room keeps for a member id (freenet/river#703).
//!
//! The room keeps one member record per member id, the one it holds:
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

/// A record for a held id is skipped, whatever its inviter: the held record
/// is kept, and a ban is judged against the held record's invite chain.
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

// Which record normalization keeps, new-record placement, members trim, bounds
// ===========================================================================
use river_core::room_state::ban_evidence::evidence_digest;

fn n_records(f: &Fx, s: &ChatRoomStateV1) -> Vec<AuthorizedMember> {
    s.members
        .members
        .iter()
        .filter(|m| m.member.id() == f.n)
        .cloned()
        .collect()
}

/// M3/M5: of two held records for one id, the one NEARER the owner is kept
/// (documented in `normalize_held_records`). The in-tree test only checks that
/// the result does not depend on storage order, so a flipped or dropped depth
/// rule passes.
#[test]
fn normalization_keeps_the_record_nearest_the_owner() {
    let f = fx();
    let shallow = rec(f.owner, f.x, f.n_sk.verifying_key(), &f.x_sk); // owner -> X -> N
    let mut s = f.base.clone();
    s.members.members.push(shallow.clone());
    let s = apply(&f, &s, ChatRoomStateV1Delta::default());
    assert_eq!(
        n_records(&f, &s),
        vec![shallow],
        "owner->X->N beats owner->A->M->N"
    );
}

/// M8: a duplicate whose inviter is unknown (so its chain never reaches the
/// owner) never displaces a record that does.
#[test]
fn normalization_never_prefers_a_record_that_does_not_reach_the_owner() {
    let f = fx();
    let ghost_sk = sk(77);
    let ghost = MemberId::from(&ghost_sk.verifying_key());
    let orphan = rec(f.owner, ghost, f.n_sk.verifying_key(), &ghost_sk);
    let mut s = f.base.clone();
    s.members.members.push(orphan);
    let s = apply(&f, &s, ChatRoomStateV1Delta::default());
    assert_eq!(n_records(&f, &s), vec![f.n_rec.clone()]);
}

/// M4: two held records at the SAME depth: the lower record digest wins.
#[test]
fn normalization_breaks_a_depth_tie_by_the_lower_digest() {
    let f = fx();
    // owner -> X -> N and owner -> Y -> N are both depth 2; the held
    // owner -> A -> M -> N is depth 3 and loses to both.
    let y_sk = sk(60);
    let y = rec(f.owner, f.owner, y_sk.verifying_key(), &sk(1));
    let via_x = rec(f.owner, f.x, f.n_sk.verifying_key(), &f.x_sk);
    let via_y = rec(f.owner, y.member.id(), f.n_sk.verifying_key(), &y_sk);
    let mut s = f.base.clone();
    s.members.members.push(y);
    s.members.members.push(via_x.clone());
    s.members.members.push(via_y.clone());
    // Y must have posted so cleanup keeps it.
    s.recent_messages.messages.push(msg(
        f.owner,
        s.members
            .members
            .iter()
            .find(|m| m.member.invited_by == f.owner && m.member.member_vk == y_sk.verifying_key())
            .unwrap()
            .member
            .id(),
        &y_sk,
        5,
    ));
    s.recent_messages.rebuild_actions_state();
    let s = apply(&f, &s, ChatRoomStateV1Delta::default());
    let want = if evidence_digest(&via_x) < evidence_digest(&via_y) {
        via_x
    } else {
        via_y
    };
    assert_eq!(n_records(&f, &s), vec![want]);
}

/// M1e: BOTH held-id guards removed. An arriving record from a nearer inviter
/// must not end up beside the held one (and win the next normalization).
/// (Redundant guards: removing either alone is invisible, see M1b/M1c/M1d.)
#[test]
fn an_arriving_record_for_a_held_id_is_never_stored_even_from_a_nearer_inviter() {
    let f = fx();
    let second = rec(f.owner, f.x, f.n_sk.verifying_key(), &f.x_sk);
    let mut s = apply(&f, &f.base, members_delta(vec![second]));
    assert_eq!(n_records(&f, &s), vec![f.n_rec.clone()], "kept immediately");
    // And it stays after a further apply that normalizes.
    s = apply(&f, &s, ChatRoomStateV1Delta::default());
    assert_eq!(
        n_records(&f, &s),
        vec![f.n_rec.clone()],
        "kept after normalization"
    );
}

/// Lowered-cap and raised-cap member trims through the public merge.
///
/// `depth1` members invited by the owner and `depth2` invited by the first of
/// them; every member posts once (a member with no message is pruned).
fn big_room(
    depth1: usize,
    depth2: usize,
    version: u32,
    max_members: usize,
    with_members: bool,
) -> (ChatRoomParametersV1, ChatRoomStateV1) {
    let o_sk = sk(1);
    let owner = MemberId::from(&o_sk.verifying_key());
    let params = ChatRoomParametersV1 {
        owner: o_sk.verifying_key(),
    };
    let key = |i: usize| {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&(i as u64 + 1000).to_le_bytes());
        b[31] = 9;
        SigningKey::from_bytes(&b)
    };
    let mut members = Vec::new();
    let mut messages = Vec::new();
    let mut first: Option<(MemberId, SigningKey)> = None;
    if with_members {
        for i in 0..depth1 {
            let k = key(i);
            let m = rec(owner, owner, k.verifying_key(), &o_sk);
            messages.push(msg(owner, m.member.id(), &k, i as u64));
            if first.is_none() {
                first = Some((m.member.id(), k.clone()));
            }
            members.push(m);
        }
        let (fid, fsk) = first.unwrap();
        for j in 0..depth2 {
            let k = key(depth1 + j);
            let m = rec(owner, fid, k.verifying_key(), &fsk);
            messages.push(msg(owner, m.member.id(), &k, (depth1 + j) as u64));
            members.push(m);
        }
    }
    members.sort_by_key(|m| m.member.id());
    let mut s = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner,
                configuration_version: version,
                max_members,
                max_recent_messages: 5_000,
                ..Default::default()
            },
            &o_sk,
        ),
        members: MembersV1 { members },
        recent_messages: MessagesV1 {
            messages,
            ..Default::default()
        },
        ..Default::default()
    };
    s.recent_messages.rebuild_actions_state();
    (params, s)
}

fn both_ways(
    params: &ChatRoomParametersV1,
    a: &ChatRoomStateV1,
    b: &ChatRoomStateV1,
) -> (ChatRoomStateV1, ChatRoomStateV1) {
    let mut ab = a.clone();
    ab.merge(a, params, b).expect("merge(A, B) must not fail");
    let mut ba = b.clone();
    ba.merge(b, params, a).expect("merge(B, A) must not fail");
    (ab, ba)
}

/// T7/T8/T8b/T8c: the sender holds more members than one delta may add AND
/// they sit at two depths. The in-tree test
/// (`lowering_max_members_from_above_the_work_bound_converges`) has every
/// member at depth 1, so the ordering of the trim (shortest chain first, then
/// lowest id) is never exercised.
#[test]
fn a_members_trim_keeps_shallow_chains_then_lowest_ids() {
    let (params, sender) = big_room(1_000, 600, 1, 1_600, true);
    let (_, receiver) = big_room(0, 0, 2, 1_500, false); // lowered cap, no members
    let (ab, ba) = both_ways(&params, &sender, &receiver);
    assert_eq!(ser(&ab), ser(&ba), "one-shot merge must commute");
    assert_eq!(ab.members.members.len(), 1_500);
    ab.verify(&ab, &params).unwrap();
}

/// T17: a RAISED cap above the work-bound floor. The receiver (cap 900, bound
/// 1000) merges a sender whose newer configuration raises the cap to 1500 and
/// who holds 1500: the trim must read the MERGED cap, not the receiver's old one.
#[test]
fn a_members_trim_reads_the_merged_cap_not_the_receivers_old_one() {
    let (params, sender) = big_room(1_000, 500, 2, 1_500, true);
    let (_, receiver) = big_room(0, 0, 1, 900, false);
    let (ab, ba) = both_ways(&params, &sender, &receiver);
    assert_eq!(ser(&ab), ser(&ba));
    assert_eq!(ab.members.members.len(), 1_500);
}

/// M12/M12b/B4: a NEW member arriving in ONE delta as two records from two
/// inviters at the same depth. The documented rule is "the lower record digest",
/// independent of the order the records are listed in. (Two peers that receive
/// the two orders and disagree hold different records for that member for good.)
#[test]
fn two_new_records_for_one_id_at_equal_depth_resolve_to_the_lower_digest_in_either_order() {
    let f = fx();
    let a_sk = sk(2);
    let a_id = MemberId::from(&a_sk.verifying_key());
    let z_sk = sk(90);
    let via_x = rec(f.owner, f.x, z_sk.verifying_key(), &f.x_sk);
    let via_a = rec(f.owner, a_id, z_sk.verifying_key(), &a_sk);
    assert_eq!(via_x.member.id(), via_a.member.id());
    let z = via_x.member.id();
    let want = if evidence_digest(&via_x) < evidence_digest(&via_a) {
        via_x.clone()
    } else {
        via_a.clone()
    };
    for order in [
        vec![via_x.clone(), via_a.clone()],
        vec![via_a.clone(), via_x.clone()],
    ] {
        let s = apply(
            &f,
            &f.base,
            ChatRoomStateV1Delta {
                members: Some(MembersDelta::new(order)),
                recent_messages: Some(vec![msg(f.owner, z, &z_sk, 40)]),
                ..Default::default()
            },
        );
        let got: Vec<_> = s
            .members
            .members
            .iter()
            .filter(|m| m.member.id() == z)
            .cloned()
            .collect();
        assert_eq!(got, vec![want.clone()]);
    }
}

/// B3/Q5 and C12/Q9: the bound on NEW records after de-duplication. A delta
/// that re-delivers nothing but carries one record more than the bound is
/// refused even when the state holds records (so the raw-length check, which
/// allows held + bound, passes). Before the lowered-cap trims this was pinned
/// by `..._is_refused_once` tests that no longer exist.
#[test]
fn a_delta_of_one_more_new_record_than_the_bound_is_refused_and_the_bound_itself_is_not() {
    use river_core::room_state::ban::{BansV1, MIN_BANS_DELTA_BOUND};
    let f = fx();
    // Members: held 4 (fx base); bound = max(50, 1000) = 1000.
    let o_sk = sk(1);
    let new_members = |n: usize| -> Vec<AuthorizedMember> {
        (0..n)
            .map(|i| {
                let mut b = [0u8; 32];
                b[..8].copy_from_slice(&(i as u64 + 7_000).to_le_bytes());
                b[31] = 5;
                rec(
                    f.owner,
                    f.owner,
                    SigningKey::from_bytes(&b).verifying_key(),
                    &o_sk,
                )
            })
            .collect()
    };
    let try_members = |n: usize| {
        let mut m = f.base.members.clone();
        m.apply_delta(&f.base, &f.params, &Some(MembersDelta::new(new_members(n))))
    };
    assert!(try_members(1_000).is_ok(), "exactly the bound is accepted");
    assert!(
        try_members(1_001).is_err(),
        "one more new member than the bound is refused"
    );

    // Bans: one held ban; bound = max(max_user_bans, MIN_BANS_DELTA_BOUND).
    let ban = |i: u64| {
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: f.owner,
                banned_at: at(i),
                banned_user: {
                    let mut b = [0u8; 32];
                    b[..8].copy_from_slice(&(i + 90_000).to_le_bytes());
                    b[31] = 6;
                    MemberId::from(&SigningKey::from_bytes(&b).verifying_key())
                },
            },
            f.owner,
            &o_sk,
        )
    };
    let mut parent = f.base.clone();
    parent.bans = BansV1(vec![ban(0)]);
    let try_bans = |n: usize| {
        let mut b = parent.bans.clone();
        b.apply_delta(
            &parent,
            &f.params,
            &Some((1..=n as u64).map(ban).collect::<Vec<_>>()),
        )
    };
    let bound = parent
        .configuration
        .configuration
        .max_user_bans
        .max(MIN_BANS_DELTA_BOUND);
    assert!(try_bans(bound).is_ok(), "exactly the bound is accepted");
    assert!(
        try_bans(bound + 1).is_err(),
        "one more new ban than the bound is refused"
    );
}

/// Held records are kept as they are when new records are placed, reached
/// from the owner or not: an apply never drops a held record for lack of an
/// inviter (cleanup and `verify` judge chains; placing new records does not).
#[test]
fn placing_new_records_keeps_a_held_record_whose_inviter_is_not_held() {
    let f = fx();
    let orphan_sk = sk(20);
    let missing_sk = sk(21);
    let orphan = rec(
        f.owner,
        MemberId::from(&missing_sk.verifying_key()),
        orphan_sk.verifying_key(),
        &missing_sk,
    );
    let mut members = f.base.members.clone();
    members.members.push(orphan.clone());
    let newcomer = rec(f.owner, f.x, sk(22).verifying_key(), &f.x_sk);
    members
        .apply_delta(
            &f.base,
            &f.params,
            &Some(MembersDelta::new(vec![newcomer.clone()])),
        )
        .expect("the new record is placed");
    assert!(members.members.contains(&orphan), "the held record is kept");
    assert!(members.members.contains(&newcomer));
}

/// Two new records for one id naming the same key, at different invite
/// depths: the nearer one is kept in either delta order (the key rule only
/// narrows records naming different keys).
#[test]
fn two_new_records_naming_one_key_keep_the_nearer_in_either_order() {
    let f = fx();
    let m_sk = sk(4);
    let m_id = MemberId::from(&m_sk.verifying_key());
    let z_sk = sk(91);
    let deep = rec(f.owner, m_id, z_sk.verifying_key(), &m_sk);
    let near = rec(f.owner, f.x, z_sk.verifying_key(), &f.x_sk);
    let z = near.member.id();
    for order in [
        vec![deep.clone(), near.clone()],
        vec![near.clone(), deep.clone()],
    ] {
        let s = apply(
            &f,
            &f.base,
            ChatRoomStateV1Delta {
                members: Some(MembersDelta::new(order)),
                recent_messages: Some(vec![msg(f.owner, z, &z_sk, 41)]),
                ..Default::default()
            },
        );
        let got: Vec<_> = s
            .members
            .members
            .iter()
            .filter(|m| m.member.id() == z)
            .cloned()
            .collect();
        assert_eq!(got, vec![near.clone()]);
    }
}
