//! Deputy ban authority tests (issue #410).
//!
//! A member may deputize another member to ban within the deputizing member's
//! invite subtree. The owner deputizing is the whole-room "global moderator"
//! case. Deputization lives in the member's own signed `MemberInfo.deputies`
//! (LWW by version), and ban ENFORCEMENT (the member cascade) is recomputed
//! from the converged (members + deputies + bans) state in
//! `post_apply_cleanup`, NOT in `verify` — so revoking a deputy retroactively
//! un-enforces their bans.

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use rand::rngs::OsRng;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::member_info::{
    AuthorizedMemberInfo, MemberInfo, MemberInfoV1, MAX_DEPUTIES,
};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::collections::HashSet;
use std::time::SystemTime;

struct Peer {
    sk: SigningKey,
    id: MemberId,
}

impl Peer {
    fn new() -> Self {
        let sk = SigningKey::generate(&mut OsRng);
        let id = sk.verifying_key().into();
        Self { sk, id }
    }
}

/// An `AuthorizedMember` for `who`, invited by `inviter` (whose signing key
/// must sign the membership).
fn member(
    who: &Peer,
    inviter_id: MemberId,
    inviter_sk: &SigningKey,
    owner_id: MemberId,
) -> AuthorizedMember {
    AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: inviter_id,
            member_vk: who.sk.verifying_key(),
        },
        inviter_sk,
    )
}

/// A self-signed `AuthorizedMemberInfo` carrying `deputies` at `version`.
fn info(who: &Peer, version: u32, deputies: Vec<MemberId>) -> AuthorizedMemberInfo {
    let mut mi = MemberInfo::new_public(who.id, version, "nick".to_string());
    mi.deputies = deputies;
    AuthorizedMemberInfo::new_with_member_key(mi, &who.sk)
}

/// A join-event message authored by `who`, so they survive the inactivity
/// prune in `post_apply_cleanup` (isolating ban behaviour from prune noise).
fn join(who: &Peer, owner_id: MemberId) -> AuthorizedMessageV1 {
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

fn ban(target: MemberId, banner: &Peer, owner_id: MemberId) -> AuthorizedUserBan {
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

fn params(owner: &Peer) -> ChatRoomParametersV1 {
    ChatRoomParametersV1 {
        owner: owner.sk.verifying_key(),
    }
}

fn config(owner: &Peer) -> AuthorizedConfigurationV1 {
    AuthorizedConfigurationV1::new(
        Configuration {
            max_members: 100,
            max_user_bans: 100,
            max_recent_messages: 1000,
            ..Default::default()
        },
        &owner.sk,
    )
}

fn member_ids(state: &ChatRoomStateV1) -> HashSet<MemberId> {
    state
        .members
        .members
        .iter()
        .map(|m| m.member.id())
        .collect()
}

/// Deputize A->B (B may be anyone), B bans T where T is in A's subtree -> T
/// (and their downstream) are removed.
#[test]
fn deputy_ban_within_subtree_is_enforced() {
    let owner = Peer::new();
    let a = Peer::new();
    let b = Peer::new();
    let t = Peer::new();
    let owner_id = owner.id;

    // owner -> A, owner -> B, A -> T
    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&a, owner_id, &owner.sk, owner_id),
                member(&b, owner_id, &owner.sk, owner_id),
                member(&t, a.id, &a.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&a, 1, vec![b.id]), // A deputizes B
                info(&b, 0, vec![]),
                info(&t, 0, vec![]),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&a, owner_id), join(&b, owner_id), join(&t, owner_id)],
            ..Default::default()
        },
        bans: BansV1(vec![ban(t.id, &b, owner_id)]),
        ..Default::default()
    };

    state.post_apply_cleanup(&params(&owner)).unwrap();

    let ids = member_ids(&state);
    assert!(
        !ids.contains(&t.id),
        "T (in A's subtree, banned by A's deputy B) must be removed"
    );
    assert!(ids.contains(&a.id) && ids.contains(&b.id), "A and B remain");
    state
        .verify(&state, &params(&owner))
        .expect("state must verify after enforcement");
}

/// B is A's deputy, but bans C who is NOT in A's subtree (C invited directly by
/// owner) -> the ban is inert, C stays.
#[test]
fn deputy_ban_outside_subtree_is_inert() {
    let owner = Peer::new();
    let a = Peer::new();
    let b = Peer::new();
    let c = Peer::new();
    let owner_id = owner.id;

    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&a, owner_id, &owner.sk, owner_id),
                member(&b, owner_id, &owner.sk, owner_id),
                member(&c, owner_id, &owner.sk, owner_id), // C is in the OWNER's subtree, not A's
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&a, 1, vec![b.id]),
                info(&b, 0, vec![]),
                info(&c, 0, vec![]),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&a, owner_id), join(&b, owner_id), join(&c, owner_id)],
            ..Default::default()
        },
        bans: BansV1(vec![ban(c.id, &b, owner_id)]),
        ..Default::default()
    };

    state.post_apply_cleanup(&params(&owner)).unwrap();

    assert!(
        member_ids(&state).contains(&c.id),
        "C is outside A's subtree; B's deputy authority does not reach C, ban is inert"
    );
    state
        .verify(&state, &params(&owner))
        .expect("state must verify");
}

/// Guardrail: a deputy cannot ban the member who deputized them.
#[test]
fn deputy_cannot_ban_their_deputizer() {
    let owner = Peer::new();
    let a = Peer::new();
    let b = Peer::new();
    let owner_id = owner.id;

    // A deputizes B, and B tries to ban A.
    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&a, owner_id, &owner.sk, owner_id),
                member(&b, a.id, &a.sk, owner_id), // B in A's subtree, but that's irrelevant
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&a, 1, vec![b.id]), info(&b, 0, vec![])],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&a, owner_id), join(&b, owner_id)],
            ..Default::default()
        },
        bans: BansV1(vec![ban(a.id, &b, owner_id)]),
        ..Default::default()
    };

    state.post_apply_cleanup(&params(&owner)).unwrap();

    assert!(
        member_ids(&state).contains(&a.id),
        "B cannot ban A because A currently deputizes B (guardrail)"
    );
}

