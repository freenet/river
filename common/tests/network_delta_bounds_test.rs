//! The network delta (sent against a receiver's summary, the receiver's
//! state unknown) is trimmed to what one delta may carry in the receiver's
//! room, so the receiver applies it rather than refusing it
//! (freenet/river#703).
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};
use std::time::{Duration, SystemTime};

fn sk(b: u64) -> SigningKey {
    let mut x = [0u8; 32];
    x[..8].copy_from_slice(&b.to_le_bytes());
    x[31] = 13;
    SigningKey::from_bytes(&x)
}
fn id(s: &SigningKey) -> MemberId {
    s.verifying_key().into()
}
fn at(t: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + t)
}

struct Room {
    o: SigningKey,
    owner: MemberId,
    p: ChatRoomParametersV1,
}

impl Room {
    fn new() -> Self {
        let o = sk(1);
        Room {
            owner: id(&o),
            p: ChatRoomParametersV1 {
                owner: o.verifying_key(),
            },
            o,
        }
    }
    fn cfg(&self, version: u32, f: impl FnOnce(&mut Configuration)) -> AuthorizedConfigurationV1 {
        let mut c = Configuration {
            owner_member_id: self.owner,
            configuration_version: version,
            ..Default::default()
        };
        f(&mut c);
        AuthorizedConfigurationV1::new(c, &self.o)
    }
    fn rec(&self, who: &VerifyingKey, inviter: &SigningKey) -> AuthorizedMember {
        AuthorizedMember::new(
            Member {
                owner_member_id: self.owner,
                invited_by: id(inviter),
                member_vk: *who,
            },
            inviter,
        )
    }
    fn msg(&self, who: &SigningKey, t: u64) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: self.owner,
                author: id(who),
                time: at(t),
                content: RoomMessageBody::public(format!("m{t}")),
            },
            who,
        )
    }
    fn ban(&self, by: &SigningKey, target: MemberId, t: u64) -> AuthorizedUserBan {
        AuthorizedUserBan::new(
            UserBan {
                owner_member_id: self.owner,
                banned_at: at(t),
                banned_user: target,
            },
            id(by),
            by,
        )
    }
    fn settled(&self, s: ChatRoomStateV1) -> ChatRoomStateV1 {
        let mut m = s.clone();
        m.merge_from(&self.p, &s).expect("self-merge");
        m.verify(&m, &self.p).expect("the state verifies");
        m
    }
    /// `sender`'s network delta for `receiver`, applied by `receiver`.
    fn exchange(
        &self,
        sender: &ChatRoomStateV1,
        receiver: &ChatRoomStateV1,
    ) -> Result<ChatRoomStateV1, String> {
        let summary = receiver.summarize(receiver, &self.p);
        let delta = sender.delta_within_work_bounds(&self.p, &summary);
        let mut r = receiver.clone();
        r.apply_delta(receiver, &self.p, &delta)?;
        Ok(r)
    }
}

/// A receiver whose newer configuration lowers `max_members` (and so the
/// ban-evidence bound) below what the sender's evidence needs: the delta's
/// evidence fits the least bound any room has, and the receiver applies it.
#[test]
fn a_network_delta_fits_a_receiver_with_a_lowered_member_cap() {
    let r = Room::new();
    let inviter = sk(7);
    let targets: Vec<SigningKey> = (0..600u64).map(|i| sk(50_000 + i)).collect();
    let mut members = vec![r.rec(&inviter.verifying_key(), &r.o)];
    members.extend(targets.iter().map(|t| r.rec(&t.verifying_key(), &inviter)));
    let mut msgs = vec![r.msg(&inviter, 0)];
    msgs.extend(
        targets
            .iter()
            .enumerate()
            .map(|(i, t)| r.msg(t, 1 + i as u64)),
    );
    let mut messages = MessagesV1 {
        messages: msgs,
        ..Default::default()
    };
    messages.rebuild_actions_state();
    let mut sender = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(1, |c| {
            c.max_user_bans = 1000;
            c.max_members = 1000;
            c.max_recent_messages = 1000;
        }),
        members: MembersV1 { members },
        recent_messages: messages,
        ..Default::default()
    });
    let bans: Vec<_> = targets
        .iter()
        .enumerate()
        .map(|(i, t)| r.ban(&inviter, id(t), 10_000 + i as u64))
        .collect();
    let base = sender.clone();
    sender
        .apply_delta(
            &base,
            &r.p,
            &Some(ChatRoomStateV1Delta {
                bans: Some(bans),
                ..Default::default()
            }),
        )
        .unwrap();
    let sender = r.settled(sender);
    assert!(sender.ban_evidence.members.len() > 200, "premise");
    let receiver = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(2, |c| {
            c.max_user_bans = 50;
            c.max_members = 1;
        }),
        ..Default::default()
    });
    r.exchange(&sender, &receiver)
        .expect("the receiver applies it");
}

