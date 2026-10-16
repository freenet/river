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
use river_core::room_state::member::{
    AuthorizedMember, BanResolution, Member, MemberId, MembersDelta, MembersV1,
};
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

    // Since #702 a removed issuer of a valid ban leaves `members`, its record
    // is kept as ban evidence and its ban stays stored, so that a later
    // counter-ban can still close a cycle. What matters is that the ban takes
    // no effect.
    let active = active_ids(&after, &room.params);
    assert!(
        active.contains(&t.id),
        "a banned moderator's stale ban must not remove anyone"
    );
    assert!(!active.contains(&b.id), "D's ban on B is enforced");
    let _ = b_bans_t;
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

    // X is removed and held as ban evidence (see the note in the test above);
    // its stored ban takes no effect.
    let active = active_ids(&after, &room.params);
    assert!(
        active.contains(&t.id),
        "a banned moderator's stored ban must not remove anyone"
    );
    assert!(!active.contains(&x.id), "D's ban on X is enforced");
    let _ = x_bans_t;
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

    /// A person holding `sk`, invited by the owner.
    fn person_signed_by(&self, sk: &SigningKey) -> Person {
        let member = Member {
            owner_member_id: self.owner_id,
            invited_by: self.owner_id,
            member_vk: sk.verifying_key(),
        };
        Person {
            id: sk.verifying_key().into(),
            auth: AuthorizedMember::new(member, &self.owner_sk),
            sk: sk.clone(),
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

/// Members who are IN the room, read from the emitted state: `member_ids`.
/// Every caller passes a cleaned state, where each enforced-banned member has
/// left `members` (its record kept as ban evidence if a ban needs it); this
/// asserts that, so a test cannot pass on the ban computation alone while the
/// state still lists the member.
fn active_ids(s: &ChatRoomStateV1, p: &ChatRoomParametersV1) -> Vec<MemberId> {
    let banned = s
        .members
        .banned_member_ids(&s.ban_evidence, &s.bans, &s.member_info, p);
    let ids = member_ids(s);
    assert!(
        ids.iter().all(|id| !banned.contains(id)),
        "an enforced-banned member is still listed in `members`"
    );
    ids
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

/// `who` re-adds their own public record with a message, as a client does
/// on its next post. Returns the resulting state.
fn rejoin(room: &Room, s: &ChatRoomStateV1, who: &Person, secs: u64) -> ChatRoomStateV1 {
    apply_checked(
        s,
        ChatRoomStateV1Delta {
            members: Some(MembersDelta::new(vec![who.auth.clone()])),
            recent_messages: Some(vec![room.msg(who, secs)]),
            ..Default::default()
        },
        &room.params,
    )
}

fn evidence_ids(s: &ChatRoomStateV1) -> Vec<MemberId> {
    s.ban_evidence
        .members
        .iter()
        .map(|m| m.member.id())
        .collect()
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
    assert!(!member_ids(&after).contains(&y.id), "Y leaves `members`");
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
fn a_self_ban_is_void() {
    let room = Room::new();
    let (a, t) = (room.person(), room.person());
    let s = room.modded_state(&[&a, &t], &[&a], vec![]);
    let after = apply_checked(&s, bans_delta(vec![room.ban(&a, &a, 10)]), &room.params);
    assert!(
        active_ids(&after, &room.params).contains(&a.id),
        "a self-ban is void"
    );
}

#[test]
fn a_ban_on_the_owner_removes_nobody() {
    let room = Room::new();
    let (a, t) = (room.person(), room.person());
    let owner = Person {
        sk: room.owner_sk.clone(),
        id: room.owner_id,
        auth: a.auth.clone(),
    };
    let s = room.modded_state(&[&a, &t], &[&a], vec![]);
    let after = apply_checked(&s, bans_delta(vec![room.ban(&a, &owner, 10)]), &room.params);
    let ids = active_ids(&after, &room.params);
    assert!(
        ids.contains(&a.id) && ids.contains(&t.id),
        "a ban on the owner removes nobody"
    );
}

/// The owner's ban wins (Ian, 2026-09-24): a member the owner bans has no
/// ban authority, even inside a mutual ban. An existing mutual ban between X
/// and Y is resolved; then the owner bans X. X's ban on Y stops counting, so
/// Y is released, and X leaves `members` (an owner-removed member's record is
/// kept only if another ban needs it).
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
    assert!(!ids.contains(&x.id), "X is removed");
    assert!(ids.contains(&t.id));
    let back = rejoin(&room, &after, &y, 30);
    assert!(
        active_ids(&back, &room.params).contains(&y.id),
        "Y is released: re-adding succeeds"
    );
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
/// with no authority left). The inviter's record must be kept as ban
/// evidence, or the cycle member's invite chain breaks and `verify` rejects
/// the state.
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
        evidence_ids(&after).contains(&w.id),
        "W's record is kept as evidence so X's record verifies"
    );
}

/// Whole-room convergence with ban evidence: a peer that never saw the mutual
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
// Removed members held as ban evidence (#702): they leave `members`, and their
// records are kept only so their bans stay verifiable. The tests below check,
// capability by capability, that such a member gets NOTHING membership
// confers, that its bans keep working, and that it is released correctly when
// the ban stops taking effect.
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

/// freenet/river#702 (K'): a removed member leaves `members`; if a stored ban
/// still needs their record, it moves to `ban_evidence`, which confers no
/// membership. Both moderators of a mutual ban end up there.
#[test]
fn removed_moderators_leave_members_and_their_records_become_evidence() {
    let room = Room::new();
    let (a, b, t, s) = resolved_mutual_ban(&room);
    let present = member_ids(&s);
    assert!(!present.contains(&a.id) && !present.contains(&b.id));
    assert!(present.contains(&t.id));
    let evidence: Vec<MemberId> = s
        .ban_evidence
        .members
        .iter()
        .map(|m| m.member.id())
        .collect();
    assert!(evidence.contains(&a.id) && evidence.contains(&b.id));
    assert!(
        !evidence.contains(&t.id),
        "an unreferenced member is not evidence"
    );
}

/// A removed moderator re-adds themselves together with a reaction, an edit
/// and a delete: none of them lands, and nothing of T's is changed.
#[test]
fn a_removed_member_cannot_react_edit_or_delete() {
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
        members: Some(MembersDelta::new(vec![a.auth.clone()])),
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
        "none of the removed member's actions are stored"
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
        "the removed member's reaction is not in the actions cache"
    );
}

/// A removed moderator re-adds themselves together with DMs in both
/// directions: none is held.
#[test]
fn a_removed_member_can_neither_send_nor_receive_dms() {
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
        members: Some(MembersDelta::new(vec![a.auth.clone()])),
        direct_messages: Some(DirectMessagesDelta {
            new_messages: vec![dm(&a, &t, 40), dm(&t, &a, 41)],
            advanced_purges: vec![],
        }),
        ..Default::default()
    };
    // Accepted, and the DMs to and from the removed member are swept.
    let after = apply_checked(&s, delta, &room.params);
    assert!(!member_ids(&after).contains(&a.id), "A stays removed");

    // Control: the same delta shape between T and the owner keeps both DMs,
    // so the sweep above is about A, not about the delta.
    let owner = Person {
        sk: room.owner_sk.clone(),
        id: room.owner_id,
        auth: t.auth.clone(),
    };
    let control = apply_checked(
        &s,
        ChatRoomStateV1Delta {
            direct_messages: Some(DirectMessagesDelta {
                new_messages: vec![dm(&t, &owner, 40), dm(&owner, &t, 41)],
                advanced_purges: vec![],
            }),
            ..Default::default()
        },
        &room.params,
    );
    assert_eq!(control.direct_messages.messages.len(), 2);
    assert!(
        after.direct_messages.messages.is_empty(),
        "no DM to or from a removed member is held"
    );
}

/// A removed moderator re-adds themselves together with a new invitee: the
/// invitee is in the removed member's subtree, so both are removed again, on
/// every attempt.
#[test]
fn a_removed_members_invitee_is_removed_and_stays_removed() {
    let room = Room::new();
    let (a, _b, _t, s) = resolved_mutual_ban(&room);
    let n = room.person_invited_by(&a);
    let mut state = s;
    for secs in [50, 51] {
        let delta = ChatRoomStateV1Delta {
            members: Some(MembersDelta::new(vec![a.auth.clone(), n.auth.clone()])),
            recent_messages: Some(vec![room.msg(&a, secs), room.msg(&n, secs + 10)]),
            ..Default::default()
        };
        state = apply_checked(&state, delta, &room.params);
        let ids = member_ids(&state);
        assert!(!ids.contains(&a.id) && !ids.contains(&n.id));
    }
}

/// A removed moderator editing their own `deputies` cannot escape a cycle
/// whose other edge rests on ABSOLUTE authority (owner-appointed moderator).
/// Their record is ban evidence, so the update applies, but a `deputies` list
/// grants authority only over its author's own subtree and never voids a ban
/// on its author. No stored ban needs the grant, so cleanup does not keep it.
#[test]
fn a_removed_moderator_cannot_escape_by_deputizing_its_partner() {
    let room = Room::new();
    let (a, b, _t, s) = resolved_mutual_ban(&room);
    let mut info = MemberInfo::new_public(a.id, 5, "a".into());
    info.deputies = vec![b.id];
    let delta = ChatRoomStateV1Delta {
        member_info: Some(vec![AuthorizedMemberInfo::new(info, &a.sk)]),
        ..Default::default()
    };
    // The record is signed by A, whose record is ban evidence, so the
    // update applies. It cannot be used to escape, and nothing needs it.
    let after = apply_checked(&s, delta, &room.params);
    assert!(
        !after.member_info.deputies_of(a.id).contains(&b.id),
        "an evidence-only member's grant no ban needs is not kept"
    );
    let active = active_ids(&after, &room.params);
    assert!(!active.contains(&a.id) && !active.contains(&b.id));
}

/// The ban evidence and both mutual bans are stable under repeated cleanup:
/// nothing about A and B (no messages, no membership) lets a later pass drop
/// their records or their bans.
#[test]
fn a_resolved_mutual_ban_is_a_cleanup_fixpoint() {
    let room = Room::new();
    let (a, b, _t, s) = resolved_mutual_ban(&room);
    let mut again = s.clone();
    for _ in 0..3 {
        again.post_apply_cleanup(&room.params).unwrap();
    }
    assert_eq!(ser(&again), ser(&s));
    assert!(evidence_ids(&again).contains(&a.id) && evidence_ids(&again).contains(&b.id));
    assert_eq!(again.bans.0.len(), 2);
}

/// The `max_members` trim cannot touch removed members' records: they are
/// ban evidence, not members, so a room filling up leaves the mutual ban in
/// force.
#[test]
fn the_member_cap_cannot_dissolve_a_mutual_ban() {
    let room = capped_room(4, 10);
    let (a, b, _t, s) = resolved_mutual_ban(&room);
    let n: Vec<Person> = (0..5).map(|_| room.person()).collect();
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
    assert!(member_ids(&after).len() <= 4);
    assert_eq!(after.bans.0.len(), 2, "both mutual bans stand");
    for who in [&a, &b] {
        let back = rejoin(&room, &after, who, 90);
        assert!(!member_ids(&back).contains(&who.id), "still removed");
    }
}

