//! freenet/river#423: two peers holding VALID states must never permanently
//! reject each other.
//!
//! A merge that returns `Err` leaves the receiver on its own state, and if
//! nothing on either side changes, the same merge fails forever: the two peers
//! never converge. These tests pin the reachable case found on `main` (an
//! orphaned-ban rule evaluated against the wrong member set), plus the
//! authorization properties the fix must not weaken.

use ed25519_dalek::{Signature, SigningKey};
use freenet_scaffold::ComposableState;
use rand::rngs::OsRng;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{sign_direct_message, DirectMessagesDelta};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersDelta, MembersV1};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::time::{Duration, SystemTime};

struct Room {
    owner_sk: SigningKey,
    owner_id: MemberId,
    params: ChatRoomParametersV1,
    config: AuthorizedConfigurationV1,
}

struct Person {
    sk: SigningKey,
    id: MemberId,
    auth: AuthorizedMember,
}

impl Room {
    fn new() -> Self {
        let owner_sk = SigningKey::generate(&mut OsRng);
        let owner_id: MemberId = owner_sk.verifying_key().into();
        let config = AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: owner_id,
                max_members: 20,
                max_user_bans: 10,
                max_recent_messages: 50,
                max_message_size: 1000,
                ..Default::default()
            },
            &owner_sk,
        );
        let params = ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        };
        Room {
            owner_sk,
            owner_id,
            params,
            config,
        }
    }

    /// A member invited directly by the owner.
    fn person(&self) -> Person {
        let sk = SigningKey::generate(&mut OsRng);
        let member = Member {
            owner_member_id: self.owner_id,
            invited_by: self.owner_id,
            member_vk: sk.verifying_key(),
        };
        Person {
            id: sk.verifying_key().into(),
            auth: AuthorizedMember::new(member, &self.owner_sk),
            sk,
        }
    }

    fn msg(&self, p: &Person, secs: u64) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: self.owner_id,
                author: p.id,
                time: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + secs),
                content: RoomMessageBody::public(format!("hello {secs}")),
            },
            &p.sk,
        )
    }

    fn ban(&self, banner: &Person, target: &Person, secs: u64) -> AuthorizedUserBan {
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: self.owner_id,
                banned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + secs),
                banned_user: target.id,
            },
            banner.id,
            &banner.sk,
        )
    }

    fn owner_ban(&self, target: &Person, secs: u64) -> AuthorizedUserBan {
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: self.owner_id,
                banned_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + secs),
                banned_user: target.id,
            },
            self.owner_id,
            &self.owner_sk,
        )
    }

    fn state(
        &self,
        members: &[&Person],
        bans: Vec<AuthorizedUserBan>,
        msgs: Vec<AuthorizedMessageV1>,
    ) -> ChatRoomStateV1 {
        let mut messages = msgs;
        messages.sort_by(|a, b| {
            a.message
                .time
                .cmp(&b.message.time)
                .then_with(|| a.id().cmp(&b.id()))
        });
        let mut members: Vec<AuthorizedMember> = members.iter().map(|p| p.auth.clone()).collect();
        members.sort_by_key(|m| m.member.id());
        let mut bans = bans;
        bans.sort_by(|a, b| {
            a.ban
                .banned_at
                .cmp(&b.ban.banned_at)
                .then_with(|| a.id().cmp(&b.id()))
        });
        let mut s = ChatRoomStateV1 {
            configuration: self.config.clone(),
            members: MembersV1 { members },
            bans: BansV1(bans),
            recent_messages: MessagesV1 {
                messages,
                ..Default::default()
            },
            ..Default::default()
        };
        s.recent_messages.rebuild_actions_state();
        s.verify(&s, &self.params)
            .unwrap_or_else(|e| panic!("fixture state must be valid: {e}"));
        s
    }
}

fn merge(
    a: &ChatRoomStateV1,
    b: &ChatRoomStateV1,
    p: &ChatRoomParametersV1,
) -> Result<ChatRoomStateV1, String> {
    let mut s = a.clone();
    let parent = s.clone();
    s.merge(&parent, p, b)?;
    s.verify(&s, p)
        .map_err(|e| format!("merged state fails verify: {e}"))?;
    Ok(s)
}

fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = vec![];
    ciborium::ser::into_writer(s, &mut v).unwrap();
    v
}

fn member_ids(s: &ChatRoomStateV1) -> Vec<MemberId> {
    s.members.members.iter().map(|m| m.member.id()).collect()
}

/// Gossip full states both ways until the peers agree, as nodes do. Every
/// merge must SUCCEED: a rejection is what made #423 permanent. Returns the
/// number of exchanges it took.
fn gossip_until_equal(
    a: &ChatRoomStateV1,
    b: &ChatRoomStateV1,
    p: &ChatRoomParametersV1,
    max_exchanges: usize,
) -> (ChatRoomStateV1, usize) {
    let (mut x, mut y) = (a.clone(), b.clone());
    for n in 1..=max_exchanges {
        let nx = merge(&x, &y, p).unwrap_or_else(|e| panic!("exchange {n}: merge rejected: {e}"));
        let ny = merge(&y, &x, p).unwrap_or_else(|e| panic!("exchange {n}: merge rejected: {e}"));
        x = nx;
        y = ny;
        if ser(&x) == ser(&y) {
            return (x, n);
        }
    }
    panic!("peers still differ after {max_exchanges} exchanges");
}

/// The permanent fork, minimised. Both states are valid.
///
/// * Peer B holds X, Y and T, and two inert bans: Y bans X and X bans T.
///   Inert because neither banner is the target's ancestor or deputy, so
///   neither removes anyone. `verify` accepts inert bans by design (#410).
/// * Peer A holds Y and T. X was pruned for inactivity there.
///
/// When A merges B, `bans` applies before `members`, so it sees A's member
/// set, where X is absent and banned (by Y) and T is present. On `main` that
/// matched the orphaned-ban rule and the WHOLE merge was rejected. B has
/// nothing to learn from A, so B never changes, and A rejected B forever.
///
/// Now A drops just X's ban on the first exchange (X is not yet a member
/// there), takes everything else including X, and accepts the ban on the next.
#[test]
fn a_pruned_banner_does_not_fork_the_room() {
    let room = Room::new();
    let (t, x, y) = (room.person(), room.person(), room.person());

    let b = room.state(
        &[&t, &x, &y],
        vec![room.ban(&y, &x, 10), room.ban(&x, &t, 11)],
        vec![room.msg(&t, 1), room.msg(&x, 2), room.msg(&y, 3)],
    );
    let a = room.state(&[&t, &y], vec![], vec![room.msg(&t, 1), room.msg(&y, 3)]);

    let (agreed, exchanges) = gossip_until_equal(&a, &b, &room.params, 3);
    assert_eq!(
        ser(&agreed),
        ser(&b),
        "A learns everything B has; B already had everything A has"
    );
    assert!(exchanges <= 2, "took {exchanges} exchanges");
}