/// A sender holding more ban evidence than the least evidence bound any room
/// has (a receiver whose newer configuration lowered `max_members`): the
/// delta's evidence fits that bound.
#[test]
fn a_network_delta_evidence_fits_the_least_member_cap() {
    let r = Room::new();
    let n = 700u64;
    let evidence: Vec<_> = (0..n)
        .map(|i| r.rec(&sk(10_000 + i).verifying_key(), &r.o))
        .collect();
    let sender = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_members = 200),
        ban_evidence: river_core::room_state::ban_evidence::BanEvidenceV1 { members: evidence },
        ..Default::default()
    };
    let receiver = ChatRoomStateV1 {
        configuration: r.cfg(2, |c| c.max_members = 2),
        ..Default::default()
    };
    r.exchange(&sender, &receiver)
        .expect("the receiver applies it");
}

/// When the receiver's summary shows a configuration that does not outrank
/// the sender's, the receiver ends with the sender's: the delta is trimmed to
/// the sender's bound (130 bans, above the 100 floor).
#[test]
fn a_network_delta_keeps_the_senders_bound_when_the_receiver_takes_the_senders_configuration() {
    let r = Room::new();
    let bans: Vec<_> = (0..130u64)
        .map(|i| r.ban(&r.o, id(&sk(50_000 + i)), i))
        .collect();
    let sender = ChatRoomStateV1 {
        configuration: r.cfg(2, |c| c.max_user_bans = 130),
        bans: BansV1(bans),
        ..Default::default()
    };
    let receiver = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_user_bans = 130),
        ..Default::default()
    };
    let summary = receiver.summarize(&receiver, &r.p);
    let delta = sender
        .delta_within_work_bounds(&r.p, &summary)
        .expect("a delta");
    assert_eq!(delta.bans.map(|b| b.len()), Some(130));
}

/// A receiver whose newer configuration the sender does not know: messages
/// and members are trimmed to the floors, a message whose author's record
/// was left out is left out too, and the receiver applies the delta.
#[test]
fn a_network_delta_fits_the_floors() {
    let r = Room::new();
    let authors: Vec<SigningKey> = (0..1_200u64).map(|i| sk(70_000 + i)).collect();
    let members: Vec<_> = authors
        .iter()
        .map(|a| r.rec(&a.verifying_key(), &r.o))
        .collect();
    let mut messages = MessagesV1 {
        messages: authors
            .iter()
            .enumerate()
            .map(|(i, a)| r.msg(a, i as u64))
            .collect(),
        ..Default::default()
    };
    messages.rebuild_actions_state();
    let sender = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| {
            c.max_user_bans = 300;
            c.max_members = 5_000;
            c.max_recent_messages = 5_000;
        }),
        members: MembersV1 { members },
        recent_messages: messages,
        ..Default::default()
    };
    let receiver = ChatRoomStateV1 {
        configuration: r.cfg(2, |c| {
            c.max_user_bans = 2;
            c.max_members = 2;
        }),
        ..Default::default()
    };
    r.exchange(&sender, &receiver)
        .expect("the receiver applies it");
}