/// Release path: the owner revokes A's moderator grant. A's ban on B no
/// longer has authority, so there is no cycle. B's ban on A takes effect,
/// and B is no longer banned: re-adding succeeds and B can post. Cleanup
/// stays idempotent throughout (`apply_checked`).
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
    assert!(active_ids(&released, &room.params).contains(&t.id));
    let back = rejoin(&room, &released, &b, 70);
    assert!(
        member_ids(&back).contains(&b.id),
        "B is released and can rejoin"
    );
    let back = rejoin(&room, &back, &a, 71);
    assert!(!member_ids(&back).contains(&a.id), "A stays removed");
}

/// A removed moderator can counter-ban, at any later time, anyone who banned
/// them: under Ian's rule a mutual ban counts whenever its halves were
/// issued, converged state cannot tell "counter" from "later" (`banned_at` is
/// signed by its issuer), and the removed moderator's record is ban
/// evidence, so the counter-ban verifies. The owner ends it by revoking the
/// removed moderator's grant (or banning them). Pinned so the consequence is
/// deliberate.
#[test]
fn a_removed_moderator_can_later_counter_ban_until_the_owner_acts() {
    let room = Room::new();
    let (a, b, c) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&a, &b, &c], &[&a, &b, &c], vec![]);
    let s = apply_checked(
        &s,
        bans_delta(vec![room.ban(&a, &b, 10), room.ban(&b, &a, 11)]),
        &room.params,
    );
    // C redundantly bans A; later, A bans C.
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
    let s = rejoin(&room, &s, &b, 30);
    let s = rejoin(&room, &s, &c, 31);
    let ids = member_ids(&s);
    assert!(
        ids.contains(&b.id) && ids.contains(&c.id),
        "revoking A releases both"
    );
    let s = rejoin(&room, &s, &a, 32);
    assert!(!member_ids(&s).contains(&a.id));
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

// ---------------------------------------------------------------------------
// Owner ban wins (Ian, 2026-09-24), and the round-4 review's attacks.
// ---------------------------------------------------------------------------

/// A member's own `member_info` naming `deputies`.
fn deputies_record(p: &Person, version: u32, deputies: &[&Person]) -> AuthorizedMemberInfo {
    let mut info = MemberInfo::new_public(p.id, version, "m".into());
    info.deputies = deputies.iter().map(|d| d.id).collect();
    AuthorizedMemberInfo::new(info, &p.sk)
}

/// A removed moderator can replay their own public `AuthorizedMember` with a
/// counter-ban on the moderator who banned them, and under the mutual-ban
/// rule both are then removed (the consequence documented on
/// `resolve_bans`). If the OWNER has also banned them, the owner's ban wins:
/// the counter-ban has no authority and the moderator who banned them stays.
#[test]
fn an_owner_ban_voids_a_replayed_counter_ban() {
    let room = Room::new();
    let (d, m, t) = (room.person(), room.person(), room.person());
    for owner_also_bans in [false, true] {
        let s = room.modded_state(&[&d, &m, &t], &[&d, &m], vec![]);
        let mut bans = vec![room.ban(&d, &m, 10)];
        if owner_also_bans {
            bans.push(room.owner_ban(&m, 11));
        }
        let s = apply_checked(&s, bans_delta(bans), &room.params);
        assert!(!member_ids(&s).contains(&m.id), "M is removed outright");

        let replay = ChatRoomStateV1Delta {
            members: Some(MembersDelta::new(vec![m.auth.clone()])),
            bans: Some(vec![room.ban(&m, &d, 20)]),
            ..Default::default()
        };
        let after = apply_checked(&s, replay, &room.params);
        let active = active_ids(&after, &room.params);
        assert!(!active.contains(&m.id));
        if owner_also_bans {
            assert!(
                active.contains(&d.id),
                "the owner's ban voids M's counter-ban"
            );
            assert!(
                !member_ids(&after).contains(&m.id),
                "and M leaves `members`"
            );
        } else {
            assert!(
                !active.contains(&d.id),
                "without it, the counter-ban counts"
            );
        }
    }
}

/// A deputizes A1 and A2 (both invited by A), and they ban each other: a
/// mutual ban, so both are removed. The owner then bans A. A's subtree, A1
/// and A2 included, is owner-removed: no authority, and nobody stays behind
/// in `members`.
#[test]
fn an_owner_ban_dissolves_a_deputy_cycle() {
    let room = Room::new();
    let a = room.person();
    let (a1, a2) = (room.person_invited_by(&a), room.person_invited_by(&a));
    let t = room.person();
    let mut s = room.modded_state(&[&a, &a1, &a2, &t], &[], vec![]);
    s.member_info
        .member_info
        .push(deputies_record(&a, 1, &[&a1, &a2]));
    s.verify(&s, &room.params).expect("fixture must be valid");
    let s = apply_checked(
        &s,
        bans_delta(vec![room.ban(&a1, &a2, 10), room.ban(&a2, &a1, 11)]),
        &room.params,
    );
    let active = active_ids(&s, &room.params);
    assert!(
        !active.contains(&a1.id) && !active.contains(&a2.id),
        "a mutual ban"
    );
    assert!(evidence_ids(&s).contains(&a1.id), "A1's record is evidence");

    let after = apply_checked(&s, bans_delta(vec![room.owner_ban(&a, 20)]), &room.params);
    let ids = member_ids(&after);
    for p in [&a, &a1, &a2] {
        assert!(
            !ids.contains(&p.id),
            "owner-removed members leave `members`"
        );
    }
    assert!(active_ids(&after, &room.params).contains(&t.id));
}

fn capped_room(max_members: usize, max_user_bans: usize) -> Room {
    let mut room = Room::new();
    room.config = AuthorizedConfigurationV1::new(
        Configuration {
            owner_member_id: room.owner_id,
            max_members,
            max_user_bans,
            max_recent_messages: 50,
            max_message_size: 1000,
            ..Default::default()
        },
        &room.owner_sk,
    );
    room
}

/// Round-4 M2: a deep invite chain of sybils with a mutual ban at the
/// bottom must not push real members out at the `max_members` trim. The trim
/// protects no one: it removes the longest chains first, so this chain is
/// trimmed first as usual.
#[test]
fn a_deep_sybil_chain_cannot_evict_real_members() {
    let room = capped_room(6, 4);
    let legit: Vec<Person> = (0..3).map(|_| room.person()).collect();
    let s1 = room.person();
    let s2 = room.person_invited_by(&s1);
    let s3 = room.person_invited_by(&s2);
    let s4 = room.person_invited_by(&s3);
    let (s5a, s5b) = (room.person_invited_by(&s4), room.person_invited_by(&s4));
    let base_members: Vec<&Person> = legit.iter().chain([&s1]).collect();
    let s = room.modded_state(&base_members, &[], vec![]);

    let sybils = [&s2, &s3, &s4, &s5a, &s5b];
    let delta = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(
            sybils.iter().map(|p| p.auth.clone()).collect(),
        )),
        bans: Some(vec![room.ban(&s5a, &s5b, 10), room.ban(&s5b, &s5a, 11)]),
        recent_messages: Some(
            sybils
                .iter()
                .enumerate()
                .map(|(i, p)| room.msg(p, 20 + i as u64))
                .collect(),
        ),
        ..Default::default()
    };
    let after = apply_checked(&s, delta, &room.params);
    let ids = member_ids(&after);
    for p in &legit {
        assert!(ids.contains(&p.id), "a real member was evicted by the trim");
    }
    assert!(ids.len() <= 6);
}

/// Round-4 M2: a member the owner has banned, re-added with a burst of
/// future-dated bans, cannot evict real bans at the `max_user_bans` cap:
/// the cap ranks by whether a ban takes effect, and an owner-removed
/// issuer's bans never do. The owner's ban and a moderator's ban survive.
#[test]
fn a_banned_issuer_cannot_flood_the_ban_cap() {
    let room = capped_room(20, 4);
    let (b, d, x, t) = (room.person(), room.person(), room.person(), room.person());
    let s = room.modded_state(&[&b, &d, &x, &t], &[&d, &b], vec![]);
    let (owner_bans_b, d_bans_x) = (room.owner_ban(&b, 10), room.ban(&d, &x, 11));
    let s = apply_checked(
        &s,
        bans_delta(vec![owner_bans_b.clone(), d_bans_x.clone()]),
        &room.params,
    );
    let flood: Vec<AuthorizedUserBan> = (0..4)
        .map(|i| room.ban(&b, if i % 2 == 0 { &t } else { &d }, 1_000_000 + i))
        .collect();
    let delta = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(vec![b.auth.clone()])),
        bans: Some(flood),
        ..Default::default()
    };
    let after = apply_checked(&s, delta, &room.params);
    assert!(has_ban(&after, &owner_bans_b), "the owner's ban survives");
    assert!(has_ban(&after, &d_bans_x), "the moderator's ban survives");
    let active = active_ids(&after, &room.params);
    assert!(!active.contains(&b.id) && active.contains(&t.id) && active.contains(&d.id));
}

/// The same for a removed mutual-ban member: its bans outside the cycle take no
/// effect, so they are evicted first, and the two mutual bans survive.
#[test]
fn a_removed_cycle_member_cannot_flood_the_ban_cap() {
    let room = capped_room(20, 4);
    let (a, b, t, u) = (room.person(), room.person(), room.person(), room.person());
    let s = room.modded_state(&[&a, &b, &t, &u], &[&a, &b], vec![]);
    let (a_bans_b, b_bans_a) = (room.ban(&a, &b, 10), room.ban(&b, &a, 11));
    let s = apply_checked(
        &s,
        bans_delta(vec![a_bans_b.clone(), b_bans_a.clone()]),
        &room.params,
    );
    let flood: Vec<AuthorizedUserBan> = (0..4)
        .map(|i| room.ban(&a, if i % 2 == 0 { &t } else { &u }, 1_000_000 + i))
        .collect();
    let after = apply_checked(&s, bans_delta(flood), &room.params);
    assert!(has_ban(&after, &a_bans_b) && has_ban(&after, &b_bans_a));
    let active = active_ids(&after, &room.params);
    assert!(!active.contains(&a.id) && !active.contains(&b.id));
    assert!(active.contains(&t.id) && active.contains(&u.id));
}

/// A new mutual ban arriving in the same delta as enough joins to push the
/// room over `max_members`. The trim runs in the `members` step, before
/// cleanup, so it can evict a cycle member's record before the cycle is ever
/// seen (a documented residual: the record is not evidence yet). The room
/// still converges to both moderators removed, with both bans standing, as
/// soon as it merges with a peer that saw the bans before the joins.
#[test]
fn an_over_cap_join_with_a_new_mutual_ban_converges() {
    let room = capped_room(4, 10);
    let (a, b, l) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&a, &b, &l], &[&a, &b], vec![]);
    let newcomers: Vec<Person> = (0..3).map(|_| room.person()).collect();
    let joins = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(
            newcomers.iter().map(|p| p.auth.clone()).collect(),
        )),
        recent_messages: Some(
            newcomers
                .iter()
                .enumerate()
                .map(|(i, p)| room.msg(p, 30 + i as u64))
                .collect(),
        ),
        ..Default::default()
    };
    let bans = vec![room.ban(&a, &b, 10), room.ban(&b, &a, 11)];
    let mut together = joins.clone();
    together.bans = Some(bans.clone());
    let p = apply_checked(&s, together, &room.params);
    let q = apply_checked(&s, bans_delta(bans), &room.params);
    let q = apply_checked(&q, joins, &room.params);
    let (agreed, _) = gossip_until_equal(&p, &q, &room.params, 4);
    let ids = member_ids(&agreed);
    assert!(ids.len() <= 4);
    assert!(!ids.contains(&a.id) && !ids.contains(&b.id));
    assert_eq!(agreed.bans.0.len(), 2, "both mutual bans stand");
}

