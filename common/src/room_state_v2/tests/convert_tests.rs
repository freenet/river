//! v1 to v2 conversion on a small v1 room built here: owner O invites k1,
//! k1 invites k2 and k3 (k1 is O's deputy), with messages, a DM, a purge,
//! member info, an owner ban, a deputy ban and a member ban.

use crate::room_state::ban::{AuthorizedUserBan, UserBan};
use crate::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use crate::room_state::direct_messages::{
    sign_direct_message, sign_recipient_purges, RecipientPurges,
};
use crate::room_state::member::{AuthorizedMember, Member, MemberId};
use crate::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use crate::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use crate::room_state::privacy::SealedBytes;
use crate::room_state::ChatRoomStateV1;
use crate::room_state_v2::convert::{
    deputy_candidates, legacy_records, owner_configuration, owner_payloads, ConvertError,
    DropReason, OwnerPayloads,
};
use crate::room_state_v2::encoding::Key;
use crate::room_state_v2::records::LegacyKind;
use crate::room_state_v2::records::Record;
use crate::room_state_v2::{normalize, validate_state, Verifier};
use ed25519_dalek::SigningKey;
use std::time::{Duration, UNIX_EPOCH};

fn sk(i: u8) -> SigningKey {
    SigningKey::from_bytes(&[i + 100; 32])
}

pub fn owner_vk() -> ed25519_dalek::VerifyingKey {
    sk(0).verifying_key()
}

fn _unused_sk(i: u8) -> SigningKey {
    SigningKey::from_bytes(&[i + 100; 32])
}

fn id(i: u8) -> MemberId {
    MemberId::from(&sk(i).verifying_key())
}

pub fn v1_room() -> ChatRoomStateV1 {
    let mut s = ChatRoomStateV1::default();
    let cfg = Configuration {
        owner_member_id: id(0),
        configuration_version: 3,
        max_members: 2000,
        max_user_bans: 400,
        ..Configuration::default()
    };
    s.configuration = AuthorizedConfigurationV1::new(cfg, &sk(0));
    for (m, inviter) in [(1, 0), (2, 1), (3, 1)] {
        let member = Member {
            owner_member_id: id(0),
            invited_by: id(inviter),
            member_vk: sk(m).verifying_key(),
        };
        s.members
            .members
            .push(AuthorizedMember::new(member, &sk(inviter)));
    }
    let t = |h: u64| UNIX_EPOCH + Duration::from_secs(1_800_000_000 + h * 3600);
    for (a, h) in [(1, 1), (2, 2), (0, 3), (2, 4)] {
        let m = MessageV1 {
            room_owner: id(0),
            author: id(a),
            time: t(h),
            content: RoomMessageBody::public(format!("m{h}")),
        };
        s.recent_messages
            .messages
            .push(AuthorizedMessageV1::new(m, &sk(a)));
    }
    let dm = sign_direct_message(
        &sk(2),
        id(2),
        id(3),
        &sk(0).verifying_key(),
        1_800_000_000,
        vec![1; 40],
    )
    .unwrap();
    s.direct_messages.messages.push(dm);
    let p = sign_recipient_purges(
        &sk(3),
        id(3),
        &sk(0).verifying_key(),
        RecipientPurges {
            version: 1,
            purged: vec![],
        },
    )
    .unwrap();
    s.direct_messages.purges.push(p);
    let mut owner_info = MemberInfo::new_public(id(0), 1, "owner".into());
    owner_info.deputies = vec![id(1)];
    s.member_info
        .member_info
        .push(AuthorizedMemberInfo::new(owner_info, &sk(0)));
    for m in 1..=3 {
        let info = MemberInfo::new_public(id(m), 1, format!("n{m}"));
        s.member_info
            .member_info
            .push(AuthorizedMemberInfo::new_with_member_key(info, &sk(m)));
    }
    // Bans on keys that are not members: by the owner, by the deputy k1,
    // and by k2 (an ordinary member).
    for (issuer, target) in [(0u8, 50u8), (1, 51), (2, 52)] {
        let b = UserBan {
            owner_member_id: id(0),
            banned_at: t(5),
            banned_user: id(target),
        };
        s.bans
            .0
            .push(AuthorizedUserBan::new(b, id(issuer), &sk(issuer)));
    }
    s
}

fn key(i: u8) -> Key {
    Key::from(&sk(i).verifying_key())
}

/// The owner's payloads with `deputies` confirmed and nothing protected.
fn payloads(v1: &ChatRoomStateV1, legacy: &[Record], deputies: &[Key]) -> OwnerPayloads {
    owner_payloads(&key(0), v1, legacy, 1, deputies, &[])
        .unwrap()
        .payloads
}