/// Over the members bound, a record whose invite chain does not reach the
/// owner in the sender's state is the first left out.
#[test]
fn a_network_delta_leaves_out_unreachable_members_first() {
    let r = Room::new();
    let stranger = sk(9);
    let mut members: Vec<_> = (0..1_000u64)
        .map(|i| r.rec(&sk(30_000 + i).verifying_key(), &r.o))
        .collect();
    let unreachable: Vec<_> = (0..5u64)
        .map(|i| r.rec(&sk(40_000 + i).verifying_key(), &stranger))
        .collect();
    members.extend(unreachable.iter().cloned());
    let sender = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_members = 10),
        members: MembersV1 { members },
        ..Default::default()
    };
    let empty = ChatRoomStateV1::default();
    let delta = sender
        .delta_within_work_bounds(&r.p, &empty.summarize(&empty, &r.p))
        .expect("a delta");
    let added = delta.members.expect("members").added().to_vec();
    assert_eq!(added.len(), 1_000, "premise: the trim ran");
    assert!(unreachable.iter().all(|m| !added.contains(m)));
}

use river_core::room_state::ban_evidence::{evidence_bound, evidence_depth_cap, BanEvidenceV1};

fn owner_msgs(r: &Room, n: u64) -> MessagesV1 {
    let mut m = MessagesV1 {
        messages: (0..n).map(|i| r.msg(&r.o, i)).collect(),
        ..Default::default()
    };
    m.rebuild_actions_state();
    m
}

/// After a whole-state merge the work bounds must be back on for network deltas.
#[test]
fn the_work_bounds_are_back_on_after_a_whole_state_merge() {
    let r = Room::new();
    let big_delta = || {
        Some(ChatRoomStateV1Delta {
            bans: Some(
                (0..250u64)
                    .map(|i| r.ban(&r.o, id(&sk(50_000 + i)), i))
                    .collect(),
            ),
            ..Default::default()
        })
    };
    let base = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_user_bans = 10),
        ..Default::default()
    };
    let mut control = base.clone();
    assert!(
        control.apply_delta(&base, &r.p, &big_delta()).is_err(),
        "control: bounded"
    );
    let mut s = base.clone();
    s.merge_from(&r.p, &base).unwrap();
    let mut t = base.clone();
    assert!(
        t.apply_delta(&base, &r.p, &big_delta()).is_err(),
        "bounded after merge_from"
    );
    let mut u = base.clone();
    u.merge_through_summary(&r.p, &base).unwrap();
    assert!(
        u.apply_delta(&base, &r.p, &big_delta()).is_err(),
        "bounded after merge_through_summary"
    );
}

fn authors_state(r: &Room, n: u64, cap: usize) -> ChatRoomStateV1 {
    let authors: Vec<SigningKey> = (0..n).map(|i| sk(70_000 + i)).collect();
    let members: Vec<_> = authors
        .iter()
        .map(|a| r.rec(&a.verifying_key(), &r.o))
        .collect();
    let mut messages = MessagesV1 {
        messages: authors
            .iter()
            .enumerate()
            .map(|(i, a)| r.msg(a, i as u64))
            .collect(),
        ..Default::default()
    };
    messages.rebuild_actions_state();
    ChatRoomStateV1 {
        configuration: r.cfg(1, |c| {
            c.max_user_bans = 300;
            c.max_members = cap;
            c.max_recent_messages = cap;
        }),
        members: MembersV1 { members },
        recent_messages: messages,
        ..Default::default()
    }
}

/// The trimmed delta's content: every message author is accounted
/// for, members and messages are within the floors.
#[test]
fn a_trimmed_delta_carries_no_message_whose_author_it_left_out() {
    let r = Room::new();
    let sender = authors_state(&r, 1_200, 5_000);
    let receiver = ChatRoomStateV1 {
        configuration: r.cfg(2, |c| {
            c.max_user_bans = 2;
            c.max_members = 2;
        }),
        ..Default::default()
    };
    let summary = receiver.summarize(&receiver, &r.p);
    let delta = sender.delta_within_work_bounds(&r.p, &summary).unwrap();
    let carried: std::collections::HashSet<MemberId> = delta
        .members
        .as_ref()
        .map(|m| m.added().iter().map(|a| a.member.id()).collect())
        .unwrap_or_default();
    let msgs = delta.recent_messages.clone().unwrap_or_default();
    assert!(
        carried.len() <= 1_000,
        "members within the floor: {}",
        carried.len()
    );
    assert!(
        msgs.len() <= 1_000,
        "messages within the floor: {}",
        msgs.len()
    );
    assert!(!msgs.is_empty());
    for m in &msgs {
        let a = m.message.author;
        assert!(
            a == r.owner || summary.members.contains(&a) || carried.contains(&a),
            "author left out"
        );
    }
}

