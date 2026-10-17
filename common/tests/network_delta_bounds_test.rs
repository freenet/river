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
