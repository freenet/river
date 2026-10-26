//! The largest record of each legacy kind `f` can keep, against
//! `MAX_RECORD_BYTES` (the v2 kinds are covered by the largest-state test).
//! v1 byte vectors encode as CBOR arrays, so a byte of 0xff costs two.

use crate::room_state::ban::{AuthorizedUserBan, UserBan};
use crate::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use crate::room_state::direct_messages::{
    sign_direct_message, sign_recipient_purges, PurgeToken, RecipientPurges,
};
use crate::room_state::member::{AuthorizedMember, Member, MemberId};
use crate::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use crate::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use crate::room_state::privacy::{RoomDisplayMetadata, SealedBytes};
use crate::room_state::secret::{AuthorizedEncryptedSecretForMember, EncryptedSecretForMemberV1};
use crate::room_state_v2::config::ceiling;
use crate::room_state_v2::encoding::{to_cbor, ByteBuf, Key};
use crate::room_state_v2::legacy;
use crate::room_state_v2::records::{Legacy, Record};
use crate::room_state_v2::state::MAX_RECORD_BYTES;
use ed25519_dalek::SigningKey;
use std::time::{Duration, UNIX_EPOCH};

fn sk(i: u8) -> SigningKey {
    SigningKey::from_bytes(&[i; 32])
}

fn wrap(signer: &SigningKey, v1: Vec<u8>) -> Legacy {
    Legacy {
        signer: Key::from(&signer.verifying_key()),
        v1: ByteBuf(v1),
    }
}

fn check(name: &str, r: Record) {
    let n = to_cbor(&r).len();
    let k = r.kind() as usize;
    println!("{name}: {n} bytes (ceiling {})", MAX_RECORD_BYTES[k]);
    assert!(
        n <= MAX_RECORD_BYTES[k],
        "{name}: {n} > {}",
        MAX_RECORD_BYTES[k]
    );
}

#[test]
fn legacy_record_size_ceilings_hold() {
    let owner = sk(1);
    let o = Key::from(&owner.verifying_key());
    let oid = MemberId::from(&owner.verifying_key());
    let m = sk(2);
    let mid = MemberId::from(&m.verifying_key());
    let big = |n: usize| vec![0xffu8; n];
    let t = UNIX_EPOCH + Duration::from_secs(u32::MAX as u64 * 4);

    let cfg = Configuration {
        owner_member_id: oid,
        display: RoomDisplayMetadata {
            name: SealedBytes::public(big(ceiling::MAX_ROOM_NAME as usize)),
            description: Some(SealedBytes::public(big(
                ceiling::MAX_ROOM_DESCRIPTION as usize
            ))),
        },
        max_room_name: ceiling::MAX_ROOM_NAME as usize,
        max_room_description: ceiling::MAX_ROOM_DESCRIPTION as usize,
        max_recent_messages: usize::MAX,
        max_user_bans: usize::MAX,
        max_message_size: usize::MAX,
        max_nickname_size: usize::MAX,
        max_members: usize::MAX,
        max_direct_messages: Some(usize::MAX),
        configuration_version: u32::MAX,
        ..Configuration::default()
    };
    let l = wrap(
        &owner,
        to_cbor(&AuthorizedConfigurationV1::new(cfg, &owner)),
    );
    assert!(legacy::decode_config(&o, &l).is_some());
    check("LegacyConfiguration", Record::LegacyConfiguration(l));

    let ban = UserBan {
        owner_member_id: oid,
        banned_at: t,
        banned_user: MemberId(freenet_scaffold::util::FastHash(i64::MIN)),
    };
    let l = wrap(&m, to_cbor(&AuthorizedUserBan::new(ban, mid, &m)));
    assert!(legacy::decode_ban(&o, &l).is_some());
    check("LegacyBan", Record::LegacyBan(l));

    let member = Member {
        owner_member_id: oid,
        invited_by: mid,
        member_vk: sk(3).verifying_key(),
    };
    let l = wrap(&m, to_cbor(&AuthorizedMember::new(member, &m)));
    assert!(legacy::decode_link(&o, &l).is_some());
    check("LegacyLink", Record::LegacyLink(l));

    let msg = MessageV1 {
        room_owner: oid,
        author: mid,
        time: t,
        content: RoomMessageBody::private(
            u32::MAX,
            u32::MAX,
            big(ceiling::MAX_MESSAGE_SIZE as usize),
            [0xff; 12],
            u32::MAX,
        ),
    };
    let bytes = to_cbor(&AuthorizedMessageV1::new(msg, &m));
    let l = wrap(&m, bytes);
    assert!(legacy::decode_message(&o, &l).is_some());
    check("LegacyMessage", Record::LegacyMessage(l.clone()));
    check("LegacyActivity", Record::LegacyActivity(l));

    let info = MemberInfo {
        member_id: mid,
        version: u32::MAX,
        preferred_nickname: SealedBytes::private(
            big(ceiling::MAX_NICKNAME_SIZE as usize + 16),
            [0xff; 12],
            u32::MAX,
            ceiling::MAX_NICKNAME_SIZE,
        ),
        deputies: (0..crate::room_state::member_info::MAX_DEPUTIES as i64)
            .map(|i| MemberId(freenet_scaffold::util::FastHash(i64::MIN + i)))
            .collect(),
    };
    let l = wrap(
        &m,
        to_cbor(&AuthorizedMemberInfo::new_with_member_key(info, &m)),
    );
    assert!(legacy::decode_member_info(&o, &l).is_some());
    check("LegacyMemberInfo", Record::LegacyMemberInfo(l));

    let dm = sign_direct_message(
        &m,
        mid,
        oid,
        &owner.verifying_key(),
        u64::MAX,
        big(ceiling::DM_CIPHERTEXT),
    )
    .unwrap();
    let l = wrap(&m, to_cbor(&dm));
    assert!(legacy::decode_dm(&o, &l).is_some());
    check("LegacyDirectMessage", Record::LegacyDirectMessage(l));

    let purges = RecipientPurges {
        version: u64::MAX,
        purged: (0..ceiling::PURGE_TOKENS as u32)
            .map(|i| {
                let mut b = [0xff; 16];
                b[..4].copy_from_slice(&i.to_le_bytes());
                PurgeToken(b)
            })
            .collect(),
    };
    let p = sign_recipient_purges(&m, mid, &owner.verifying_key(), purges).unwrap();
    let l = wrap(&m, to_cbor(&p));
    assert!(legacy::decode_purge(&o, &l).is_some());
    check("LegacyPurge", Record::LegacyPurge(l));

    let blob = EncryptedSecretForMemberV1 {
        member_id: mid,
        secret_version: u32::MAX,
        ciphertext: big(ceiling::SECRET_BLOB_CIPHERTEXT),
        nonce: [0xff; 12],
        sender_ephemeral_public_key: [0xff; 32],
        provider: oid,
    };
    let l = wrap(
        &owner,
        to_cbor(&AuthorizedEncryptedSecretForMember::new(blob, &owner)),
    );
    assert!(legacy::decode_blob(&o, &l).is_some());
    check("LegacySecretBlob", Record::LegacySecretBlob(l));
}
