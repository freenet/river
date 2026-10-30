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
    owner_payloads(&key(0), v1, legacy, 1, deputies, &[], &[])
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
    let c = owner_payloads(&o, &v1, &conv.records, clock, &[members[0]], &[], &[]).unwrap();
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
    let before = owner_payloads(
        &o,
        &v1,
        &legacy_records(&o, &v1).records,
        1,
        &[key(1)],
        &[],
        &[],
    )
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
    let c = owner_payloads(&o, &v1, &conv.records, 1, &[key(1)], &[], &[]).unwrap();
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
    let mut recs = owner_payloads(&o, &v1, &conv.records, 1, &[key(1)], &[k4], &[])
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
        owner_configuration(&o, &v1, &[o], &[], &[]),
        Err(ConvertError::OwnerAsDeputy)
    );
    let many: Vec<Key> = (0..65u8).map(|i| key(i + 10)).collect();
    assert_eq!(
        owner_configuration(&o, &v1, &many, &[], &[]),
        Err(ConvertError::TooManyAuthorityKeys)
    );
    // A v1 name longer than v2 allows is refused, never truncated.
    let mut cfg = v1.configuration.configuration.clone();
    cfg.max_room_name = 10_000;
    cfg.display.name = SealedBytes::public(vec![b'x'; 201]);
    v1.configuration = AuthorizedConfigurationV1::new(cfg.clone(), &sk(0));
    assert_eq!(
        owner_configuration(&o, &v1, &[], &[], &[]),
        Err(ConvertError::NameTooLong { len: 201, max: 200 })
    );
    cfg.display.name = SealedBytes::public(vec![b'x'; 200]);
    cfg.max_room_description = 10_000;
    cfg.display.description = Some(SealedBytes::public(vec![b'y'; 2_001]));
    v1.configuration = AuthorizedConfigurationV1::new(cfg, &sk(0));
    assert_eq!(
        owner_configuration(&o, &v1, &[], &[], &[]),
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
    let c = owner_payloads(&o, &f.v1, &conv.records, 24 * 300, &deputies, &[bot], &[]).unwrap();
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

/// What the owner sees before signing: what declining a v1 deputy costs
/// (its bans carry over narrowed to its own subtree), configuration values
/// lowered to the v2 ceilings, secret versions beyond the v2 cap, and links
/// whose v1 chain does not reach the owner.
#[test]
fn the_report_shows_narrowed_bans_clamps_and_unreachable_links() {
    use crate::room_state::privacy::RoomCipherSpec;
    use crate::room_state::secret::{AuthorizedSecretVersionRecord, SecretVersionRecordV1};
    let mut v1 = v1_room();
    let t = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    // k4, invited by the owner, is outside deputy k1's subtree; k1 bans it.
    let member = |m: u8, by: u8| {
        AuthorizedMember::new(
            Member {
                owner_member_id: id(0),
                invited_by: id(by),
                member_vk: sk(m).verifying_key(),
            },
            &sk(by),
        )
    };
    v1.members.members.push(member(4, 0));
    let b = UserBan {
        owner_member_id: id(0),
        banned_at: t,
        banned_user: id(4),
    };
    v1.bans.0.push(AuthorizedUserBan::new(b, id(1), &sk(1)));
    // k6 invited by k5, which the room holds only as ban evidence: no
    // chain to the owner.
    v1.ban_evidence.members.push(member(5, 0));
    v1.members.members.push(member(6, 5));
    // 258 secret versions, two more than v2 keeps.
    for v in 0..258u32 {
        let r = SecretVersionRecordV1 {
            version: v,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: t,
        };
        v1.secrets
            .versions
            .push(AuthorizedSecretVersionRecord::new(r, &sk(0)));
    }
    let o = key(0);
    let k1 = deputy_candidates(&o, &v1)
        .into_iter()
        .find(|c| c.key == key(1))
        .unwrap();
    // k1's bans: a non-member (51) and k4, neither under k1.
    // k1's bans: a key the room holds no record of (51), and k4, not under k1.
    assert_eq!(
        (k1.v1_bans, k1.bans_outside_subtree, k1.bans_on_unknown_keys),
        (2, 1, 1)
    );

    let conv = legacy_records(&o, &v1);
    let report = |deputies: &[Key]| {
        owner_payloads(&o, &v1, &conv.records, 1, deputies, &[], &[])
            .unwrap()
            .report
    };
    let declined = report(&[]);
    let bans = &declined.kinds[&LegacyKind::Ban];
    assert_eq!(bans.narrowed, 2, "{bans:?}");
    assert_eq!(
        bans.kept + bans.narrowed + bans.dropped.values().sum::<usize>(),
        bans.total
    );
    let confirmed = report(&[key(1)]);
    assert_eq!(confirmed.kinds[&LegacyKind::Ban].narrowed, 0);

    assert!(
        confirmed
            .clamps
            .iter()
            .any(|c| c.field == "max_members" && c.v1 == 2000 && c.v2 == 400),
        "{:?}",
        confirmed.clamps
    );
    assert_eq!(confirmed.secret_versions_dropped, 2);
    let links = &confirmed.kinds[&LegacyKind::Link];
    assert_eq!(
        links.dropped.get(&DropReason::NoChainToOwner),
        Some(&1),
        "{links:?}"
    );
    assert_eq!(links.dropped.get(&DropReason::LegacyLinkCap), None);
}

/// A private room's blobs beyond the blob cap (the oldest versions first)
/// are reported as such, not as ordinary retention: those members cannot
/// read messages sealed under the dropped versions.
#[test]
fn blobs_beyond_the_blob_cap_are_reported() {
    use crate::room_state::privacy::{PrivacyMode, RoomCipherSpec};
    use crate::room_state::secret::{
        AuthorizedEncryptedSecretForMember, AuthorizedSecretVersionRecord,
        EncryptedSecretForMemberV1, SecretVersionRecordV1,
    };
    let mut v1 = v1_room();
    let t = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let mut cfg = v1.configuration.configuration.clone();
    cfg.privacy_mode = PrivacyMode::Private;
    cfg.display.name = SealedBytes::Private {
        ciphertext: vec![1; 20],
        nonce: [0; 12],
        secret_version: 0,
        declared_len_bytes: 4,
    };
    v1.configuration = AuthorizedConfigurationV1::new(cfg, &sk(0));
    // 179 versions, a blob for each of the three members at each: 537 blobs
    // against the cap of 534, whatever the room's member limit.
    for v in 0..179u32 {
        let r = SecretVersionRecordV1 {
            version: v,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: t,
        };
        v1.secrets
            .versions
            .push(AuthorizedSecretVersionRecord::new(r, &sk(0)));
        for m in 1..=3u8 {
            let b = EncryptedSecretForMemberV1 {
                member_id: id(m),
                secret_version: v,
                ciphertext: vec![m; 48],
                nonce: [v as u8; 12],
                sender_ephemeral_public_key: [7; 32],
                provider: id(0),
            };
            v1.secrets
                .encrypted_secrets
                .push(AuthorizedEncryptedSecretForMember::new(b, &sk(0)));
        }
    }
    let o = key(0);
    let conv = legacy_records(&o, &v1);
    let c = owner_payloads(&o, &v1, &conv.records, 1, &[key(1)], &[], &[]).unwrap();
    assert_eq!(c.payloads.configuration.max_secret_blobs, 534);
    let blobs = &c.report.kinds[&LegacyKind::SecretBlob];
    assert_eq!(blobs.total, 537, "{blobs:?}");
    assert_eq!(blobs.kept, 534, "{blobs:?}");
    assert_eq!(
        blobs.dropped.get(&DropReason::SecretBlobCap),
        Some(&3),
        "{blobs:?}"
    );
}

/// Past the legacy link cap (a cap of 2, injected): the owner rosters the
/// best ranked links (depth, then newest message), and the report gives
/// every drop its reason.
#[test]
fn conversion_past_the_caps_reports_every_drop_reason() {
    use crate::room_state_v2::convert::add_unconverted;
    use crate::room_state_v2::encoding::ByteBuf;
    use crate::room_state_v2::records::{Legacy, RosterEntry};
    use crate::room_state_v2::retain::test_cap;
    let mut v1 = v1_room();
    let t = |h: u64| UNIX_EPOCH + Duration::from_secs(1_800_000_000 + h * 3600);
    // A message over the legacy content bound.
    let m = MessageV1 {
        room_owner: id(0),
        author: id(3),
        time: t(1),
        content: RoomMessageBody::public("x".repeat(1_001)),
    };
    v1.recent_messages
        .messages
        .push(AuthorizedMessageV1::new(m, &sk(3)));
    // A ban by a key the state does not hold.
    let b = UserBan {
        owner_member_id: id(0),
        banned_at: t(5),
        banned_user: id(53),
    };
    v1.bans.0.push(AuthorizedUserBan::new(b, id(60), &sk(60)));
    let o = key(0);
    let conv = legacy_records(&o, &v1);
    assert_eq!(conv.unconverted.len(), 1);
    let mut legacy = conv.records.clone();
    // A legacy record whose v1 bytes do not decode.
    legacy.push(Record::LegacyMessage(Legacy {
        signer: key(1),
        v1: ByteBuf(vec![0x82, 0x01]),
    }));

    test_cap::set(Some(2));
    let c = owner_payloads(&o, &v1, &legacy, 1, &[key(1)], &[], &[]);
    test_cap::set(None);
    let c = c.unwrap();
    let mut report = c.report;
    add_unconverted(&mut report, &conv.unconverted);

    // k1 (depth 1) and k2 (depth 2, newest message) are rostered; k3
    // (depth 2, an older message) is beyond the cap.
    let link_digest = |m: u8| {
        legacy
            .iter()
            .find(|r| match r {
                Record::LegacyLink(l) => crate::room_state_v2::legacy::decode_link(&o, l)
                    .is_some_and(|v| v.member == key(m)),
                _ => false,
            })
            .unwrap()
            .digest()
    };
    let rostered: Vec<RosterEntry> = c
        .payloads
        .roster
        .iter()
        .flat_map(|b| b.batch.clone())
        .filter(|e| e.kind == LegacyKind::Link)
        .collect();
    let has = |m: u8| rostered.iter().any(|e| e.digest == link_digest(m));
    assert!(has(1) && has(2) && !has(3), "the cap keeps k1 and k2");
    let links = &report.kinds[&LegacyKind::Link];
    assert_eq!(
        links.dropped.get(&DropReason::LegacyLinkCap),
        Some(&1),
        "{links:?}"
    );
    let msgs = &report.kinds[&LegacyKind::Message];
    assert_eq!(
        msgs.dropped.get(&DropReason::TooLarge),
        Some(&1),
        "{msgs:?}"
    );
    assert_eq!(
        msgs.dropped.get(&DropReason::Undecodable),
        Some(&1),
        "{msgs:?}"
    );
    let bans = &report.kinds[&LegacyKind::Ban];
    assert_eq!(
        bans.dropped.get(&DropReason::MissingSigner),
        Some(&1),
        "{bans:?}"
    );
}

/// Two v1 generations of one room hold different records (writes that
/// reached one and not the other). Record digests do not depend on the
/// contract, so converting the union keeps both generations' records once.
#[test]
fn a_conversion_of_two_generations_keeps_the_records_of_both() {
    let a = v1_room();
    let mut b = a.clone();
    let t = UNIX_EPOCH + Duration::from_secs(1_800_000_000 + 9 * 3600);
    let only = |author: u8, text: &str| {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: id(0),
                author: id(author),
                time: t,
                content: RoomMessageBody::public(text.into()),
            },
            &sk(author),
        )
    };
    let mut a = a;
    a.recent_messages.messages.push(only(1, "only in a"));
    b.recent_messages.messages.push(only(3, "only in b"));
    let o = key(0);
    let mut union = legacy_records(&o, &a).records;
    let mut seen: std::collections::HashSet<_> = union.iter().map(|r| r.digest()).collect();
    union.extend(
        legacy_records(&o, &b)
            .records
            .into_iter()
            .filter(|r| seen.insert(r.digest())),
    );
    let c = owner_payloads(&o, &a, &union, 1, &[key(1)], &[], &[]).unwrap();
    let msgs = &c.report.kinds[&LegacyKind::Message];
    assert_eq!((msgs.total, msgs.kept), (6, 6), "{msgs:?}");
    let mut recs = c.payloads.sign(&sk(0));
    recs.extend(union);
    let (s, _) = normalize(&o, recs, &mut Verifier::new());
    let kept_msgs = s
        .records
        .iter()
        .filter(|r| matches!(r, Record::LegacyMessage(_)))
        .count();
    assert_eq!(kept_msgs, 6);
    assert!(validate_state(&o, &s.to_bytes(), &mut Verifier::new()).is_ok());
}

