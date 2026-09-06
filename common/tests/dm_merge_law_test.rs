//! Regression tests for freenet/river#671 — DMs with a dead endpoint were
//! handled inconsistently, breaking two laws the contract relies on.
//!
//! # The defect
//!
//! An INCOMING DM whose sender or recipient is not a current member is
//! rejected inside `DirectMessagesV1::apply_delta` (`resolve_member_vk`
//! returns `None`). An ALREADY-HELD copy of the same DM was not: `apply_delta`
//! never re-validated the held set, so such a DM survived until
//! `post_apply_cleanup` step 6 swept it. Those two removal points sat on
//! opposite sides of `trim_to_global_cap`, which produced two symptoms:
//!
//! 1. **Data loss.** In a room saturated at the global DM cap, a doomed DM
//!    ranked against live ones and won a cap slot on `order_key`, evicting a
//!    legitimate DM between two current members — and was then swept anyway.
//!    Measured on live Official-room state: six real messages destroyed by a
//!    merge in one direction and not the other, which is also why the
//!    whole-state merge was not commutative.
//!
//! 2. **Non-idempotent cleanup.** `post_apply_cleanup` step 1 counted DM
//!    participants from EVERY held DM, so a member whose only claim to
//!    retention was a DM with a banned counterparty was exempted from
//!    inactivity-prune on pass 1, had that DM swept by step 6 of the same
//!    pass, and was pruned on pass 2 — contradicting the IDEMPOTENCE invariant
//!    the function's own doc comment declares a MUST.
//!
//! # The fix
//!
//! Two predicate alignments, one per symptom:
//!
//! (a) `apply_delta` sweeps unresolvable endpoints out of the HELD set before
//!     the caps run, so the caps rank only DMs that can survive the pass.
//! (b) step 1 counts participants only from DMs that survive the step-6 sweep,
//!     sharing one `dm_endpoint_is_live` predicate with it, so exemption ⟺
//!     retention. Same remedy #411 round 4 applied to the banner exemption.
//!
//! Each test below isolates one half: `cleanup_is_idempotent_*` fails if (b)
//! is reverted, `global_cap_must_not_evict_*` fails if (a) is reverted.
//!
//! The three `whole_state_merge_*` / `none_delta_path_*` tests at the bottom
//! came from the PR review rather than from the author of the fix, and are
//! mutation-verified the same way — see their own doc comments.

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{
    sign_direct_message, AuthorizedDirectMessage, DirectMessagesDelta, DirectMessagesV1,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::member_info::{
    AuthorizedMemberInfo, MemberInfo, MemberInfoV1, MAX_DEPUTIES,
};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::collections::HashSet;
use std::time::SystemTime;

/// Epoch offset so generated timestamps read as small integers.
const BASE_TS: u64 = 1_700_000_000;

struct Peer {
    sk: SigningKey,
    id: MemberId,
}

impl Peer {
    /// Deterministic keys, so a failure reproduces exactly — signatures are
    /// the tiebreak half of `DmOrderKey`, so a random key would make the
    /// cap's eviction boundary vary between runs.
    fn new(seed: u8) -> Self {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let id = sk.verifying_key().into();
        Self { sk, id }
    }
}

fn params(owner: &Peer) -> ChatRoomParametersV1 {
    ChatRoomParametersV1 {
        owner: owner.sk.verifying_key(),
    }
}

fn config(owner: &Peer, max_direct_messages: Option<usize>) -> AuthorizedConfigurationV1 {
    AuthorizedConfigurationV1::new(
        Configuration {
            max_members: 1000,
            max_direct_messages,
            ..Default::default()
        },
        &owner.sk,
    )
}

fn member(who: &Peer, owner: &Peer) -> AuthorizedMember {
    AuthorizedMember::new(
        Member {
            owner_member_id: owner.id,
            invited_by: owner.id,
            member_vk: who.sk.verifying_key(),
        },
        &owner.sk,
    )
}

fn info(who: &Peer) -> AuthorizedMemberInfo {
    AuthorizedMemberInfo::new_with_member_key(
        MemberInfo::new_public(who.id, 0, "nick".to_string()),
        &who.sk,
    )
}

/// A ban of `target` issued by the room owner (always authorized, so the test
/// does not depend on deputy resolution).
fn owner_ban(target: MemberId, owner: &Peer) -> AuthorizedUserBan {
    AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner.id,
            banned_at: SystemTime::now(),
            banned_user: target,
        },
        owner.id,
        &owner.sk,
    )
}