/// Messages by the owner and by a member the receiver lists are kept.
#[test]
fn messages_by_the_owner_and_by_listed_members_are_kept() {
    let r = Room::new();
    let mut sender = authors_state(&r, 3, 200);
    sender.recent_messages = owner_msgs(&r, 3);
    let receiver = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_members = 200),
        ..Default::default()
    };
    let summary = receiver.summarize(&receiver, &r.p);
    let delta = sender.delta_within_work_bounds(&r.p, &summary).unwrap();
    assert_eq!(
        delta.recent_messages.map(|m| m.len()),
        Some(3),
        "owner's messages kept"
    );
    // an author the receiver's summary lists, absent from the delta's members
    let sender2 = authors_state(&r, 3, 200);
    let mut receiver2 = sender2.clone();
    receiver2.recent_messages = MessagesV1::default();
    receiver2.configuration = r.cfg(1, |c| c.max_members = 200);
    let summary2 = receiver2.summarize(&receiver2, &r.p);
    let delta2 = sender2.delta_within_work_bounds(&r.p, &summary2).unwrap();
    assert!(
        delta2.members.is_none() || delta2.members.as_ref().unwrap().added().is_empty(),
        "premise: receiver holds the members"
    );
    assert_eq!(
        delta2.recent_messages.map(|m| m.len()),
        Some(3),
        "summary-listed authors kept"
    );
}

/// The newest messages are kept.
#[test]
fn the_messages_trim_keeps_the_newest() {
    let r = Room::new();
    let sender = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_recent_messages = 1_200),
        recent_messages: owner_msgs(&r, 1_200),
        ..Default::default()
    };
    let receiver = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_recent_messages = 1_200),
        ..Default::default()
    };
    let summary = receiver.summarize(&receiver, &r.p);
    let delta = sender.delta_within_work_bounds(&r.p, &summary).unwrap();
    let msgs = delta.recent_messages.unwrap();
    assert_eq!(
        msgs.len(),
        1_200,
        "sender's own bound (1200) when receiver takes the sender's configuration"
    );
    // now the floors: a newer receiver configuration
    let receiver2 = ChatRoomStateV1 {
        configuration: r.cfg(2, |c| c.max_recent_messages = 5),
        ..Default::default()
    };
    let summary2 = receiver2.summarize(&receiver2, &r.p);
    let delta2 = sender.delta_within_work_bounds(&r.p, &summary2).unwrap();
    let m2 = delta2.recent_messages.unwrap();
    assert_eq!(m2.len(), 1_000, "the floors bound");
    let newest = sender
        .recent_messages
        .messages
        .iter()
        .map(|m| m.message.time)
        .max()
        .unwrap();
    assert!(
        m2.iter().any(|m| m.message.time == newest),
        "the newest kept"
    );
    let oldest = sender
        .recent_messages
        .messages
        .iter()
        .map(|m| m.message.time)
        .min()
        .unwrap();
    assert!(
        m2.iter().all(|m| m.message.time != oldest),
        "the oldest dropped"
    );
}

fn evidence_sender(r: &Room, n: u64, max_members: usize) -> ChatRoomStateV1 {
    let evidence: Vec<_> = (0..n)
        .map(|i| r.rec(&sk(10_000 + i).verifying_key(), &r.o))
        .collect();
    ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_members = max_members),
        ban_evidence: BanEvidenceV1 { members: evidence },
        ..Default::default()
    }
}

/// The floors take the LEAST depth cap (max_members 1).
#[test]
fn the_floor_evidence_bound_uses_the_least_depth_cap() {
    let r = Room::new();
    let sender = evidence_sender(&r, 700, 200);
    let receiver = ChatRoomStateV1 {
        configuration: r.cfg(2, |c| c.max_members = 1),
        ..Default::default()
    };
    let summary = receiver.summarize(&receiver, &r.p);
    let delta = sender.delta_within_work_bounds(&r.p, &summary).unwrap();
    assert_eq!(
        delta.ban_evidence.unwrap().len(),
        evidence_bound(100, evidence_depth_cap(1))
    );
}

