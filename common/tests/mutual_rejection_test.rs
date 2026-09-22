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
