//! Ban signatures checked where the issuer is known, the bounds on what one
//! delta may add (counted after re-deliveries are removed), and the trims
//! that keep an offered delta within them (freenet/river#703).
use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
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
    x[31] = 3;
    SigningKey::from_bytes(&x)
}
fn at(t: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + t)
}

fn forged_as(
    claimed: MemberId,
    real: &SigningKey,
    target: MemberId,
    t: u64,
    owner: MemberId,
) -> AuthorizedUserBan {
    // Signed by `real`, but claiming to be issued by `claimed`.
    let signed = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner,
            banned_at: at(t),
            banned_user: target,
        },
        MemberId::from(&real.verifying_key()),
        real,
    );
    AuthorizedUserBan {
        banned_by: claimed,
        ..signed
    }
}

/// A new ban whose issuer this peer knows, and whose signature that issuer's
/// key does not verify (the owner's, or a held member's), is skipped, not an
/// error: erroring fails the merge one way only, for a peer that does not know
/// the issuer holds the same ban validly.
#[test]
fn a_new_ban_a_known_issuers_key_does_not_verify_is_skipped_not_refused() {
    let o = sk(1);
    let owner = MemberId::from(&o.verifying_key());
    let params = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let (a, evil) = (sk(2), sk(3));
    let a_rec = AuthorizedMember::new(
        Member {
            owner_member_id: owner,
            invited_by: owner,
            member_vk: a.verifying_key(),
        },
        &o,
    );
    let a_id = a_rec.member.id();
    let target = MemberId::from(&sk(50).verifying_key());
    let parent = ChatRoomStateV1 {
        members: MembersV1 {
            members: vec![a_rec],
        },
        ..Default::default()
    };
    let as_owner = forged_as(owner, &evil, target, 5, owner);
    let as_member = forged_as(a_id, &evil, target, 6, owner);
    let genuine = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner,
            banned_at: at(7),
            banned_user: target,
        },
        a_id,
        &a,
    );
    for forged in [
        vec![as_owner.clone()],
        vec![as_member.clone()],
        vec![as_owner, as_member, genuine.clone()],
    ] {
        let mut bans = BansV1(vec![]);
        bans.apply_delta(&parent, &params, &Some(forged.clone()))
            .expect("a forged ban from a known issuer is skipped, not refused");
        let kept: Vec<_> = bans.0.iter().collect();
        if forged.contains(&genuine) {
            assert_eq!(kept, vec![&genuine], "only the genuine ban is kept");
        } else {
            assert!(kept.is_empty(), "the forged ban is not stored");
        }
    }
}

/// A forged ban claiming an issuer known here only through ban evidence (a
/// removed member whose record a stored ban still needs) is skipped too.
#[test]
fn a_new_ban_an_evidence_issuers_key_does_not_verify_is_skipped() {
    use river_core::room_state::ban_evidence::BanEvidenceV1;
    let o = sk(1);
    let owner = MemberId::from(&o.verifying_key());
    let params = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let (e, evil) = (sk(4), sk(5));
    let e_rec = AuthorizedMember::new(
        Member {
            owner_member_id: owner,
            invited_by: owner,
            member_vk: e.verifying_key(),
        },
        &o,
    );
    let e_id = e_rec.member.id();
    let parent = ChatRoomStateV1 {
        ban_evidence: BanEvidenceV1 {
            members: vec![e_rec],
        },
        ..Default::default()
    };
    let target = MemberId::from(&sk(51).verifying_key());
    let forged = forged_as(e_id, &evil, target, 5, owner);
    let genuine = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner,
            banned_at: at(6),
            banned_user: target,
        },
        e_id,
        &e,
    );
    let mut bans = BansV1(vec![]);
    bans.apply_delta(&parent, &params, &Some(vec![forged, genuine.clone()]))
        .expect("skipped, not refused");
    assert_eq!(bans.0, vec![genuine]);
}