/// Under the sender's own configuration evidence is bounded at the sender's depth cap.
#[test]
fn evidence_under_the_senders_configuration_uses_its_depth_cap() {
    let r = Room::new();
    let sender = evidence_sender(&r, 1_200, 200);
    let receiver = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_members = 200),
        ..Default::default()
    };
    let summary = receiver.summarize(&receiver, &r.p);
    let delta = sender.delta_within_work_bounds(&r.p, &summary).unwrap();
    let want = evidence_bound(100, evidence_depth_cap(200));
    assert!(1_200 > want, "premise");
    assert_eq!(delta.ban_evidence.unwrap().len(), want);
}

/// A state at its own legitimate evidence bound is not refused.
#[test]
fn a_state_at_its_own_evidence_bound_is_not_refused() {
    let r = Room::new();
    let n = evidence_bound(100, evidence_depth_cap(300)); // 800
    let s = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| {
            c.max_user_bans = 100;
            c.max_members = 300;
        }),
        ban_evidence: BanEvidenceV1 {
            members: (0..n as u64)
                .map(|i| r.rec(&sk(10_000 + i).verifying_key(), &r.o))
                .collect(),
        },
        ..Default::default()
    };
    let mut recv = ChatRoomStateV1::default();
    recv.merge_from(&r.p, &s).expect("not refused");
}

/// An unsigned configuration earns no members/evidence above the floors.
#[test]
fn an_unsigned_configuration_earns_no_member_or_evidence_bound() {
    let r = Room::new();
    let unsigned = |m: usize, b: usize| {
        AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: r.owner,
                max_members: m,
                max_user_bans: b,
                ..Default::default()
            },
            &sk(9),
        )
    };
    let members = ChatRoomStateV1 {
        configuration: unsigned(5_000, 10),
        members: MembersV1 {
            members: (0..1_001u64)
                .map(|i| r.rec(&sk(20_000 + i).verifying_key(), &r.o))
                .collect(),
        },
        ..Default::default()
    };
    assert!(
        ChatRoomStateV1::default()
            .merge_from(&r.p, &members)
            .is_err(),
        "members"
    );
    let ev = ChatRoomStateV1 {
        configuration: unsigned(5_000, 1_000),
        ban_evidence: BanEvidenceV1 {
            members: (0..201u64)
                .map(|i| r.rec(&sk(20_000 + i).verifying_key(), &r.o))
                .collect(),
        },
        ..Default::default()
    };
    assert!(
        ChatRoomStateV1::default().merge_from(&r.p, &ev).is_err(),
        "evidence"
    );
}

/// The DM apply's enforced-ban set must not count a ban no known key
/// verifies at the cap: a ban of a present member stays enforced.
#[test]
fn a_ban_whose_issuer_is_known_nowhere_does_not_unseat_a_stored_one() {
    let r = Room::new();
    let (m, x, unknown) = (sk(2), sk(3), sk(4));
    let mut msgs = MessagesV1 {
        messages: vec![r.msg(&m, 1), r.msg(&x, 2)],
        ..Default::default()
    };
    msgs.rebuild_actions_state();
    let held = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_user_bans = 2),
        members: MembersV1 {
            members: vec![
                r.rec(&m.verifying_key(), &r.o),
                r.rec(&x.verifying_key(), &r.o),
            ],
        },
        recent_messages: msgs,
        // the owner bans present member X (oldest) and an id held nowhere
        bans: BansV1(vec![r.ban(&r.o, id(&x), 5), r.ban(&r.o, id(&sk(90)), 6)]),
        ..Default::default()
    });
    assert_eq!(held.bans.0.len(), 2);
    let mut applied = held.clone();
    applied
        .apply_delta(
            &held,
            &r.p,
            &Some(ChatRoomStateV1Delta {
                bans: Some(vec![r.ban(&unknown, id(&sk(91)), 20)]),
                ..Default::default()
            }),
        )
        .expect("applies");
    assert_eq!(applied.bans, held.bans);
}