/// A delta that carries an orphaned ban is not rejected: the ban is dropped
/// and the rest of the delta lands. X is banned by the owner and absent; X's
/// ban on T arrives in the same delta as an unrelated message.
#[test]
fn a_delta_carrying_an_orphaned_ban_still_applies_the_rest() {
    let room = Room::new();
    let (t, x) = (room.person(), room.person());
    let a = room.state(&[&t], vec![], vec![room.msg(&t, 1)]);

    let owner_bans_x = room.owner_ban(&x, 5);
    let x_bans_t = room.ban(&x, &t, 6);
    let later_msg = room.msg(&t, 7);
    let delta = ChatRoomStateV1Delta {
        bans: Some(vec![owner_bans_x.clone(), x_bans_t.clone()]),
        recent_messages: Some(vec![later_msg.clone()]),
        ..Default::default()
    };
    let mut after = a.clone();
    after
        .apply_delta(&a, &room.params, &Some(delta))
        .expect("a delta carrying an orphaned ban must not be rejected wholesale (#423)");
    after
        .verify(&after, &room.params)
        .expect("result must verify");

    assert!(member_ids(&after).contains(&t.id), "T must stay");
    assert!(
        !after.bans.0.iter().any(|b| b.id() == x_bans_t.id()),
        "the orphaned ban must not be stored"
    );
    assert!(
        after.bans.0.iter().any(|b| b.id() == owner_bans_x.id()),
        "the owner's ban is legitimate and must be kept"
    );
    assert!(
        after
            .recent_messages
            .messages
            .iter()
            .any(|m| m.id() == later_msg.id()),
        "the rest of the delta must land"
    );
}

/// A moderator banned by another moderator must not regain ban authority by
/// being re-added in the same delta as one of their own bans (found in
/// review of #702's first revision).
///
/// B and D are both owner-appointed moderators (listed in the owner's
/// `deputies`). D banned B, so B was removed. B's public `AuthorizedMember`
/// can be replayed by anyone. One delta carries it together with a ban B
/// signed on T. `bans` applies before `members`, so B is absent at that point
/// and the ban is orphaned. If it were stored, the `members` step would re-add
/// B (it cannot evaluate deputy authority), and cleanup step 0 would see B as
/// a current moderator with a matching signature and remove T and T's whole
/// subtree in the same pass that removes B.
#[test]
fn a_banned_moderator_cannot_act_through_a_same_delta_re_add() {
    let room = Room::new();
    let (t, d, b) = (room.person(), room.person(), room.person());

    let mut owner_info = MemberInfo::new_public(room.owner_id, 1, "owner".into());
    owner_info.deputies = vec![d.id, b.id];
    let owner_info = AuthorizedMemberInfo::new(owner_info, &room.owner_sk);

    let d_bans_b = room.ban(&d, &b, 5);
    let mut a = room.state(
        &[&t, &d],
        vec![d_bans_b.clone()],
        vec![room.msg(&t, 1), room.msg(&d, 2)],
    );
    a.member_info = MemberInfoV1 {
        member_info: vec![owner_info],
    };
    a.verify(&a, &room.params).expect("fixture must be valid");

    let b_bans_t = room.ban(&b, &t, 10);
    let delta = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![b.auth.clone()])),
        bans: Some(vec![b_bans_t.clone()]),
        recent_messages: Some(vec![room.msg(&b, 11)]),
        ..Default::default()
    };
    let mut after = a.clone();
    after
        .apply_delta(&a, &room.params, &Some(delta))
        .expect("the delta must not be rejected wholesale (#423)");
    after
        .verify(&after, &room.params)
        .expect("result must verify");

    assert!(
        member_ids(&after).contains(&t.id),
        "a banned moderator's stale ban must not remove anyone"
    );
    assert!(
        !member_ids(&after).contains(&b.id),
        "D's ban on B is enforced"
    );
    assert!(
        !after.bans.0.iter().any(|x| x.id() == b_bans_t.id()),
        "the orphaned ban must not be stored"
    );
}

/// A forged signature is still rejected at apply time when the banner is known
/// here: dropping the orphaned-ban rule must not have dropped the signature
/// check with it.
#[test]
fn a_forged_ban_by_a_current_member_is_still_rejected() {
    let room = Room::new();
    let (t, x) = (room.person(), room.person());
    let a = room.state(&[&t, &x], vec![], vec![room.msg(&t, 1), room.msg(&x, 2)]);

    let mut forged = room.ban(&x, &t, 5);
    forged.signature = Signature::from_bytes(&[7u8; 64]);
    let delta = ChatRoomStateV1Delta {
        bans: Some(vec![forged]),
        ..Default::default()
    };
    let mut after = a.clone();
    let err = after
        .apply_delta(&a, &room.params, &Some(delta))
        .expect_err("a forged ban attributed to a current member must be rejected");
    assert!(err.contains("signature"), "unexpected error: {err}");
}

/// The old draft of #672 made `MemberInfoV1::verify` accept a record for an
/// absent member WITHOUT checking its signature. Review found that an
/// attacker could then plant a forged record (for example naming themselves
/// as the member's deputy, at `version = u32::MAX`), wait for the member to be
/// re-added, and have the forged record become canonical: ban authority over
/// the member's whole invite subtree. The redo does not tolerate orphaned
/// records at all, so that record never enters state. These two tests pin
/// that, at both entry points.
///
/// Entry point 1, a full state (PUT / validate_state).
#[test]
fn an_unsigned_member_info_for_an_absent_member_is_rejected_by_verify() {
    let room = Room::new();
    let (t, attacker, victim) = (room.person(), room.person(), room.person());
    let mut s = room.state(&[&t], vec![], vec![room.msg(&t, 1)]);

    let mut forged = MemberInfo::new_public(victim.id, u32::MAX, "victim".into());
    forged.deputies = vec![attacker.id];
    s.member_info = MemberInfoV1 {
        member_info: vec![AuthorizedMemberInfo::with_signature(
            forged,
            Signature::from_bytes(&[0u8; 64]),
        )],
    };
    let err = s
        .verify(&s, &room.params)
        .expect_err("a state carrying an unsigned member_info record must be rejected");
    assert!(
        err.contains("non-existent member"),
        "unexpected error: {err}"
    );
}