/// A signed DM from `from` to `to` at `BASE_TS + offset`.
fn dm(from: &Peer, to: &Peer, owner: &Peer, offset: u64) -> AuthorizedDirectMessage {
    sign_direct_message(
        &from.sk,
        from.id,
        to.id,
        &owner.sk.verifying_key(),
        BASE_TS + offset,
        vec![(offset % 251) as u8; 8],
    )
    .expect("sign dm")
}

/// A ban of `target` issued by `banner` under a deputy grant (not owner authority).
fn deputy_ban(target: MemberId, banner: &Peer, owner_id: MemberId) -> AuthorizedUserBan {
    AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::now(),
            banned_user: target,
        },
        banner.id,
        &banner.sk,
    )
}

/// A join-event so the member survives inactivity-prune, isolating the DM behaviour.
fn join_msg(who: &Peer, owner_id: MemberId) -> AuthorizedMessageV1 {
    AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: who.id,
            time: SystemTime::now(),
            content: RoomMessageBody::join_event(),
        },
        &who.sk,
    )
}

fn member_ids(s: &ChatRoomStateV1) -> HashSet<MemberId> {
    s.members.members.iter().map(|m| m.member.id()).collect()
}

/// The exact bytes the contract compares — `update_state` merges by
/// serializing, so byte equality is the law being tested, not `PartialEq`.
fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = Vec::new();
    ciborium::ser::into_writer(s, &mut v).expect("state serializes");
    v
}

/// `merge(a, b)` exactly as `update_state` performs it for `UpdateData::State`.
fn merged(a: &ChatRoomStateV1, b: &ChatRoomStateV1, p: &ChatRoomParametersV1) -> ChatRoomStateV1 {
    let mut s = a.clone();
    let parent = s.clone();
    s.merge(&parent, p, b).expect("merge");
    s
}

/// The timestamp offsets currently held, ascending.
fn held_offsets(s: &ChatRoomStateV1) -> Vec<u64> {
    let mut v: Vec<u64> = s
        .direct_messages
        .messages
        .iter()
        .map(|m| m.message.timestamp - BASE_TS)
        .collect();
    v.sort_unstable();
    v
}

// ---------------------------------------------------------------------------
// (b) Idempotence: the step-1 exemption must equal the step-6 sweep
// ---------------------------------------------------------------------------

/// `cleanup(cleanup(S)) == cleanup(S)` for a state where a member's only claim
/// to retention is a DM with a banned counterparty.
///
/// Shape (from the issue): owner + member M + member Q, one DM each way
/// between M and Q, M authors no room messages, Q banned by the owner.
///
/// Before the fix, step 1 counted M as a DM participant from a DM that step 6
/// of the same pass deleted, so M survived pass 1 and was pruned on pass 2.
/// The two-pass member sets differ, which permanently diverges peers that run
/// cleanup a different number of times (and full-state PUTs run it zero).
#[test]
fn cleanup_is_idempotent_when_a_member_is_held_alive_only_by_a_doomed_dm() {
    let owner = Peer::new(1);
    let m = Peer::new(2);
    let q = Peer::new(3);
    let p = params(&owner);

    let mut state = ChatRoomStateV1 {
        configuration: config(&owner, None),
        members: MembersV1 {
            members: vec![member(&m, &owner), member(&q, &owner)],
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&m), info(&q)],
        },
        // Q is banned, so both DMs are doomed: step 6 sweeps a DM whose
        // endpoint is enforced-banned or no longer a member.
        bans: BansV1(vec![owner_ban(q.id, &owner)]),
        direct_messages: DirectMessagesV1 {
            messages: vec![dm(&m, &q, &owner, 10), dm(&q, &m, &owner, 11)],
            purges: vec![],
        },
        // M authors nothing, so nothing but the DMs can keep M present.
        ..Default::default()
    };

    // Premise: M really is retained by nothing else. Without this the test
    // could pass while measuring the wrong thing.
    assert!(
        state.recent_messages.messages.is_empty(),
        "test premise: M must have no room messages"
    );

    state.post_apply_cleanup(&p).expect("cleanup pass 1");
    let after_one = state.clone();

    state.post_apply_cleanup(&p).expect("cleanup pass 2");

    assert_eq!(
        after_one, state,
        "post_apply_cleanup must be a fixpoint: pass 2 changed the state, so \
         peers that run it a different number of times hold different states"
    );

    // And the fixpoint is the correct one — M gone, not M kept.
    assert!(
        !member_ids(&after_one).contains(&m.id),
        "M is retained by nothing once the doomed DMs are swept, so the FIRST \
         pass must already have pruned M"
    );
    assert!(
        !member_ids(&after_one).contains(&q.id),
        "Q is banned and must be removed"
    );
    assert!(
        after_one.direct_messages.messages.is_empty(),
        "both DMs have a dead endpoint and must be swept"
    );
}