/// Guardrail across subtrees: B is a deputy of both A1 and A2, where A2 is
/// inside A1's subtree. A1's scope would otherwise let B ban A2, but A2 also
/// deputizes B, so B cannot ban A2.
#[test]
fn deputy_cannot_ban_fellow_deputizer_across_subtrees() {
    let owner = Peer::new();
    let a1 = Peer::new();
    let a2 = Peer::new();
    let b = Peer::new();
    let owner_id = owner.id;

    // owner -> A1 -> A2, and owner -> B
    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&a1, owner_id, &owner.sk, owner_id),
                member(&a2, a1.id, &a1.sk, owner_id),
                member(&b, owner_id, &owner.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&a1, 1, vec![b.id]), // A1 deputizes B (scope: A1's subtree, incl. A2)
                info(&a2, 1, vec![b.id]), // A2 also deputizes B
                info(&b, 0, vec![]),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&a1, owner_id), join(&a2, owner_id), join(&b, owner_id)],
            ..Default::default()
        },
        bans: BansV1(vec![ban(a2.id, &b, owner_id)]),
        ..Default::default()
    };

    state.post_apply_cleanup(&params(&owner)).unwrap();

    assert!(
        member_ids(&state).contains(&a2.id),
        "B cannot ban A2 (a fellow deputizer) even via A1's subtree scope"
    );
}

/// Owner-deputized global moderator can ban any member — including their own
/// inviter (an acknowledged consequence of appointing someone who outranks the
/// invite tree).
#[test]
fn owner_deputized_global_mod_can_ban_anyone_including_inviter() {
    let owner = Peer::new();
    let inviter = Peer::new();
    let mod_peer = Peer::new();
    let victim = Peer::new();
    let owner_id = owner.id;

    // owner -> inviter -> mod, owner -> victim. Owner deputizes `mod`.
    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&inviter, owner_id, &owner.sk, owner_id),
                member(&mod_peer, inviter.id, &inviter.sk, owner_id),
                member(&victim, owner_id, &owner.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&owner, 1, vec![mod_peer.id]), // owner deputizes mod (global)
                info(&inviter, 0, vec![]),
                info(&mod_peer, 0, vec![]),
                info(&victim, 0, vec![]),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![
                join(&inviter, owner_id),
                join(&mod_peer, owner_id),
                join(&victim, owner_id),
            ],
            ..Default::default()
        },
        // mod bans a random member AND their own inviter.
        bans: BansV1(vec![
            ban(victim.id, &mod_peer, owner_id),
            ban(inviter.id, &mod_peer, owner_id),
        ]),
        ..Default::default()
    };

    state.post_apply_cleanup(&params(&owner)).unwrap();

    let ids = member_ids(&state);
    assert!(!ids.contains(&victim.id), "global mod can ban any member");
    assert!(
        !ids.contains(&inviter.id) && !ids.contains(&mod_peer.id),
        "global mod can ban their own inviter (cascades to mod, who is downstream)"
    );
}

/// A deputy banning an internal subtree root removes the whole branch beneath.
#[test]
fn deputy_ban_of_subtree_root_cascades() {
    let owner = Peer::new();
    let a = Peer::new();
    let b = Peer::new(); // deputy
    let r = Peer::new(); // subtree root inside A
    let child = Peer::new(); // r's child
    let owner_id = owner.id;

    // owner -> A -> R -> child ; owner -> B (deputy of A)
    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&a, owner_id, &owner.sk, owner_id),
                member(&b, owner_id, &owner.sk, owner_id),
                member(&r, a.id, &a.sk, owner_id),
                member(&child, r.id, &r.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&a, 1, vec![b.id]),
                info(&b, 0, vec![]),
                info(&r, 0, vec![]),
                info(&child, 0, vec![]),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![
                join(&a, owner_id),
                join(&b, owner_id),
                join(&r, owner_id),
                join(&child, owner_id),
            ],
            ..Default::default()
        },
        bans: BansV1(vec![ban(r.id, &b, owner_id)]),
        ..Default::default()
    };

    state.post_apply_cleanup(&params(&owner)).unwrap();

    let ids = member_ids(&state);
    assert!(!ids.contains(&r.id), "banned subtree root removed");
    assert!(
        !ids.contains(&child.id),
        "root's downstream child cascaded out"
    );
    assert!(ids.contains(&a.id) && ids.contains(&b.id), "A and B remain");
}

/// Revoking a deputy retroactively un-enforces their bans: a previously-banned
/// member who is still present (or re-added) stays. Also proves a stale replay
/// of the pre-revoke member_info loses to the higher-version revoke (LWW).
#[test]
fn revoking_deputy_unenforces_and_lww_defeats_stale_replay() {
    let owner = Peer::new();
    let a = Peer::new();
    let b = Peer::new();
    let t = Peer::new();
    let owner_id = owner.id;
    let p = params(&owner);

    let members = MembersV1 {
        members: vec![
            member(&a, owner_id, &owner.sk, owner_id),
            member(&b, owner_id, &owner.sk, owner_id),
            member(&t, a.id, &a.sk, owner_id),
        ],
    };
    let msgs = MessagesV1 {
        messages: vec![join(&a, owner_id), join(&b, owner_id), join(&t, owner_id)],
        ..Default::default()
    };

    // Converged state AFTER revoke: A's member_info is v2 with empty deputies,
    // the ban by B of T is still in the (add-only) bans list, and T is present.
    let mut revoked = ChatRoomStateV1 {
        configuration: config(&owner),
        members: members.clone(),
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&a, 2, vec![]),
                info(&b, 0, vec![]),
                info(&t, 0, vec![]),
            ],
        },
        recent_messages: msgs.clone(),
        bans: BansV1(vec![ban(t.id, &b, owner_id)]),
        ..Default::default()
    };
    revoked.post_apply_cleanup(&p).unwrap();
    assert!(
        member_ids(&revoked).contains(&t.id),
        "after revoke, B's ban of T is inert; T must remain / be able to rejoin"
    );
    revoked
        .verify(&revoked, &p)
        .expect("revoked state must verify");

    // Stale replay: a peer re-broadcasts A's OLD member_info (v1, deputies=[B]).
    // LWW-by-version keeps v2 (empty), so the ban stays inert and T stays.
    let stale_delta = river_core::room_state::ChatRoomStateV1Delta {
        member_info: Some(vec![info(&a, 1, vec![b.id])]),
        ..Default::default()
    };
    let before = revoked.clone();
    revoked
        .apply_delta(&before, &p, &Some(stale_delta))
        .expect("stale replay applies");
    assert_eq!(
        revoked.member_info.deputies_of(a.id).to_vec(),
        Vec::<MemberId>::new(),
        "stale v1 deputies must lose to the higher-version v2 revoke (LWW)"
    );
    assert!(
        member_ids(&revoked).contains(&t.id),
        "T stays present: the revoke was not resurrected by the stale replay"
    );
}