/// Entry point 2, a delta that RE-ADDS the member in the same update, so the
/// record is no longer an orphan by the time `member_info` applies. The
/// record's signature must be checked against the member's key and fail.
#[test]
fn an_unsigned_member_info_riding_a_re_add_is_rejected() {
    let room = Room::new();
    let (t, attacker, victim) = (room.person(), room.person(), room.person());
    let a = room.state(&[&t], vec![], vec![room.msg(&t, 1)]);

    let mut forged = MemberInfo::new_public(victim.id, u32::MAX, "victim".into());
    forged.deputies = vec![attacker.id];
    let delta = ChatRoomStateV1Delta {
        // The victim's AuthorizedMember is public and replayable.
        members: Some(MembersDelta::new(vec![victim.auth.clone()])),
        member_info: Some(vec![AuthorizedMemberInfo::with_signature(
            forged,
            Signature::from_bytes(&[0u8; 64]),
        )]),
        recent_messages: Some(vec![room.msg(&victim, 2)]),
        ..Default::default()
    };
    let mut after = a.clone();
    let result = after.apply_delta(&a, &room.params, &Some(delta));
    assert!(
        result.is_err(),
        "the forged record must be rejected, not installed"
    );
    assert!(
        after.member_info.deputies_of(victim.id).is_empty(),
        "the attacker must not have gained deputy authority over the victim"
    );
}

/// Only VERIFIABLE bans may mark a banner as banned for the drop (#702
/// review, round 2). A full-state PUT or migration can leave the receiver
/// holding X's genuine ban on T while X, pruned, is absent. A crafted delta
/// re-adds X (public record) and carries a forged "Z bans X" from an absent Z,
/// whose signature cannot be checked here. The forgery must not delete X's
/// real ban.
#[test]
fn a_forged_ban_on_the_banner_cannot_delete_a_stored_ban() {
    let room = Room::new();
    let (t, x, z) = (room.person(), room.person(), room.person());
    let x_bans_t = room.ban(&x, &t, 5);
    let a = room.state(&[&t], vec![x_bans_t.clone()], vec![room.msg(&t, 1)]);

    let mut forged = room.ban(&z, &x, 6);
    forged.signature = Signature::from_bytes(&[9u8; 64]);
    let delta = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![x.auth.clone()])),
        bans: Some(vec![forged]),
        recent_messages: Some(vec![room.msg(&x, 7)]),
        ..Default::default()
    };
    let mut after = a.clone();
    after
        .apply_delta(&a, &room.params, &Some(delta))
        .expect("delta must apply");
    after
        .verify(&after, &room.params)
        .expect("result must verify");

    assert!(member_ids(&after).contains(&x.id), "X is re-added");
    assert!(
        after.bans.0.iter().any(|b| b.id() == x_bans_t.id()),
        "X's genuine ban must survive a forged ban on X"
    );
}

/// The drop also applies to a ban already STORED, when a verifiable ban on
/// its banner arrives in the same delta that re-adds the banner. X and D are
/// owner-appointed moderators. The receiver holds X's ban on T while X is
/// absent (a full-state PUT can leave that). One delta re-adds X and carries
/// D's ban on X. X's stored ban must not act: T stays, X is removed.
#[test]
fn a_stored_ban_cannot_act_through_a_same_delta_re_add_of_its_banned_banner() {
    let room = Room::new();
    let (t, d, x) = (room.person(), room.person(), room.person());

    let mut owner_info = MemberInfo::new_public(room.owner_id, 1, "owner".into());
    owner_info.deputies = vec![d.id, x.id];
    let owner_info = AuthorizedMemberInfo::new(owner_info, &room.owner_sk);

    let x_bans_t = room.ban(&x, &t, 5);
    let mut a = room.state(
        &[&t, &d],
        vec![x_bans_t.clone()],
        vec![room.msg(&t, 1), room.msg(&d, 2)],
    );
    a.member_info = MemberInfoV1 {
        member_info: vec![owner_info],
    };
    a.verify(&a, &room.params).expect("fixture must be valid");

    let delta = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![x.auth.clone()])),
        bans: Some(vec![room.ban(&d, &x, 10)]),
        recent_messages: Some(vec![room.msg(&x, 11)]),
        ..Default::default()
    };
    let mut after = a.clone();
    after
        .apply_delta(&a, &room.params, &Some(delta))
        .expect("the delta must not be rejected wholesale (#423)");
    after
        .verify(&after, &room.params)
        .expect("result must verify");

    assert!(
        member_ids(&after).contains(&t.id),
        "a banned moderator's stored ban must not remove anyone"
    );
    assert!(
        !member_ids(&after).contains(&x.id),
        "D's ban on X is enforced"
    );
    assert!(
        !after.bans.0.iter().any(|b| b.id() == x_bans_t.id()),
        "the orphaned stored ban must not survive"
    );
}

// ---------------------------------------------------------------------------
// Ban resolution from converged state (#702, Ian's rule of 2026-09-23).
//
// A ban does not take effect if its issuer is removed by a ban that does,
// EXCEPT in a mutual ban: both bans take effect and both issuers are removed.
// Cycles longer than two are handled the same way. See
// `MembersV1::resolve_bans` for the formalization these tests pin.
// ---------------------------------------------------------------------------

impl Room {
    /// A member invited by `inviter` rather than the owner.
    fn person_invited_by(&self, inviter: &Person) -> Person {
        let sk = SigningKey::generate(&mut OsRng);
        let member = Member {
            owner_member_id: self.owner_id,
            invited_by: inviter.id,
            member_vk: sk.verifying_key(),
        };
        Person {
            id: sk.verifying_key().into(),
            auth: AuthorizedMember::new(member, &inviter.sk),
            sk,
        }
    }