/// The counterpart that keeps the test above honest: when the counterparty is
/// NOT banned, the DM exemption still works and both members are retained.
///
/// Without this, "prune everyone" would satisfy the idempotence assertion.
#[test]
fn dm_participants_are_still_exempt_from_inactivity_prune_when_both_are_live() {
    let owner = Peer::new(1);
    let m = Peer::new(2);
    let q = Peer::new(3);
    let p = params(&owner);

    let mut state = ChatRoomStateV1 {
        configuration: config(&owner, None),
        members: MembersV1 {
            members: vec![member(&m, &owner), member(&q, &owner)],
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&m), info(&q)],
        },
        direct_messages: DirectMessagesV1 {
            messages: vec![dm(&m, &q, &owner, 10), dm(&q, &m, &owner, 11)],
            purges: vec![],
        },
        ..Default::default()
    };

    state.post_apply_cleanup(&p).expect("cleanup pass 1");
    let after_one = state.clone();
    state.post_apply_cleanup(&p).expect("cleanup pass 2");

    assert_eq!(after_one, state, "cleanup must still be a fixpoint");
    assert_eq!(
        member_ids(&after_one),
        HashSet::from([m.id, q.id]),
        "a DM between two LIVE members must keep both exempt from prune"
    );
    assert_eq!(
        after_one.direct_messages.messages.len(),
        2,
        "live DMs must survive"
    );
}

// ---------------------------------------------------------------------------
// (a) The cap must not spend slots on DMs that will not survive the pass
// ---------------------------------------------------------------------------

/// No legitimate DM is evicted when the global cap binds and the newest held
/// DMs are doomed.
///
/// Shape (from the issue): cap N, the receiver holds N DMs of which k involve
/// a since-banned member and are the NEWEST by `order_key`, and the sender
/// offers k legitimate DMs above the receiver's retention horizon.
///
/// Before the fix the union of N + k candidates was trimmed to N with the k
/// doomed DMs still in the ranking; being newest, they won and evicted the k
/// oldest legitimate ones, and cleanup step 6 then deleted them anyway —
/// leaving N - k. The k legitimate DMs were destroyed by messages that were
/// removed moments later in the same call.
#[test]
fn global_cap_must_not_evict_legitimate_dms_for_doomed_ones() {
    const CAP: usize = 20;
    const K: usize = 5;

    let owner = Peer::new(1);
    // Four live members give two disjoint DM pairs, both far below
    // MAX_DM_MESSAGES_PER_PAIR (100), so only the GLOBAL cap can bite.
    let a = Peer::new(10);
    let b = Peer::new(11);
    let c = Peer::new(12);
    let d = Peer::new(13);
    // Q is a member of the receiver's state and is banned by the delta.
    let q = Peer::new(20);
    let p = params(&owner);

    // Receiver holds CAP DMs: CAP-K legitimate ones at offsets 0..15, and K
    // DMs with Q at offsets 100..105 — the NEWEST in the room, so they are
    // exactly the ones the cap would keep.
    let legit_held: Vec<AuthorizedDirectMessage> = (0..(CAP - K) as u64)
        .map(|i| {
            if i % 2 == 0 {
                dm(&a, &b, &owner, i)
            } else {
                dm(&c, &d, &owner, i)
            }
        })
        .collect();
    let doomed_held: Vec<AuthorizedDirectMessage> = (0..K as u64)
        .map(|i| {
            if i % 2 == 0 {
                dm(&a, &q, &owner, 100 + i)
            } else {
                dm(&q, &a, &owner, 100 + i)
            }
        })
        .collect();

    let mut receiver = ChatRoomStateV1 {
        configuration: config(&owner, Some(CAP)),
        members: MembersV1 {
            members: vec![
                member(&a, &owner),
                member(&b, &owner),
                member(&c, &owner),
                member(&d, &owner),
                member(&q, &owner),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&a), info(&b), info(&c), info(&d), info(&q)],
        },
        direct_messages: DirectMessagesV1 {
            messages: legit_held.iter().chain(&doomed_held).cloned().collect(),
            purges: vec![],
        },
        ..Default::default()
    };

    // Premise 1: the receiver is exactly AT the cap, so any addition forces an
    // eviction. A fixture that started below the cap would pass vacuously.
    assert_eq!(
        receiver.direct_messages.messages.len(),
        CAP,
        "test premise: the receiver must start saturated at the cap"
    );
    // Premise 2: the doomed DMs are the newest, so a cap that ranks them will
    // keep them in preference to the legitimate ones.
    let newest_legit = legit_held
        .iter()
        .map(|m| m.order_key())
        .max()
        .expect("legit DMs");
    let oldest_doomed = doomed_held
        .iter()
        .map(|m| m.order_key())
        .min()
        .expect("doomed DMs");
    assert!(
        oldest_doomed > newest_legit,
        "test premise: every doomed DM must outrank every legitimate one, or \
         the cap would discard them for free and the test proves nothing"
    );

    // The delta bans Q and offers K legitimate DMs at offsets 50..55 — above
    // the receiver's horizon (its oldest held key), so the horizon filter is
    // not what keeps them out.
    let offered: Vec<AuthorizedDirectMessage> =
        (0..K as u64).map(|i| dm(&b, &a, &owner, 50 + i)).collect();

    let delta = ChatRoomStateV1Delta {
        bans: Some(vec![owner_ban(q.id, &owner)]),
        direct_messages: Some(DirectMessagesDelta {
            new_messages: offered.clone(),
            advanced_purges: vec![],
        }),
        ..Default::default()
    };

    let parent = receiver.clone();
    receiver
        .apply_delta(&parent, &p, &Some(delta))
        .expect("delta must apply");

    let surviving: HashSet<[u8; 64]> = receiver
        .direct_messages
        .messages
        .iter()
        .map(|m| m.sender_signature.to_bytes())
        .collect();

    // The k legitimate DMs the doomed ones would have evicted are the OLDEST
    // held ones; assert on the whole legitimate set so the test also catches a
    // fix that drops a different legitimate DM instead.
    for m in legit_held.iter().chain(&offered) {
        assert!(
            surviving.contains(&m.sender_signature.to_bytes()),
            "legitimate DM at offset {} was evicted — a DM destined for deletion \
             took its cap slot",
            m.message.timestamp - BASE_TS
        );
    }
    for m in &doomed_held {
        assert!(
            !surviving.contains(&m.sender_signature.to_bytes()),
            "DM at offset {} has a banned endpoint and must not survive",
            m.message.timestamp - BASE_TS
        );
    }

    // Exact shape, so a change that merely keeps MORE also fails.
    assert_eq!(
        held_offsets(&receiver),
        (0..(CAP - K) as u64)
            .chain(50..(50 + K as u64))
            .collect::<Vec<u64>>(),
        "the surviving set must be exactly the legitimate held DMs plus the \
         legitimate offered ones"
    );
    assert!(
        !member_ids(&receiver).contains(&q.id),
        "Q is banned and must be removed"
    );
}

