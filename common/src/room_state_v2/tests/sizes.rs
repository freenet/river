//! The largest record of each kind `f` can keep, against `MAX_RECORD_BYTES`:
//! every field at the largest value a kept record can carry (integers at
//! their maximum, lists and byte strings at their ceilings, the larger enum
//! variant). v1 byte vectors encode as CBOR arrays, so a byte of 0xff costs
//! two.

use crate::room_state::ban::{AuthorizedUserBan, UserBan};
use crate::room_state::direct_messages::{
    sign_direct_message, sign_recipient_purges, PurgeToken, RecipientPurges,
};
use crate::room_state::member::{AuthorizedMember, Member, MemberId};
use crate::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use crate::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use crate::room_state::privacy::SealedBytes;
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
            big(ceiling::LEGACY_MESSAGE_SIZE),
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
        big(ceiling::LEGACY_DM_CIPHERTEXT),
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

#[test]
fn v2_record_size_ceilings_hold() {
    use crate::room_state_v2::config::ConfigurationV2;
    use crate::room_state_v2::encoding::{Bytes32, Nonce12, Sig, Token16};
    use crate::room_state_v2::records::*;
    let o = Key::from(&sk(1).verifying_key());
    let sig = Sig([0xff; 64]);
    let key = |i: u32| {
        let mut b = [0xffu8; 32];
        b[..4].copy_from_slice(&i.to_be_bytes());
        Bytes32(b)
    };
    let buf = |n: usize| ByteBuf(vec![0xff; n]);
    let private = |declared: usize| {
        Sealed::Private(SealedPrivate {
            ciphertext: buf(declared + AEAD_TAG),
            nonce: Nonce12([0xff; 12]),
            secret_version: u32::MAX,
            declared_len: declared as u32,
        })
    };

    // Configuration: every cap at its ceiling, the longest name and
    // description (sealed), 64 deputies, 64 protected keys and 64 child
    // exceptions at the largest quota.
    use ceiling::*;
    let mut c = ConfigurationV2::defaults(MAX_MEMBERS);
    c.version = u32::MAX;
    c.privacy_mode = 1;
    c.max_room_name = MAX_ROOM_NAME;
    c.max_room_description = MAX_ROOM_DESCRIPTION;
    c.display_name = private(MAX_ROOM_NAME as usize);
    c.display_description = Some(private(MAX_ROOM_DESCRIPTION as usize));
    c.max_recent_messages = MAX_RECENT_MESSAGES;
    c.max_message_size = MAX_MESSAGE_SIZE;
    c.max_nickname_size = MAX_NICKNAME_SIZE;
    c.max_direct_messages = MAX_DIRECT_MESSAGES;
    c.max_chain_depth = MAX_CHAIN_DEPTH;
    c.max_links = MAX_LINKS;
    c.max_links_per_member = MAX_LINKS_PER_MEMBER;
    c.max_children_per_link = MAX_CHILDREN_PER_LINK;
    c.max_owner_children = MAX_CHILDREN_PER_LINK;
    c.child_exceptions = (0..CHILD_EXCEPTIONS as u32)
        .map(|i| (key(i), MAX_CHILDREN_PER_LINK))
        .collect();
    c.max_owner_bans = MAX_OWNER_BANS;
    c.max_deputy_bans = MAX_DEPUTY_BANS;
    c.max_bans_per_deputy = MAX_BANS_PER_DEPUTY;
    c.max_member_bans = MAX_MEMBER_BANS;
    c.max_bans_per_issuer = MAX_BANS_PER_ISSUER;
    c.deputies = (0..AUTHORITY_KEYS as u32).map(key).collect();
    c.protected = (0..AUTHORITY_KEYS as u32).map(|i| key(i + 1000)).collect();
    c.max_secret_versions = MAX_SECRET_VERSIONS;
    c.max_secret_blobs = MAX_SECRET_BLOBS;
    c.legacy_window_closed_at = u32::MAX;
    c.link_retention = u32::MAX;
    c.member_ban_activity_window = u32::MAX;
    assert!(c.check(&o));
    check(
        "Configuration",
        Record::Configuration(Signed { payload: c, sig }),
    );

    check(
        "Clock",
        Record::Clock(Signed {
            payload: ClockP { hours: u32::MAX },
            sig,
        }),
    );
    check(
        "Checkpoint",
        Record::Checkpoint(Signed {
            payload: CheckpointP {
                version: u32::MAX,
                retire_before: u32::MAX,
                keep: (0..MAX_LINKS).map(key).collect(),
            },
            sig,
        }),
    );
    check(
        "Upgrade",
        Record::Upgrade(Signed {
            payload: UpgradeP {
                version: u32::MAX,
                new_address: key(0),
            },
            sig,
        }),
    );
    check(
        "Roster",
        Record::Roster(Signed {
            payload: RosterP {
                seq: u32::MAX,
                batch: (0..ROSTER_BATCH_ENTRIES as u32)
                    .map(|i| RosterEntry {
                        kind: LegacyKind::SecretBlob,
                        digest: key(i),
                    })
                    .collect(),
            },
            sig,
        }),
    );
    check(
        "SecretVersion",
        Record::SecretVersion(Signed {
            payload: SecretVersionP {
                version: u32::MAX,
                cipher: u8::MAX,
            },
            sig,
        }),
    );
    check(
        "Ban",
        Record::Ban(Signed {
            payload: BanP {
                target: BanTarget::Invitation(InvitationTarget {
                    invitation: key(1),
                    inviter: key(2),
                }),
                issuer: Issuer::Member(MemberIssuer {
                    link: key(3),
                    key: key(4),
                }),
                time: u64::MAX,
                clock_ref: u32::MAX,
                reason: buf(BAN_REASON),
            },
            sig,
        }),
    );
    let link = |i: u32| AcceptedLink {
        invitation: Signed {
            payload: InvitationP {
                inviter: key(i),
                invite_key: key(i + 1),
                parent: Parent::Link(key(i + 2)),
                seq: u32::MAX,
                clock: u32::MAX,
            },
            sig,
        },
        acceptance: AcceptanceP {
            invitation: key(i + 3),
            member: key(i + 4),
        },
        invite_key_sig: sig,
        member_sig: sig,
    };
    check("Link", Record::Link(link(0)));
    check(
        "Activity",
        Record::Activity(Signed {
            payload: ActivityP {
                member: key(0),
                hours: u32::MAX,
            },
            sig,
        }),
    );
    check(
        "Restore",
        Record::Restore(Signed {
            payload: RestoreP {
                chain: (0..MAX_CHAIN_DEPTH)
                    .map(|i| ChainLink::Accepted(link(i * 10)))
                    .collect(),
                hours: u32::MAX,
                clock_ref: u32::MAX,
            },
            sig,
        }),
    );
    check(
        "MemberInfo",
        Record::MemberInfo(Signed {
            payload: MemberInfoP {
                member: key(0),
                version: u32::MAX,
                nickname: private(MAX_NICKNAME_SIZE as usize),
            },
            sig,
        }),
    );
    check(
        "Message",
        Record::Message(Signed {
            payload: MessageP {
                author: key(0),
                time: u64::MAX,
                clock_ref: u32::MAX,
                body: MessageBody::Private(PrivateBody {
                    content_type: u32::MAX,
                    content_version: u32::MAX,
                    ciphertext: buf(MAX_MESSAGE_SIZE as usize),
                    nonce: Nonce12([0xff; 12]),
                    secret_version: u32::MAX,
                }),
            },
            sig,
        }),
    );
    check(
        "DirectMessage",
        Record::DirectMessage(Signed {
            payload: DirectMessageP {
                sender: key(0),
                recipient: key(1),
                time: u64::MAX,
                clock_ref: u32::MAX,
                ciphertext: buf(DM_CIPHERTEXT),
            },
            sig,
        }),
    );
    check(
        "Purge",
        Record::Purge(Signed {
            payload: PurgeP {
                recipient: key(0),
                version: u64::MAX,
                purged: (0..PURGE_TOKENS as u32)
                    .map(|i| {
                        let mut t = [0xffu8; 16];
                        t[..4].copy_from_slice(&i.to_be_bytes());
                        Token16(t)
                    })
                    .collect(),
            },
            sig,
        }),
    );
    check(
        "SecretBlob",
        Record::SecretBlob(Signed {
            payload: SecretBlobP {
                recipient: key(0),
                version: u32::MAX,
                ciphertext: buf(SECRET_BLOB_CIPHERTEXT),
                nonce: Nonce12([0xff; 12]),
                ephemeral_key: key(1),
            },
            sig,
        }),
    );
}