/// The work bounds are back on after a whole-state merge that fails, too.
#[test]
fn the_work_bounds_are_back_on_after_a_failed_whole_state_merge() {
    let r = Room::new();
    let base = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_user_bans = 10),
        ..Default::default()
    };
    // A member record attributed to the owner's invitation that the owner
    // did not sign: the merge fails inside the apply.
    let mut bad = base.clone();
    bad.members.members.push(AuthorizedMember {
        member: Member {
            owner_member_id: r.owner,
            invited_by: r.owner,
            member_vk: sk(77).verifying_key(),
        },
        signature: ed25519_dalek::Signature::from_bytes(&[0; 64]),
    });
    let mut s = base.clone();
    assert!(
        s.merge_from(&r.p, &bad).is_err(),
        "premise: the merge fails"
    );
    let mut u = base.clone();
    assert!(u.merge_through_summary(&r.p, &bad).is_err(), "premise");
    let big: Vec<_> = (0..250u64)
        .map(|i| r.ban(&r.o, id(&sk(50_000 + i)), i))
        .collect();
    let mut t = base.clone();
    assert!(
        t.apply_delta(
            &base,
            &r.p,
            &Some(ChatRoomStateV1Delta {
                bans: Some(big),
                ..Default::default()
            })
        )
        .is_err(),
        "bounded after a failed merge"
    );
}

/// More members missing than one delta may carry, each authoring one
/// message, with the newest messages by the members of highest id: every
/// trimmed delta still advances the receiver (it keeps messages and their
/// authors), and repeated exchanges deliver the whole window.
#[test]
fn every_trimmed_network_delta_advances_the_receiver() {
    let r = Room::new();
    let mut authors: Vec<SigningKey> = (0..2_000u64).map(|i| sk(90_000 + i)).collect();
    authors.sort_by_key(id);
    let members: Vec<_> = authors
        .iter()
        .map(|a| r.rec(&a.verifying_key(), &r.o))
        .collect();
    let mut messages = MessagesV1 {
        messages: authors
            .iter()
            .enumerate()
            .map(|(i, a)| r.msg(a, i as u64))
            .collect(),
        ..Default::default()
    };
    messages.rebuild_actions_state();
    let sender = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| {
            c.max_members = 5_000;
            c.max_recent_messages = 5_000;
        }),
        members: MembersV1 { members },
        recent_messages: messages,
        ..Default::default()
    };
    // The receiver's newer configuration (same caps) is unknown to the
    // sender, so the delta is held to the floors.
    let mut receiver = ChatRoomStateV1 {
        configuration: r.cfg(2, |c| {
            c.max_members = 5_000;
            c.max_recent_messages = 5_000;
        }),
        ..Default::default()
    };
    let first = r.exchange(&sender, &receiver).expect("applies");
    assert!(
        !first.recent_messages.messages.is_empty(),
        "the first trimmed delta delivers messages"
    );
    for _ in 0..4 {
        receiver = r.exchange(&sender, &receiver).expect("applies");
    }
    assert_eq!(
        receiver.recent_messages.messages.len(),
        2_000,
        "the whole window arrives"
    );
}

/// More missing members than one delta may carry, in 100 disjoint invite
/// chains of depth 11 with one message by the deepest member of each: an
/// author only verifies with its whole chain, so the trim takes authors as
/// whole chains while they fit and leaves out the messages of the rest.
/// Every trimmed delta advances the receiver, and the exchange converges.
#[test]
fn every_trimmed_network_delta_advances_the_receiver_on_floor_caps() {
    let r = Room::new();
    let mut members = Vec::new();
    let mut messages = Vec::new();
    for a in 0..100u64 {
        let mut inviter = r.o.clone();
        for d in 0..11u64 {
            let m = sk(1_000_000 + a * 100 + d);
            members.push(r.rec(&m.verifying_key(), &inviter));
            if d == 10 {
                messages.push(r.msg(&m, 10 + a));
            }
            inviter = m;
        }
    }
    let mut held = MessagesV1 {
        messages,
        ..Default::default()
    };
    held.rebuild_actions_state();
    let sender = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(1, |c| {
            c.max_members = 5_000;
            c.max_recent_messages = 1_000;
        }),
        members: MembersV1 { members },
        recent_messages: held,
        ..Default::default()
    });
    assert_eq!(sender.members.members.len(), 1_100, "premise");
    // The receiver's newer configuration (same caps) is unknown to the
    // sender, so each delta is held to the floors.
    let mut receiver = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(2, |c| {
            c.max_members = 5_000;
            c.max_recent_messages = 1_000;
        }),
        ..Default::default()
    });
    let mut converged = false;
    for _ in 0..5 {
        let summary = receiver.summarize(&receiver, &r.p);
        if sender.delta_within_work_bounds(&r.p, &summary).is_none() {
            converged = true;
            break;
        }
        let before = (
            receiver.members.members.len(),
            receiver.recent_messages.messages.len(),
        );
        receiver = r.exchange(&sender, &receiver).expect("applies");
        let after = (
            receiver.members.members.len(),
            receiver.recent_messages.messages.len(),
        );
        assert_ne!(before, after, "every trimmed delta advances the receiver");
    }
    assert!(converged, "the exchange converges within 5 rounds");
    assert_eq!(receiver.members.members.len(), 1_100);
    assert_eq!(receiver.recent_messages.messages.len(), 100);
}