// ---------------------------------------------------------------------------
// From the PR review. The tests above pin the two halves of the fix at the
// field level; these pin the whole-state laws the corpus measured, plus the
// `delta == None` arm, which a reviewer's mutation showed had no coverage at
// all — removing the sweep from that arm alone failed zero of 415 tests.
// ---------------------------------------------------------------------------

/// Build the #671 shape as two peers: X is saturated at the cap and holds `k`
/// doomed DMs with Q as its NEWEST entries; Y knows Q is banned and holds `k`
/// legitimate DMs of its own.
fn two_peer_671_shape(
    cap: usize,
    k: usize,
) -> (Peer, ChatRoomStateV1, ChatRoomStateV1, ChatRoomParametersV1) {
    let owner = Peer::new(1);
    let (a, b, c, d) = (Peer::new(10), Peer::new(11), Peer::new(12), Peer::new(13));
    let q = Peer::new(20);
    let p = params(&owner);

    let legit: Vec<AuthorizedDirectMessage> = (0..(cap - k) as u64)
        .map(|i| {
            if i % 2 == 0 {
                dm(&a, &b, &owner, i)
            } else {
                dm(&c, &d, &owner, i)
            }
        })
        .collect();
    let doomed: Vec<AuthorizedDirectMessage> = (0..k as u64)
        .map(|i| {
            if i % 2 == 0 {
                dm(&a, &q, &owner, 100 + i)
            } else {
                dm(&q, &a, &owner, 100 + i)
            }
        })
        .collect();

    let members = MembersV1 {
        members: vec![
            member(&a, &owner),
            member(&b, &owner),
            member(&c, &owner),
            member(&d, &owner),
            member(&q, &owner),
        ],
    };
    let minfo = MemberInfoV1 {
        member_info: vec![info(&a), info(&b), info(&c), info(&d), info(&q)],
    };

    let x = ChatRoomStateV1 {
        configuration: config(&owner, Some(cap)),
        members: members.clone(),
        member_info: minfo.clone(),
        direct_messages: DirectMessagesV1 {
            messages: legit.iter().chain(&doomed).cloned().collect(),
            purges: vec![],
        },
        ..Default::default()
    };
    let y_dms: Vec<AuthorizedDirectMessage> =
        (0..k as u64).map(|i| dm(&b, &a, &owner, 50 + i)).collect();
    let y = ChatRoomStateV1 {
        configuration: config(&owner, Some(cap)),
        members,
        member_info: minfo,
        bans: BansV1(vec![owner_ban(q.id, &owner)]),
        direct_messages: DirectMessagesV1 {
            messages: y_dms,
            purges: vec![],
        },
        ..Default::default()
    };
    (owner, x, y, p)
}