/// The window closes end to end: the owner converts; during the window a
/// new v1 member joins under k1 and posts, and k2 posts; the owner closes
/// the window with a roster of what the room then holds. Every attested
/// record stays, and a legacy record written after the close is dropped.
#[test]
fn closing_the_window_attests_what_the_room_holds() {
    use crate::room_state_v2::convert::{closing_payloads, ClosingError};
    let v1 = v1_room();
    let o = key(0);
    let conv = legacy_records(&o, &v1);
    let p = payloads(&v1, &conv.records, &[key(1)]);
    let mut recs = p.sign(&sk(0));
    recs.extend(conv.records.iter().cloned());
    let (opened, _) = normalize(&o, recs, &mut Verifier::new());

    // Written to v1 during the window, merged in as legacy records.
    let mut later = v1.clone();
    later.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: id(0),
            invited_by: id(1),
            member_vk: sk(4).verifying_key(),
        },
        &sk(1),
    ));
    let t = |h: u64| UNIX_EPOCH + Duration::from_secs(1_800_000_000 + h * 3600);
    let msg = |a: u8, h: u64| {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: id(0),
                author: id(a),
                time: t(h),
                content: RoomMessageBody::public(format!("window {a} {h}")),
            },
            &sk(a),
        )
    };
    later.recent_messages.messages.push(msg(4, 6));
    later.recent_messages.messages.push(msg(2, 7));
    let window_recs = legacy_records(&o, &later).records;
    let mut recs = opened.records.clone();
    recs.extend(window_recs.iter().cloned());
    let (during, r) = normalize(&o, recs, &mut Verifier::new());
    assert!(
        r.members.contains(&key(4)),
        "a window join counts while open"
    );
    let held_legacy = |s: &crate::room_state_v2::RoomStateV2| {
        s.records
            .iter()
            .filter(|r| r.legacy_kind().is_some())
            .count()
    };
    let n_during = held_legacy(&during);

    let close = closing_payloads(&o, &during, 2).unwrap();
    assert!(
        !close.payloads.roster.is_empty(),
        "the window's records need attesting"
    );
    // The report names what it attests: k4's link, under k1, and the two
    // window messages with k4's activity.
    assert_eq!(close.report.new_links.get(&key(1)), Some(&1));
    assert_eq!(close.report.attested[&LegacyKind::Message], 2);
    let close = close.payloads;
    // Its batches number on from the conversion's, and the close is at the
    // hour asked for.
    let held_seq = during
        .records
        .iter()
        .filter_map(|r| match r {
            Record::Roster(b) => Some(b.payload.seq),
            _ => None,
        })
        .max()
        .unwrap();
    assert!(close.roster.iter().all(|b| b.seq > held_seq));
    assert_eq!(close.configuration.legacy_window_closed_at, 2);
    assert_eq!(close.clock.hours, 2);
    let mut recs = during.records.clone();
    recs.extend(close.sign(&sk(0)));
    let (closed, r) = normalize(&o, recs, &mut Verifier::new());
    assert!(r.members.contains(&key(4)), "an attested window join stays");
    assert_eq!(held_legacy(&closed), n_during, "nothing attested is lost");
    assert!(validate_state(&o, &closed.to_bytes(), &mut Verifier::new()).is_ok());
    assert_eq!(
        closing_payloads(&o, &closed, 3).unwrap_err(),
        ClosingError::AlreadyClosed
    );

    // A legacy record written after the close is not attested.
    let mut after = later.clone();
    after.recent_messages.messages.push(msg(2, 8));
    let mut recs = closed.records.clone();
    recs.extend(legacy_records(&o, &after).records);
    let (s, _) = normalize(&o, recs, &mut Verifier::new());
    assert_eq!(s, closed, "an unattested record does not count");
}