/// A receiver whose configuration outranks the sender's is sent a delta held
/// to the floor bounds. The bans trimmed to that bound are judged against
/// the sender's own records: a moderator two invites down still has 100 of
/// its 150 bans sent, not none.
#[test]
fn a_floor_trimmed_delta_sends_bans_by_an_issuer_below_the_first_level() {
    let r = Room::new();
    let (a, d) = (sk(70), sk(71));
    let bans: Vec<AuthorizedUserBan> = (0..150u64)
        .map(|i| r.ban(&d, id(&sk(80_000 + i)), 100 + i))
        .collect();
    let mut held = MessagesV1 {
        messages: vec![r.msg(&a, 1), r.msg(&d, 2)],
        ..Default::default()
    };
    held.rebuild_actions_state();
    let sender = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_user_bans = 200),
        members: MembersV1 {
            members: vec![
                r.rec(&a.verifying_key(), &r.o),
                r.rec(&d.verifying_key(), &a),
            ],
        },
        recent_messages: held,
        bans: BansV1(bans),
        ..Default::default()
    });
    assert_eq!(sender.bans.0.len(), 150, "premise");
    let receiver = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(2, |c| c.max_user_bans = 200),
        ..Default::default()
    });
    let summary = receiver.summarize(&receiver, &r.p);
    let delta = sender
        .delta_within_work_bounds(&r.p, &summary)
        .expect("a delta");
    assert_eq!(
        delta.bans.as_ref().map(|b| b.len()),
        Some(100),
        "the floor bound of bans is sent"
    );
    let after = r.exchange(&sender, &receiver).expect("applies");
    assert_eq!(after.bans.0.len(), 100);
}

/// A receiver that pruned a member (no record, no messages of theirs) is
/// sent that member's record with their new message, so the message
/// verifies on arrival (freenet/river#145).
#[test]
fn a_new_message_arrives_with_its_authors_record() {
    let r = Room::new();
    let m = sk(5);
    let mut held = MessagesV1 {
        messages: vec![r.msg(&r.o, 1)],
        ..Default::default()
    };
    held.rebuild_actions_state();
    let receiver = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(1, |_| {}),
        recent_messages: held,
        ..Default::default()
    });
    let mut msgs = MessagesV1 {
        messages: vec![r.msg(&r.o, 1), r.msg(&m, 2)],
        ..Default::default()
    };
    msgs.rebuild_actions_state();
    let sender = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(1, |_| {}),
        members: MembersV1 {
            members: vec![r.rec(&m.verifying_key(), &r.o)],
        },
        recent_messages: msgs,
        ..Default::default()
    });
    let after = r.exchange(&sender, &receiver).expect("applies");
    assert!(after
        .recent_messages
        .messages
        .iter()
        .any(|x| x.message.author == id(&m)));
    assert!(after
        .members
        .members
        .iter()
        .any(|x| x.member.id() == id(&m)));
}

