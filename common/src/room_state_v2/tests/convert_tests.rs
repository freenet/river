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
use crate::room_state::ChatRoomStateV1;
use crate::room_state_v2::convert::{legacy_configuration, legacy_records, owner_payloads};
use crate::room_state_v2::encoding::Key;
use crate::room_state_v2::records::Record;
use crate::room_state_v2::{normalize, validate_state, Verifier};
use ed25519_dalek::SigningKey;
use std::time::{Duration, UNIX_EPOCH};

fn sk(i: u8) -> SigningKey {
    SigningKey::from_bytes(&[i + 100; 32])
}

fn id(i: u8) -> MemberId {
    MemberId::from(&sk(i).verifying_key())
}

fn v1_room() -> ChatRoomStateV1 {
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

#[test]
fn conversion_keeps_every_member_and_record() {
    let v1 = v1_room();
    let o = Key::from(&sk(0).verifying_key());
    let conv = legacy_records(&o, &v1);
    assert!(conv.unconverted.is_empty());
    let members: Vec<Key> = (1..=3).map(|m| Key::from(&sk(m).verifying_key())).collect();
    let clock = 1;
    // The owner converts first: every legacy record attested, all kept.
    let p = owner_payloads(&o, &v1, &conv.records, clock);
    assert_eq!(p.configuration.deputies, vec![members[0]]);
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

    // The owner never converts: the v1 configuration decides.
    let mut recs3 = vec![legacy_configuration(&o, &v1)];
    recs3.extend(conv.records.iter().cloned());
    let (_, r3) = normalize(&o, recs3, &mut Verifier::new());
    assert_eq!(r3.members.len(), 3);
}

#[test]
fn a_record_signed_by_another_key_is_dropped() {
    let mut v1 = v1_room();
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
    let o = Key::from(&sk(0).verifying_key());
    let conv = legacy_records(&o, &v1);
    let mut recs = vec![legacy_configuration(&o, &v1)];
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