    /// The owner's `member_info` record naming `mods` as owner-appointed
    /// moderators (global ban authority).
    fn owner_mods(&self, mods: &[&Person]) -> MemberInfoV1 {
        let mut info = MemberInfo::new_public(self.owner_id, 1, "owner".into());
        info.deputies = mods.iter().map(|p| p.id).collect();
        MemberInfoV1 {
            member_info: vec![AuthorizedMemberInfo::new(info, &self.owner_sk)],
        }
    }

    /// A valid state where everyone in `members` has posted, `mods` are
    /// owner-appointed moderators, and `bans` are stored. Cleanup has run.
    fn modded_state(
        &self,
        members: &[&Person],
        mods: &[&Person],
        bans: Vec<AuthorizedUserBan>,
    ) -> ChatRoomStateV1 {
        let msgs = members
            .iter()
            .enumerate()
            .map(|(i, p)| self.msg(p, i as u64))
            .collect();
        let mut s = self.state(members, vec![], msgs);
        s.member_info = self.owner_mods(mods);
        s.verify(&s, &self.params).expect("fixture must be valid");
        if !bans.is_empty() {
            let delta = ChatRoomStateV1Delta {
                bans: Some(bans),
                ..Default::default()
            };
            let parent = s.clone();
            s.apply_delta(&parent, &self.params, &Some(delta))
                .expect("fixture bans must apply");
            s.verify(&s, &self.params).expect("fixture must verify");
        }
        s
    }
}

fn has_ban(s: &ChatRoomStateV1, ban: &AuthorizedUserBan) -> bool {
    s.bans.0.iter().any(|b| b.id() == ban.id())
}

/// Members who are IN the room: present and not enforced-banned. Members of a
/// mutual-ban cycle stay in `members` as tombstones (so their bans remain
/// verifiable) but are enforced-banned, so they are not in this set.
fn active_ids(s: &ChatRoomStateV1, p: &ChatRoomParametersV1) -> Vec<MemberId> {
    let banned = s.members.banned_member_ids(&s.bans, &s.member_info, p);
    member_ids(s)
        .into_iter()
        .filter(|id| !banned.contains(id))
        .collect()
}

/// Apply `delta` to `s`, require success, `verify`, and idempotent cleanup.
fn apply_checked(
    s: &ChatRoomStateV1,
    delta: ChatRoomStateV1Delta,
    p: &ChatRoomParametersV1,
) -> ChatRoomStateV1 {
    let mut after = s.clone();
    after
        .apply_delta(s, p, &Some(delta))
        .unwrap_or_else(|e| panic!("delta rejected: {e}"));
    after.verify(&after, p).expect("result must verify");
    let mut again = after.clone();
    again.post_apply_cleanup(p).unwrap();
    assert_eq!(ser(&again), ser(&after), "cleanup must be idempotent");
    after
}

fn bans_delta(bans: Vec<AuthorizedUserBan>) -> ChatRoomStateV1Delta {
    ChatRoomStateV1Delta {
        bans: Some(bans),
        ..Default::default()
    }
}

/// The core of the rule: two moderators who ban each other are BOTH removed.
/// A naive "a banned issuer's ban does not count" fixpoint would void both
/// bans, letting the moderator facing a ban escape it by counter-banning.
#[test]
fn a_mutual_ban_removes_both_moderators() {
    let room = Room::new();
    let (a, b, t) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&a, &b, &t], &[&a, &b], vec![]);

    let after = apply_checked(
        &s,
        bans_delta(vec![room.ban(&a, &b, 10), room.ban(&b, &a, 11)]),
        &room.params,
    );
    let ids = active_ids(&after, &room.params);
    assert!(!ids.contains(&a.id), "A is removed");
    assert!(!ids.contains(&b.id), "B is removed");
    assert!(ids.contains(&t.id), "a bystander stays");
}

/// The counter-ban race: A's ban reaches one peer and B's counter-ban
/// another, so each peer first removes only the other moderator. Once they
/// exchange state, both must end with both moderators removed. Every merge
/// must succeed (a rejection is the #423 fork).
#[test]
fn a_counter_ban_race_converges_to_both_removed() {
    let room = Room::new();
    let (a, b, t) = (room.person(), room.person(), room.person());
    let base = room.modded_state(&[&a, &b, &t], &[&a, &b], vec![]);

    let p = apply_checked(&base, bans_delta(vec![room.ban(&a, &b, 10)]), &room.params);
    let q = apply_checked(&base, bans_delta(vec![room.ban(&b, &a, 11)]), &room.params);
    assert!(
        !active_ids(&p, &room.params).contains(&b.id)
            && active_ids(&p, &room.params).contains(&a.id)
    );
    assert!(
        !active_ids(&q, &room.params).contains(&a.id)
            && active_ids(&q, &room.params).contains(&b.id)
    );

    let (agreed, _) = gossip_until_equal(&p, &q, &room.params, 4);
    let ids = active_ids(&agreed, &room.params);
    assert!(!ids.contains(&a.id), "A is removed");
    assert!(!ids.contains(&b.id), "B is removed");
    assert!(ids.contains(&t.id), "a bystander stays");
}

/// Removal by a mutual ban must be DURABLE. After both moderators are
/// removed, either one re-adding themselves (their `AuthorizedMember` is
/// public, and the UI re-sends it with the next message) must not bring them
/// back. Otherwise counter-banning is still an escape, only delayed.
#[test]
fn a_mutually_banned_moderator_cannot_rejoin() {
    let room = Room::new();
    let (a, b, t) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&a, &b, &t], &[&a, &b], vec![]);
    let (a_bans_b, b_bans_a) = (room.ban(&a, &b, 10), room.ban(&b, &a, 11));
    let resolved = apply_checked(
        &s,
        bans_delta(vec![a_bans_b.clone(), b_bans_a.clone()]),
        &room.params,
    );
    assert!(
        has_ban(&resolved, &a_bans_b) && has_ban(&resolved, &b_bans_a),
        "both mutual bans stand"
    );

    for (who, secs) in [(&b, 20), (&a, 21)] {
        let post = room.msg(who, secs);
        let rejoin = ChatRoomStateV1Delta {
            members: Some(MembersDelta::new(vec![who.auth.clone()])),
            recent_messages: Some(vec![post.clone()]),
            ..Default::default()
        };
        let after = apply_checked(&resolved, rejoin, &room.params);
        assert!(
            !active_ids(&after, &room.params).contains(&who.id),
            "a mutually banned moderator must stay removed after re-adding themselves"
        );
        assert!(
            !after
                .recent_messages
                .messages
                .iter()
                .any(|m| m.id() == post.id()),
            "and cannot post"
        );
        assert!(has_ban(&after, &a_bans_b) && has_ban(&after, &b_bans_a));
    }
}

