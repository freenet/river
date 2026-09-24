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

/// A mutual ban where one moderator invited the other. X's ban on Y is an
/// ancestor ban, which the `members` step used to enforce before cleanup ever
/// saw Y's counter-ban, so X survived. Both must be removed.
#[test]
fn a_mutual_ban_with_ones_own_inviter_removes_both() {
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
    assert!(!ids.contains(&x.id), "X is removed");
    assert!(!ids.contains(&y.id), "Y is removed");
    assert!(ids.contains(&t.id));
}

/// A moderator banning their own inviter removes the inviter and, through the
/// cascade, themselves. That is not a cycle: a ban's effect on its own issuer
/// cannot void it.
#[test]
fn a_moderator_banning_their_own_inviter_removes_the_inviter() {
    let room = Room::new();
    let x = room.person();
    let y = room.person_invited_by(&x);
    let s = room.modded_state(&[&x, &y], &[&y], vec![]);
    let after = apply_checked(&s, bans_delta(vec![room.ban(&y, &x, 10)]), &room.params);
    let ids = active_ids(&after, &room.params);
    assert!(!ids.contains(&x.id), "the inviter is removed");
    assert!(!ids.contains(&y.id), "and Y with their subtree");
}

/// A self-ban takes effect (it removes its issuer), and a ban on the owner
/// never does.
#[test]
fn self_bans_and_bans_on_the_owner() {
    let room = Room::new();
    let (a, t) = (room.person(), room.person());
    let s = room.modded_state(&[&a, &t], &[&a], vec![]);
    let after = apply_checked(&s, bans_delta(vec![room.ban(&a, &a, 10)]), &room.params);
    assert!(
        !active_ids(&after, &room.params).contains(&a.id),
        "a self-ban removes its issuer"
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

/// A mutual ban inside a subtree the owner also bans: both mutual bans still
/// take effect, so the cycle partner outside the owner's ban is removed too.
#[test]
fn an_owner_banned_member_in_a_mutual_ban_still_removes_their_partner() {
    let room = Room::new();
    let (x, y, t) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&x, &y, &t], &[&x, &y], vec![]);
    let after = apply_checked(
        &s,
        bans_delta(vec![
            room.owner_ban(&x, 9),
            room.ban(&x, &y, 10),
            room.ban(&y, &x, 11),
        ]),
        &room.params,
    );
    let ids = active_ids(&after, &room.params);
    assert!(!ids.contains(&x.id) && !ids.contains(&y.id));
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

/// A mutual-ban cycle member whose inviter is ALSO banned (here by the
/// owner). The inviter must be retained as a tombstone too, or the cycle
/// member's invite chain breaks and `verify` rejects the state. Everyone in
/// the chain stays enforced-banned, and a second cleanup changes nothing.
#[test]
fn a_cycle_members_banned_inviter_is_retained_with_them() {
    let room = Room::new();
    let w = room.person();
    let x = room.person_invited_by(&w);
    let (y, t) = (room.person(), room.person());
    let s = room.modded_state(&[&w, &x, &y, &t], &[&x, &y], vec![]);
    let after = apply_checked(
        &s,
        bans_delta(vec![
            room.owner_ban(&w, 9),
            room.ban(&x, &y, 10),
            room.ban(&y, &x, 11),
        ]),
        &room.params,
    );
    let ids = active_ids(&after, &room.params);
    for p in [&w, &x, &y] {
        assert!(!ids.contains(&p.id), "W, X and Y are all removed");
    }
    assert!(ids.contains(&t.id));
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