/// Ban-vs-revoke applied in BOTH orders converge to the same state via
/// anti-entropy (full-state merge is the CRDT ground truth). The converged
/// answer recomputes enforcement over the merged (grow-only bans, LWW
/// deputies, unioned members) state, so both peers agree T is present.
#[test]
fn ban_and_revoke_converge_in_both_orders_via_merge() {
    let owner = Peer::new();
    let a = Peer::new();
    let b = Peer::new();
    let t = Peer::new();
    let owner_id = owner.id;
    let p = params(&owner);

    // Baseline: A deputizes B (v1), T is in A's subtree, all active.
    let baseline = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&a, owner_id, &owner.sk, owner_id),
                member(&b, owner_id, &owner.sk, owner_id),
                member(&t, a.id, &a.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&a, 1, vec![b.id]),
                info(&b, 0, vec![]),
                info(&t, 0, vec![]),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&a, owner_id), join(&b, owner_id), join(&t, owner_id)],
            ..Default::default()
        },
        ..Default::default()
    };

    let ban_delta = river_core::room_state::ChatRoomStateV1Delta {
        bans: Some(vec![ban(t.id, &b, owner_id)]),
        ..Default::default()
    };
    let revoke_delta = river_core::room_state::ChatRoomStateV1Delta {
        member_info: Some(vec![info(&a, 2, vec![])]),
        ..Default::default()
    };

    // Peer 1: ban then revoke (T is removed by the ban, then the revoke lands).
    let mut peer1 = baseline.clone();
    peer1
        .apply_delta(&baseline.clone(), &p, &Some(ban_delta.clone()))
        .unwrap();
    let mid1 = peer1.clone();
    peer1
        .apply_delta(&mid1, &p, &Some(revoke_delta.clone()))
        .unwrap();

    // Peer 2: revoke then ban (ban is already inert when it lands).
    let mut peer2 = baseline.clone();
    peer2
        .apply_delta(&baseline.clone(), &p, &Some(revoke_delta.clone()))
        .unwrap();
    let mid2 = peer2.clone();
    peer2
        .apply_delta(&mid2, &p, &Some(ban_delta.clone()))
        .unwrap();

    // Anti-entropy: mutually merge full states (the CRDT ground truth).
    let snap1 = peer1.clone();
    let snap2 = peer2.clone();
    peer1.merge(&snap1, &p, &snap2).unwrap();
    peer2.merge(&snap2, &p, &snap1).unwrap();

    assert_eq!(
        member_ids(&peer1),
        member_ids(&peer2),
        "both peers converge to the same member set after anti-entropy"
    );
    assert!(
        member_ids(&peer1).contains(&t.id),
        "converged answer: the ban is inert (deputy revoked), so T is present"
    );
    peer1
        .verify(&peer1, &p)
        .expect("peer1 converged state verifies");
    peer2
        .verify(&peer2, &p)
        .expect("peer2 converged state verifies");
}

/// A `MemberInfo` listing more than `MAX_DEPUTIES` deputies is rejected by
/// `MemberInfoV1::verify` (state-bloat guard).
#[test]
fn too_many_deputies_is_rejected() {
    let owner = Peer::new();
    let a = Peer::new();
    let owner_id = owner.id;

    let too_many: Vec<MemberId> = (0..=MAX_DEPUTIES).map(|_| Peer::new().id).collect();
    assert!(too_many.len() > MAX_DEPUTIES);

    let state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![member(&a, owner_id, &owner.sk, owner_id)],
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&a, 1, too_many)],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&a, owner_id)],
            ..Default::default()
        },
        ..Default::default()
    };

    let err = state
        .verify(&state, &params(&owner))
        .expect_err("verify must reject an over-long deputy list");
    assert!(
        err.contains("deputies"),
        "error should mention deputies: {err}"
    );
}

// ===================================================================
// Self-immunization + reorder guardrail tests (review round 1, #410)
// ===================================================================

/// A genuine strict ancestor keeps ABSOLUTE ban authority even if the target
/// tries to "self-immunize" by listing the ancestor in their own deputies.
#[test]
fn strict_ancestor_immune_to_self_immunization() {
    let owner = Peer::new();
    let a = Peer::new();
    let t = Peer::new();
    let owner_id = owner.id;

    // owner -> A -> T, and T self-immunizes: T.deputies = [A].
    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&a, owner_id, &owner.sk, owner_id),
                member(&t, a.id, &a.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&a, 0, vec![]), info(&t, 1, vec![a.id])],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&a, owner_id), join(&t, owner_id)],
            ..Default::default()
        },
        bans: BansV1(vec![ban(t.id, &a, owner_id)]),
        ..Default::default()
    };

    state.post_apply_cleanup(&params(&owner)).unwrap();
    assert!(
        !member_ids(&state).contains(&t.id),
        "A is a genuine ancestor of T; T listing A in deputies must NOT strip A's authority"
    );
}

/// An owner-appointed global moderator keeps ABSOLUTE authority even if the
/// spammer self-immunizes by listing the mod in their own deputies.
#[test]
fn global_mod_immune_to_self_immunization() {
    let owner = Peer::new();
    let b = Peer::new(); // global mod
    let s = Peer::new(); // spammer
    let owner_id = owner.id;

    // owner deputizes B (global). owner -> S (spammer), S.deputies = [B].
    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&b, owner_id, &owner.sk, owner_id),
                member(&s, owner_id, &owner.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&owner, 1, vec![b.id]), // owner deputizes B globally
                info(&b, 0, vec![]),
                info(&s, 1, vec![b.id]), // S self-immunizes
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&b, owner_id), join(&s, owner_id)],
            ..Default::default()
        },
        bans: BansV1(vec![ban(s.id, &b, owner_id)]),
        ..Default::default()
    };

    state.post_apply_cleanup(&params(&owner)).unwrap();
    assert!(
        !member_ids(&state).contains(&s.id),
        "owner-appointed global mod B must be able to ban spammer S despite S self-immunizing"
    );
    assert!(member_ids(&state).contains(&b.id), "B remains");
}