/// A cycle of three is a mutual ban too: every member of it is removed.
#[test]
fn a_three_cycle_removes_every_member() {
    let room = Room::new();
    let (a, b, c, t) = (room.person(), room.person(), room.person(), room.person());
    let s = room.modded_state(&[&a, &b, &c, &t], &[&a, &b, &c], vec![]);
    let after = apply_checked(
        &s,
        bans_delta(vec![
            room.ban(&a, &b, 10),
            room.ban(&b, &c, 11),
            room.ban(&c, &a, 12),
        ]),
        &room.params,
    );
    let ids = active_ids(&after, &room.params);
    for p in [&a, &b, &c] {
        assert!(!ids.contains(&p.id), "every cycle member is removed");
    }
    assert!(ids.contains(&t.id), "a bystander stays");
}

/// A chain is not a cycle. A bans B, B bans C: B's ban does not take effect
/// because B is removed by a ban that does, so C stays.
#[test]
fn a_ban_chain_voids_the_banned_issuers_ban() {
    let room = Room::new();
    let (a, b, c) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&a, &b, &c], &[&a, &b], vec![]);
    let after = apply_checked(
        &s,
        bans_delta(vec![room.ban(&a, &b, 10), room.ban(&b, &c, 11)]),
        &room.params,
    );
    let ids = active_ids(&after, &room.params);
    assert!(ids.contains(&a.id), "A stays");
    assert!(!ids.contains(&b.id), "B is removed");
    assert!(ids.contains(&c.id), "B's ban does not take effect: C stays");
}

/// Same chain, other arrival order: B's ban on C lands (and removes C)
/// before A's ban on B arrives. Once A's ban arrives, B's ban stops taking
/// effect, so C may return: the answer depends on the final state, not on
/// what arrived first. C comes back as soon as a peer that still has C
/// shares it.
#[test]
fn a_ban_chain_is_order_independent() {
    let room = Room::new();
    let (a, b, c) = (room.person(), room.person(), room.person());
    let base = room.modded_state(&[&a, &b, &c], &[&a, &b], vec![]);

    // P saw A's ban first, Q saw B's ban first.
    let p = apply_checked(&base, bans_delta(vec![room.ban(&a, &b, 10)]), &room.params);
    let q = apply_checked(&base, bans_delta(vec![room.ban(&b, &c, 11)]), &room.params);
    assert!(active_ids(&p, &room.params).contains(&c.id));
    assert!(!active_ids(&q, &room.params).contains(&c.id));

    let (agreed, _) = gossip_until_equal(&p, &q, &room.params, 4);
    let ids = active_ids(&agreed, &room.params);
    assert!(ids.contains(&a.id));
    assert!(!ids.contains(&b.id));
    assert!(
        ids.contains(&c.id),
        "C is not removed by a banned issuer's ban"
    );
}

/// A cycle member's ban on someone OUTSIDE the cycle does not take effect:
/// only the mutual bans are excepted, and the issuer is removed.
#[test]
fn a_cycle_members_ban_on_an_outsider_does_not_take_effect() {
    let room = Room::new();
    let (a, b, z) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&a, &b, &z], &[&a, &b], vec![]);
    let after = apply_checked(
        &s,
        bans_delta(vec![
            room.ban(&a, &b, 10),
            room.ban(&b, &a, 11),
            room.ban(&a, &z, 12),
        ]),
        &room.params,
    );
    let ids = active_ids(&after, &room.params);
    assert!(!ids.contains(&a.id) && !ids.contains(&b.id));
    assert!(ids.contains(&z.id), "the outsider stays");
}

/// A counter-ban on one's own inviter is void, like every ban whose cascade
/// would remove its own issuer (`resolve_bans` step 3). X invited Y; X bans
/// Y and Y bans X. Y's ban would remove Y too, so it takes no effect, and
/// Y's counter-ban does not shield Y: X's ban removes Y, and X stays.
#[test]
fn a_counter_ban_on_ones_own_inviter_is_void() {
    let room = Room::new();
    let x = room.person();
    let y = room.person_invited_by(&x);
    let t = room.person();
    let s = room.modded_state(&[&x, &y, &t], &[&y], vec![]);
    let after = apply_checked(
        &s,
        bans_delta(vec![room.ban(&x, &y, 10), room.ban(&y, &x, 11)]),
        &room.params,
    );
    let ids = active_ids(&after, &room.params);
    assert!(ids.contains(&x.id), "Y's ban on its own inviter is void");
    assert!(!ids.contains(&y.id), "X's ban on Y takes effect");
    assert!(!member_ids(&after).contains(&y.id), "Y is no tombstone");
    assert!(ids.contains(&t.id));
}

/// A ban on one's own inviter is void on its own too: the moderator stays,
/// and so does the inviter.
#[test]
fn a_ban_on_ones_own_inviter_is_void() {
    let room = Room::new();
    let x = room.person();
    let y = room.person_invited_by(&x);
    let s = room.modded_state(&[&x, &y], &[&y], vec![]);
    let after = apply_checked(&s, bans_delta(vec![room.ban(&y, &x, 10)]), &room.params);
    let ids = active_ids(&after, &room.params);
    assert!(ids.contains(&x.id) && ids.contains(&y.id));
}

/// A self-ban is void (it would remove its issuer), and a ban on the owner
/// never takes effect.
#[test]
fn self_bans_and_bans_on_the_owner() {
    let room = Room::new();
    let (a, t) = (room.person(), room.person());
    let s = room.modded_state(&[&a, &t], &[&a], vec![]);
    let after = apply_checked(&s, bans_delta(vec![room.ban(&a, &a, 10)]), &room.params);
    assert!(
        active_ids(&after, &room.params).contains(&a.id),
        "a self-ban is void"
    );

    let owner = Person {
        sk: room.owner_sk.clone(),
        id: room.owner_id,
        auth: a.auth.clone(),
    };
    let s = room.modded_state(&[&a, &t], &[&a], vec![]);
    let after = apply_checked(
        &s,
        bans_delta(vec![room.ban(&a, &owner, 10), room.owner_ban(&a, 11)]),
        &room.params,
    );
    let ids = active_ids(&after, &room.params);
    assert!(!ids.contains(&a.id), "the owner's ban takes effect");
    assert!(ids.contains(&t.id), "a ban on the owner removes nobody");
}