/// A 3-cycle whose bans reach three different peers first: every gossip
/// order converges to the state where all three arrived together.
#[test]
fn a_three_cycle_is_arrival_order_independent() {
    let room = Room::new();
    let (a, b, c, t) = (room.person(), room.person(), room.person(), room.person());
    let base = room.modded_state(&[&a, &b, &c, &t], &[&a, &b, &c], vec![]);
    let bans = [
        room.ban(&a, &b, 10),
        room.ban(&b, &c, 11),
        room.ban(&c, &a, 12),
    ];
    let together = apply_checked(&base, bans_delta(bans.to_vec()), &room.params);
    let peers: Vec<ChatRoomStateV1> = bans
        .iter()
        .map(|b| apply_checked(&base, bans_delta(vec![b.clone()]), &room.params))
        .collect();
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let mut ps = peers.clone();
        for _round in 0..6 {
            for &i in &order {
                for &j in &order {
                    if i != j {
                        ps[i] = merge(&ps[i], &ps[j], &room.params)
                            .unwrap_or_else(|e| panic!("merge rejected: {e}"));
                    }
                }
            }
        }
        for p in &ps {
            assert!(
                ser(p) == ser(&together),
                "order {order:?} did not converge: members {:?} vs {:?}, {} vs {} bans",
                member_ids(p),
                member_ids(&together),
                p.bans.0.len(),
                together.bans.0.len()
            );
        }
    }
}

/// The owner-ban-wins rule in `resolve_bans` itself, on a state that has not
/// been through the `members` step (which removes owner-banned members early
/// and would otherwise mask it): the UI evaluates bans this way, and so does
/// `post_apply_cleanup` on any state it is handed.
#[test]
fn owner_ban_wins_inside_resolve_bans() {
    let room = Room::new();
    let (x, y, t) = (room.person(), room.person(), room.person());
    let mut s = room.state(
        &[&x, &y, &t],
        vec![
            room.owner_ban(&x, 9),
            room.ban(&x, &y, 10),
            room.ban(&y, &x, 11),
        ],
        vec![room.msg(&x, 1), room.msg(&y, 2), room.msg(&t, 3)],
    );
    s.member_info = room.owner_mods(&[&x, &y]);
    let removed =
        s.members
            .banned_member_ids(&s.ban_evidence, &s.bans, &s.member_info, &room.params);
    assert!(removed.contains(&x.id));
    assert!(
        !removed.contains(&y.id),
        "X has no authority once the owner bans X"
    );
    s.post_apply_cleanup(&room.params).unwrap();
    let active = active_ids(&s, &room.params);
    assert!(active.contains(&y.id) && active.contains(&t.id));
    assert!(!member_ids(&s).contains(&x.id));
}

// ---------------------------------------------------------------------------
// Ban evidence (K', Ian 2026-09-24): round-5 findings.
// ---------------------------------------------------------------------------

/// Round 5 Must Fix 1: A invites T, T invites Y, and one delta carries A's
/// ban on T and T's ban on Y. T's authority over Y is ancestry, so it must
/// not change when the same pass removes Y. `apply_checked` asserts
/// `cleanup(cleanup(S)) == cleanup(S)`.
#[test]
fn an_ancestor_ban_chain_in_one_delta_is_idempotent() {
    let room = Room::new();
    let a = room.person();
    let t = room.person_invited_by(&a);
    let y = room.person_invited_by(&t);
    let s = room.modded_state(&[&a, &t, &y], &[], vec![]);
    let (a_bans_t, t_bans_y) = (room.ban(&a, &t, 10), room.ban(&t, &y, 11));
    let after = apply_checked(
        &s,
        bans_delta(vec![a_bans_t.clone(), t_bans_y.clone()]),
        &room.params,
    );
    let ids = member_ids(&after);
    assert!(ids.contains(&a.id));
    assert!(!ids.contains(&t.id) && !ids.contains(&y.id));
    assert!(has_ban(&after, &a_bans_t) && has_ban(&after, &t_bans_y));
    assert!(evidence_ids(&after).contains(&t.id));
}

/// Round 5 Must Fix 1, second trigger: T's authority over Y is a subtree
/// deputy grant (A lists T), T is banned by a moderator, and Y is pruned for
/// inactivity in the same pass. Y's record and A's grant stay available, so
/// the pass is idempotent.
#[test]
fn a_deputy_ban_on_a_pruned_target_is_idempotent() {
    let room = Room::new();
    let a = room.person();
    let y = room.person_invited_by(&a);
    let (t, d) = (room.person(), room.person());
    let mut s = room.state(
        &[&a, &y, &t, &d],
        vec![],
        vec![room.msg(&a, 1), room.msg(&t, 2), room.msg(&d, 3)],
    );
    let mut infos = room.owner_mods(&[&d]).member_info;
    infos.push(deputies_record(&a, 1, &[&t]));
    s.member_info = MemberInfoV1 { member_info: infos };
    s.verify(&s, &room.params).expect("fixture");
    let after = apply_checked(
        &s,
        bans_delta(vec![room.ban(&d, &t, 10), room.ban(&t, &y, 11)]),
        &room.params,
    );
    let ids = member_ids(&after);
    assert!(!ids.contains(&t.id), "D's ban on T takes effect");
    assert!(!ids.contains(&y.id), "Y is pruned (no messages)");
    assert!(ids.contains(&a.id) && ids.contains(&d.id));
}

/// Ian (2026-09-24): a mutual-ban cycle removes only its members. A bans P;
/// P's invitee B bans A back. A's ban reaches B through P's subtree, so A and
/// B form a cycle and both are removed, but P, the target of A's ban, stays:
/// B's counter-ban shields P at the cost of B. The owner (or another
/// moderator) can still ban P.
#[test]
fn a_mutual_ban_removes_only_the_cycle_members() {
    let room = Room::new();
    let (a, p) = (room.person(), room.person());
    let b = room.person_invited_by(&p);
    let s = room.modded_state(&[&a, &p, &b], &[&a, &b], vec![]);
    let after = apply_checked(
        &s,
        bans_delta(vec![room.ban(&a, &p, 10), room.ban(&b, &a, 11)]),
        &room.params,
    );
    let ids = member_ids(&after);
    assert!(
        !ids.contains(&a.id) && !ids.contains(&b.id),
        "the cycle is removed"
    );
    assert!(ids.contains(&p.id), "the target outside the cycle stays");
    let after = apply_checked(
        &after,
        bans_delta(vec![room.owner_ban(&p, 20)]),
        &room.params,
    );
    assert!(
        !member_ids(&after).contains(&p.id),
        "the owner can still ban P"
    );
}

/// Round 5 Must Fix 2, revised for freenet/river#796: an ordinary member X
/// deputizes sybils it invited and has them ban each other in pairs with
/// far-future dates. Every pair is a cycle, so the bans are live and compete
/// on recency in the non-owner pool. The owner pool keeps its half of the cap,
/// so the owner's ban survives the flood. A moderator's older ban in the
/// non-owner pool is evicted: that is the accepted future-dating residual
/// (freenet/river#413), and the evicted ban's target is back.
#[test]
fn a_sybil_deputy_cycle_flood_cannot_evict_owner_bans() {
    let room = capped_room(20, 4);
    let (v, w, d, x) = (room.person(), room.person(), room.person(), room.person());
    let sybils: Vec<Person> = (0..4).map(|_| room.person_invited_by(&x)).collect();
    let mut members: Vec<&Person> = vec![&v, &w, &d, &x];
    members.extend(sybils.iter());
    let mut s = room.modded_state(&members, &[&d], vec![]);
    s.member_info
        .member_info
        .push(deputies_record(&x, 1, &sybils.iter().collect::<Vec<_>>()));
    s.verify(&s, &room.params).expect("fixture");
    let (owner_bans_v, d_bans_w) = (room.owner_ban(&v, 10), room.ban(&d, &w, 11));
    let s = apply_checked(
        &s,
        bans_delta(vec![owner_bans_v.clone(), d_bans_w.clone()]),
        &room.params,
    );
    let far = 3_000_000_000;
    let flood = vec![
        room.ban(&sybils[0], &sybils[1], far),
        room.ban(&sybils[1], &sybils[0], far),
        room.ban(&sybils[2], &sybils[3], far + 1),
        room.ban(&sybils[3], &sybils[2], far + 1),
    ];
    let after = apply_checked(&s, bans_delta(flood), &room.params);
    assert!(has_ban(&after, &owner_bans_v), "the owner's ban survives");
    assert!(
        !member_ids(&after).contains(&v.id),
        "the owner's ban still takes effect"
    );
    assert!(
        !has_ban(&after, &d_bans_w),
        "accepted residual (#413): the older non-owner ban is evicted"
    );
}

/// The owner pool's guarantee holds under a flood even when the owner has more
/// bans than its share: with a cap of 4 and three owner bans, a flood of
/// far-future non-owner bans evicts only the OLDEST owner ban, and the newest
/// two (half the cap) survive.
#[test]
fn a_flood_leaves_the_newest_half_cap_of_owner_bans() {
    let room = capped_room(20, 4);
    let x = room.person();
    let sybils: Vec<Person> = (0..4).map(|_| room.person_invited_by(&x)).collect();
    let mut members: Vec<&Person> = vec![&x];
    members.extend(sybils.iter());
    let mut s = room.modded_state(&members, &[], vec![]);
    s.member_info
        .member_info
        .push(deputies_record(&x, 1, &sybils.iter().collect::<Vec<_>>()));
    s.verify(&s, &room.params).expect("fixture");
    let gone: Vec<Person> = (0..3).map(|_| room.person()).collect();
    let owner_bans: Vec<AuthorizedUserBan> = gone
        .iter()
        .enumerate()
        .map(|(i, p)| room.owner_ban(p, 10 + i as u64))
        .collect();
    let s = apply_checked(&s, bans_delta(owner_bans.clone()), &room.params);
    let far = 3_000_000_000;
    let flood = vec![
        room.ban(&sybils[0], &sybils[1], far),
        room.ban(&sybils[1], &sybils[0], far),
        room.ban(&sybils[2], &sybils[3], far + 1),
        room.ban(&sybils[3], &sybils[2], far + 1),
    ];
    let after = apply_checked(&s, bans_delta(flood), &room.params);
    assert!(
        !has_ban(&after, &owner_bans[0]),
        "the oldest owner ban goes"
    );
    assert!(
        has_ban(&after, &owner_bans[1]) && has_ban(&after, &owner_bans[2]),
        "the newest half-cap of owner bans survives"
    );
}

