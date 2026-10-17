//! Whole-state merges (`merge_from`, `merge_through_summary`): an incoming
//! state holding more records than its own owner-signed configuration allows
//! is refused before any record is checked; otherwise every incoming record
//! is applied and the merged room's caps decide what is kept, so one exchange
//! gives the same state in both directions (freenet/river#703).
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
use river_core::room_state::ban_evidence::BanEvidenceV1;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::time::{Duration, SystemTime};

fn sk(b: u64) -> SigningKey {
    let mut x = [0u8; 32];
    x[..8].copy_from_slice(&b.to_le_bytes());
    x[31] = 5;
    SigningKey::from_bytes(&x)
}
fn id(s: &SigningKey) -> MemberId {
    s.verifying_key().into()
}
fn at(t: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + t)
}
fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = Vec::new();
    ciborium::ser::into_writer(s, &mut v).unwrap();
    v
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
    fn messages(&self, msgs: Vec<AuthorizedMessageV1>) -> MessagesV1 {
        let mut m = MessagesV1 {
            messages: msgs,
            ..Default::default()
        };
        m.rebuild_actions_state();
        m
    }
    fn merged(&self, a: &ChatRoomStateV1, b: &ChatRoomStateV1) -> Result<ChatRoomStateV1, String> {
        let mut x = a.clone();
        x.merge_from(&self.p, b)?;
        Ok(x)
    }
    /// A state a merge produced, verified.
    fn settled(&self, s: ChatRoomStateV1) -> ChatRoomStateV1 {
        let m = self.merged(&s, &s).expect("self-merge");
        m.verify(&m, &self.p).expect("the state verifies");
        m
    }
}

/// One exchange between `a` and `b`, as two peers hold it afterwards. A side
/// holding more than one delta may carry in the other's room, under a
/// configuration that does not outrank the other's, is refused first; it
/// takes the other's configuration from the other direction's merge, and
/// the refused side then merges that (freenet/river#703).
fn exchange(
    params: &ChatRoomParametersV1,
    a: &ChatRoomStateV1,
    b: &ChatRoomStateV1,
) -> (ChatRoomStateV1, ChatRoomStateV1) {
    let merged = |x: &ChatRoomStateV1, y: &ChatRoomStateV1| {
        let mut out = x.clone();
        out.merge_from(params, y).map(|()| out)
    };
    match (merged(a, b), merged(b, a)) {
        (Ok(ab), Ok(ba)) => (ab, ba),
        (Err(e), Ok(ba)) => {
            assert!(e.contains("does not outrank"), "{e}");
            (
                merged(a, &ba).expect("the refused side merges the other's result"),
                ba,
            )
        }
        (Ok(ab), Err(e)) => {
            assert!(e.contains("does not outrank"), "{e}");
            let ba = merged(b, &ab).expect("the refused side merges the other's result");
            (ab, ba)
        }
        (Err(e1), Err(e2)) => panic!("both directions refused: {e1} / {e2}"),
    }
}

fn target(i: u64) -> MemberId {
    id(&sk(100_000 + i))
}

/// A state holding more bans than its own configuration allows (which
/// `verify` refuses) is refused by a whole-state merge; one at the cap merges.
#[test]
fn an_incoming_state_over_its_own_ban_cap_is_refused() {
    let r = Room::new();
    let over = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_user_bans = 5),
        bans: BansV1((0..6).map(|i| r.ban(&r.o, target(i), i)).collect()),
        ..Default::default()
    };
    assert!(
        over.verify(&over, &r.p).is_err(),
        "premise: verify refuses it"
    );
    let receiver = ChatRoomStateV1::default();
    assert!(r.merged(&receiver, &over).is_err());
    let mut ui = receiver.clone();
    assert!(ui.merge_through_summary(&r.p, &over).is_err());

    let at_cap = ChatRoomStateV1 {
        bans: BansV1((0..5).map(|i| r.ban(&r.o, target(i), i)).collect()),
        ..over
    };
    at_cap
        .verify(&at_cap, &r.p)
        .expect("premise: verify accepts it");
    assert_eq!(
        r.merged(&receiver, &at_cap).expect("merges").bans.0.len(),
        5
    );
}