/// The owner's ban wins (Ian, 2026-09-24): a member the owner bans has no
/// ban authority, even inside a mutual ban. An existing mutual ban between X
/// and Y is resolved; then the owner bans X. X's ban on Y stops counting, so
/// Y is released, and X leaves `members` entirely (an owner-removed member is
/// never a tombstone).
#[test]
fn an_owner_ban_breaks_a_mutual_ban() {
    let room = Room::new();
    let (x, y, t) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&x, &y, &t], &[&x, &y], vec![]);
    let s = apply_checked(
        &s,
        bans_delta(vec![room.ban(&x, &y, 10), room.ban(&y, &x, 11)]),
        &room.params,
    );
    assert!(!active_ids(&s, &room.params).contains(&y.id));
    let after = apply_checked(&s, bans_delta(vec![room.owner_ban(&x, 20)]), &room.params);
    let ids = active_ids(&after, &room.params);
    assert!(ids.contains(&y.id), "Y is released");
    assert!(!member_ids(&after).contains(&x.id), "X is removed outright");
    assert!(ids.contains(&t.id));
}

/// #702 review round 3: a signature-valid but INERT ban (an ordinary member
/// "banning" a moderator they have no authority over) must not void that
/// moderator's stored ban. The apply-time rule counted it as "the issuer is
/// banned" and deleted X's real ban on T.
#[test]
fn an_inert_ban_on_an_issuer_cannot_delete_their_stored_ban() {
    let room = Room::new();
    let (x, t, m) = (room.person(), room.person(), room.person());
    // X's ban on T is stored while X is absent (a full-state PUT can leave
    // that); T is still present because X's ban cannot be verified here.
    let x_bans_t = room.ban(&x, &t, 5);
    let mut a = room.state(
        &[&t, &m],
        vec![x_bans_t.clone()],
        vec![room.msg(&t, 1), room.msg(&m, 2)],
    );
    a.member_info = room.owner_mods(&[&x]);
    a.verify(&a, &room.params).expect("fixture must be valid");

    let delta = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![x.auth.clone()])),
        bans: Some(vec![room.ban(&m, &x, 10)]),
        recent_messages: Some(vec![room.msg(&x, 11)]),
        ..Default::default()
    };
    let after = apply_checked(&a, delta, &room.params);
    let ids = active_ids(&after, &room.params);
    assert!(ids.contains(&x.id), "M has no authority over X: X stays");
    assert!(has_ban(&after, &x_bans_t), "X's genuine ban must survive");
    assert!(!ids.contains(&t.id), "and it takes effect");
}

/// A mutual-ban cycle member whose inviter is ALSO banned, here by a
/// moderator (an owner ban would make the cycle member owner-removed too,
/// with no authority left). The inviter must be retained as a tombstone, or
/// the cycle member's invite chain breaks and `verify` rejects the state.
/// Everyone in the chain stays enforced-banned, and a second cleanup changes
/// nothing.
#[test]
fn a_cycle_members_banned_inviter_is_retained_with_them() {
    let room = Room::new();
    let w = room.person();
    let x = room.person_invited_by(&w);
    let (y, d, t) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&w, &x, &y, &d, &t], &[&x, &y, &d], vec![]);
    let after = apply_checked(
        &s,
        bans_delta(vec![
            room.ban(&d, &w, 9),
            room.ban(&x, &y, 10),
            room.ban(&y, &x, 11),
        ]),
        &room.params,
    );
    let ids = active_ids(&after, &room.params);
    for p in [&w, &x, &y] {
        assert!(!ids.contains(&p.id), "W, X and Y are all removed");
    }
    assert!(ids.contains(&t.id) && ids.contains(&d.id));
    assert!(
        member_ids(&after).contains(&w.id),
        "W stays as a tombstone so X's chain verifies"
    );
}

/// Whole-room convergence with a tombstone: a peer that never saw the mutual
/// ban and a peer that resolved it agree after gossip, whichever merges first.
#[test]
fn a_resolved_mutual_ban_converges_with_a_peer_that_never_saw_it() {
    let room = Room::new();
    let (a, b, t) = (room.person(), room.person(), room.person());
    let base = room.modded_state(&[&a, &b, &t], &[&a, &b], vec![]);
    let resolved = apply_checked(
        &base,
        bans_delta(vec![room.ban(&a, &b, 10), room.ban(&b, &a, 11)]),
        &room.params,
    );
    let (agreed, _) = gossip_until_equal(&base, &resolved, &room.params, 3);
    assert_eq!(ser(&agreed), ser(&resolved));
}

// ---------------------------------------------------------------------------
// Tombstones (#702): members removed by a ban that must stay in `members` so
// their own effective ban stays verifiable. For them "removed" means "present
// in `members` but enforced-banned". The tests below check, capability by
// capability, that a tombstone gets NOTHING membership confers, that nothing
// evicts it while its ban stands, and that it is released correctly when the
// ban stops taking effect.
// ---------------------------------------------------------------------------

/// A room where owner-appointed moderators A and B have banned each other,
/// resolved. T is a bystander with a message.
fn resolved_mutual_ban(room: &Room) -> (Person, Person, Person, ChatRoomStateV1) {
    let (a, b, t) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&a, &b, &t], &[&a, &b], vec![]);
    let resolved = apply_checked(
        &s,
        bans_delta(vec![room.ban(&a, &b, 10), room.ban(&b, &a, 11)]),
        &room.params,
    );
    (a, b, t, resolved)
}

#[test]
fn a_tombstone_stays_in_members_but_is_not_active() {
    let room = Room::new();
    let (a, b, t, s) = resolved_mutual_ban(&room);
    let present = member_ids(&s);
    assert!(present.contains(&a.id) && present.contains(&b.id));
    let active = active_ids(&s, &room.params);
    assert!(!active.contains(&a.id) && !active.contains(&b.id));
    assert!(active.contains(&t.id));
    // A listing that goes through the shared accessor does not show them.
    let listed: Vec<MemberId> = s
        .members
        .active_members(&s.bans, &s.member_info, &room.params)
        .iter()
        .map(|m| m.member.id())
        .collect();
    assert!(!listed.contains(&a.id) && !listed.contains(&b.id));
}