/// Round 5 Must Fix 2: a member removed by a moderator's ban re-adds
/// themselves with deputized sybils banning each other and a counter-ban on
/// the moderator. The sybils are in the removed member's subtree, removed
/// before their cycles form, so their bans rank lowest; the moderator's ban
/// survives and the member stays removed.
#[test]
fn a_banned_member_cannot_flood_the_cap_to_escape() {
    let room = capped_room(20, 5);
    let (d, x, t) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&d, &x, &t], &[&d], vec![]);
    let d_bans_x = room.ban(&d, &x, 10);
    let s = apply_checked(&s, bans_delta(vec![d_bans_x.clone()]), &room.params);
    assert!(!member_ids(&s).contains(&x.id));

    let sybils: Vec<Person> = (0..4).map(|_| room.person_invited_by(&x)).collect();
    let far = 3_000_000_000;
    let mut members = vec![x.auth.clone()];
    members.extend(sybils.iter().map(|p| p.auth.clone()));
    let delta = ChatRoomStateV1Delta {
        members: Some(MembersDelta::new(members)),
        member_info: Some(vec![deputies_record(
            &x,
            1,
            &sybils.iter().collect::<Vec<_>>(),
        )]),
        bans: Some(vec![
            room.ban(&sybils[0], &sybils[1], far),
            room.ban(&sybils[1], &sybils[0], far),
            room.ban(&sybils[2], &sybils[3], far + 1),
            room.ban(&sybils[3], &sybils[2], far + 1),
            room.ban(&x, &d, far + 2),
        ]),
        recent_messages: Some(vec![room.msg(&x, 50)]),
        ..Default::default()
    };
    let after = apply_checked(&s, delta, &room.params);
    assert!(has_ban(&after, &d_bans_x), "the moderator's ban survives");
    let ids = member_ids(&after);
    assert!(!ids.contains(&x.id), "X stays removed");
    assert!(ids.contains(&d.id) && ids.contains(&t.id));
}

/// A peer that has the evidence and a peer that never saw the removed
/// member converge: the evidence travels in its own delta.
#[test]
fn ban_evidence_travels_to_a_peer_that_never_saw_the_member() {
    let room = Room::new();
    let (a, b, t) = (room.person(), room.person(), room.person());
    let full = room.modded_state(&[&a, &b, &t], &[&a, &b], vec![]);
    let resolved = apply_checked(
        &full,
        bans_delta(vec![room.ban(&a, &b, 10), room.ban(&b, &a, 11)]),
        &room.params,
    );
    // A peer that only ever knew T.
    let mut fresh = room.state(&[&t], vec![], vec![room.msg(&t, 2)]);
    fresh.member_info = room.owner_mods(&[&a, &b]);
    let (agreed, _) = gossip_until_equal(&fresh, &resolved, &room.params, 4);
    assert_eq!(ser(&agreed), ser(&resolved));
}

/// Ban evidence is self-authorizing: a record whose invite signature does
/// not verify is rejected by `verify` (a full-state PUT) and skipped by
/// `apply_delta` (a merge), and a record for a current member is refused.
#[test]
fn ban_evidence_records_must_verify() {
    use river_core::room_state::ban_evidence::BanEvidenceV1;
    use river_core::room_state::member::AuthorizedMember;
    let room = Room::new();
    let (t, x) = (room.person(), room.person());
    // T's ban on X references both, so neither check below is the
    // "not referenced by a stored ban" one.
    let s = room.state(&[&t], vec![room.ban(&t, &x, 0)], vec![room.msg(&t, 1)]);

    let forged =
        AuthorizedMember::with_signature(x.auth.member.clone(), Signature::from_bytes(&[3u8; 64]));
    let mut bad = s.clone();
    bad.ban_evidence = BanEvidenceV1 {
        members: vec![forged.clone()],
    };
    let err = bad.verify(&bad, &room.params).unwrap_err();
    assert!(err.contains("Invalid signature"), "forged evidence: {err}");

    let mut present = s.clone();
    present.ban_evidence = BanEvidenceV1 {
        members: vec![t.auth.clone()],
    };
    let err = present.verify(&present, &room.params).unwrap_err();
    assert!(
        err.contains("is a current member"),
        "a current member is not evidence: {err}"
    );

    // The genuine record is accepted, so the two rejections above are about
    // the records, not the state.
    let mut good = s.clone();
    good.ban_evidence = BanEvidenceV1 {
        members: vec![x.auth.clone()],
    };
    good.verify(&good, &room.params).unwrap();

    let after = apply_checked(
        &s,
        ChatRoomStateV1Delta {
            ban_evidence: Some(vec![forged]),
            ..Default::default()
        },
        &room.params,
    );
    assert!(
        after.ban_evidence.members.is_empty(),
        "forged record skipped"
    );
}

/// A state with no ban evidence serializes exactly as it did before the
/// field existed, so an old stored state re-serialized by the new contract is
/// byte-identical (fdev verify-merge `state_idempotence` on the real corpus).
#[test]
fn empty_ban_evidence_is_not_serialized() {
    let room = Room::new();
    let t = room.person();
    let s = room.state(&[&t], vec![], vec![room.msg(&t, 1)]);
    assert!(s.ban_evidence.members.is_empty());
    let bytes = ser(&s);
    let needle = b"ban_evidence";
    assert!(
        !bytes.windows(needle.len()).any(|w| w == needle),
        "an empty evidence field must not appear in the bytes"
    );
    let back: ChatRoomStateV1 = ciborium::de::from_reader(&bytes[..]).unwrap();
    assert_eq!(ser(&back), bytes);
}

/// A ban that closes a mutual-ban cycle is live at the `max_user_bans` cap
/// even when an earlier ban already removed its issuer (Ian, 2026-09-24: both
/// bans of a mutual ban stand). Under cap pressure the cycle's bans are kept
/// and bans that take no effect go first.
#[test]
fn cycle_bans_rank_mutual_after_an_earlier_removal() {
    let room = capped_room(20, 4);
    let (a, b, c, t, u) = (
        room.person(),
        room.person(),
        room.person(),
        room.person(),
        room.person(),
    );
    let s = room.modded_state(&[&a, &b, &c, &t, &u], &[&a, &b, &c], vec![]);
    let c_bans_a = room.ban(&c, &a, 5);
    let (a_bans_b, b_bans_a) = (room.ban(&a, &b, 10), room.ban(&b, &a, 11));
    let s = apply_checked(
        &s,
        bans_delta(vec![c_bans_a.clone(), a_bans_b.clone(), b_bans_a.clone()]),
        &room.params,
    );
    let resolution = s
        .members
        .resolve_bans(&s.ban_evidence, &s.bans, &s.member_info, &room.params);
    assert_eq!(resolution.tier(&a_bans_b.id()), BanResolution::MUTUAL);
    assert_eq!(resolution.tier(&b_bans_a.id()), BanResolution::MUTUAL);

    // Two later bans by B that take no effect push the room over the cap.
    let extra = vec![room.ban(&b, &t, 1_000_000), room.ban(&b, &u, 1_000_001)];
    let after = apply_checked(&s, bans_delta(extra), &room.params);
    assert!(has_ban(&after, &c_bans_a));
    assert!(has_ban(&after, &a_bans_b) && has_ban(&after, &b_bans_a));
    let active = active_ids(&after, &room.params);
    assert!(!active.contains(&a.id) && !active.contains(&b.id));
    assert!(active.contains(&t.id) && active.contains(&u.id));
}

/// Only signature-verified bans compete for the one ranked slot per
/// `(issuer, target)` pair: a ban attributed to D for the same target, dated
/// earlier but not signed by D, does not take the slot from D's own ban.
#[test]
fn pair_ranking_counts_only_verified_bans() {
    let room = capped_room(20, 2);
    let (d, x, y, z) = (room.person(), room.person(), room.person(), room.person());
    let s = room.modded_state(&[&d, &x, &y, &z], &[&d], vec![]);
    let real = room.ban(&d, &x, 20);
    let other_sk = SigningKey::generate(&mut OsRng);
    let other = room.ban(&room.person_signed_by(&other_sk), &x, 1);
    let unsigned_by_d = AuthorizedUserBan {
        banned_by: d.id,
        ..other
    };
    let mut bans = vec![
        unsigned_by_d,
        real.clone(),
        room.ban(&d, &y, 30),
        room.ban(&y, &z, 40),
    ];
    BansV1::enforce_user_ban_cap(
        &mut bans,
        2,
        &s.members,
        &s.ban_evidence,
        &s.member_info,
        &room.params,
    );
    assert!(
        bans.iter().any(|b| b.id() == real.id()),
        "D's own ban keeps its slot"
    );
}

/// A member's record is kept as ban evidence when it is only an ISSUER: D,
/// invited by P, bans X; then moderator M bans P, which removes P's subtree,
/// D included. D's record stays as evidence because D's stored ban
/// references D, and D's ban takes no effect (its issuer was removed).
#[test]
fn an_issuers_record_is_kept_as_evidence() {
    let room = Room::new();
    let (p, m, x) = (room.person(), room.person(), room.person());
    let d = room.person_invited_by(&p);
    let s = room.modded_state(&[&p, &m, &x, &d], &[&m, &d], vec![]);
    let d_bans_x = room.ban(&d, &x, 10);
    let s = apply_checked(&s, bans_delta(vec![d_bans_x.clone()]), &room.params);
    assert!(
        !member_ids(&s).contains(&x.id),
        "precondition: D's ban works"
    );
    let after = apply_checked(&s, bans_delta(vec![room.ban(&m, &p, 20)]), &room.params);
    let ids = member_ids(&after);
    assert!(!ids.contains(&p.id) && !ids.contains(&d.id));
    assert!(
        evidence_ids(&after).contains(&d.id),
        "D's record is evidence"
    );
    assert!(has_ban(&after, &d_bans_x), "D's ban stays stored");
    let resolution = after.members.resolve_bans(
        &after.ban_evidence,
        &after.bans,
        &after.member_info,
        &room.params,
    );
    assert_eq!(resolution.tier(&d_bans_x.id()), BanResolution::INERT);
}

/// A member held as ban evidence keeps the ban authority its grants give.
/// Owner-appointed moderator D invited T and never posts; moderator M bans T,
/// so D's record is T's ancestor in M's ban; cleanup prunes D (D issued no
/// ban, so the banner exemption does not apply) and keeps D's record as
/// evidence. A NEW ban by D on U then takes effect: the same authority D would
/// have by re-adding its own public record, and nothing distinguishes a new
/// ban from one issued before the prune (`banned_at` is signed by the issuer).
#[test]
fn a_pruned_moderator_held_as_evidence_keeps_ban_authority() {
    let room = Room::new();
    let (d, m, u) = (room.person(), room.person(), room.person());
    let t = room.person_invited_by(&d);
    let mut s = room.state(
        &[&d, &m, &u, &t],
        vec![],
        vec![room.msg(&m, 1), room.msg(&u, 2), room.msg(&t, 3)],
    );
    s.member_info = room.owner_mods(&[&d, &m]);
    let s = apply_checked(&s, bans_delta(vec![room.ban(&m, &t, 5)]), &room.params);
    assert!(!member_ids(&s).contains(&d.id), "precondition: D is pruned");
    assert!(
        evidence_ids(&s).contains(&d.id),
        "precondition: D is evidence"
    );
    let after = apply_checked(&s, bans_delta(vec![room.ban(&d, &u, 30)]), &room.params);
    assert!(
        !member_ids(&after).contains(&u.id),
        "D's new ban takes effect"
    );
}

/// Only one ban per (issuer, target) is live at the `max_user_bans` cap: a
/// later duplicate of an effective ban is a dead entry, evicted before any
/// live ban however old.
#[test]
fn cap_evicts_a_duplicate_before_any_live_ban() {
    let room = capped_room(20, 3);
    let (d, x, y, z) = (room.person(), room.person(), room.person(), room.person());
    let s = room.modded_state(&[&d, &x, &y, &z], &[&d], vec![]);
    let owner_bans_x = room.owner_ban(&x, 50);
    let d_bans_y = room.ban(&d, &y, 10);
    let d_bans_y_again = room.ban(&d, &y, 60);
    let d_bans_z = room.ban(&d, &z, 20);
    let s = apply_checked(
        &s,
        bans_delta(vec![
            owner_bans_x.clone(),
            d_bans_y.clone(),
            d_bans_z.clone(),
        ]),
        &room.params,
    );
    let after = apply_checked(&s, bans_delta(vec![d_bans_y_again.clone()]), &room.params);
    assert!(has_ban(&after, &owner_bans_x));
    assert!(has_ban(&after, &d_bans_y) && has_ban(&after, &d_bans_z));
    assert!(
        !has_ban(&after, &d_bans_y_again),
        "the duplicate goes first"
    );
}