/// The fellow-deputizer guardrail still holds (B cannot ban A2 who deputizes B),
/// while B CAN still ban another member of A1's subtree (A3) via A1's grant.
#[test]
fn fellow_deputizer_protected_but_other_subtree_member_bannable() {
    let owner = Peer::new();
    let a1 = Peer::new();
    let a2 = Peer::new();
    let a3 = Peer::new();
    let b = Peer::new();
    let owner_id = owner.id;

    // owner -> A1 -> {A2, A3}; owner -> B. A1.deputies=[B], A2.deputies=[B].
    let base_members = vec![
        member(&a1, owner_id, &owner.sk, owner_id),
        member(&a2, a1.id, &a1.sk, owner_id),
        member(&a3, a1.id, &a1.sk, owner_id),
        member(&b, owner_id, &owner.sk, owner_id),
    ];
    let base_info = vec![
        info(&a1, 1, vec![b.id]),
        info(&a2, 1, vec![b.id]),
        info(&a3, 0, vec![]),
        info(&b, 0, vec![]),
    ];
    let base_msgs = vec![
        join(&a1, owner_id),
        join(&a2, owner_id),
        join(&a3, owner_id),
        join(&b, owner_id),
    ];

    // B bans A2 (a fellow deputizer) -> guardrail denies, A2 stays.
    let mut s1 = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: base_members.clone(),
        },
        member_info: MemberInfoV1 {
            member_info: base_info.clone(),
        },
        recent_messages: MessagesV1 {
            messages: base_msgs.clone(),
            ..Default::default()
        },
        bans: BansV1(vec![ban(a2.id, &b, owner_id)]),
        ..Default::default()
    };
    s1.post_apply_cleanup(&params(&owner)).unwrap();
    assert!(
        member_ids(&s1).contains(&a2.id),
        "B cannot ban A2 (A2 deputizes B — fellow-deputizer guardrail)"
    );

    // B bans A3 (a non-deputizer in A1's subtree) -> authorized via A1's grant.
    let mut s2 = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: base_members,
        },
        member_info: MemberInfoV1 {
            member_info: base_info,
        },
        recent_messages: MessagesV1 {
            messages: base_msgs,
            ..Default::default()
        },
        bans: BansV1(vec![ban(a3.id, &b, owner_id)]),
        ..Default::default()
    };
    s2.post_apply_cleanup(&params(&owner)).unwrap();
    assert!(
        !member_ids(&s2).contains(&a3.id),
        "B CAN ban A3 (another member of A1's subtree) via A1's deputy grant"
    );
}

/// No transitive re-deputization: A deputizes B, B deputizes C in B's OWN
/// MemberInfo — C gets NO authority over A's subtree.
#[test]
fn no_transitive_re_deputization() {
    let owner = Peer::new();
    let a = Peer::new();
    let b = Peer::new();
    let c = Peer::new();
    let t = Peer::new();
    let owner_id = owner.id;

    // owner -> A -> T ; owner -> B ; owner -> C. A.deputies=[B], B.deputies=[C].
    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&a, owner_id, &owner.sk, owner_id),
                member(&b, owner_id, &owner.sk, owner_id),
                member(&c, owner_id, &owner.sk, owner_id),
                member(&t, a.id, &a.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&a, 1, vec![b.id]), // A deputizes B
                info(&b, 1, vec![c.id]), // B (a deputy) tries to sub-deputize C
                info(&c, 0, vec![]),
                info(&t, 0, vec![]),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![
                join(&a, owner_id),
                join(&b, owner_id),
                join(&c, owner_id),
                join(&t, owner_id),
            ],
            ..Default::default()
        },
        // C tries to ban T (in A's subtree). C only holds authority via B, who
        // is not an ancestor of T — so this must be inert.
        bans: BansV1(vec![ban(t.id, &c, owner_id)]),
        ..Default::default()
    };

    state.post_apply_cleanup(&params(&owner)).unwrap();
    assert!(
        member_ids(&state).contains(&t.id),
        "C's authority (only via deputy B) does NOT reach into A's subtree — no transitive re-deputization"
    );
}

/// The owner is never a valid ban target: an "authorized" ban of the owner
/// would cascade `get_downstream_members(owner)` = the whole room. Even an
/// owner-appointed global mod cannot ban the owner.
#[test]
fn deputy_cannot_ban_owner_and_room_survives() {
    let owner = Peer::new();
    let b = Peer::new(); // global mod
    let x = Peer::new();
    let y = Peer::new();
    let owner_id = owner.id;

    // owner deputizes B globally. owner -> B, owner -> X -> Y.
    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&b, owner_id, &owner.sk, owner_id),
                member(&x, owner_id, &owner.sk, owner_id),
                member(&y, x.id, &x.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&owner, 1, vec![b.id]),
                info(&b, 0, vec![]),
                info(&x, 0, vec![]),
                info(&y, 0, vec![]),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&b, owner_id), join(&x, owner_id), join(&y, owner_id)],
            ..Default::default()
        },
        // Global mod B attempts to ban the OWNER.
        bans: BansV1(vec![ban(owner_id, &b, owner_id)]),
        ..Default::default()
    };

    state.post_apply_cleanup(&params(&owner)).unwrap();
    let ids = member_ids(&state);
    assert!(
        ids.contains(&b.id) && ids.contains(&x.id) && ids.contains(&y.id),
        "banning the owner must be INERT — the whole room must survive, got {ids:?}"
    );
}

/// An over-cap deputy record (> MAX_DEPUTIES) is SKIPPED by
/// `MemberInfoV1::apply_delta` (not stored) and the resulting state stays
/// valid — so a self-signed 65-deputy record can neither enter state via a
/// delta nor block full-state validation. (#410, review round 1)
#[test]
fn over_cap_deputies_delta_is_skipped_and_state_stays_valid() {
    let owner = Peer::new();
    let a = Peer::new();
    let owner_id = owner.id;
    let p = params(&owner);

    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![member(&a, owner_id, &owner.sk, owner_id)],
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&a, 0, vec![])],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&a, owner_id)],
            ..Default::default()
        },
        ..Default::default()
    };

    // A self-signs a record with MAX_DEPUTIES + 1 deputies at version 1.
    let too_many: Vec<MemberId> = (0..=MAX_DEPUTIES).map(|_| Peer::new().id).collect();
    assert!(too_many.len() > MAX_DEPUTIES);
    let delta = river_core::room_state::ChatRoomStateV1Delta {
        member_info: Some(vec![info(&a, 1, too_many)]),
        ..Default::default()
    };

    let before = state.clone();
    state
        .apply_delta(&before, &p, &Some(delta))
        .expect("apply_delta must not error — the over-cap entry is skipped, not rejected");

    assert!(
        state.member_info.deputies_of(a.id).is_empty(),
        "the over-cap deputy record must be skipped, leaving A's original (empty) deputies"
    );
    state
        .verify(&state, &p)
        .expect("state stays valid because the over-cap record never entered");
}