/// `merge(X,Y)` must byte-equal `merge(Y,X)` on the #671 shape.
///
/// The field-level tests above cannot see this: the defect is only visible once
/// the ban arrives from the *other* peer, which is what makes the two merge
/// directions differ. This is the corpus result reduced to a synthetic pair.
#[test]
fn whole_state_merge_is_commutative_for_the_671_shape() {
    let (_owner, x, y, p) = two_peer_671_shape(20, 5);
    let xy = merged(&x, &y, &p);
    let yx = merged(&y, &x, &p);
    assert_eq!(
        ser(&xy),
        ser(&yx),
        "merge(X,Y) != merge(Y,X)\n  xy dms {:?}\n  yx dms {:?}\n  xy members {} / yx members {}",
        held_offsets(&xy),
        held_offsets(&yx),
        xy.members.members.len(),
        yx.members.members.len()
    );
}

/// `merge(merge(X,Y),Z)` must byte-equal `merge(X,merge(Y,Z))`.
///
/// Associativity is a separate law from commutativity, and the corpus harnesses
/// originally exercised only the latter — `fdev verify-merge` reported
/// violations of both, and on the live corpus this branch takes associativity
/// from 30 failing ordered triples out of 120 to 0.
///
/// The shape matters and the obvious one does not work. All three peers must be
/// SATURATED at the cap with disjoint DM windows, so that the intermediate merge
/// publishes a different retention horizon depending on the bracketing:
///
/// * X holds 15 legitimate DMs plus the 5 doomed ones (with Q) as its NEWEST,
///   and does not know Q is banned.
/// * Y holds 20 legitimate DMs in a middle window, and carries the ban.
/// * Z holds 20 legitimate DMs in the newest window, and does not know the ban.
///
/// Pre-fix, `(X.Y).Z` and `X.(Y.Z)` disagree because X's doomed DMs occupy cap
/// slots in one bracketing and not the other, which shifts the horizon that
/// decides what the third peer is allowed to contribute. Reverting the whole
/// fix gives `left = [60..=79]` (20 DMs) against `right = [65..=79]` (15) —
/// five legitimate messages lost to the bracketing alone.
///
/// The discarded first shape, recorded exactly because a reviewer could not
/// reproduce it from a paraphrase: Z was built as
///
/// ```ignore
/// let mut z = x.clone();
/// z.bans = BansV1(vec![]);
/// z.direct_messages = DirectMessagesV1 {
///     messages: (0..3u64).map(|i| dm(&a, &q, &owner, 100 + i)).collect(),
///     purges: vec![],
/// };
/// ```
///
/// i.e. a lagging peer holding only three of the doomed DMs and no ban, with X's
/// members and configuration. That version passes with the ENTIRE fix reverted —
/// verified — because Z is not saturated, so its horizon is `Open` and the
/// bracketing cannot change what any peer is allowed to contribute. Saturation
/// of all three peers is the load-bearing part of the shape.
#[test]
fn whole_state_merge_is_associative_for_the_671_shape() {
    const CAP: usize = 20;
    let owner = Peer::new(1);
    let (a, b, c, d) = (Peer::new(10), Peer::new(11), Peer::new(12), Peer::new(13));
    let q = Peer::new(20);
    let p = params(&owner);

    let members = MembersV1 {
        members: vec![
            member(&a, &owner),
            member(&b, &owner),
            member(&c, &owner),
            member(&d, &owner),
            member(&q, &owner),
        ],
    };
    let minfo = MemberInfoV1 {
        member_info: vec![info(&a), info(&b), info(&c), info(&d), info(&q)],
    };
    let window = |from: &Peer, to: &Peer, base: u64, n: u64| -> Vec<AuthorizedDirectMessage> {
        (0..n).map(|i| dm(from, to, &owner, base + i)).collect()
    };
    let state = |dms: Vec<AuthorizedDirectMessage>, bans: Vec<AuthorizedUserBan>| ChatRoomStateV1 {
        configuration: config(&owner, Some(CAP)),
        members: members.clone(),
        member_info: minfo.clone(),
        bans: BansV1(bans),
        direct_messages: DirectMessagesV1 {
            messages: dms,
            purges: vec![],
        },
        ..Default::default()
    };

    // X: oldest window, saturated, its NEWEST five doomed. No ban knowledge.
    let mut x_dms = window(&a, &b, 0, 15);
    x_dms.extend(window(&a, &q, 100, 5));
    let x = state(x_dms, vec![]);
    // Y: middle window, saturated, carries the ban on Q.
    let y = state(
        window(&c, &d, 30, CAP as u64),
        vec![owner_ban(q.id, &owner)],
    );
    // Z: newest window, saturated, no ban knowledge.
    let z = state(window(&b, &a, 60, CAP as u64), vec![]);

    for (label, s) in [("X", &x), ("Y", &y), ("Z", &z)] {
        assert_eq!(
            s.direct_messages.messages.len(),
            CAP,
            "test premise: {label} must be saturated at the cap, or its horizon is Open \
             and the bracketing cannot matter"
        );
    }

    let left = merged(&merged(&x, &y, &p), &z, &p);
    let right = merged(&x, &merged(&y, &z, &p), &p);
    assert_eq!(
        ser(&left),
        ser(&right),
        "(X.Y).Z != X.(Y.Z)\n  left  dms {:?}\n  right dms {:?}",
        held_offsets(&left),
        held_offsets(&right)
    );
}