/// freenet/river#796: with the ban list full of OWNER bans, a deputy's new ban
/// used to rank lowest and be evicted the moment it arrived, so the deputy
/// could ban nobody. Owner bans are guaranteed only half the cap, so the
/// oldest owner ban beyond that half goes instead and the deputy's ban takes
/// effect.
#[test]
fn a_full_list_of_owner_bans_does_not_evict_a_new_deputy_ban() {
    let room = capped_room(20, 4);
    let (d, x) = (room.person(), room.person());
    let s = room.modded_state(&[&d, &x], &[&d], vec![]);
    let gone: Vec<Person> = (0..4).map(|_| room.person()).collect();
    let owner_bans: Vec<AuthorizedUserBan> = gone
        .iter()
        .enumerate()
        .map(|(i, p)| room.owner_ban(p, 10 + i as u64))
        .collect();
    let s = apply_checked(&s, bans_delta(owner_bans.clone()), &room.params);
    assert_eq!(s.bans.0.len(), 4, "the list is full of owner bans");

    let d_bans_x = room.ban(&d, &x, 100);
    let after = apply_checked(&s, bans_delta(vec![d_bans_x.clone()]), &room.params);
    assert!(has_ban(&after, &d_bans_x), "the deputy's ban is kept");
    assert!(
        !member_ids(&after).contains(&x.id),
        "the deputy's ban takes effect"
    );
    assert!(
        !has_ban(&after, &owner_bans[0]),
        "the oldest owner ban is evicted"
    );
    for ban in &owner_bans[1..] {
        assert!(has_ban(&after, ban), "the newer owner bans are kept");
    }
}

/// Owner bans are often rare, so their half of the cap is a floor, not an
/// allocation: with no owner bans, the other pool uses the whole cap and
/// evicts strictly by age.
#[test]
fn rare_owner_bans_leave_the_whole_cap_to_other_bans() {
    let room = capped_room(20, 4);
    let d = room.person();
    let s = room.modded_state(&[&d], &[&d], vec![]);
    let gone: Vec<Person> = (0..5).map(|_| room.person()).collect();
    let d_bans: Vec<AuthorizedUserBan> = gone
        .iter()
        .enumerate()
        .map(|(i, p)| room.ban(&d, p, 10 + i as u64))
        .collect();
    // A single delta may carry at most `max_user_bans` bans.
    let s = apply_checked(&s, bans_delta(d_bans[..4].to_vec()), &room.params);
    let after = apply_checked(&s, bans_delta(vec![d_bans[4].clone()]), &room.params);
    assert_eq!(after.bans.0.len(), 4, "the whole cap is used");
    assert!(!has_ban(&after, &d_bans[0]), "the oldest is evicted");
    for ban in &d_bans[1..] {
        assert!(has_ban(&after, ban));
    }
}

/// The two pools trade unused slots by age: with one owner ban and a cap of 4,
/// three other bans fit, and a newer owner ban evicts the oldest OTHER ban
/// once the owner pool is within its share, because the other pool is then
/// the one over its share.
#[test]
fn a_new_owner_ban_evicts_the_oldest_ban_of_the_pool_over_its_share() {
    let room = capped_room(20, 4);
    let d = room.person();
    let s = room.modded_state(&[&d], &[&d], vec![]);
    let gone: Vec<Person> = (0..5).map(|_| room.person()).collect();
    let old_owner = room.owner_ban(&gone[0], 5);
    let d_bans: Vec<AuthorizedUserBan> = gone[1..4]
        .iter()
        .enumerate()
        .map(|(i, p)| room.ban(&d, p, 10 + i as u64))
        .collect();
    let mut first = vec![old_owner.clone()];
    first.extend(d_bans.iter().cloned());
    let s = apply_checked(&s, bans_delta(first), &room.params);
    assert_eq!(s.bans.0.len(), 4);

    let new_owner = room.owner_ban(&gone[4], 100);
    let after = apply_checked(&s, bans_delta(vec![new_owner.clone()]), &room.params);
    assert!(
        has_ban(&after, &old_owner) && has_ban(&after, &new_owner),
        "both owner bans fit the owner pool's share, so the older one is kept"
    );
    assert!(
        !has_ban(&after, &d_bans[0]),
        "the oldest other ban goes, its pool being over its share"
    );
    assert!(has_ban(&after, &d_bans[1]) && has_ban(&after, &d_bans[2]));
}

/// An odd cap's spare slot goes to the newer ban, whichever pool it is in.
/// Cap 3: each pool is guaranteed one slot and the third is shared by age.
#[test]
fn an_odd_caps_spare_slot_goes_to_the_newer_ban() {
    for owner_older in [true, false] {
        let room = capped_room(20, 3);
        let d = room.person();
        let s = room.modded_state(&[&d], &[&d], vec![]);
        let gone: Vec<Person> = (0..4).map(|_| room.person()).collect();
        let (o_at, d_at) = if owner_older { (10, 15) } else { (15, 10) };
        let o1 = room.owner_ban(&gone[0], o_at);
        let o2 = room.owner_ban(&gone[1], 20);
        let d1 = room.ban(&d, &gone[2], d_at);
        let d2 = room.ban(&d, &gone[3], 30);
        let s = apply_checked(
            &s,
            bans_delta(vec![o1.clone(), o2.clone(), d1.clone()]),
            &room.params,
        );
        let after = apply_checked(&s, bans_delta(vec![d2.clone()]), &room.params);
        assert_eq!(after.bans.0.len(), 3);
        assert!(has_ban(&after, &o2) && has_ban(&after, &d2));
        if owner_older {
            assert!(!has_ban(&after, &o1) && has_ban(&after, &d1));
        } else {
            assert!(has_ban(&after, &o1) && !has_ban(&after, &d1));
        }
    }
}

/// At cap 1 neither pool is guaranteed a slot, so the newer ban wins.
#[test]
fn cap_one_keeps_the_newer_ban_whatever_its_pool() {
    for owner_newer in [true, false] {
        let room = capped_room(20, 1);
        let d = room.person();
        let s = room.modded_state(&[&d], &[&d], vec![]);
        let (x, y) = (room.person(), room.person());
        let (o_at, d_at) = if owner_newer { (20, 10) } else { (10, 20) };
        let o = room.owner_ban(&x, o_at);
        let db = room.ban(&d, &y, d_at);
        let s = apply_checked(&s, bans_delta(vec![o.clone()]), &room.params);
        let after = apply_checked(&s, bans_delta(vec![db.clone()]), &room.params);
        assert_eq!(after.bans.0.len(), 1);
        assert_eq!(has_ban(&after, &o), owner_newer);
        assert_eq!(has_ban(&after, &db), !owner_newer);
    }
}

/// One cap pass that drops a dead entry AND then evicts from a pool: cap 4,
/// three owner bans, a deputy's ban plus a later duplicate of it, and another
/// deputy ban. Six bans, two to go: the duplicate (dead) first, then the
/// oldest owner ban, the owner pool being the only one over its share. The
/// result does not depend on input order, and a second pass changes nothing.
#[test]
fn a_dead_entry_and_a_pool_eviction_in_one_pass() {
    let room = capped_room(20, 4);
    let (d, x, y) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&d, &x, &y], &[&d], vec![]);
    let gone: Vec<Person> = (0..3).map(|_| room.person()).collect();
    let owner_bans: Vec<AuthorizedUserBan> = gone
        .iter()
        .enumerate()
        .map(|(i, p)| room.owner_ban(p, 10 + i as u64))
        .collect();
    let d_bans_x = room.ban(&d, &x, 20);
    let d_bans_x_again = room.ban(&d, &x, 40);
    let d_bans_y = room.ban(&d, &y, 21);
    let mut input = owner_bans.clone();
    input.extend([d_bans_x.clone(), d_bans_x_again.clone(), d_bans_y.clone()]);

    let cap = |bans: &[AuthorizedUserBan]| {
        let mut bans = bans.to_vec();
        BansV1::enforce_user_ban_cap(
            &mut bans,
            4,
            &s.members,
            &s.ban_evidence,
            &s.member_info,
            &room.params,
        );
        bans
    };
    let out = cap(&input);
    let ids: Vec<_> = out.iter().map(|b| b.id()).collect();
    let want: Vec<_> = [&owner_bans[1], &owner_bans[2], &d_bans_x, &d_bans_y]
        .iter()
        .map(|b| b.id())
        .collect();
    assert_eq!(
        ids, want,
        "duplicate and oldest owner ban evicted, in stored order"
    );

    let mut reversed = input.clone();
    reversed.reverse();
    let mut rotated = input.clone();
    rotated.rotate_left(2);
    assert_eq!(cap(&reversed), out, "input order does not matter");
    assert_eq!(cap(&rotated), out, "input order does not matter");
    assert_eq!(cap(&out), out, "a second pass changes nothing");
}

/// More dead entries than the overflow: only the oldest dead entries go, and
/// no live ban is touched. Cap 5 with four live bans, then three later
/// duplicates of the deputy's pairs (dead): two over, so the two oldest
/// duplicates go and the newest stays.
#[test]
fn surplus_dead_entries_go_oldest_first_and_spare_live_bans() {
    let room = capped_room(20, 5);
    let (d, x, y) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&d, &x, &y], &[&d], vec![]);
    let gone: Vec<Person> = (0..2).map(|_| room.person()).collect();
    let o1 = room.owner_ban(&gone[0], 10);
    let o2 = room.owner_ban(&gone[1], 11);
    let dx = room.ban(&d, &x, 12);
    let dy = room.ban(&d, &y, 13);
    let s = apply_checked(
        &s,
        bans_delta(vec![o1.clone(), o2.clone(), dx.clone(), dy.clone()]),
        &room.params,
    );
    // Three duplicates of live pairs: all dead. 7 bans against cap 5, so two
    // go: the two OLDEST duplicates. The newest duplicate stays as a dead
    // entry, since the set is then within the cap.
    let dup1 = room.ban(&d, &x, 20);
    let dup2 = room.ban(&d, &y, 21);
    let dup3 = room.ban(&d, &x, 22);
    let after = apply_checked(
        &s,
        bans_delta(vec![dup1.clone(), dup2.clone(), dup3.clone()]),
        &room.params,
    );
    for live in [&o1, &o2, &dx, &dy] {
        assert!(has_ban(&after, live), "no live ban is evicted");
    }
    assert!(!has_ban(&after, &dup1) && !has_ban(&after, &dup2));
    assert!(has_ban(&after, &dup3), "only the overflow is evicted");
}