fn room_config(
    o: &SigningKey,
    max_members: usize,
    max_user_bans: usize,
    max_recent_messages: usize,
) -> AuthorizedConfigurationV1 {
    AuthorizedConfigurationV1::new(
        Configuration {
            owner_member_id: MemberId::from(&o.verifying_key()),
            max_members,
            max_user_bans,
            max_recent_messages,
            ..Default::default()
        },
        o,
    )
}

/// One evidence delta may add at most `2 x max(max_user_bans, floor) x
/// min(max_members, depth cap)` records this peer does not hold: records it
/// holds do not count, one new record more is refused, and a delta longer
/// than everything held plus the bound is refused whatever it repeats. The
/// two caps the bound reads differ here, so reading one for the other
/// changes it.
#[test]
fn the_evidence_bound_counts_new_records_only() {
    use river_core::room_state::ban::MIN_BANS_DELTA_BOUND;
    use river_core::room_state::ban_evidence::{evidence_bound, evidence_depth_cap, BanEvidenceV1};
    let o = sk(1);
    let owner = MemberId::from(&o.verifying_key());
    let params = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let (max_members, max_user_bans) = (2, 3);
    let bound = evidence_bound(
        MIN_BANS_DELTA_BOUND.max(max_user_bans),
        evidence_depth_cap(max_members),
    );
    assert_eq!(bound, 400, "premise");
    let parent = ChatRoomStateV1 {
        configuration: room_config(&o, max_members, max_user_bans, 10),
        ..Default::default()
    };
    let record = |i: u64| {
        AuthorizedMember::new(
            Member {
                owner_member_id: owner,
                invited_by: owner,
                member_vk: sk(1_000 + i).verifying_key(),
            },
            &o,
        )
    };
    let held: Vec<_> = (0..5).map(record).collect();
    let apply = |delta: Vec<AuthorizedMember>| {
        let mut evidence = BanEvidenceV1 {
            members: held.clone(),
        };
        evidence.apply_delta(&parent, &params, &Some(delta))
    };
    let new = |n: usize| (100..100 + n as u64).map(record).collect::<Vec<_>>();
    assert!(apply(new(bound)).is_ok());
    assert!(
        apply(new(bound + 1)).is_err(),
        "one new record over the bound"
    );
    let mut with_held = held.clone();
    with_held.extend(new(bound));
    assert!(
        apply(with_held).is_ok(),
        "held records do not count toward the bound"
    );
    let repeats: Vec<_> = held
        .iter()
        .cycle()
        .take(held.len() + bound + 1)
        .cloned()
        .collect();
    assert!(apply(repeats).is_err(), "longer than held plus the bound");
}

/// One messages delta may add at most `max(max_recent_messages, floor)`
/// messages this peer does not hold: held ones do not count, one new message
/// more is refused, and a delta longer than everything held plus the bound
/// is refused whatever it repeats.
#[test]
fn the_messages_bound_counts_new_messages_only() {
    use river_core::room_state::message::messages_work_bound;
    let o = sk(1);
    let owner = MemberId::from(&o.verifying_key());
    let params = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let parent = ChatRoomStateV1 {
        configuration: room_config(&o, 10, 10, 10),
        ..Default::default()
    };
    let bound = messages_work_bound(10);
    let msg = |i: u64| {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner,
                author: owner,
                time: at(i),
                content: RoomMessageBody::public(format!("m{i}")),
            },
            &o,
        )
    };
    let held: Vec<_> = (0..5).map(msg).collect();
    let apply = |delta: Vec<AuthorizedMessageV1>| {
        let mut messages = MessagesV1 {
            messages: held.clone(),
            ..Default::default()
        };
        messages.apply_delta(&parent, &params, &Some(delta))
    };
    let new = |n: usize| (100..100 + n as u64).map(msg).collect::<Vec<_>>();
    assert!(apply(new(bound)).is_ok());
    assert!(
        apply(new(bound + 1)).is_err(),
        "one new message over the bound"
    );
    let mut with_held = held.clone();
    with_held.extend(new(bound));
    assert!(
        apply(with_held).is_ok(),
        "held messages do not count toward the bound"
    );
    let repeats: Vec<_> = held
        .iter()
        .cycle()
        .take(held.len() + bound + 1)
        .cloned()
        .collect();
    assert!(apply(repeats).is_err(), "longer than held plus the bound");
}