#[test]
fn a_tombstone_cannot_react_edit_or_delete() {
    let room = Room::new();
    let (a, _b, t, s) = resolved_mutual_ban(&room);
    let target = s
        .recent_messages
        .messages
        .iter()
        .find(|m| m.message.author == t.id)
        .expect("T has a message")
        .id();
    let action = |body: RoomMessageBody, secs: u64| {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: room.owner_id,
                author: a.id,
                time: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + secs),
                content: body,
            },
            &a.sk,
        )
    };
    let delta = ChatRoomStateV1Delta {
        recent_messages: Some(vec![
            action(RoomMessageBody::reaction(target.clone(), "x".into()), 30),
            action(RoomMessageBody::edit(target.clone(), "hijack".into()), 31),
            action(RoomMessageBody::delete(target.clone()), 32),
        ]),
        ..Default::default()
    };
    let after = apply_checked(&s, delta, &room.params);
    assert!(
        !after
            .recent_messages
            .messages
            .iter()
            .any(|m| m.message.author == a.id),
        "none of the tombstone's actions are stored"
    );
    assert!(
        after
            .recent_messages
            .messages
            .iter()
            .any(|m| m.id() == target),
        "T's message is not deleted"
    );
    let reacted = after
        .recent_messages
        .reactions(&target)
        .map(|r| r.values().any(|who| who.contains(&a.id)))
        .unwrap_or(false);
    assert!(
        !reacted,
        "the tombstone's reaction is not in the actions cache"
    );
}

#[test]
fn a_tombstone_can_neither_send_nor_receive_dms() {
    let room = Room::new();
    let (a, _b, t, s) = resolved_mutual_ban(&room);
    let dm = |from: &Person, to: &Person, ts: u64| {
        sign_direct_message(
            &from.sk,
            from.id,
            to.id,
            &room.params.owner,
            1_800_000_000 + ts,
            vec![1u8; 8],
        )
        .expect("sign dm")
    };
    let delta = ChatRoomStateV1Delta {
        direct_messages: Some(DirectMessagesDelta {
            new_messages: vec![dm(&a, &t, 40), dm(&t, &a, 41)],
            advanced_purges: vec![],
        }),
        ..Default::default()
    };
    let mut after = s.clone();
    // Either rejected outright or accepted and swept: both leave no DM.
    if after.apply_delta(&s, &room.params, &Some(delta)).is_ok() {
        after
            .verify(&after, &room.params)
            .expect("result must verify");
    } else {
        after = s.clone();
    }
    assert!(
        after.direct_messages.messages.is_empty(),
        "no DM to or from a tombstone is held"
    );
}

#[test]
fn a_tombstones_invitee_is_removed_and_stays_removed() {
    let room = Room::new();
    let (a, _b, _t, s) = resolved_mutual_ban(&room);
    let n = room.person_invited_by(&a);
    let delta = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![n.auth.clone()])),
        recent_messages: Some(vec![room.msg(&n, 50)]),
        ..Default::default()
    };
    let after = apply_checked(&s, delta, &room.params);
    assert!(
        !member_ids(&after).contains(&n.id),
        "the invitee is in the tombstone's subtree, so the cascade removes them"
    );
    // And again, from the resulting state (a rejoin attempt).
    let delta = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![n.auth.clone()])),
        recent_messages: Some(vec![room.msg(&n, 51)]),
        ..Default::default()
    };
    let again = apply_checked(&after, delta, &room.params);
    assert!(!member_ids(&again).contains(&n.id));
}

/// A tombstone editing its own `deputies` cannot escape a cycle whose other
/// edge rests on ABSOLUTE authority (owner-appointed moderator): the
/// "cannot ban your deputizer" guardrail is checked after absolute grants.
/// See the PR for the deputy-derived case, which is pre-existing and applies
/// to every ban, not only to tombstones.
#[test]
fn a_tombstone_cannot_escape_by_deputizing_its_partner() {
    let room = Room::new();
    let (a, b, _t, s) = resolved_mutual_ban(&room);
    let mut info = MemberInfo::new_public(a.id, 5, "a".into());
    info.deputies = vec![b.id];
    let delta = ChatRoomStateV1Delta {
        member_info: Some(vec![AuthorizedMemberInfo::new(info, &a.sk)]),
        ..Default::default()
    };
    let mut after = s.clone();
    if after.apply_delta(&s, &room.params, &Some(delta)).is_err() {
        after = s.clone();
    }
    after.verify(&after, &room.params).expect("verify");
    let active = active_ids(&after, &room.params);
    assert!(!active.contains(&a.id) && !active.contains(&b.id));
}

/// The inactivity prune never evicts a tombstone: A and B have no messages
/// left, yet stay (as tombstones) across repeated cleanups.
#[test]
fn inactivity_prune_does_not_evict_a_tombstone() {
    let room = Room::new();
    let (a, b, _t, s) = resolved_mutual_ban(&room);
    assert!(!s
        .recent_messages
        .messages
        .iter()
        .any(|m| m.message.author == a.id || m.message.author == b.id));
    let mut again = s.clone();
    for _ in 0..3 {
        again.post_apply_cleanup(&room.params).unwrap();
    }
    assert_eq!(ser(&again), ser(&s));
    assert!(member_ids(&again).contains(&a.id) && member_ids(&again).contains(&b.id));
}

/// The `max_members` trim ranks ban issuers last, so a room filling up does
/// not evict a tombstone (which would let its ban decay). Non-issuers are
/// trimmed instead.
#[test]
fn the_member_cap_does_not_evict_a_tombstone() {
    let mut room = Room::new();
    room.config = AuthorizedConfigurationV1::new(
        Configuration {
            owner_member_id: room.owner_id,
            max_members: 4,
            max_user_bans: 10,
            max_recent_messages: 50,
            max_message_size: 1000,
            ..Default::default()
        },
        &room.owner_sk,
    );
    let (a, b, t, s) = resolved_mutual_ban(&room);
    // A, B (tombstones) and T fill 3 of 4 slots. Three newcomers arrive, each
    // invited by T so their chains are LONGER than the tombstones', which the
    // old trim would have kept in preference to... nothing: it evicts longest
    // chains first, and the tombstones must survive regardless.
    let n: Vec<Person> = (0..3).map(|_| room.person()).collect();
    let delta = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(
            n.iter().map(|p| p.auth.clone()).collect(),
        )),
        recent_messages: Some(
            n.iter()
                .enumerate()
                .map(|(i, p)| room.msg(p, 60 + i as u64))
                .collect(),
        ),
        ..Default::default()
    };
    let after = apply_checked(&s, delta, &room.params);
    let ids = member_ids(&after);
    assert!(
        ids.contains(&a.id) && ids.contains(&b.id),
        "tombstones survive the cap"
    );
    assert!(ids.len() <= 4);
    let active = active_ids(&after, &room.params);
    assert!(
        !active.contains(&a.id) && !active.contains(&b.id),
        "and stay banned"
    );
    let _ = t;
}