/// Both pools over their share with several evictions in one pass. Cap 4
/// (share 2): owner bans at 10, 30, 40, 50 and deputy bans at 20, 60, 70.
/// Seven live, three go: each pool's bans beyond its share, oldest first, so
/// the newest two of each pool survive. (With an even cap and both pools at
/// or over their share, the candidates number exactly the overflow; the age
/// choice between pools is pinned by `an_odd_caps_spare_slot_goes_to_the_newer_ban`.)
#[test]
fn several_evictions_take_each_pools_oldest_beyond_its_share() {
    let room = capped_room(20, 4);
    let d = room.person();
    let s = room.modded_state(&[&d], &[&d], vec![]);
    let gone: Vec<Person> = (0..7).map(|_| room.person()).collect();
    let o: Vec<AuthorizedUserBan> = [10, 30, 40, 50]
        .iter()
        .zip(&gone[..4])
        .map(|(t, p)| room.owner_ban(p, *t))
        .collect();
    let db: Vec<AuthorizedUserBan> = [20, 60, 70]
        .iter()
        .zip(&gone[4..])
        .map(|(t, p)| room.ban(&d, p, *t))
        .collect();
    let s = apply_checked(&s, bans_delta(o.clone()), &room.params);
    let after = apply_checked(&s, bans_delta(db.clone()), &room.params);
    assert_eq!(after.bans.0.len(), 4);
    assert!(!has_ban(&after, &o[0]) && !has_ban(&after, &o[1]));
    assert!(!has_ban(&after, &db[0]));
    assert!(has_ban(&after, &o[2]) && has_ban(&after, &o[3]));
    assert!(has_ban(&after, &db[1]) && has_ban(&after, &db[2]));
}

/// A ban that takes no effect is dead and goes before any live ban, however
/// new: a plain member B bans D, whom B has no authority over (D is present,
/// so the ban can be judged, and it is inert). It is the NEWEST ban, yet it is
/// the one evicted at the cap.
#[test]
fn an_inert_ban_goes_before_older_live_bans() {
    let room = capped_room(20, 3);
    let (b, d, x) = (room.person(), room.person(), room.person());
    let s = room.modded_state(&[&b, &d, &x], &[&d], vec![]);
    let gone: Vec<Person> = (0..2).map(|_| room.person()).collect();
    let o = room.owner_ban(&gone[0], 10);
    let d_ban = room.ban(&d, &gone[1], 11);
    let d_bans_x = room.ban(&d, &x, 12);
    let s = apply_checked(
        &s,
        bans_delta(vec![o.clone(), d_ban.clone(), d_bans_x.clone()]),
        &room.params,
    );
    let inert = room.ban(&b, &d, 100);
    let after = apply_checked(&s, bans_delta(vec![inert.clone()]), &room.params);
    assert!(!has_ban(&after, &inert), "the inert ban is evicted");
    for live in [&o, &d_ban, &d_bans_x] {
        assert!(has_ban(&after, live), "no live ban is evicted");
    }
}

/// A ban on an id held nowhere by an issuer known nowhere is dead (its
/// signature cannot be checked), and is evicted before older live bans.
#[test]
fn an_absent_target_ban_by_an_absent_issuer_is_dead() {
    let room = capped_room(20, 2);
    let d = room.person();
    let s = room.modded_state(&[&d], &[&d], vec![]);
    let gone: Vec<Person> = (0..2).map(|_| room.person()).collect();
    let o = room.owner_ban(&gone[0], 10);
    let d_ban = room.ban(&d, &gone[1], 11);
    let stranger = room.person();
    let by_stranger = room.ban(&stranger, &room.person(), 100);
    let mut bans = vec![by_stranger.clone(), d_ban.clone(), o.clone()];
    BansV1::enforce_user_ban_cap(
        &mut bans,
        2,
        &s.members,
        &s.ban_evidence,
        &s.member_info,
        &room.params,
    );
    let ids: Vec<_> = bans.iter().map(|b| b.id()).collect();
    assert_eq!(ids, vec![o.id(), d_ban.id()]);
}

/// The widened residual of freenet/river#796, pinned so a future fix flips it
/// deliberately: a plain member with no deputy grant and no invitees floods
/// far-future bans against random ids whose record is held nowhere (each
/// counts as live). With three owner bans at cap 4 the oldest owner ban is
/// evicted; the newest two (the owner pool's share) survive.
#[test]
fn a_plain_members_random_target_flood_evicts_owner_bans_down_to_the_share() {
    let room = capped_room(20, 4);
    let m = room.person();
    let s = room.modded_state(&[&m], &[], vec![]);
    let gone: Vec<Person> = (0..3).map(|_| room.person()).collect();
    let owner_bans: Vec<AuthorizedUserBan> = gone
        .iter()
        .enumerate()
        .map(|(i, p)| room.owner_ban(p, 10 + i as u64))
        .collect();
    let s = apply_checked(&s, bans_delta(owner_bans.clone()), &room.params);
    let far = 3_000_000_000;
    let flood: Vec<AuthorizedUserBan> = (0..4)
        .map(|i| room.ban(&m, &room.person(), far + i))
        .collect();
    let after = apply_checked(&s, bans_delta(flood.clone()), &room.params);
    assert!(
        !has_ban(&after, &owner_bans[0]),
        "the oldest owner ban goes"
    );
    assert!(has_ban(&after, &owner_bans[1]) && has_ban(&after, &owner_bans[2]));
    assert!(
        has_ban(&after, &flood[2]) && has_ban(&after, &flood[3]),
        "the newest flood bans fill the other pool"
    );
}

/// An evidence record whose inviter leaves `members` at the `max_members` trim
/// (which runs before cleanup) no longer reaches the owner, so every reader
/// leaves it out and cleanup drops it. The result verifies and cleanup is a
/// fixpoint. (The issuer's own record is lost to the trim before it can
/// become evidence, a documented residual, so its ban is swept.)
#[test]
fn evidence_whose_inviter_is_trimmed_is_dropped() {
    let room = Room::new();
    let mut people: Vec<Person> = (0..4).map(|_| room.person()).collect();
    people.sort_by_key(|p| p.id);
    // The trim evicts the highest id among equal chain lengths.
    let i = people.pop().unwrap();
    let e = room.person_invited_by(&i);
    let mut members: Vec<&Person> = people.iter().collect();
    members.push(&i);
    members.push(&e);
    let s = room.modded_state(&members, &[], vec![]);
    let s = apply_checked(&s, bans_delta(vec![room.ban(&i, &e, 10)]), &room.params);
    assert!(evidence_ids(&s).contains(&e.id), "precondition");

    let lower = AuthorizedConfigurationV1::new(
        Configuration {
            configuration_version: 2,
            max_members: people.len(),
            ..room.config.configuration.clone()
        },
        &room.owner_sk,
    );
    let after = apply_checked(
        &s,
        ChatRoomStateV1Delta {
            configuration: Some(lower),
            ..Default::default()
        },
        &room.params,
    );
    assert!(
        !member_ids(&after).contains(&i.id),
        "precondition: I is trimmed"
    );
    assert!(
        evidence_ids(&after).is_empty(),
        "E no longer reaches the owner"
    );
}

/// A state stored before the `ban_evidence` field existed (no key at all in
/// its CBOR map) deserializes, cleans up into the new shape and verifies.
#[test]
fn a_state_without_the_ban_evidence_field_deserializes_and_cleans_up() {
    let room = Room::new();
    let (a, b, t) = (room.person(), room.person(), room.person());
    let mut s = room.modded_state(&[&a, &b, &t], &[&a, &b], vec![]);
    s.bans = BansV1(vec![room.ban(&a, &b, 10), room.ban(&b, &a, 11)]);
    // Serialize, then drop the key to get the old shape.
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&s, &mut bytes).unwrap();
    let value: ciborium::value::Value = ciborium::de::from_reader(&bytes[..]).unwrap();
    let map = value.into_map().unwrap();
    assert!(
        !map.iter().any(|(k, _)| k.as_text() == Some("ban_evidence")),
        "an empty field is not serialized, so this is the old shape"
    );
    let mut old = Vec::new();
    ciborium::ser::into_writer(&ciborium::value::Value::Map(map), &mut old).unwrap();
    let mut decoded: ChatRoomStateV1 = ciborium::de::from_reader(&old[..]).unwrap();
    assert!(decoded.ban_evidence.members.is_empty());
    decoded.post_apply_cleanup(&room.params).unwrap();
    decoded.verify(&decoded, &room.params).unwrap();
    let ids = member_ids(&decoded);
    assert!(!ids.contains(&a.id) && !ids.contains(&b.id) && ids.contains(&t.id));
    assert!(evidence_ids(&decoded).contains(&a.id) && evidence_ids(&decoded).contains(&b.id));
}

/// Bans stored before ban evidence existed, by a present member on targets
/// whose records are held nowhere (they left before the upgrade), keep their
/// place at the `max_user_bans` cap as they had it before: ranked with the
/// bans that take effect. The Official room held 120-144 such bans at its
/// cap. Here the room is at its cap of 3 when a ban the room cannot verify
/// arrives: that one is evicted, not the oldest stored ban.
#[test]
fn bans_on_targets_held_nowhere_keep_their_cap_rank() {
    let room = capped_room(20, 3);
    let (d, t) = (room.person(), room.person());
    let (g1, g2, g3) = (room.person(), room.person(), room.person()); // never members
    let s = room.modded_state(&[&d, &t], &[], vec![]);
    let (d_g1, d_g2) = (room.ban(&d, &g1, 1), room.ban(&d, &g2, 2));
    let owner_g3 = room.owner_ban(&g3, 3);
    let s = apply_checked(
        &s,
        bans_delta(vec![d_g1.clone(), d_g2.clone(), owner_g3.clone()]),
        &room.params,
    );
    let resolution = s
        .members
        .resolve_bans(&s.ban_evidence, &s.bans, &s.member_info, &room.params);
    assert_eq!(
        resolution.tier(&d_g1.id()),
        BanResolution::INERT,
        "precondition: authority over a target held nowhere cannot be judged"
    );
    // A ban by someone the room has no record of, newer than all three.
    let stranger = room.person();
    let after = apply_checked(
        &s,
        bans_delta(vec![room.ban(&stranger, &t, 100)]),
        &room.params,
    );
    assert!(has_ban(&after, &d_g1) && has_ban(&after, &d_g2) && has_ban(&after, &owner_g3));
    assert!(active_ids(&after, &room.params).contains(&t.id));
}

/// owner -> c1 -> ... -> c{n}, all members who have posted.
fn invite_chain_of(room: &Room, n: usize) -> Vec<Person> {
    let mut chain = vec![room.person()];
    while chain.len() < n {
        let next = room.person_invited_by(chain.last().unwrap());
        chain.push(next);
    }
    chain
}

/// Ban evidence keeps a record at depth `MAX_EVIDENCE_DEPTH` (4 invites from
/// the owner) and not one at depth 5. Either way the ban stands, its target
/// stays removed, and cleanup is a fixpoint.
#[test]
fn evidence_depth_cap_keeps_depth_4_and_drops_depth_5() {
    use river_core::room_state::ban_evidence::MAX_EVIDENCE_DEPTH;
    for depth in [MAX_EVIDENCE_DEPTH, MAX_EVIDENCE_DEPTH + 1] {
        let room = capped_room(20, 10);
        let chain = invite_chain_of(&room, depth);
        let members: Vec<&Person> = chain.iter().collect();
        let s = room.modded_state(&members, &[], vec![]);
        let (top, deepest) = (&chain[0], &chain[depth - 1]);
        let ban = room.ban(top, deepest, 10);
        let after = apply_checked(&s, bans_delta(vec![ban.clone()]), &room.params);
        assert!(!member_ids(&after).contains(&deepest.id), "depth {depth}");
        assert!(has_ban(&after, &ban), "depth {depth}: the ban stands");
        assert_eq!(
            evidence_ids(&after).contains(&deepest.id),
            depth <= MAX_EVIDENCE_DEPTH,
            "depth {depth}"
        );
    }
}