// ===================================================================
// Forged/inert-ban un-ban DoS defense (review round 1, #410)
// ===================================================================

fn ban_at(target: MemberId, banner: &Peer, owner_id: MemberId, secs: u64) -> AuthorizedUserBan {
    AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
            banned_user: target,
        },
        banner.id,
        &banner.sk,
    )
}

fn config_max_bans(owner: &Peer, max_user_bans: usize) -> AuthorizedConfigurationV1 {
    AuthorizedConfigurationV1::new(
        Configuration {
            max_members: 100,
            max_user_bans,
            max_recent_messages: 1000,
            ..Default::default()
        },
        &owner.sk,
    )
}

// ===================================================================
// Round-3: ban valid only while banner is owner/current member (#411)
// ===================================================================

/// A ban whose banner is a NON-member (a forged/random id) is SWEPT from state
/// entirely by post_apply_cleanup — not merely evicted under the cap. Real
/// owner/member bans survive and still enforce.
#[test]
fn forged_non_member_banner_bans_are_swept() {
    let owner = Peer::new();
    let a = Peer::new();
    let s1 = Peer::new();
    let s2 = Peer::new();
    let owner_id = owner.id;
    let p = params(&owner);

    let real1 = ban_at(s1.id, &owner, owner_id, 1);
    let real2 = ban_at(s2.id, &owner, owner_id, 2);
    // Five forged bans by fresh NON-member keys targeting present member A.
    let forged: Vec<AuthorizedUserBan> = (0..5)
        .map(|i| ban_at(a.id, &Peer::new(), owner_id, 100 + i))
        .collect();
    let mut all = forged;
    all.push(real1.clone());
    all.push(real2.clone());

    let mut state = ChatRoomStateV1 {
        configuration: config_max_bans(&owner, 4),
        members: MembersV1 {
            members: vec![
                member(&a, owner_id, &owner.sk, owner_id),
                member(&s1, owner_id, &owner.sk, owner_id),
                member(&s2, owner_id, &owner.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&a, 0, vec![]),
                info(&s1, 0, vec![]),
                info(&s2, 0, vec![]),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&a, owner_id), join(&s1, owner_id), join(&s2, owner_id)],
            ..Default::default()
        },
        bans: BansV1(all),
        ..Default::default()
    };

    state.post_apply_cleanup(&p).unwrap();

    assert_eq!(
        state.bans.0.len(),
        2,
        "forged non-member-banner bans are swept; only the 2 real owner bans remain"
    );
    assert!(
        state.bans.0.contains(&real1) && state.bans.0.contains(&real2),
        "real bans survive"
    );
    let ids = member_ids(&state);
    assert!(
        !ids.contains(&s1.id) && !ids.contains(&s2.id),
        "real bans still enforce"
    );
    assert!(ids.contains(&a.id), "A (forged targets) stays a member");
    state.verify(&state, &p).expect("state verifies");
}

/// A forged ban BY a stale/pruned deputy id (the deputy was removed from the
/// members list but still appears in a `deputies` list) grants nothing and is
/// swept — the victim survives. Closes the round-2 gap where a non-member
/// deputy id was honored as an authorized banner.
#[test]
fn stale_deputy_forged_ban_is_swept_and_not_enforcing() {
    let owner = Peer::new();
    let a = Peer::new();
    let b = Peer::new(); // deputy of A, but NOT a current member
    let v = Peer::new(); // victim, in A's subtree
    let owner_id = owner.id;

    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&a, owner_id, &owner.sk, owner_id),
                member(&v, a.id, &a.sk, owner_id),
            ], // B is intentionally NOT a member
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&a, 1, vec![b.id]), info(&v, 0, vec![])], // A deputizes the stale id B
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&a, owner_id), join(&v, owner_id)],
            ..Default::default()
        },
        // Forged ban BY the stale deputy id B (signed with B's key; B is not a member).
        bans: BansV1(vec![ban(v.id, &b, owner_id)]),
        ..Default::default()
    };

    state.post_apply_cleanup(&params(&owner)).unwrap();

    assert!(
        member_ids(&state).contains(&v.id),
        "victim V survives a forged ban by a stale non-member deputy id"
    );
    assert!(
        state.bans.0.is_empty(),
        "the forged non-member-banner ban is swept from state"
    );
    state
        .verify(&state, &params(&owner))
        .expect("state verifies");
}

/// A ban with a GARBAGE signature from a PRESENT member is rejected by `verify`
/// (the banner's key is available, so the signature is checked).
#[test]
fn garbage_sig_ban_from_present_member_rejected_by_verify() {
    let owner = Peer::new();
    let m = Peer::new();
    let t = Peer::new();
    let owner_id = owner.id;

    let state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&m, owner_id, &owner.sk, owner_id),
                member(&t, m.id, &m.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&m, 0, vec![]), info(&t, 0, vec![])],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&m, owner_id), join(&t, owner_id)],
            ..Default::default()
        },
        bans: BansV1(vec![AuthorizedUserBan::with_signature(
            UserBan {
                owner_member_id: owner_id,
                banned_at: SystemTime::now(),
                banned_user: t.id,
            },
            m.id,
            ed25519_dalek::Signature::from_bytes(&[0u8; 64]),
        )]),
        ..Default::default()
    };

    let err = state
        .verify(&state, &params(&owner))
        .expect_err("garbage-sig ban from a present member must be rejected");
    assert!(
        err.to_lowercase().contains("signature") || err.contains("Invalid ban"),
        "unexpected error: {err}"
    );
}