/// One author whose missing invite chain is exactly the receiver's member
/// bound (1000 at the floor), plus a few shallower members the receiver
/// also lacks: the trimmed delta takes the whole chain, so the author and
/// its message arrive, rather than filling the bound with the shallower
/// records first. (Checked on the delta itself: applying a 1000-deep chain
/// verifies every link of every record.)
#[test]
fn a_chain_that_exactly_fills_the_member_bound_is_sent_whole() {
    let r = Room::new();
    let mut members = Vec::new();
    let mut inviter = r.o.clone();
    let mut author = r.o.clone();
    for d in 0..1_000u64 {
        let m = sk(3_000_000 + d);
        members.push(r.rec(&m.verifying_key(), &inviter));
        inviter = m.clone();
        author = m;
    }
    for e in 0..5u64 {
        members.push(r.rec(&sk(4_000_000 + e).verifying_key(), &r.o));
    }
    members.sort_by_key(|m| m.member.id());
    let mut messages = MessagesV1 {
        messages: vec![r.msg(&author, 10)],
        ..Default::default()
    };
    messages.rebuild_actions_state();
    let sender = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| {
            c.max_members = 5_000;
            c.max_recent_messages = 1_000;
        }),
        members: MembersV1 { members },
        recent_messages: messages,
        ..Default::default()
    };
    // The receiver's newer configuration is unknown to the sender, so the
    // delta is held to the floor bound.
    let receiver = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(2, |c| {
            c.max_members = 5_000;
            c.max_recent_messages = 1_000;
        }),
        ..Default::default()
    });
    let summary = receiver.summarize(&receiver, &r.p);
    let delta = sender
        .delta_within_work_bounds(&r.p, &summary)
        .expect("a delta");
    let sent: Vec<MemberId> = delta
        .members
        .as_ref()
        .expect("members")
        .added()
        .iter()
        .map(|m| m.member.id())
        .collect();
    assert_eq!(sent.len(), 1_000, "the delta fills the bound");
    let author_id: MemberId = author.verifying_key().into();
    assert!(
        sent.contains(&author_id),
        "the author's whole chain is sent"
    );
    let sent_messages = delta.recent_messages.as_ref().map_or(0, |m| m.len());
    assert_eq!(sent_messages, 1, "and its message");
}

/// The receiver already holds the top of an author's invite chain: the
/// chain the trim counts stops at the first member the receiver lists, so
/// the 1000 records it lacks fit the floor bound exactly and are sent with
/// the author's message, though the whole chain (1050) would not fit.
#[test]
fn a_chain_counts_only_the_members_the_receiver_lacks() {
    let r = Room::new();
    let mut chain = Vec::new();
    let mut inviter = r.o.clone();
    for d in 0..1_050u64 {
        let m = sk(5_000_000 + d);
        chain.push(r.rec(&m.verifying_key(), &inviter));
        inviter = m;
    }
    let author = inviter;
    let top_holder = sk(5_000_000 + 49);
    let config = |version| {
        r.cfg(version, |c| {
            c.max_members = 5_000;
            c.max_recent_messages = 1_000;
        })
    };
    let messages = |list: Vec<AuthorizedMessageV1>| {
        let mut m = MessagesV1 {
            messages: list,
            ..Default::default()
        };
        m.rebuild_actions_state();
        m
    };
    let mut members = chain.clone();
    for e in 0..5u64 {
        members.push(r.rec(&sk(6_000_000 + e).verifying_key(), &r.o));
    }
    members.sort_by_key(|m| m.member.id());
    let sender = ChatRoomStateV1 {
        configuration: config(1),
        members: MembersV1 { members },
        recent_messages: messages(vec![r.msg(&top_holder, 5), r.msg(&author, 10)]),
        ..Default::default()
    };
    // The receiver holds the top 50 records (its member at depth 50 has a
    // message, so they stay) on a newer configuration the sender lacks.
    let receiver = r.settled(ChatRoomStateV1 {
        configuration: config(2),
        members: MembersV1 {
            members: chain[..50].to_vec(),
        },
        recent_messages: messages(vec![r.msg(&top_holder, 5)]),
        ..Default::default()
    });
    assert_eq!(receiver.members.members.len(), 50, "premise");
    let summary = receiver.summarize(&receiver, &r.p);
    let delta = sender
        .delta_within_work_bounds(&r.p, &summary)
        .expect("a delta");
    let sent: Vec<MemberId> = delta
        .members
        .as_ref()
        .expect("members")
        .added()
        .iter()
        .map(|m| m.member.id())
        .collect();
    let author_id: MemberId = author.verifying_key().into();
    assert!(
        sent.contains(&author_id),
        "the 1000 missing records are sent"
    );
    let sent_messages = delta.recent_messages.as_ref().map_or(0, |m| m.len());
    assert_eq!(sent_messages, 1, "and the author's message");
}