/// At the legacy link cap, the conversion roster holds every link slot, so
/// a v1 member joining during the window cannot be attested at close and
/// is dropped (the link roster ceiling is the cap; other kinds have twice
/// their cap).
#[test]
fn at_the_link_cap_a_window_join_cannot_be_attested() {
    use crate::room_state_v2::convert::closing_payloads;
    use crate::room_state_v2::retain::test_cap;
    test_cap::set(Some(3));
    let v1 = v1_room();
    let o = key(0);
    let conv = legacy_records(&o, &v1);
    let mut recs = payloads(&v1, &conv.records, &[key(1)]).sign(&sk(0));
    recs.extend(conv.records.iter().cloned());
    let (opened, _) = normalize(&o, recs, &mut Verifier::new());
    let mut later = v1.clone();
    later.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: id(0),
            invited_by: id(1),
            member_vk: sk(4).verifying_key(),
        },
        &sk(1),
    ));
    let mut recs = opened.records.clone();
    recs.extend(legacy_records(&o, &later).records);
    let (during, r) = normalize(&o, recs, &mut Verifier::new());
    assert!(!r.members.contains(&key(4)), "no unrostered share is left");
    let close = closing_payloads(&o, &during, 2).unwrap().payloads;
    let mut recs = during.records.clone();
    recs.extend(close.sign(&sk(0)));
    let (_, r) = normalize(&o, recs, &mut Verifier::new());
    test_cap::set(None);
    assert_eq!(r.members.len(), 3);
    assert!(!r.members.contains(&key(4)));
}