/// Inert bans by CURRENT-MEMBER banners are not swept (banner is a member) but
/// are evicted BEFORE enforcing bans when over the cap.
#[test]
fn inert_member_bans_evicted_before_enforcing_bans() {
    let owner = Peer::new();
    let a = Peer::new();
    let s1 = Peer::new();
    let s2 = Peer::new();
    let mods: Vec<Peer> = (0..5).map(|_| Peer::new()).collect();
    let owner_id = owner.id;
    let p = params(&owner);

    let mut members = vec![
        member(&a, owner_id, &owner.sk, owner_id),
        member(&s1, owner_id, &owner.sk, owner_id),
        member(&s2, owner_id, &owner.sk, owner_id),
    ];
    let mut infos = vec![
        info(&a, 0, vec![]),
        info(&s1, 0, vec![]),
        info(&s2, 0, vec![]),
    ];
    for mm in &mods {
        members.push(member(mm, owner_id, &owner.sk, owner_id));
        infos.push(info(mm, 0, vec![]));
    }

    // Real enforcing owner bans (oldest).
    let real1 = ban_at(s1.id, &owner, owner_id, 1);
    let real2 = ban_at(s2.id, &owner, owner_id, 2);
    // Inert bans by MEMBER banners (each mod bans A, outside their authority).
    let inert: Vec<AuthorizedUserBan> = mods
        .iter()
        .enumerate()
        .map(|(i, mm)| ban_at(a.id, mm, owner_id, 100 + i as u64))
        .collect();

    let mut all = inert;
    all.push(real1.clone());
    all.push(real2.clone());

    let mut state = ChatRoomStateV1 {
        configuration: config_max_bans(&owner, 4),
        members: MembersV1 { members },
        member_info: MemberInfoV1 { member_info: infos },
        recent_messages: MessagesV1 {
            messages: vec![join(&a, owner_id)],
            ..Default::default()
        },
        bans: BansV1(all),
        ..Default::default()
    };

    state.post_apply_cleanup(&p).unwrap();

    assert_eq!(state.bans.0.len(), 4, "capped to max_user_bans");
    assert!(
        state.bans.0.contains(&real1) && state.bans.0.contains(&real2),
        "the enforcing owner bans survive the inert member-ban flood"
    );
    let ids = member_ids(&state);
    assert!(
        !ids.contains(&s1.id) && !ids.contains(&s2.id),
        "enforcing bans still enforce"
    );
    assert!(
        ids.contains(&a.id),
        "A stays (inert bans do not remove them)"
    );
    state.verify(&state, &p).expect("state verifies");
}

/// A member is exempt from inactivity-prune while they hold a retained ban;
/// once their bans are gone they become prunable again.
#[test]
fn banner_prunable_once_their_bans_are_gone() {
    let owner = Peer::new();
    let m = Peer::new();
    let c = Peer::new();
    let owner_id = owner.id;
    let p = params(&owner);

    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![member(&m, owner_id, &owner.sk, owner_id)],
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&m, 0, vec![])],
        },
        // M has NO messages; only being a banner keeps them present.
        bans: BansV1(vec![ban(c.id, &m, owner_id)]),
        ..Default::default()
    };

    state.post_apply_cleanup(&p).unwrap();
    assert!(
        member_ids(&state).contains(&m.id),
        "M is exempt from prune while a banner"
    );
    assert_eq!(
        state.bans.0.len(),
        1,
        "M's ban is retained (banner is a current member)"
    );

    // Remove M's ban; M is now a plain inactive member.
    state.bans.0.clear();
    state.post_apply_cleanup(&p).unwrap();
    assert!(
        !member_ids(&state).contains(&m.id),
        "once M's bans are gone, M is prunable again"
    );
}