/// The same for members and for ban evidence.
#[test]
fn an_incoming_state_over_its_own_member_or_evidence_bound_is_refused() {
    let r = Room::new();
    let members = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_members = 2),
        members: MembersV1 {
            members: (0..3)
                .map(|i| r.rec(&sk(10 + i).verifying_key(), &r.o))
                .collect(),
        },
        ..Default::default()
    };
    assert!(members.verify(&members, &r.p).is_err(), "premise");
    assert!(r.merged(&ChatRoomStateV1::default(), &members).is_err());

    let evidence = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| {
            c.max_user_bans = 1;
            c.max_members = 1;
        }),
        ban_evidence: BanEvidenceV1 {
            members: (0..3)
                .map(|i| r.rec(&sk(20 + i).verifying_key(), &r.o))
                .collect(),
        },
        ..Default::default()
    };
    assert!(evidence.verify(&evidence, &r.p).is_err(), "premise");
    assert!(r.merged(&ChatRoomStateV1::default(), &evidence).is_err());
}

/// An incoming configuration the owner did not sign earns no caps above the
/// floors one delta may carry: a state holding more bans than those is
/// refused whatever caps it claims.
#[test]
fn an_unsigned_configuration_does_not_raise_the_bound() {
    let r = Room::new();
    let claimed = AuthorizedConfigurationV1::new(
        Configuration {
            owner_member_id: r.owner,
            max_user_bans: 1_000,
            ..Default::default()
        },
        &sk(9),
    );
    let state = ChatRoomStateV1 {
        configuration: claimed,
        bans: BansV1((0..101).map(|i| r.ban(&r.o, target(i), i)).collect()),
        ..Default::default()
    };
    assert!(r.merged(&ChatRoomStateV1::default(), &state).is_err());
}

/// The merged room's `max_members` cut removes an issuer, making that
/// issuer's bans dead; every incoming ban is applied before the cap judges
/// them, so both directions keep the same bans.
#[test]
fn bans_are_judged_after_the_merged_member_cap() {
    let r = Room::new();
    let (w, x) = (sk(3), sk(2));
    let mut bans: Vec<AuthorizedUserBan> =
        (0..60).map(|i| r.ban(&x, target(i), 10_000 + i)).collect();
    bans.extend((0..30).map(|i| r.ban(&w, target(1_000 + i), 100 + i)));
    let sender = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_user_bans = 300),
        members: MembersV1 {
            members: vec![
                r.rec(&w.verifying_key(), &r.o),
                r.rec(&x.verifying_key(), &w),
            ],
        },
        bans: BansV1(bans),
        recent_messages: r.messages(vec![r.msg(&w, 1), r.msg(&x, 2)]),
        ..Default::default()
    });
    assert_eq!(sender.bans.0.len(), 90, "premise");
    let receiver = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(2, |c| {
            c.max_user_bans = 10;
            c.max_members = 1;
        }),
        ..Default::default()
    });
    let (rs, sr) = exchange(&r.p, &receiver, &sender);
    assert!(
        ser(&rs) == ser(&sr),
        "one exchange gives the same state both ways"
    );
    let mut ui = receiver.clone();
    ui.merge_through_summary(&r.p, &sender).expect("UI path");
    assert!(
        ser(&ui) == ser(&rs),
        "the UI path ends where merge_from does"
    );
}