/// The Invite Bot's room to invite after a conversion: 50 new children by
/// default, and the owner's exception raises it. The report says which limit
/// applies. Rostered v1 children do not count against either.
#[test]
fn the_invite_bot_admits_past_the_default_with_an_exception() {
    use crate::room_state_v2::encoding::{digest_of, sign_payload};
    use crate::room_state_v2::official_shaped::official_shaped;
    use crate::room_state_v2::records::{
        tag, AcceptanceP, AcceptedLink, InvitationP, Parent, Signed,
    };
    let f = official_shaped(1, 1.0);
    let o = Key::from(&f.owner.verifying_key());
    let bot = Key::from(&f.bot);
    let conv = legacy_records(&o, &f.v1);
    let deputies: Vec<Key> = f.deputies.iter().map(Key::from).collect();
    let bot_link =
        conv.records
            .iter()
            .find(|r| match r {
                Record::LegacyLink(l) => crate::room_state_v2::legacy::decode_link(&o, l)
                    .is_some_and(|v| v.member == bot),
                _ => false,
            })
            .unwrap()
            .digest();
    let new_links: Vec<Record> = (0..60u32)
        .map(|i| {
            let mut seed = [0x61u8; 32];
            seed[..4].copy_from_slice(&i.to_le_bytes());
            let member = SigningKey::from_bytes(&seed);
            seed[4] = 0x62;
            let k0 = SigningKey::from_bytes(&seed);
            let ip = InvitationP {
                inviter: bot,
                invite_key: Key::from(&k0.verifying_key()),
                parent: Parent::Link(bot_link),
                seq: 1_000 + i,
                clock: 0,
            };
            let invitation = Signed {
                sig: sign_payload(tag::INVITATION, &o, &ip, &f.bot_key),
                payload: ip,
            };
            let acceptance = AcceptanceP {
                invitation: digest_of(&invitation),
                member: Key::from(&member.verifying_key()),
            };
            Record::Link(AcceptedLink {
                invite_key_sig: sign_payload(tag::ACCEPTANCE, &o, &acceptance, &k0),
                member_sig: sign_payload(tag::ACCEPTANCE, &o, &acceptance, &member),
                invitation,
                acceptance,
            })
        })
        .collect();
    let kept_new = |exceptions: &[(Key, u32)]| {
        let c = owner_payloads(
            &o,
            &f.v1,
            &conv.records,
            24 * 300,
            &deputies,
            &[bot],
            exceptions,
        )
        .unwrap();
        let mut recs = c.payloads.sign(&f.owner);
        recs.extend(conv.records.iter().cloned());
        recs.extend(new_links.iter().cloned());
        let (s, _) = normalize(&o, recs, &mut Verifier::new());
        assert!(validate_state(&o, &s.to_bytes(), &mut Verifier::new()).is_ok());
        let n = s
            .records
            .iter()
            .filter(|r| matches!(r, Record::Link(_)))
            .count();
        (n, c.report)
    };
    // The default: 50 more, and the report warns the bot has more children
    // than that already.
    let (n, report) = kept_new(&[]);
    assert_eq!(n, 50);
    let l = report
        .child_limits
        .iter()
        .find(|l| l.inviter == bot)
        .unwrap();
    assert_eq!((l.limit, l.exception), (50, false));
    assert!(l.v1_children >= 50);
    // The owner's exception, at the ceiling.
    let (n, report) = kept_new(&[(bot, 534)]);
    assert_eq!(n, 60);
    let l = report
        .child_limits
        .iter()
        .find(|l| l.inviter == bot)
        .unwrap();
    assert_eq!((l.limit, l.exception), (534, true));
    // A key with no link cannot have one.
    assert!(matches!(
        owner_configuration(&o, &f.v1, &[], &[], &[(key(9), 100)]),
        Err(ConvertError::ChildExceptionWithoutLink(_))
    ));
}