/// Convergence: inert member-ban eviction is deterministic across delta order
/// and anti-entropy merge (both peers reach identical members + bans).
#[test]
fn inert_member_ban_eviction_converges_across_delta_order() {
    let owner = Peer::new();
    let a = Peer::new();
    let s1 = Peer::new();
    let m1 = Peer::new();
    let m2 = Peer::new();
    let m3 = Peer::new();
    let owner_id = owner.id;
    let p = params(&owner);

    let baseline = ChatRoomStateV1 {
        configuration: config_max_bans(&owner, 3),
        members: MembersV1 {
            members: vec![
                member(&a, owner_id, &owner.sk, owner_id),
                member(&s1, owner_id, &owner.sk, owner_id),
                member(&m1, owner_id, &owner.sk, owner_id),
                member(&m2, owner_id, &owner.sk, owner_id),
                member(&m3, owner_id, &owner.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&a, 0, vec![]),
                info(&s1, 0, vec![]),
                info(&m1, 0, vec![]),
                info(&m2, 0, vec![]),
                info(&m3, 0, vec![]),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![
                join(&a, owner_id),
                join(&s1, owner_id),
                join(&m1, owner_id),
                join(&m2, owner_id),
                join(&m3, owner_id),
            ],
            ..Default::default()
        },
        ..Default::default()
    };

    let real = ban_at(s1.id, &owner, owner_id, 1); // enforcing, oldest
                                                   // Three inert bans by MEMBER banners (delta size 3 == max_user_bans, allowed).
    let inert = vec![
        ban_at(a.id, &m1, owner_id, 100),
        ban_at(a.id, &m2, owner_id, 101),
        ban_at(a.id, &m3, owner_id, 102),
    ];
    let real_delta = river_core::room_state::ChatRoomStateV1Delta {
        bans: Some(vec![real.clone()]),
        ..Default::default()
    };
    let inert_delta = river_core::room_state::ChatRoomStateV1Delta {
        bans: Some(inert),
        ..Default::default()
    };

    // Peer 1: inert first, then real.
    let mut peer1 = baseline.clone();
    peer1
        .apply_delta(&baseline.clone(), &p, &Some(inert_delta.clone()))
        .unwrap();
    let m = peer1.clone();
    peer1
        .apply_delta(&m, &p, &Some(real_delta.clone()))
        .unwrap();

    // Peer 2: real first, then inert.
    let mut peer2 = baseline.clone();
    peer2
        .apply_delta(&baseline.clone(), &p, &Some(real_delta))
        .unwrap();
    let m = peer2.clone();
    peer2.apply_delta(&m, &p, &Some(inert_delta)).unwrap();

    // Anti-entropy merge.
    let snap1 = peer1.clone();
    let snap2 = peer2.clone();
    peer1.merge(&snap1, &p, &snap2).unwrap();
    peer2.merge(&snap2, &p, &snap1).unwrap();

    assert_eq!(
        peer1.bans, peer2.bans,
        "capped ban sets converge across delta order"
    );
    assert_eq!(
        peer1.members, peer2.members,
        "member sets converge across delta order"
    );
    assert!(
        peer1.bans.0.contains(&real),
        "the real enforcing ban survives on both peers"
    );
    assert!(
        !member_ids(&peer1).contains(&s1.id),
        "S1 stays banned after convergence"
    );
    peer1.verify(&peer1, &p).expect("converged peer1 verifies");
    peer2.verify(&peer2, &p).expect("converged peer2 verifies");
}

// ===================================================================
// Migration compatibility (Ian's hard constraint, #411)
// ===================================================================

/// Re-PUTting an OLD-generation room state to the NEW contract runs ONLY
/// `verify` (validate_state), NOT post_apply_cleanup. The new `verify` MUST
/// accept an old state — otherwise the Official room's migration PUT is refused
/// and the room strands EMPTY. Old states legitimately contain (a) `member_info`
/// WITHOUT the `deputies` field (old serialization) and (b) bans by NON-member
/// (pruned) banners that the old code sig-skipped and accepted. The round-3
/// "a ban is only valid while its banner is a current member" rule is a
/// post_apply_cleanup REMOVAL, never a `verify` rejection.
///
/// This test FAILS if someone (a) turns the banner-membership check into a
/// `verify` rejection, or (b) drops `skip_serializing_if` on `deputies` (old
/// member_info signatures would then stop verifying).
#[test]
fn new_verify_accepts_legacy_state_with_nonmember_banner_bans() {
    use river_core::room_state::privacy::SealedBytes;

    let owner = Peer::new();
    let a = Peer::new();
    let b = Peer::new();
    // A banner that was a member in the old room but has since been pruned for
    // inactivity: NOT in the members list now, and NOT itself banned.
    let pruned_banner = Peer::new();
    let owner_id = owner.id;
    let p = params(&owner);

    // Build an OLD-style AuthorizedMemberInfo: sign over the pre-deputy 3-field
    // ciborium bytes (what an old client actually signed), then wrap the new
    // (empty-deputies) struct with that signature. Verifies iff empty `deputies`
    // serialize byte-identically to the old record.
    let old_info = |who: &Peer| -> AuthorizedMemberInfo {
        #[derive(serde::Serialize)]
        struct OldMemberInfo {
            member_id: MemberId,
            version: u32,
            preferred_nickname: SealedBytes,
        }
        let nick = SealedBytes::public(b"nick".to_vec());
        let old = OldMemberInfo {
            member_id: who.id,
            version: 0,
            preferred_nickname: nick.clone(),
        };
        let sig = river_core::util::sign_struct(&old, &who.sk);
        let new_mi = MemberInfo {
            member_id: who.id,
            version: 0,
            preferred_nickname: nick,
            deputies: vec![],
        };
        AuthorizedMemberInfo::with_signature(new_mi, sig)
    };

    let state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                member(&a, owner_id, &owner.sk, owner_id),
                member(&b, a.id, &a.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![old_info(&a), old_info(&b)],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&a, owner_id), join(&b, owner_id)],
            ..Default::default()
        },
        bans: BansV1(vec![
            // (a) ban by a PRESENT member — new verify checks the signature.
            ban(b.id, &a, owner_id),
            // (b) ban by a NON-member (pruned, not banned) banner — old code
            // sig-SKIPPED + accepted; new verify must NOT reject it (removal is
            // post_apply_cleanup's job).
            ban(a.id, &pruned_banner, owner_id),
        ]),
        ..Default::default()
    };

    state.verify(&state, &p).expect(
        "new verify MUST accept an old-generation state (old member_info without \
         deputies + bans by non-member banners); rejecting it strands the Official room",
    );

    // Show the two-phase design: the non-member-banner ban is removed by
    // post_apply_cleanup (NOT verify), and the resulting state still verifies.
    let mut cleaned = state.clone();
    cleaned.post_apply_cleanup(&p).unwrap();
    assert!(
        !cleaned
            .bans
            .0
            .iter()
            .any(|bn| bn.banned_by == pruned_banner.id),
        "the non-member-banner ban is swept by post_apply_cleanup"
    );
    cleaned
        .verify(&cleaned, &p)
        .expect("post-cleanup state still verifies");
}

// ===================================================================
// Round 4 — signature-bypass and equal-version convergence (Codex)
// ===================================================================

/// #411 round 4 A — same-delta pruned-deputy REPLAY forgery.
///
/// A pruned deputy D's PUBLIC `AuthorizedMember` (replayable by anyone, since it
/// is signed by D's inviter, not D) is re-added as a member in the SAME delta
/// that carries a GARBAGE-signature ban attributed to D. Field order applies
/// `bans` before `members`, so `verify` SKIPS the ban's signature at apply time
/// (D is absent then). Without the enforcement-time re-check, `post_apply_cleanup`
/// would see D as a current member, honor D's retained owner-global-mod grant,
/// and remove the victim via a ban D never signed. The fix re-verifies the ban's
/// signature against the banner's CURRENT converged key during enforcement, so
/// the forged ban is inert (victim survives) AND is swept from state. A
/// validly-signed present-member ban in the SAME state still enforces.
#[test]
fn same_delta_replayed_deputy_forged_ban_is_inert_and_swept() {
    let owner = Peer::new();
    let d = Peer::new(); // pruned deputy, replayed as a member; owner's global mod
    let v = Peer::new(); // victim of the forged ban
    let m = Peer::new(); // legit banner (owner's invitee)
    let t = Peer::new(); // legit ban target, in M's subtree
    let owner_id = owner.id;
    let p = params(&owner);

    // GARBAGE-signature ban attributed to D against V (D never signed it).
    let forged = AuthorizedUserBan::with_signature(
        UserBan {
            owner_member_id: owner_id,
            banned_at: SystemTime::now(),
            banned_user: v.id,
        },
        d.id,
        ed25519_dalek::Signature::from_bytes(&[0u8; 64]),
    );
    // A real, validly-signed ban by present member M of T (M is T's ancestor).
    let real = ban(t.id, &m, owner_id);

    let mut state = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![
                // D re-added via the replayed public AuthorizedMember.
                member(&d, owner_id, &owner.sk, owner_id),
                member(&v, owner_id, &owner.sk, owner_id),
                member(&m, owner_id, &owner.sk, owner_id),
                member(&t, m.id, &m.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![
                info(&owner, 1, vec![d.id]), // owner deputizes D (global mod)
                info(&d, 0, vec![]),
                info(&v, 0, vec![]),
                info(&m, 0, vec![]),
                info(&t, 0, vec![]),
            ],
        },
        recent_messages: MessagesV1 {
            messages: vec![
                join(&d, owner_id),
                join(&v, owner_id),
                join(&m, owner_id),
                join(&t, owner_id),
            ],
            ..Default::default()
        },
        bans: BansV1(vec![forged.clone(), real.clone()]),
        ..Default::default()
    };

    state.post_apply_cleanup(&p).unwrap();

    let ids = member_ids(&state);
    assert!(
        ids.contains(&v.id),
        "victim survives the forged ban (signature re-checked at enforcement)"
    );
    assert!(
        !ids.contains(&t.id),
        "the validly-signed present-member ban still enforces"
    );
    assert!(
        !state.bans.0.iter().any(|bn| bn.banned_by == d.id),
        "the forged garbage-sig ban is swept from state"
    );
    assert!(
        state.bans.0.iter().any(|bn| bn.banned_by == m.id),
        "the valid present-member ban is retained"
    );
    state
        .verify(&state, &p)
        .expect("cleaned state verifies (no unvalidated ban remains)");
}