/// A ban the merged cap evicts can still decide which other bans are live
/// (it removes their issuer); applying every incoming ban before the cap
/// judges them gives the same bans in both directions.
#[test]
fn a_ban_the_cap_evicts_still_decides_liveness_in_both_directions() {
    let r = Room::new();
    let (y, x, w) = (sk(2), sk(3), sk(4));
    let members = || MembersV1 {
        members: vec![
            r.rec(&y.verifying_key(), &r.o),
            r.rec(&x.verifying_key(), &y),
            r.rec(&w.verifying_key(), &r.o),
        ],
    };
    let msgs = || r.messages(vec![r.msg(&y, 1), r.msg(&x, 2), r.msg(&w, 3)]);
    let receiver = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(2, |c| c.max_user_bans = 10),
        members: members(),
        recent_messages: msgs(),
        bans: BansV1((0..10).map(|i| r.ban(&x, target(i), 20_000 + i)).collect()),
        ..Default::default()
    });
    let mut sb = vec![r.ban(&y, id(&x), 50)];
    sb.extend((0..90).map(|i| r.ban(&w, target(1_000 + i), 1_000 + i)));
    let sender = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_user_bans = 300),
        members: members(),
        recent_messages: msgs(),
        bans: BansV1(sb),
        ..Default::default()
    });
    let (rs, sr) = exchange(&r.p, &receiver, &sender);
    assert!(
        ser(&rs) == ser(&sr),
        "one exchange gives the same state both ways"
    );
}

/// An incoming state whose configuration ranks below the receiver's ends in
/// the receiver's room, so it is held to the receiver's per-delta bounds
/// before any signature is checked: a replayed older configuration with a
/// high cap does not admit thousands of bans the receiver would then check.
#[test]
fn a_lower_ranked_state_over_the_receivers_bounds_is_refused() {
    let r = Room::new();
    let x = sk(2);
    let receiver = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(2, |c| c.max_user_bans = 10),
        members: MembersV1 {
            members: vec![r.rec(&x.verifying_key(), &r.o)],
        },
        recent_messages: r.messages(vec![r.msg(&x, 1)]),
        ..Default::default()
    });
    // Older owner-signed configuration with a high cap, and bans attributed
    // to X that X's key does not verify.
    let other = sk(3);
    let bans: Vec<AuthorizedUserBan> = (0..20_000u64)
        .map(|i| AuthorizedUserBan {
            banned_by: id(&x),
            ..r.ban(&other, target(i), i)
        })
        .collect();
    let incoming = ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_user_bans = 30_000),
        bans: BansV1(bans),
        ..Default::default()
    };
    let started = std::time::Instant::now();
    assert!(r.merged(&receiver, &incoming).is_err());
    let mut ui = receiver.clone();
    assert!(ui.merge_through_summary(&r.p, &incoming).is_err());
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "refused before checking signatures"
    );
}

/// The honest case it covers: after the owner lowers the ban cap, a peer
/// still on the old configuration and holding more bans than the new
/// bounds allow is refused once, takes the new configuration from the
/// receiver, and the next exchange converges.
#[test]
fn a_peer_on_an_older_higher_cap_converges_after_one_more_exchange() {
    let r = Room::new();
    let bans: Vec<AuthorizedUserBan> = (0..150).map(|i| r.ban(&r.o, target(i), i)).collect();
    let stale = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(1, |c| c.max_user_bans = 300),
        bans: BansV1(bans),
        ..Default::default()
    });
    assert_eq!(stale.bans.0.len(), 150, "premise");
    let lowered = r.settled(ChatRoomStateV1 {
        configuration: r.cfg(2, |c| c.max_user_bans = 10),
        ..Default::default()
    });
    assert!(r.merged(&lowered, &stale).is_err(), "refused this round");
    let stale_after = r
        .merged(&stale, &lowered)
        .expect("the stale peer takes the new configuration");
    assert_eq!(stale_after.bans.0.len(), 10);
    let a = r.merged(&lowered, &stale_after).expect("next exchange");
    let b = r.merged(&stale_after, &lowered).expect("next exchange");
    assert!(ser(&a) == ser(&b), "they converge");
}