/// The `delta == None` arm of `DirectMessagesV1::apply_delta` must sweep before
/// the cap too.
///
/// Not reached by a PUT — `Contract::validate_state` calls only `verify`, never
/// `apply_delta`. It is reached from both `update_state` variants whenever the
/// top-level delta is present but its `direct_messages` sub-delta is `None`,
/// i.e. any update carrying only a room message, member, ban or member_info. A
/// PUT is how an unnormalised state gets IN; this arm is what converges it on
/// the first update after, which is precisely the path a re-keying release
/// depends on.
///
/// TRAP: a bare top-level `None` short-circuits the whole generated
/// `apply_delta` (the `#[composable]` macro wraps its body in
/// `if let Some(delta)`), so neither the field's `apply_delta` nor
/// `post_apply_cleanup` runs and the test passes vacuously. It must be driven
/// with `Some(ChatRoomStateV1Delta { direct_messages: None, .. })`.
#[test]
fn none_delta_path_sweeps_before_the_cap() {
    const CAP: usize = 20;
    const K: usize = 5;
    let owner = Peer::new(1);
    let (a, b, c, d) = (Peer::new(10), Peer::new(11), Peer::new(12), Peer::new(13));
    let ghost = Peer::new(30); // never a member at all
    let p = params(&owner);

    let legit: Vec<AuthorizedDirectMessage> = (0..(CAP - K) as u64)
        .map(|i| {
            if i % 2 == 0 {
                dm(&a, &b, &owner, i)
            } else {
                dm(&c, &d, &owner, i)
            }
        })
        .collect();
    let doomed: Vec<AuthorizedDirectMessage> = (0..K as u64)
        .map(|i| dm(&a, &ghost, &owner, 100 + i))
        .collect();
    // Extra legitimate DMs so the held set is OVER cap and the cap really binds.
    let extra: Vec<AuthorizedDirectMessage> =
        (0..K as u64).map(|i| dm(&b, &a, &owner, 50 + i)).collect();

    let mut state = ChatRoomStateV1 {
        configuration: config(&owner, Some(CAP)),
        members: MembersV1 {
            members: vec![
                member(&a, &owner),
                member(&b, &owner),
                member(&c, &owner),
                member(&d, &owner),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&a), info(&b), info(&c), info(&d)],
        },
        direct_messages: DirectMessagesV1 {
            messages: legit.iter().chain(&extra).chain(&doomed).cloned().collect(),
            purges: vec![],
        },
        ..Default::default()
    };
    assert_eq!(
        state.direct_messages.messages.len(),
        CAP + K,
        "test premise: the held set must start OVER cap, or the cap cannot bite"
    );

    let delta = ChatRoomStateV1Delta {
        member_info: Some(vec![info(&a)]),
        direct_messages: None,
        ..Default::default()
    };
    let parent = state.clone();
    state
        .apply_delta(&parent, &p, &Some(delta))
        .expect("a delta with no DM sub-delta must apply");

    assert_eq!(
        held_offsets(&state),
        (0..(CAP - K) as u64)
            .chain(50..(50 + K as u64))
            .collect::<Vec<u64>>(),
        "the None-delta arm must sweep the ghost-endpoint DMs BEFORE the cap, keeping \
         all {CAP} legitimate ones"
    );
}