/// Release path: the owner revokes A's moderator grant. A's ban on B no
/// longer has authority, so there is no cycle. B's ban on A takes effect;
/// A leaves `members` (A issues no effective ban, so is no tombstone) and B
/// is an active member again who can post. Cleanup stays idempotent.
#[test]
fn revoking_a_cycle_members_grant_releases_the_other() {
    let room = Room::new();
    let (a, b, t, s) = resolved_mutual_ban(&room);
    let mut owner_info = MemberInfo::new_public(room.owner_id, 2, "owner".into());
    owner_info.deputies = vec![b.id];
    let revoke = ChatRoomStateV1Delta {
        member_info: Some(vec![AuthorizedMemberInfo::new(owner_info, &room.owner_sk)]),
        ..Default::default()
    };
    let released = apply_checked(&s, revoke, &room.params);
    let active = active_ids(&released, &room.params);
    assert!(active.contains(&b.id), "B is released");
    assert!(active.contains(&t.id));
    assert!(
        !member_ids(&released).contains(&a.id),
        "A is removed outright"
    );

    let post = room.msg(&b, 70);
    let after = apply_checked(
        &released,
        ChatRoomStateV1Delta {
            recent_messages: Some(vec![post.clone()]),
            ..Default::default()
        },
        &room.params,
    );
    assert!(
        after
            .recent_messages
            .messages
            .iter()
            .any(|m| m.id() == post.id()),
        "the released moderator can post again"
    );
}

/// A tombstoned moderator can pull anyone who LATER bans them into a cycle:
/// under Ian's rule a counter-ban counts whenever it was issued, and a pure
/// function of converged state cannot tell "counter" from "later" (the ban
/// timestamp is signed by its issuer). The owner ends it by revoking the
/// tombstone's moderator grant. Pinned so the consequence is deliberate.
#[test]
fn a_tombstone_pulls_in_a_later_banner_until_its_grant_is_revoked() {
    let room = Room::new();
    let (a, b, c) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&a, &b, &c], &[&a, &b, &c], vec![]);
    let s = apply_checked(
        &s,
        bans_delta(vec![room.ban(&a, &b, 10), room.ban(&b, &a, 11)]),
        &room.params,
    );
    // C redundantly bans A; A (a tombstone, still holding its key) bans C.
    let s = apply_checked(
        &s,
        bans_delta(vec![room.ban(&c, &a, 20), room.ban(&a, &c, 21)]),
        &room.params,
    );
    assert!(
        !active_ids(&s, &room.params).contains(&c.id),
        "C is pulled in"
    );

    let mut owner_info = MemberInfo::new_public(room.owner_id, 2, "owner".into());
    owner_info.deputies = vec![b.id, c.id];
    let s = apply_checked(
        &s,
        ChatRoomStateV1Delta {
            member_info: Some(vec![AuthorizedMemberInfo::new(owner_info, &room.owner_sk)]),
            ..Default::default()
        },
        &room.params,
    );
    let active = active_ids(&s, &room.params);
    assert!(
        active.contains(&b.id) && active.contains(&c.id),
        "revoking A releases both"
    );
    assert!(!active.contains(&a.id));
}

/// Arrival order must not matter for the `members` step's early removal
/// (now owner bans only) either. P sees X's ban on Y first, Q sees Y's
/// (void) ban on its inviter X first, and after gossip both agree with the
/// state where both bans arrive together: Y removed, X stays.
#[test]
fn a_mutual_ban_with_ones_own_inviter_is_arrival_order_independent() {
    let room = Room::new();
    let x = room.person();
    let y = room.person_invited_by(&x);
    let t = room.person();
    let base = room.modded_state(&[&x, &y, &t], &[&y], vec![]);
    let (x_bans_y, y_bans_x) = (room.ban(&x, &y, 10), room.ban(&y, &x, 11));

    let together = apply_checked(
        &base,
        bans_delta(vec![x_bans_y.clone(), y_bans_x.clone()]),
        &room.params,
    );
    let p = apply_checked(&base, bans_delta(vec![x_bans_y]), &room.params);
    let q = apply_checked(&base, bans_delta(vec![y_bans_x]), &room.params);
    for first in [(&p, &q), (&q, &p)] {
        let (agreed, _) = gossip_until_equal(first.0, first.1, &room.params, 4);
        assert_eq!(
            ser(&agreed),
            ser(&together),
            "the result must not depend on which ban arrived first"
        );
    }
    let active = active_ids(&together, &room.params);
    assert!(
        active.contains(&x.id),
        "Y's counter-ban on its inviter is void"
    );
    assert!(!active.contains(&y.id));
    assert!(active.contains(&t.id));
}

/// A self-removing ban (here, Y banning its own inviter X) is void, and the
/// issuer keeps no special power from it: Y is not removed, so Y's other
/// ban, on Z, takes effect the ordinary way, whatever the order of the stored
/// ban list.
#[test]
fn a_self_removing_ban_is_void_and_the_issuers_other_bans_stand() {
    let room = Room::new();
    let x = room.person();
    let y = room.person_invited_by(&x);
    let (z, t) = (room.person(), room.person());
    let s = room.modded_state(&[&x, &y, &z, &t], &[&y], vec![]);
    let after = apply_checked(
        &s,
        bans_delta(vec![room.ban(&y, &x, 10), room.ban(&y, &z, 11)]),
        &room.params,
    );
    let active = active_ids(&after, &room.params);
    assert!(active.contains(&x.id) && active.contains(&y.id));
    assert!(!active.contains(&z.id), "Y's ban on Z takes effect");
    assert!(active.contains(&t.id));
}
