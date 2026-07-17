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

/// A flood of forged/inert bans over `max_user_bans` must NOT evict the real,
/// enforcing moderator bans — the real bans survive AND still keep the spammers
/// out. The real bans are made OLDER than the forged flood, so the previous
/// oldest-first eviction WOULD have dropped them (this test fails under that
/// policy and passes under inert-first eviction).
#[test]
fn forged_ban_flood_does_not_evict_enforcing_bans() {
    let owner = Peer::new();
    let a = Peer::new();
    let s1 = Peer::new();
    let s2 = Peer::new();
    let owner_id = owner.id;
    let p = params(&owner);

    let members = vec![
        member(&a, owner_id, &owner.sk, owner_id),
        member(&s1, owner_id, &owner.sk, owner_id),
        member(&s2, owner_id, &owner.sk, owner_id),
    ];
    let infos = vec![
        info(&a, 0, vec![]),
        info(&s1, 0, vec![]),
        info(&s2, 0, vec![]),
    ];
    let msgs = vec![join(&a, owner_id), join(&s1, owner_id), join(&s2, owner_id)];

    // Two real enforcing owner bans, made OLDEST (secs 1, 2).
    let real1 = ban_at(s1.id, &owner, owner_id, 1);
    let real2 = ban_at(s2.id, &owner, owner_id, 2);

    // Five FORGED inert bans by fresh NON-member keys targeting present member A,
    // all NEWER (secs 100+). is_ban_authorized(fake, A) == false -> inert.
    let forged: Vec<AuthorizedUserBan> = (0..5)
        .map(|i| ban_at(a.id, &Peer::new(), owner_id, 100 + i))
        .collect();

    // Order the stored list forged-first so an oldest-first cap would drop the
    // (older) real bans.
    let mut all = forged.clone();
    all.push(real1.clone());
    all.push(real2.clone());

    let mut state = ChatRoomStateV1 {
        configuration: config_max_bans(&owner, 4),
        members: MembersV1 { members },
        member_info: MemberInfoV1 { member_info: infos },
        recent_messages: MessagesV1 {
            messages: msgs,
            ..Default::default()
        },
        bans: BansV1(all),
        ..Default::default()
    };

    state.post_apply_cleanup(&p).unwrap();

    assert_eq!(state.bans.0.len(), 4, "bans capped to max_user_bans");
    assert!(
        state.bans.0.contains(&real1),
        "the real (older) enforcing ban of S1 must survive the forged flood"
    );
    assert!(
        state.bans.0.contains(&real2),
        "the real (older) enforcing ban of S2 must survive the forged flood"
    );
    let ids = member_ids(&state);
    assert!(
        !ids.contains(&s1.id) && !ids.contains(&s2.id),
        "the enforcing bans still enforce — S1 and S2 stay removed"
    );
    assert!(
        ids.contains(&a.id),
        "A (target of the inert forged bans) stays a member"
    );
    state
        .verify(&state, &p)
        .expect("capped post-cleanup state must verify");
}

/// The inert-first eviction is deterministic across delta order: two peers that
/// receive the real + forged bans in opposite orders and then anti-entropy
/// merge converge to the same capped ban set.
#[test]
fn inert_ban_eviction_converges_across_delta_order() {
    let owner = Peer::new();
    let a = Peer::new();
    let s1 = Peer::new();
    let owner_id = owner.id;
    let p = params(&owner);

    let baseline = ChatRoomStateV1 {
        configuration: config_max_bans(&owner, 3),
        members: MembersV1 {
            members: vec![
                member(&a, owner_id, &owner.sk, owner_id),
                member(&s1, owner_id, &owner.sk, owner_id),
            ],
        },
        member_info: MemberInfoV1 {
            member_info: vec![info(&a, 0, vec![]), info(&s1, 0, vec![])],
        },
        recent_messages: MessagesV1 {
            messages: vec![join(&a, owner_id), join(&s1, owner_id)],
            ..Default::default()
        },
        ..Default::default()
    };

    let real = ban_at(s1.id, &owner, owner_id, 1); // enforcing, oldest
    let forged: Vec<AuthorizedUserBan> = (0..4)
        .map(|i| ban_at(a.id, &Peer::new(), owner_id, 100 + i))
        .collect();

    let real_delta = river_core::room_state::ChatRoomStateV1Delta {
        bans: Some(vec![real.clone()]),
        ..Default::default()
    };
    let forged_delta = river_core::room_state::ChatRoomStateV1Delta {
        bans: Some(forged.clone()),
        ..Default::default()
    };

    // Peer 1: forged first, then real.
    let mut peer1 = baseline.clone();
    peer1
        .apply_delta(&baseline.clone(), &p, &Some(forged_delta.clone()))
        .unwrap();
    let m1 = peer1.clone();
    peer1
        .apply_delta(&m1, &p, &Some(real_delta.clone()))
        .unwrap();

    // Peer 2: real first, then forged.
    let mut peer2 = baseline.clone();
    peer2
        .apply_delta(&baseline.clone(), &p, &Some(real_delta))
        .unwrap();
    let m2 = peer2.clone();
    peer2.apply_delta(&m2, &p, &Some(forged_delta)).unwrap();

    // Anti-entropy merge.
    let snap1 = peer1.clone();
    let snap2 = peer2.clone();
    peer1.merge(&snap1, &p, &snap2).unwrap();
    peer2.merge(&snap2, &p, &snap1).unwrap();

    assert_eq!(
        peer1.bans, peer2.bans,
        "capped ban sets converge across delta order"
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