/// A member anchored solely by a DM the GLOBAL TRIM discards must not survive
/// one cleanup pass and be pruned by the next.
///
/// This is the case that killed a rejected variant of the #671 fix, in which
/// `trim_to_global_cap` was moved into `post_apply_cleanup` after step 6: step 1
/// would then count DM participants from the UNTRIMMED set, so a member whose
/// only DM the trim is about to discard is exempted on pass 1 and pruned on
/// pass 2 — the #671 defect re-introduced one layer up. Two reviewers proposed
/// that reordering independently, so it is worth an explicit pin rather than
/// relying on the live corpus covering it incidentally.
///
/// This is A detector, not the only one. Measured, that mutant fails 17 tests
/// for TWO independent reasons: the idempotence break above, and the fact that
/// moving the trim out of `apply_delta` stops the DM field enforcing the global
/// cap at all for any caller that does not run cleanup — which is why fifteen
/// pre-existing field-level tests fail too. `post_apply_cleanup_stays_idempotent_under_the_global_cap`
/// catches the same idempotence class as this test does. This one is kept
/// because it is the sharper and more direct statement of the property, not
/// because it is unique.
///
/// The trim lives in `apply_delta`, not in cleanup, so the state has to be
/// driven through a real `apply_delta` — asserting cleanup on a hand-built
/// over-cap state would prove nothing, because cleanup never trims.
#[test]
fn cleanup_is_a_fixpoint_after_the_trim_discards_a_members_only_dm() {
    const CAP: usize = 6;
    let owner = Peer::new(1);
    let (a, b) = (Peer::new(10), Peer::new(11));
    // M's ONLY claim to membership is one DM with P, and it is the oldest in
    // the room, so the global trim is what removes it.
    let (m, peer) = (Peer::new(40), Peer::new(41));
    let p = params(&owner);

    let m_dm = dm(&m, &peer, &owner, 0);
    let busy: Vec<AuthorizedDirectMessage> = (0..CAP as u64)
        .map(|i| dm(&a, &b, &owner, 1000 + i))
        .collect();

    let mut state = ChatRoomStateV1 {
        configuration: config(&owner, Some(CAP)),
        members: MembersV1 {
            members: vec![
                member(&a, &owner),
                member(&b, &owner),
                member(&m, &owner),
                member(&peer, &owner),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&a), info(&b), info(&m), info(&peer)],
        },
        direct_messages: DirectMessagesV1 {
            messages: std::iter::once(m_dm.clone()).chain(busy).collect(),
            purges: vec![],
        },
        ..Default::default()
    };
    assert_eq!(
        state.direct_messages.messages.len(),
        CAP + 1,
        "test premise: over cap by exactly M's DM"
    );

    // Drive the real path: apply_delta trims, then post_apply_cleanup runs.
    let delta = ChatRoomStateV1Delta {
        member_info: Some(vec![info(&a)]),
        direct_messages: None,
        ..Default::default()
    };
    let parent = state.clone();
    state.apply_delta(&parent, &p, &Some(delta)).expect("apply");

    assert!(
        !held_offsets(&state).contains(&0),
        "test premise: the global trim must be what discards M's DM"
    );

    let after_apply = state.clone();
    state.post_apply_cleanup(&p).expect("second cleanup pass");
    assert_eq!(
        after_apply, state,
        "apply_delta must leave a cleanup fixpoint: a member anchored only by a DM the \
         trim discarded must already be gone, not pruned on the next pass"
    );
    assert!(
        !member_ids(&after_apply).contains(&m.id),
        "M is anchored by nothing once the trim drops its DM, so the FIRST pass must \
         already have pruned M"
    );
}

// ---------------------------------------------------------------------------
// freenet/river#675 — the deputy-banned endpoint.
//
// This began life as the reproduction attached to #675, when the apply-time
// sweep was membership-only and this case was a known-open residual. The
// ban-aware sweep closes it, so the reproduction becomes the regression test.
// ---------------------------------------------------------------------------