/// Lowering `max_members` lowers the evidence depth cap with it
/// (`min(max_members, MAX_EVIDENCE_DEPTH)`): a record deeper than the new cap
/// leaves the evidence. Two peers that took the ban and the new configuration
/// in opposite orders end in the same state.
#[test]
fn lowering_max_members_drops_deeper_evidence_and_converges() {
    let room = capped_room(20, 10);
    let chain = invite_chain_of(&room, 3);
    let (a, b, c) = (&chain[0], &chain[1], &chain[2]);
    let s = room.modded_state(&[a, b, c], &[], vec![]);
    let ban = bans_delta(vec![room.ban(a, c, 10)]);
    let lower = ChatRoomStateV1Delta {
        configuration: Some(AuthorizedConfigurationV1::new(
            Configuration {
                configuration_version: 2,
                max_members: 2,
                ..room.config.configuration.clone()
            },
            &room.owner_sk,
        )),
        ..Default::default()
    };
    let banned_first = apply_checked(&s, ban.clone(), &room.params);
    assert!(
        evidence_ids(&banned_first).contains(&c.id),
        "depth 3 <= cap 4"
    );
    let x = apply_checked(&banned_first, lower.clone(), &room.params);
    let lowered_first = apply_checked(&s, lower, &room.params);
    let y = apply_checked(&lowered_first, ban, &room.params);
    assert!(!evidence_ids(&x).contains(&c.id), "depth 3 > cap 2");
    assert_eq!(ser(&x), ser(&y), "both orders end in one state");
    let (agreed, _) = gossip_until_equal(&x, &y, &room.params, 3);
    assert_eq!(ser(&agreed), ser(&x));
}

/// An owner ban on a member who is already absent (held as ban evidence)
/// sweeps that member's bans, and with them the evidence only those bans
/// referenced. The same owner ban on a present member does the same, so the
/// result does not depend on whether the member had left yet.
#[test]
fn an_owner_ban_on_an_absent_member_sweeps_their_bans_and_evidence() {
    let room = capped_room(20, 10);
    let x = room.person();
    let x1 = room.person_invited_by(&x);
    let x2 = room.person_invited_by(&x1);
    let t = room.person();
    // X is no longer a member: X ships its own record as evidence with a ban
    // on its invitee X2 and the chain linking them.
    let s = room.modded_state(&[&t], &[], vec![]);
    let x_bans = room.ban(&x, &x2, 10);
    let s = apply_checked(
        &s,
        ChatRoomStateV1Delta {
            bans: Some(vec![x_bans.clone()]),
            ban_evidence: Some(vec![x.auth.clone(), x1.auth.clone(), x2.auth.clone()]),
            ..Default::default()
        },
        &room.params,
    );
    assert!(has_ban(&s, &x_bans), "precondition");
    assert!(evidence_ids(&s).contains(&x.id), "precondition");
    let owner_bans_x = room.owner_ban(&x, 20);
    let after = apply_checked(&s, bans_delta(vec![owner_bans_x.clone()]), &room.params);
    assert!(has_ban(&after, &owner_bans_x));
    assert!(!has_ban(&after, &x_bans), "X's ban is swept");
    assert!(
        evidence_ids(&after).is_empty(),
        "nothing references the records"
    );
    assert!(member_ids(&after).contains(&t.id));
}

/// A member, or a former member, cannot make the ban evidence grow beyond the
/// depth cap. A delta carrying a ban on the tail of a 1500-deep invite chain
/// they signed, with the whole chain as evidence, is over the evidence delta
/// bound (`2 x max_user_bans x depth cap`, 80 here) and refused. The same
/// with a 70-deep chain applies and keeps at most the records within
/// `MAX_EVIDENCE_DEPTH` invites.
#[test]
fn a_deep_signed_chain_cannot_grow_the_evidence() {
    use river_core::room_state::ban_evidence::MAX_EVIDENCE_DEPTH;
    for x_is_member in [true, false] {
        for depth in [1500, 70] {
            let room = capped_room(20, 10);
            let t = room.person();
            let chain = invite_chain_of(&room, depth);
            let x = &chain[0];
            let tail = chain.last().unwrap();
            let mut members = vec![&t];
            if x_is_member {
                members.push(x);
            }
            let s = room.modded_state(&members, &[], vec![]);
            let evidence: Vec<AuthorizedMember> = chain
                .iter()
                .skip(usize::from(x_is_member))
                .map(|p| p.auth.clone())
                .collect();
            let delta = ChatRoomStateV1Delta {
                bans: Some(vec![room.ban(x, tail, 10)]),
                ban_evidence: Some(evidence),
                ..Default::default()
            };
            if depth > 80 {
                let mut after = s.clone();
                let err = after
                    .apply_delta(&s, &room.params, &Some(delta))
                    .unwrap_err();
                assert!(err.contains("exceeds 80"), "{err}");
                continue;
            }
            let after = apply_checked(&s, delta, &room.params);
            assert!(
                after.ban_evidence.members.len() <= 2 * MAX_EVIDENCE_DEPTH,
                "x_is_member {x_is_member}: {} records kept",
                after.ban_evidence.members.len()
            );
            assert!(member_ids(&after).contains(&t.id));
        }
    }
}

/// The `member_info` of a member held only as ban evidence is kept when it is
/// a strict ancestor of a stored ban's target: A invited T and deputized D;
/// D bans T on A's grant. A never posts and is pruned, so A's record and A's
/// `member_info` (an evidence ancestor of T, kept whatever it holds) are
/// kept. The ban keeps taking effect across cleanups.
#[test]
fn a_grant_a_ban_rests_on_is_kept_for_an_evidence_member() {
    let room = Room::new();
    let (a, d) = (room.person(), room.person());
    let t = room.person_invited_by(&a);
    let mut s = room.state(
        &[&a, &d, &t],
        vec![],
        vec![room.msg(&d, 1), room.msg(&t, 2)],
    );
    s.member_info = MemberInfoV1 {
        member_info: vec![deputies_record(&a, 1, &[&d])],
    };
    s.verify(&s, &room.params).unwrap();
    let after = apply_checked(&s, bans_delta(vec![room.ban(&d, &t, 10)]), &room.params);
    let ids = member_ids(&after);
    assert!(
        !ids.contains(&t.id) && !ids.contains(&a.id),
        "T banned, A pruned"
    );
    assert!(evidence_ids(&after).contains(&a.id));
    assert!(
        after.member_info.deputies_of(a.id).contains(&d.id),
        "A's grant is kept"
    );
    let mut again = after.clone();
    again.post_apply_cleanup(&room.params).unwrap();
    assert!(!member_ids(&again).contains(&t.id));
}

/// A member at invite depth `depth`: `depth - 1` posting ancestors above it.
fn person_at_depth(room: &Room, depth: usize) -> (Person, Vec<Person>) {
    let mut chain = invite_chain_of(room, depth);
    let p = chain.pop().unwrap();
    (p, chain)
}

/// Mutual bans between moderators deeper than `MAX_EVIDENCE_DEPTH` invites
/// (intended under the depth cap; no real room has such a member, the
/// deepest chain seen is 3). Two deep moderators who ban each other in one
/// delta are both removed, and both bans lapse with their records, so both
/// may return. Delivered one at a time, the first ban removes the other
/// moderator and the second is swept with its issuer's record, so the outcome
/// depends on delivery order; every pair of peers still converges.
#[test]
fn a_mutual_ban_between_deep_moderators_lapses_and_converges() {
    use river_core::room_state::ban_evidence::MAX_EVIDENCE_DEPTH;
    let room = capped_room(40, 10);
    let (a, a_up) = person_at_depth(&room, MAX_EVIDENCE_DEPTH + 1);
    let (b, b_up) = person_at_depth(&room, MAX_EVIDENCE_DEPTH + 1);
    let mut members: Vec<&Person> = a_up.iter().chain(b_up.iter()).collect();
    members.extend([&a, &b]);
    let s = room.modded_state(&members, &[&a, &b], vec![]);
    let (ab, ba) = (room.ban(&a, &b, 10), room.ban(&b, &a, 11));
    let both = apply_checked(&s, bans_delta(vec![ab.clone(), ba.clone()]), &room.params);
    let ids = member_ids(&both);
    assert!(!ids.contains(&a.id) && !ids.contains(&b.id), "both removed");
    assert!(
        !has_ban(&both, &ab) && !has_ban(&both, &ba),
        "both bans lapse"
    );
    let a_first = apply_checked(
        &apply_checked(&s, bans_delta(vec![ab.clone()]), &room.params),
        bans_delta(vec![ba.clone()]),
        &room.params,
    );
    let b_first = apply_checked(
        &apply_checked(&s, bans_delta(vec![ba.clone()]), &room.params),
        bans_delta(vec![ab.clone()]),
        &room.params,
    );
    assert!(member_ids(&a_first).contains(&a.id) && !member_ids(&a_first).contains(&b.id));
    assert!(member_ids(&b_first).contains(&b.id) && !member_ids(&b_first).contains(&a.id));
    for (x, y) in [(&a_first, &b_first), (&both, &a_first), (&both, &b_first)] {
        let (p, q) = (
            merge(x, y, &room.params).unwrap(),
            merge(y, x, &room.params).unwrap(),
        );
        let (agreed, _) = gossip_until_equal(&p, &q, &room.params, 4);
        agreed.verify(&agreed, &room.params).unwrap();
    }
}

/// One moderator deeper than the cap, one shallow, banning each other. The
/// deep one's ban lapses with its record; the shallow one's ban stands and
/// keeps the deep one out. Whether the shallow one is removed depends on
/// delivery order (removed when both bans arrive together, kept when its own
/// ban arrives first); peers converge either way.
#[test]
fn a_mutual_ban_with_one_deep_moderator_converges() {
    use river_core::room_state::ban_evidence::MAX_EVIDENCE_DEPTH;
    let room = capped_room(40, 10);
    let (a, a_up) = person_at_depth(&room, MAX_EVIDENCE_DEPTH + 1);
    let b = room.person();
    let mut members: Vec<&Person> = a_up.iter().collect();
    members.extend([&a, &b]);
    let s = room.modded_state(&members, &[&a, &b], vec![]);
    let (ab, ba) = (room.ban(&a, &b, 10), room.ban(&b, &a, 11));
    let both = apply_checked(&s, bans_delta(vec![ab.clone(), ba.clone()]), &room.params);
    let a_first = apply_checked(
        &apply_checked(&s, bans_delta(vec![ab.clone()]), &room.params),
        bans_delta(vec![ba.clone()]),
        &room.params,
    );
    let b_first = apply_checked(
        &apply_checked(&s, bans_delta(vec![ba.clone()]), &room.params),
        bans_delta(vec![ab.clone()]),
        &room.params,
    );
    assert!(!member_ids(&both).contains(&b.id), "together: B is removed");
    assert!(
        member_ids(&b_first).contains(&b.id),
        "B's ban first: B stays"
    );
    for state in [&both, &a_first, &b_first] {
        assert!(!member_ids(state).contains(&a.id));
        assert!(has_ban(state, &ba) && !has_ban(state, &ab));
    }
    for (x, y) in [(&a_first, &b_first), (&both, &a_first), (&both, &b_first)] {
        let (agreed, _) = gossip_until_equal(x, y, &room.params, 4);
        assert!(!member_ids(&agreed).contains(&a.id));
        assert!(has_ban(&agreed, &ba) && !has_ban(&agreed, &ab));
    }
}