/// A bans delta longer than everything held plus the bound is refused
/// whatever it repeats; one at that length that only repeats a held ban is
/// taken, and the ban is stored once.
#[test]
fn the_bans_raw_length_bound() {
    use river_core::room_state::ban::MIN_BANS_DELTA_BOUND;
    let o = sk(1);
    let owner = MemberId::from(&o.verifying_key());
    let params = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let parent = ChatRoomStateV1 {
        configuration: room_config(&o, 10, 3, 10),
        ..Default::default()
    };
    let ban = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: owner,
            banned_at: at(1),
            banned_user: MemberId::from(&sk(60).verifying_key()),
        },
        owner,
        &o,
    );
    let apply = |n: usize| {
        let mut bans = BansV1(vec![ban.clone()]);
        let r = bans.apply_delta(&parent, &params, &Some(vec![ban.clone(); n]));
        r.map(|_| bans.0.len())
    };
    assert_eq!(apply(1 + MIN_BANS_DELTA_BOUND), Ok(1));
    assert!(apply(2 + MIN_BANS_DELTA_BOUND).is_err());
}

/// The delta the UI applies for an incoming state (`delta_from_merged_summary`)
/// is read against the configuration the merge ends with: an incoming
/// configuration that raises the message cap gets every message the raised
/// cap keeps, not only those newer than the receiver's old horizon.
#[test]
fn the_merged_summary_delta_reads_a_raised_cap() {
    let o = sk(1);
    let owner = MemberId::from(&o.verifying_key());
    let params = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let msg = |i: u64| {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner,
                author: owner,
                time: at(i),
                content: RoomMessageBody::public(format!("m{i}")),
            },
            &o,
        )
    };
    let mut cfg_low = room_config(&o, 10, 10, 3).configuration;
    cfg_low.configuration_version = 1;
    let mut cfg_high = cfg_low.clone();
    cfg_high.configuration_version = 2;
    cfg_high.max_recent_messages = 10;
    let mut receiver = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(cfg_low, &o),
        recent_messages: MessagesV1 {
            messages: (7..10).map(msg).collect(),
            ..Default::default()
        },
        ..Default::default()
    };
    receiver.recent_messages.rebuild_actions_state();
    let mut incoming = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(cfg_high, &o),
        recent_messages: MessagesV1 {
            messages: (0..10).map(msg).collect(),
            ..Default::default()
        },
        ..Default::default()
    };
    incoming.recent_messages.rebuild_actions_state();
    let delta = receiver
        .delta_from_merged_summary(&params, &incoming)
        .expect("a delta");
    assert_eq!(delta.recent_messages.map(|m| m.len()), Some(7));
}

fn owner_messages(o: &SigningKey, range: std::ops::Range<u64>) -> MessagesV1 {
    let owner = MemberId::from(&o.verifying_key());
    let mut m = MessagesV1 {
        messages: range
            .map(|i| {
                AuthorizedMessageV1::new(
                    MessageV1 {
                        room_owner: owner,
                        author: owner,
                        time: at(i),
                        content: RoomMessageBody::public(format!("m{i}")),
                    },
                    o,
                )
            })
            .collect(),
        ..Default::default()
    };
    m.rebuild_actions_state();
    m
}