#[test]
fn conversion_keeps_every_member_and_record() {
    let v1 = v1_room();
    let o = key(0);
    let conv = legacy_records(&o, &v1);
    assert!(conv.unconverted.is_empty());
    let members: Vec<Key> = (1..=3).map(key).collect();
    let clock = 1;
    // The owner converts, confirming k1 (its own v1 deputy): every legacy
    // record attested, all kept, and the report says so.
    let c = owner_payloads(&o, &v1, &conv.records, clock, &[members[0]], &[]).unwrap();
    let p = c.payloads;
    assert_eq!(p.configuration.deputies, vec![members[0]]);
    for (kind, kr) in &c.report.kinds {
        assert_eq!(kr.kept, kr.total, "{kind:?}: {kr:?}");
        assert!(kr.dropped.is_empty(), "{kind:?}: {kr:?}");
    }
    assert_eq!(c.report.kinds[&LegacyKind::Link].total, 3);
    let mut recs = p.sign(&sk(0));
    recs.extend(conv.records.iter().cloned());
    let (s, r) = normalize(&o, recs.clone(), &mut Verifier::new());
    assert_eq!(r.members, {
        let mut m = members.clone();
        m.sort();
        m
    });
    // 3 links, 3 bans, 4 member info, 4 messages, 2 activity (k1, k2; the
    // owner's message is no activity), 1 DM, 1 purge; plus the owner's
    // configuration, clock and roster batches.
    let legacy_kept = s
        .records
        .iter()
        .filter(|r| r.legacy_kind().is_some())
        .count();
    assert_eq!(legacy_kept, 3 + 3 + 4 + 4 + 2 + 1 + 1);
    assert!(validate_state(&o, &s.to_bytes(), &mut Verifier::new()).is_ok());
    // The deputy's ban is a deputy ban and the member's a member ban: both
    // kept, as is the owner's.
    assert_eq!(
        s.records
            .iter()
            .filter(|r| matches!(r, Record::LegacyBan(_)))
            .count(),
        3
    );

    // With the window closed, the roster alone keeps them valid.
    let mut closed = p.clone();
    closed.configuration.version = 2;
    closed.configuration.legacy_window_open = false;
    closed.configuration.legacy_window_closed_at = clock;
    let mut recs2 = closed.sign(&sk(0));
    recs2.extend(
        s.records
            .iter()
            .filter(|r| r.legacy_kind().is_some())
            .cloned(),
    );
    let (s2, r2) = normalize(&o, recs2, &mut Verifier::new());
    assert_eq!(r2.members.len(), 3);
    assert_eq!(
        s2.records
            .iter()
            .filter(|r| r.legacy_kind().is_some())
            .count(),
        legacy_kept
    );

    // Without the owner's configuration no legacy record counts: the room
    // stays on v1 (decision (p)).
    let (s3, r3) = normalize(&o, conv.records.clone(), &mut Verifier::new());
    assert!(r3.members.is_empty());
    assert!(s3.records.is_empty());
}

#[test]
fn a_record_signed_by_another_key_is_dropped() {
    let mut v1 = v1_room();
    let o = key(0);
    let before = owner_payloads(&o, &v1, &legacy_records(&o, &v1).records, 1, &[key(1)], &[])
        .unwrap()
        .report;
    assert_eq!(before.kinds[&LegacyKind::Message].kept, 4);
    // A message whose signature is k3's while it claims k2 as author.
    let m = MessageV1 {
        room_owner: id(0),
        author: id(2),
        time: UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        content: RoomMessageBody::public("forged".into()),
    };
    let mut forged = AuthorizedMessageV1::new(m, &sk(3));
    forged.message.author = id(2);
    v1.recent_messages.messages.push(forged);
    let conv = legacy_records(&o, &v1);
    let c = owner_payloads(&o, &v1, &conv.records, 1, &[key(1)], &[]).unwrap();
    let msgs = &c.report.kinds[&LegacyKind::Message];
    assert_eq!((msgs.total, msgs.kept), (5, 4));
    assert_eq!(msgs.dropped.get(&DropReason::Signature), Some(&1));
    let mut recs = c.payloads.sign(&sk(0));
    recs.extend(conv.records);
    let (s, _) = normalize(&o, recs, &mut Verifier::new());
    assert_eq!(
        s.records
            .iter()
            .filter(|r| matches!(r, Record::LegacyMessage(_)))
            .count(),
        4
    );
}