/// A revoked grant stays revoked once its author is held as ban evidence. A
/// invited T and deputized D (version 1); D bans T on that grant; A is pruned
/// and kept as evidence. A then publishes version 2 with no deputies. When
/// version 1 is offered again, version 2 still wins: cleanup keeps an
/// evidence ancestor's `member_info` whatever it holds, so it never drops the
/// newer record and lets the older one back in.
#[test]
fn a_revoked_grant_of_an_evidence_member_cannot_be_replayed() {
    let room = Room::new();
    let (a, d) = (room.person(), room.person());
    let t = room.person_invited_by(&a);
    let mut s = room.state(
        &[&a, &d, &t],
        vec![],
        vec![room.msg(&d, 1), room.msg(&t, 2)],
    );
    let v1 = deputies_record(&a, 1, &[&d]);
    s.member_info = MemberInfoV1 {
        member_info: vec![v1.clone()],
    };
    s.verify(&s, &room.params).unwrap();
    let s = apply_checked(&s, bans_delta(vec![room.ban(&d, &t, 10)]), &room.params);
    assert!(
        evidence_ids(&s).contains(&a.id),
        "precondition: A is evidence"
    );
    let info_delta = |r: AuthorizedMemberInfo| ChatRoomStateV1Delta {
        member_info: Some(vec![r]),
        ..Default::default()
    };
    let revoked = apply_checked(&s, info_delta(deputies_record(&a, 2, &[])), &room.params);
    assert!(
        revoked.member_info.deputies_of(a.id).is_empty(),
        "v2 revokes"
    );
    let replayed = apply_checked(&revoked, info_delta(v1), &room.params);
    assert!(
        replayed.member_info.deputies_of(a.id).is_empty(),
        "the older grant does not come back"
    );
}

/// Two peers holding different valid evidence records for one id (T invited
/// by the owner, and a second record for T's key signed by member Y) end with
/// the same record, the one with the lower digest, whichever merges first,
/// and the result is a cleanup fixpoint.
#[test]
fn competing_evidence_records_for_one_id_converge() {
    use river_core::room_state::ban_evidence::evidence_digest;
    let room = Room::new();
    let (d, y, t) = (room.person(), room.person(), room.person());
    let ban = room.ban(&d, &t, 10);
    let by_owner = t.auth.clone();
    let by_y = AuthorizedMember::new(
        Member {
            owner_member_id: room.owner_id,
            invited_by: y.id,
            member_vk: t.sk.verifying_key(),
        },
        &y.sk,
    );
    let lower = std::cmp::min_by_key(by_owner.clone(), by_y.clone(), evidence_digest);
    // P1 saw T as a member and removed them: it holds the owner's record.
    let p1 = apply_checked(
        &room.modded_state(&[&d, &y, &t], &[&d], vec![]),
        bans_delta(vec![ban.clone()]),
        &room.params,
    );
    // P2 never had T as a member: it learned the ban with Y's record.
    let p2 = apply_checked(
        &room.modded_state(&[&d, &y], &[&d], vec![]),
        ChatRoomStateV1Delta {
            bans: Some(vec![ban.clone()]),
            ban_evidence: Some(vec![by_y.clone()]),
            ..Default::default()
        },
        &room.params,
    );
    let record_of = |s: &ChatRoomStateV1| {
        s.ban_evidence
            .members
            .iter()
            .find(|m| m.member.id() == t.id)
            .cloned()
    };
    assert_eq!(record_of(&p1), Some(by_owner.clone()));
    assert_eq!(record_of(&p2), Some(by_y.clone()));
    let a = merge(&p1, &p2, &room.params).unwrap();
    let b = merge(&p2, &p1, &room.params).unwrap();
    let (agreed, _) = gossip_until_equal(&a, &b, &room.params, 3);
    assert_eq!(record_of(&agreed), Some(lower));
    assert!(!member_ids(&agreed).contains(&t.id));
}

/// Split delivery with competing evidence records: one peer gets the owner's
/// record for T, one gets Y's, two get both in either order. After gossip
/// every peer holds the same state, with the lower-digest record.
#[test]
fn split_delivery_of_competing_evidence_records_converges() {
    use river_core::room_state::ban_evidence::evidence_digest;
    let room = Room::new();
    let (d, y, t) = (room.person(), room.person(), room.person());
    let ban = room.ban(&d, &t, 10);
    let by_owner = t.auth.clone();
    let by_y = AuthorizedMember::new(
        Member {
            owner_member_id: room.owner_id,
            invited_by: y.id,
            member_vk: t.sk.verifying_key(),
        },
        &y.sk,
    );
    let lower = std::cmp::min_by_key(by_owner.clone(), by_y.clone(), evidence_digest);
    let base = apply_checked(
        &room.modded_state(&[&d, &y], &[&d], vec![]),
        bans_delta(vec![ban]),
        &room.params,
    );
    let deliver = |records: Vec<AuthorizedMember>| {
        apply_checked(
            &base,
            ChatRoomStateV1Delta {
                ban_evidence: Some(records),
                ..Default::default()
            },
            &room.params,
        )
    };
    let mut peers = vec![
        deliver(vec![by_owner.clone()]),
        deliver(vec![by_y.clone()]),
        deliver(vec![by_owner.clone(), by_y.clone()]),
        deliver(vec![by_y.clone(), by_owner.clone()]),
    ];
    assert_eq!(ser(&peers[2]), ser(&peers[3]), "one delta, either order");
    for _ in 0..3 {
        for i in 0..peers.len() {
            for j in 0..peers.len() {
                if i != j {
                    peers[i] = merge(&peers[i], &peers[j], &room.params).unwrap();
                }
            }
        }
    }
    for p in &peers {
        assert_eq!(ser(p), ser(&peers[0]));
    }
    assert!(peers[0].ban_evidence.members.contains(&lower));
}

/// Cleanup keeps `member_info` for every evidence strict ancestor of a ban's
/// target, up to the first present ancestor. A target at depth 4 whose three
/// ancestors are all absent keeps exactly those three records, and not the
/// target's own. When the depth-2 ancestor posts, it and its inviter stay
/// members and the walk stops there: of the absent members, only the depth-3
/// ancestor keeps its `member_info`.
#[test]
fn evidence_member_info_is_kept_up_to_the_first_present_ancestor() {
    use river_core::room_state::ban_evidence::MAX_EVIDENCE_DEPTH;
    for depth2_posts in [false, true] {
        let room = Room::new();
        let m = room.person();
        let chain = invite_chain_of(&room, MAX_EVIDENCE_DEPTH);
        let t = &chain[MAX_EVIDENCE_DEPTH - 1];
        let mut msgs = vec![room.msg(&m, 1), room.msg(t, 2)];
        if depth2_posts {
            msgs.push(room.msg(&chain[1], 3));
        }
        let mut members: Vec<&Person> = chain.iter().collect();
        members.push(&m);
        let mut s = room.state(&members, vec![], msgs);
        let mut infos = room.owner_mods(&[&m]).member_info;
        infos.extend(chain.iter().map(|p| deputies_record(p, 1, &[])));
        s.member_info = MemberInfoV1 { member_info: infos };
        s.verify(&s, &room.params).unwrap();

        let after = apply_checked(&s, bans_delta(vec![room.ban(&m, t, 10)]), &room.params);
        let present = member_ids(&after);
        assert!(!present.contains(&t.id), "T banned");
        let kept: Vec<MemberId> = after
            .member_info
            .member_info
            .iter()
            .map(|r| r.member_info.member_id)
            .filter(|id| *id != room.owner_id && !present.contains(id))
            .collect();
        let expected: Vec<MemberId> = if depth2_posts {
            assert!(present.contains(&chain[0].id) && present.contains(&chain[1].id));
            vec![chain[2].id]
        } else {
            chain[..MAX_EVIDENCE_DEPTH - 1]
                .iter()
                .map(|p| p.id)
                .collect()
        };
        let mut kept_sorted = kept.clone();
        kept_sorted.sort();
        let mut expected_sorted = expected.clone();
        expected_sorted.sort();
        assert_eq!(
            kept_sorted, expected_sorted,
            "depth-2 posts: {depth2_posts}"
        );
        for id in &expected {
            assert!(
                evidence_ids(&after).contains(id),
                "ancestor held as evidence"
            );
        }
    }
}

// --- freenet/river#797: each ban record is judged by its own signature ---
//
// A `BanId` hashes the signature alone, and records that share an id but
// differ in content are all stored. `enforce_user_ban_cap` must not credit a
// record with the verdict `resolve_bans` reached for another record of the
// same id: a record whose own signature does not verify against its claimed
// issuer is dead. Each case is run in two input orders and compares whole
// records, since the forged record and the genuine one share an id.
type BanRecord = (MemberId, std::time::SystemTime, MemberId);
fn ban_record(b: &AuthorizedUserBan) -> BanRecord {
    (b.banned_by, b.ban.banned_at, b.ban.banned_user)
}
fn copy_signature(
    sig_of: &AuthorizedUserBan,
    issuer: MemberId,
    target: MemberId,
    at: u64,
    owner: MemberId,
) -> AuthorizedUserBan {
    AuthorizedUserBan::with_signature(
        UserBan {
            owner_member_id: owner,
            banned_at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000 + at),
            banned_user: target,
        },
        issuer,
        sig_of.signature,
    )
}
fn cap_both_orders(
    room: &Room,
    s: &ChatRoomStateV1,
    bans: Vec<AuthorizedUserBan>,
    cap: usize,
) -> Vec<BanRecord> {
    let mut out = Vec::new();
    for rev in [false, true] {
        let mut b = bans.clone();
        if rev {
            b.reverse();
        }
        BansV1::enforce_user_ban_cap(
            &mut b,
            cap,
            &s.members,
            &s.ban_evidence,
            &s.member_info,
            &room.params,
        );
        out.push(b.iter().map(ban_record).collect::<Vec<_>>());
    }
    assert_eq!(out[0], out[1], "input order changed the result");
    out.pop().unwrap()
}
#[test]
fn a_record_copying_an_owner_bans_signature_is_dead_at_the_cap() {
    let room = capped_room(20, 2);
    let s = room.modded_state(&[], &[], vec![]);
    let (x, y, z) = (room.person(), room.person(), room.person());
    let b1 = room.owner_ban(&x, 10);
    let b2 = room.owner_ban(&y, 11);
    let forged = copy_signature(&b1, room.owner_id, z.id, 99, room.owner_id);
    let got = cap_both_orders(&room, &s, vec![b1.clone(), b2.clone(), forged], 2);
    assert_eq!(got, vec![ban_record(&b1), ban_record(&b2)]);
}
#[test]
fn a_record_copying_a_bans_signature_cannot_take_its_pair() {
    let room = capped_room(20, 1);
    let a = room.person();
    let s = room.modded_state(&[&a], &[], vec![]);
    let t = room.person();
    let g = room.ban(&a, &t, 20);
    let forged = copy_signature(&g, a.id, t.id, 10, room.owner_id);
    let got = cap_both_orders(&room, &s, vec![g.clone(), forged], 1);
    assert_eq!(got, vec![ban_record(&g)]);
}
#[test]
fn a_record_copying_a_bans_signature_is_not_live_on_an_unknown_target() {
    let room = capped_room(20, 2);
    let a = room.person();
    let s = room.modded_state(&[&a], &[], vec![]);
    let (t1, t2, t3) = (room.person(), room.person(), room.person());
    let g1 = room.ban(&a, &t1, 10);
    let g2 = room.ban(&a, &t2, 11);
    let forged = copy_signature(&g1, a.id, t3.id, 99, room.owner_id);
    let got = cap_both_orders(&room, &s, vec![g1.clone(), g2.clone(), forged], 2);
    assert_eq!(got, vec![ban_record(&g1), ban_record(&g2)]);
}