/// An incoming state holding more messages than one delta may carry still
/// merges through `delta_from_merged_summary` in one apply: the delta is
/// trimmed to the newest the bound allows, and the room keeps its newest.
#[test]
fn the_merged_summary_delta_is_trimmed_to_the_work_bound() {
    use river_core::room_state::message::messages_work_bound;
    let o = sk(1);
    let params = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let config = room_config(&o, 10, 10, 10);
    let local = ChatRoomStateV1 {
        configuration: config.clone(),
        ..Default::default()
    };
    let n = messages_work_bound(10) as u64 + 100;
    let incoming = ChatRoomStateV1 {
        configuration: config,
        recent_messages: owner_messages(&o, 0..n),
        ..Default::default()
    };
    let delta = local
        .delta_from_merged_summary(&params, &incoming)
        .expect("a delta");
    assert_eq!(
        delta.recent_messages.as_ref().map(|m| m.len()),
        Some(messages_work_bound(10))
    );
    let mut merged = local.clone();
    merged
        .apply_delta(&ChatRoomStateV1::default(), &params, &Some(delta))
        .expect("the trimmed delta applies");
    let times: Vec<_> = merged
        .recent_messages
        .messages
        .iter()
        .map(|m| m.message.time)
        .collect();
    assert_eq!(times, (n - 10..n).map(at).collect::<Vec<_>>());
}

/// The trim reads the configuration the merge ends with: an incoming
/// configuration raising the message cap past the floor gets every message
/// the raised cap keeps in one apply.
#[test]
fn the_merged_summary_delta_is_trimmed_by_the_merged_cap() {
    use river_core::room_state::message::messages_work_bound;
    let o = sk(1);
    let params = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let mut low = room_config(&o, 10, 10, 10).configuration;
    low.configuration_version = 1;
    let mut high = low.clone();
    high.configuration_version = 2;
    high.max_recent_messages = 3_000;
    let n = messages_work_bound(10) as u64 + 500;
    let local = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(low, &o),
        ..Default::default()
    };
    let incoming = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(high, &o),
        recent_messages: owner_messages(&o, 0..n),
        ..Default::default()
    };
    let delta = local.delta_from_merged_summary(&params, &incoming);
    let mut merged = local.clone();
    merged
        .apply_delta(&ChatRoomStateV1::default(), &params, &delta)
        .expect("apply");
    assert_eq!(merged.recent_messages.messages.len(), n as usize);
}

/// The members trim ranks a record by its invite chain in the sender's
/// state, the ban evidence included: a member whose inviter is known only as
/// evidence (a removed member a held ban still needs) ranks by that chain,
/// not as unreachable.
#[test]
fn the_members_trim_walks_chains_through_ban_evidence() {
    use river_core::room_state::ban_evidence::BanEvidenceV1;
    let o = sk(1);
    let owner = MemberId::from(&o.verifying_key());
    let params = ChatRoomParametersV1 {
        owner: o.verifying_key(),
    };
    let rec = |inviter: &SigningKey, who: &SigningKey| {
        AuthorizedMember::new(
            Member {
                owner_member_id: owner,
                invited_by: MemberId::from(&inviter.verifying_key()),
                member_vk: who.verifying_key(),
            },
            inviter,
        )
    };
    let (a, b, e) = (sk(2), sk(3), sk(4));
    // A (depth 1) invited B (depth 2), who invited 1000 members (depth 3);
    // E, known only as evidence, invited five (depth 2 through E).
    let mut members = vec![rec(&o, &a), rec(&a, &b)];
    members.extend((0..1_000u64).map(|i| rec(&b, &sk(10_000 + i))));
    let through_evidence: Vec<_> = (0..5u64).map(|i| rec(&e, &sk(20_000 + i))).collect();
    members.extend(through_evidence.iter().cloned());
    let sender = ChatRoomStateV1 {
        configuration: room_config(&o, 10, 10, 10),
        members: MembersV1 { members },
        ban_evidence: BanEvidenceV1 {
            members: vec![rec(&o, &e)],
        },
        ..Default::default()
    };
    let empty = ChatRoomStateV1::default();
    let summary = empty.summarize(&empty, &params);
    let delta = sender
        .delta_within_work_bounds(&params, &summary)
        .expect("a delta");
    let added = delta.members.expect("members").added().to_vec();
    assert_eq!(added.len(), 1_000, "premise: the trim ran");
    for m in &through_evidence {
        assert!(
            added.contains(m),
            "a member whose inviter is evidence is kept at its depth"
        );
    }
}