/// #411 round 4 B — equal-version `MemberInfo` conflict resolves deterministically
/// regardless of apply order (the `apply_delta` tiebreak half of the fix).
#[test]
fn equal_version_member_info_resolves_deterministically_across_apply_order() {
    let owner = Peer::new();
    let m = Peer::new();
    let x = Peer::new();
    let y = Peer::new();
    let owner_id = owner.id;
    let p = params(&owner);

    // Two self-signed records for M at the SAME version but different deputies —
    // hence different signatures.
    let ra = info(&m, 1, vec![x.id]);
    let rb = info(&m, 1, vec![y.id]);
    assert_ne!(ra, rb, "the two equal-version records must actually differ");

    let baseline = ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![member(&m, owner_id, &owner.sk, owner_id)],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&m, owner_id)],
            ..Default::default()
        },
        ..Default::default()
    };

    let da = river_core::room_state::ChatRoomStateV1Delta {
        member_info: Some(vec![ra.clone()]),
        ..Default::default()
    };
    let db = river_core::room_state::ChatRoomStateV1Delta {
        member_info: Some(vec![rb.clone()]),
        ..Default::default()
    };

    // Peer1 applies RA then RB; Peer2 applies RB then RA.
    let mut peer1 = baseline.clone();
    peer1
        .apply_delta(&baseline.clone(), &p, &Some(da.clone()))
        .unwrap();
    let mid = peer1.clone();
    peer1.apply_delta(&mid, &p, &Some(db.clone())).unwrap();

    let mut peer2 = baseline.clone();
    peer2.apply_delta(&baseline.clone(), &p, &Some(db)).unwrap();
    let mid = peer2.clone();
    peer2.apply_delta(&mid, &p, &Some(da)).unwrap();

    assert_eq!(
        peer1.member_info, peer2.member_info,
        "equal-version conflict must resolve identically regardless of apply order"
    );
    // Canonical winner: higher version (equal here), else greater signature.
    let winner = if ra.signature.to_bytes() > rb.signature.to_bytes() {
        &ra
    } else {
        &rb
    };
    let got = peer1
        .member_info
        .member_info
        .iter()
        .find(|i| i.member_info.member_id == m.id)
        .unwrap();
    assert_eq!(got, winner, "resolves to the greater-signature record");
    peer1.verify(&peer1, &p).expect("peer1 verifies");
}

/// #411 round 4 B — a same-version content difference is DETECTED and corrected
/// by anti-entropy (the `summarize` discriminator half of the fix). Each peer has
/// only ONE of the two equal-version records; because the summary now carries the
/// signature, the merge transfers the canonical winner and both converge. If
/// `summarize` regressed to version-only, the delta would be empty and the peers
/// would disagree on ban authority forever.
#[test]
fn equal_version_member_info_diff_detected_by_anti_entropy() {
    let owner = Peer::new();
    let m = Peer::new();
    let x = Peer::new();
    let y = Peer::new();
    let owner_id = owner.id;
    let p = params(&owner);

    let ra = info(&m, 1, vec![x.id]);
    let rb = info(&m, 1, vec![y.id]);
    assert_ne!(ra, rb, "the two equal-version records must actually differ");

    let base = |mi: AuthorizedMemberInfo| ChatRoomStateV1 {
        configuration: config(&owner),
        members: MembersV1 {
            members: vec![member(&m, owner_id, &owner.sk, owner_id)],
        },
        member_info: MemberInfoV1 {
            member_info: vec![mi],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&m, owner_id)],
            ..Default::default()
        },
        ..Default::default()
    };

    let mut peer1 = base(ra.clone()); // only ever saw RA
    let mut peer2 = base(rb.clone()); // only ever saw RB
    assert_ne!(
        peer1.member_info, peer2.member_info,
        "precondition: the peers start out disagreeing"
    );

    // The summary MUST expose the equal-version content difference, so at least
    // one direction produces a non-empty delta (the winner → loser correction).
    let s1 = peer1.summarize(&peer1, &p);
    let s2 = peer2.summarize(&peer2, &p);
    assert!(
        peer2.delta(&peer1, &p, &s1).is_some() || peer1.delta(&peer2, &p, &s2).is_some(),
        "anti-entropy must DETECT the equal-version deputies difference"
    );

    // Anti-entropy merge, both directions.
    let snap1 = peer1.clone();
    let snap2 = peer2.clone();
    peer1.merge(&snap1, &p, &snap2).unwrap();
    peer2.merge(&snap2, &p, &snap1).unwrap();

    assert_eq!(
        peer1.member_info, peer2.member_info,
        "peers converge on the SAME MemberInfo record after anti-entropy"
    );
    let winner = if ra.signature.to_bytes() > rb.signature.to_bytes() {
        &ra
    } else {
        &rb
    };
    let got = peer1
        .member_info
        .member_info
        .iter()
        .find(|i| i.member_info.member_id == m.id)
        .unwrap();
    assert_eq!(got, winner, "both converge to the greater-signature record");
    peer1.verify(&peer1, &p).expect("peer1 verifies");
    peer2.verify(&peer2, &p).expect("peer2 verifies");
}