/// The deputies the owner is asked to confirm: its own v1 list and the
/// lists of current, unbanned members, each naming who listed it. Only
/// confirmed ones become deputies (decision (r)).
#[test]
fn deputies_are_proposed_and_only_confirmed_ones_act() {
    let mut v1 = v1_room();
    // k4, invited by the owner, is outside deputy k1's subtree; k1 bans it.
    let member = Member {
        owner_member_id: id(0),
        invited_by: id(0),
        member_vk: sk(4).verifying_key(),
    };
    v1.members
        .members
        .push(AuthorizedMember::new(member, &sk(0)));
    let b = UserBan {
        owner_member_id: id(0),
        banned_at: UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        banned_user: id(4),
    };
    v1.bans.0.push(AuthorizedUserBan::new(b, id(1), &sk(1)));
    // k2 lists k3 as its own deputy.
    for i in &mut v1.member_info.member_info {
        if i.member_info.member_id == id(2) {
            i.member_info.deputies = vec![id(3)];
            *i = AuthorizedMemberInfo::new_with_member_key(i.member_info.clone(), &sk(2));
        }
    }
    let o = key(0);
    let cands = deputy_candidates(&o, &v1);
    let listed: Vec<(Key, Vec<Key>)> = cands.iter().map(|c| (c.key, c.listed_by.clone())).collect();
    let mut want = vec![(key(1), vec![o]), (key(3), vec![key(2)])];
    want.sort();
    assert_eq!(listed, want);
    let conv = legacy_records(&o, &v1);
    let k4 = key(4);
    // Confirmed: k1's ban reaches k4, outside k1's subtree.
    let mut recs = payloads(&v1, &conv.records, &[key(1)]).sign(&sk(0));
    recs.extend(conv.records.iter().cloned());
    let (_, r) = normalize(&o, recs, &mut Verifier::new());
    assert!(
        !r.members.contains(&k4),
        "the deputy's ban did not reach k4"
    );
    assert_eq!(r.members.len(), 3);
    // Not confirmed: k1's ban is a member ban and k4 stays.
    let mut recs = payloads(&v1, &conv.records, &[]).sign(&sk(0));
    recs.extend(conv.records.iter().cloned());
    let (_, r) = normalize(&o, recs, &mut Verifier::new());
    assert!(r.members.contains(&k4));
    // Confirmed, with k4 protected: a deputy ban cannot target it.
    let mut recs = owner_payloads(&o, &v1, &conv.records, 1, &[key(1)], &[k4])
        .unwrap()
        .payloads
        .sign(&sk(0));
    recs.extend(conv.records.iter().cloned());
    let (_, r) = normalize(&o, recs, &mut Verifier::new());
    assert!(r.members.contains(&k4));
}

#[test]
fn an_unconvertible_configuration_is_an_error() {
    let o = key(0);
    let mut v1 = v1_room();
    assert_eq!(
        owner_configuration(&o, &v1, &[o], &[]),
        Err(ConvertError::OwnerAsDeputy)
    );
    let many: Vec<Key> = (0..65u8).map(|i| key(i + 10)).collect();
    assert_eq!(
        owner_configuration(&o, &v1, &many, &[]),
        Err(ConvertError::TooManyAuthorityKeys)
    );
    // A v1 name longer than v2 allows is refused, never truncated.
    let mut cfg = v1.configuration.configuration.clone();
    cfg.max_room_name = 10_000;
    cfg.display.name = SealedBytes::public(vec![b'x'; 201]);
    v1.configuration = AuthorizedConfigurationV1::new(cfg.clone(), &sk(0));
    assert_eq!(
        owner_configuration(&o, &v1, &[], &[]),
        Err(ConvertError::NameTooLong { len: 201, max: 200 })
    );
    cfg.display.name = SealedBytes::public(vec![b'x'; 200]);
    cfg.max_room_description = 10_000;
    cfg.display.description = Some(SealedBytes::public(vec![b'y'; 2_001]));
    v1.configuration = AuthorizedConfigurationV1::new(cfg, &sk(0));
    assert_eq!(
        owner_configuration(&o, &v1, &[], &[]),
        Err(ConvertError::DescriptionTooLong {
            len: 2_001,
            max: 2_000
        })
    );
}

/// The Official-shaped room (169 members, 400 bans, 200 messages, 300 DMs,
/// real signatures) converted by its owner, confirming the bot's deputies
/// and protecting the bot: every member and every record of a member
/// carries over, and the state validates.
#[test]
fn the_official_shaped_room_converts_whole() {
    use crate::room_state_v2::official_shaped::official_shaped;
    let f = official_shaped(1, 1.0);
    let o = Key::from(&f.owner.verifying_key());
    let conv = legacy_records(&o, &f.v1);
    assert!(conv.unconverted.is_empty());
    let deputies: Vec<Key> = f.deputies.iter().map(Key::from).collect();
    let bot = Key::from(&f.bot);
    let cands = deputy_candidates(&o, &f.v1);
    assert_eq!(
        cands
            .iter()
            .filter(|c| c.listed_by == vec![bot])
            .map(|c| c.key)
            .collect::<Vec<_>>(),
        {
            let mut d = deputies.clone();
            d.sort();
            d
        }
    );
    let c = owner_payloads(&o, &f.v1, &conv.records, 24 * 300, &deputies, &[bot]).unwrap();
    // Only the member info of the 20 banned former members (held in v1 as
    // ban evidence) does not carry over: v2 keeps member info for members.
    for (kind, kr) in &c.report.kinds {
        if *kind == LegacyKind::MemberInfo {
            assert_eq!(kr.dropped.get(&DropReason::Retention), Some(&20));
            assert_eq!(kr.dropped.len(), 1);
        } else {
            assert!(kr.dropped.is_empty(), "{kind:?}: {kr:?}");
        }
    }
    let mut recs = c.payloads.sign(&f.owner);
    recs.extend(conv.records);
    let (s, r) = normalize(&o, recs, &mut Verifier::new());
    assert_eq!(r.members.len(), f.v1.members.members.len());
    let mut v = Verifier::new();
    assert!(validate_state(&o, &s.to_bytes(), &mut v).is_ok());
    // Every record checked once.
    assert_eq!(v.checks, s.records.len());
}