/// A DM doomed by a DEPUTY-issued ban must not take a cap slot either.
///
/// `MembersV1::apply_delta` is handed an EMPTY `MemberInfoV1`, so it can only
/// enforce OWNER and ANCESTOR ban authority; a deputy grant lives in
/// `member_info.deputies` and is invisible to it. A deputy-banned member is
/// therefore still in `parent_state.members` when the DM field applies. While
/// the apply-time sweep was membership-only, that member's DMs were still
/// ranked by `trim_to_global_cap` and only removed afterwards by cleanup step
/// 6 — the #671 data loss, on the flow the Official room's moderators actually
/// use. `enforced_ban_set_of` closes it by deriving the same enforced-ban set
/// step 0 will compute.
///
/// TRAP, and the reason the obvious version of this test passes vacuously: it
/// only reproduces when the deputy is a **NON-ancestor** of the banned member.
/// A deputy who invited the target holds ancestor authority independently, so
/// `remove_banned_members` removes the target during `MembersV1::apply_delta`
/// after all and nothing reaches the cap. Here Q is invited by the OWNER, so
/// D's only authority over Q is the owner's deputization. Do not "simplify"
/// that.
///
/// Mutation-verified: with the apply-time sweep reverted to membership-only,
/// this fails with `legitimate DMs at offsets [0, 1, 2, 3, 4] were evicted`.
#[test]
fn deputy_ban_must_not_let_a_doomed_dm_evict_a_legitimate_one() {
    const CAP: usize = 20;
    const K: usize = 5;
    assert!(MAX_DEPUTIES >= 1, "test premise: deputies must be possible");

    let owner = Peer::new(1);
    let (a, b, c) = (Peer::new(10), Peer::new(11), Peer::new(12));
    let d = Peer::new(13); // the deputy
    let q = Peer::new(20); // banned BY the deputy, invited by the owner

    let p = params(&owner);

    let legit: Vec<AuthorizedDirectMessage> = (0..(CAP - K) as u64)
        .map(|i| {
            if i % 2 == 0 {
                dm(&a, &b, &owner, i)
            } else {
                dm(&c, &d, &owner, i)
            }
        })
        .collect();
    let doomed: Vec<AuthorizedDirectMessage> = (0..K as u64)
        .map(|i| {
            if i % 2 == 0 {
                dm(&a, &q, &owner, 100 + i)
            } else {
                dm(&q, &a, &owner, 100 + i)
            }
        })
        .collect();

    let mut owner_info = MemberInfo::new_public(owner.id, 1, "owner".to_string());
    owner_info.deputies = vec![d.id];

    let mut receiver = ChatRoomStateV1 {
        configuration: config(&owner, Some(CAP)),
        members: MembersV1 {
            members: vec![
                member(&a, &owner),
                member(&b, &owner),
                member(&c, &owner),
                member(&d, &owner),
                member(&q, &owner),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&a),
                info(&b),
                info(&c),
                info(&d),
                AuthorizedMemberInfo::new(owner_info, &owner.sk),
                info(&q),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![
                join_msg(&a, owner.id),
                join_msg(&b, owner.id),
                join_msg(&c, owner.id),
                join_msg(&d, owner.id),
                join_msg(&q, owner.id),
            ],
            ..Default::default()
        },
        direct_messages: DirectMessagesV1 {
            messages: legit.iter().chain(&doomed).cloned().collect(),
            purges: vec![],
        },
        ..Default::default()
    };
    assert_eq!(
        receiver.direct_messages.messages.len(),
        CAP,
        "test premise: saturated at the cap"
    );

    let offered: Vec<AuthorizedDirectMessage> =
        (0..K as u64).map(|i| dm(&b, &a, &owner, 50 + i)).collect();
    let delta = ChatRoomStateV1Delta {
        bans: Some(vec![deputy_ban(q.id, &d, owner.id)]),
        direct_messages: Some(DirectMessagesDelta {
            new_messages: offered.clone(),
            advanced_purges: vec![],
        }),
        ..Default::default()
    };
    let parent = receiver.clone();
    receiver
        .apply_delta(&parent, &p, &Some(delta))
        .expect("delta must apply");

    assert!(
        !member_ids(&receiver).contains(&q.id),
        "test premise: the DEPUTY ban must actually be enforced by cleanup, or the \
         shape is wrong and this test proves nothing"
    );

    let surviving: HashSet<[u8; 64]> = receiver
        .direct_messages
        .messages
        .iter()
        .map(|m| m.sender_signature.to_bytes())
        .collect();
    let lost: Vec<u64> = legit
        .iter()
        .chain(&offered)
        .filter(|m| !surviving.contains(&m.sender_signature.to_bytes()))
        .map(|m| m.message.timestamp - BASE_TS)
        .collect();
    assert!(
        lost.is_empty(),
        "legitimate DMs at offsets {lost:?} were evicted by DMs a DEPUTY ban dooms; \
         held now = {:?}",
        held_offsets(&receiver)
    );
    for m in &doomed {
        assert!(
            !surviving.contains(&m.sender_signature.to_bytes()),
            "a deputy-banned endpoint's DM must not survive"
        );
    }
}