/// A closing whose batches do not fit the roster's limits is refused, not
/// signed: the records it fails to attest would go when the window closes.
#[test]
fn a_closing_that_would_lose_records_is_refused() {
    use crate::room_state_v2::convert::{closing_payloads, ClosingError};
    use crate::room_state_v2::retain::test_cap;
    let v1 = v1_room();
    let o = key(0);
    let conv = legacy_records(&o, &v1);
    let mut recs = payloads(&v1, &conv.records, &[key(1)]).sign(&sk(0));
    recs.extend(conv.records.iter().cloned());
    let (opened, _) = normalize(&o, recs, &mut Verifier::new());
    let batches = opened
        .records
        .iter()
        .filter(|r| matches!(r, Record::Roster(_)))
        .count();
    // A record written during the window, to attest.
    let mut later = v1.clone();
    later
        .recent_messages
        .messages
        .push(AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: id(0),
                author: id(2),
                time: UNIX_EPOCH + Duration::from_secs(1_800_000_000 + 7 * 3600),
                content: RoomMessageBody::public("window".into()),
            },
            &sk(2),
        ));
    let mut recs = opened.records.clone();
    recs.extend(legacy_records(&o, &later).records);
    let (during, _) = normalize(&o, recs, &mut Verifier::new());
    // No batch left for the closing's.
    test_cap::set_limits(test_cap::Limits {
        roster_batches: Some(batches),
        ..Default::default()
    });
    let r = closing_payloads(&o, &during, 2);
    test_cap::set_limits(test_cap::Limits::default());
    assert!(
        matches!(r, Err(ClosingError::WouldLose(n)) if n >= 1),
        "{r:?}"
    );
}

/// A ban that takes no effect does not hide a deputy from the owner, and a
/// deputy's bans are reported narrowed from the v1 lists themselves.
#[test]
fn an_inert_ban_does_not_hide_a_deputy() {
    let mut v1 = v1_room();
    let t = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    // k2 stores a ban on deputy k1, outside its authority: v1 keeps it and
    // it takes no effect.
    let b = UserBan {
        owner_member_id: id(0),
        banned_at: t,
        banned_user: id(1),
    };
    v1.bans.0.push(AuthorizedUserBan::new(b, id(2), &sk(2)));
    let o = key(0);
    let cands = deputy_candidates(&o, &v1);
    assert!(cands.iter().any(|c| c.key == key(1)), "{cands:?}");
    // Declined, its ban on a non-member is narrowed in the report.
    let conv = legacy_records(&o, &v1);
    let c = owner_payloads(&o, &v1, &conv.records, 1, &[], &[], &[]).unwrap();
    assert!(c.report.kinds[&LegacyKind::Ban].narrowed >= 1);
}

/// Two generations holding different links for one member: the roster takes
/// the lowest digest, whichever order they arrive in.
#[test]
fn two_links_for_one_member_roster_the_lowest_digest_in_any_order() {
    let v1 = v1_room();
    let o = key(0);
    let with_link = |inviter: u8| {
        let mut g = v1.clone();
        g.members.members.push(AuthorizedMember::new(
            Member {
                owner_member_id: id(0),
                invited_by: id(inviter),
                member_vk: sk(4).verifying_key(),
            },
            &sk(inviter),
        ));
        g
    };
    let a = legacy_records(&o, &with_link(1)).records;
    let b = legacy_records(&o, &with_link(2)).records;
    let k4_links = |recs: &[Record]| -> Vec<_> {
        recs.iter()
            .filter(|r| match r {
                Record::LegacyLink(l) => l.signer == key(1) || l.signer == key(2),
                _ => false,
            })
            .map(|r| r.digest())
            .collect()
    };
    let in_only = |x: &[Record], y: &[Record]| {
        let ys = k4_links(y);
        k4_links(x).into_iter().find(|d| !ys.contains(d)).unwrap()
    };
    let (da, db) = (in_only(&a, &b), in_only(&b, &a));
    let (low, high) = (da.min(db), da.max(db));
    let rostered = |first: &[Record], second: &[Record]| {
        let mut all = first.to_vec();
        all.extend(second.iter().cloned());
        let p = payloads(&v1, &all, &[]);
        p.roster
            .iter()
            .flat_map(|b| b.batch.iter())
            .filter(|e| e.kind == LegacyKind::Link)
            .map(|e| e.digest)
            .collect::<Vec<_>>()
    };
    for r in [rostered(&a, &b), rostered(&b, &a)] {
        assert!(r.contains(&low), "the lowest digest is rostered");
        assert!(!r.contains(&high), "the other link is not");
    }
}
