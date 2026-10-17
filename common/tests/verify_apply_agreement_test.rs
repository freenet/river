//! `verify` (what a stored or PUT state may hold) and `apply_delta` (what a
//! merge admits) must agree on every validly signed record: a record that
//! `verify` accepts but `apply_delta` refuses makes every merge with a peer
//! holding it fail, and nothing but its author could ever repair it
//! (freenet/river#703). Such records are now handled the same way on
//! both sides of a merge: skipped on arrival and dropped where held, so both
//! merge directions end in the same state and the result verifies.
//!
//! `verify` itself is unchanged (the migration PUT runs only `verify`).

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::privacy::{PrivacyMode, RoomCipherSpec, SealedBytes};
use river_core::room_state::secret::{
    AuthorizedSecretVersionRecord, RoomSecretsV1, SecretVersionRecordV1,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::time::{Duration, SystemTime};

struct Room {
    owner_sk: SigningKey,
    owner_id: MemberId,
    params: ChatRoomParametersV1,
    member_sk: SigningKey,
    member: AuthorizedMember,
}

fn room() -> Room {
    let owner_sk = SigningKey::from_bytes(&[3; 32]);
    let owner_id = MemberId::from(&owner_sk.verifying_key());
    let member_sk = SigningKey::from_bytes(&[4; 32]);
    let member = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member_sk.verifying_key(),
        },
        &owner_sk,
    );
    Room {
        params: ChatRoomParametersV1 {
            owner: owner_sk.verifying_key(),
        },
        owner_sk,
        owner_id,
        member_sk,
        member,
    }
}

fn at(t: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + t)
}

fn state(r: &Room, config: Configuration) -> ChatRoomStateV1 {
    ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(config, &r.owner_sk),
        members: MembersV1 {
            members: vec![r.member.clone()],
        },
        ..Default::default()
    }
}

fn public_config(r: &Room) -> Configuration {
    Configuration {
        owner_member_id: r.owner_id,
        max_nickname_size: 10,
        ..Default::default()
    }
}

fn private_room(r: &Room) -> ChatRoomStateV1 {
    let mut s = state(
        r,
        Configuration {
            owner_member_id: r.owner_id,
            privacy_mode: PrivacyMode::Private,
            display: river_core::room_state::privacy::RoomDisplayMetadata {
                name: SealedBytes::private(vec![1; 8], [0; 12], 1, 8),
                description: None,
            },
            ..Default::default()
        },
    );
    s.secrets = RoomSecretsV1 {
        current_version: 1,
        versions: vec![AuthorizedSecretVersionRecord::new(
            SecretVersionRecordV1 {
                version: 1,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at: at(0),
            },
            &r.owner_sk,
        )],
        encrypted_secrets: vec![],
    };
    s
}

fn msg(r: &Room, t: u64, content: RoomMessageBody) -> AuthorizedMessageV1 {
    AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: r.owner_id,
            author: r.member.member.id(),
            time: at(t),
            content,
        },
        &r.member_sk,
    )
}

fn ser(s: &ChatRoomStateV1) -> Vec<u8> {
    let mut v = Vec::new();
    ciborium::ser::into_writer(s, &mut v).unwrap();
    v
}

/// `holder` holds a record `verify` accepts; `other` does not hold it. Both
/// merge directions must succeed, agree, and verify.
fn assert_merges_agree(
    r: &Room,
    holder: &ChatRoomStateV1,
    other: &ChatRoomStateV1,
) -> ChatRoomStateV1 {
    holder
        .verify(holder, &r.params)
        .expect("premise: verify accepts it");
    other.verify(other, &r.params).unwrap();
    let mut a = other.clone();
    a.merge(other, &r.params, holder)
        .expect("a merge with a peer holding it must not fail");
    let mut b = holder.clone();
    b.merge(holder, &r.params, other).unwrap();
    assert!(ser(&a) == ser(&b), "merge(A, B) must equal merge(B, A)");
    a.verify(&a, &r.params).unwrap();
    a
}

#[test]
fn a_nickname_over_the_ceiling_is_dropped_in_both_directions() {
    let r = room();
    let base = state(&r, public_config(&r));
    let mut holder = base.clone();
    // A message keeps the member from being pruned for inactivity.
    holder.recent_messages.messages = vec![msg(&r, 1, RoomMessageBody::public("hi".into()))];
    holder.member_info = MemberInfoV1 {
        member_info: vec![AuthorizedMemberInfo::new_with_member_key(
            MemberInfo::new_public(
                r.member.member.id(),
                1,
                "a".repeat(river_core::room_state::member_info::NICKNAME_CEILING + 1),
            ),
            &r.member_sk,
        )],
    };
    let mut other = base.clone();
    other.recent_messages.messages = holder.recent_messages.messages.clone();
    let merged = assert_merges_agree(&r, &holder, &other);
    assert!(merged.member_info.member_info.is_empty());
}

#[test]
fn a_private_message_at_an_unknown_secret_version_is_dropped_in_both_directions() {
    let r = room();
    let base = private_room(&r);
    let mut holder = base.clone();
    holder.recent_messages.messages = vec![
        msg(
            &r,
            1,
            RoomMessageBody::private_text(vec![7; 16], [0; 12], 1),
        ),
        msg(
            &r,
            2,
            RoomMessageBody::private_text(vec![8; 16], [0; 12], 9),
        ),
    ];
    let mut other = base.clone();
    other.recent_messages.messages = vec![holder.recent_messages.messages[0].clone()];
    let merged = assert_merges_agree(&r, &holder, &other);
    assert_eq!(
        merged.recent_messages.messages,
        other.recent_messages.messages
    );
}

#[test]
fn a_public_message_in_a_private_room_is_dropped_in_both_directions() {
    let r = room();
    let base = private_room(&r);
    let mut holder = base.clone();
    holder.recent_messages.messages = vec![
        msg(
            &r,
            1,
            RoomMessageBody::private_text(vec![7; 16], [0; 12], 1),
        ),
        msg(&r, 2, RoomMessageBody::public("plaintext".into())),
    ];
    let mut other = base.clone();
    other.recent_messages.messages = vec![holder.recent_messages.messages[0].clone()];
    let merged = assert_merges_agree(&r, &holder, &other);
    assert_eq!(
        merged.recent_messages.messages,
        other.recent_messages.messages
    );
}

#[test]
fn an_encrypted_secret_at_an_unknown_version_is_dropped_in_both_directions() {
    use river_core::room_state::secret::{
        AuthorizedEncryptedSecretForMember, EncryptedSecretForMemberV1,
    };
    let r = room();
    let base = private_room(&r);
    let blob = |version| {
        AuthorizedEncryptedSecretForMember::new(
            EncryptedSecretForMemberV1 {
                member_id: r.member.member.id(),
                secret_version: version,
                ciphertext: vec![1, 2, 3],
                nonce: [0; 12],
                sender_ephemeral_public_key: [0; 32],
                provider: r.owner_id,
            },
            &r.owner_sk,
        )
    };
    let mut holder = base.clone();
    holder.secrets.encrypted_secrets = vec![blob(1), blob(9)];
    let mut other = base.clone();
    other.secrets.encrypted_secrets = vec![blob(1)];
    let merged = assert_merges_agree(&r, &holder, &other);
    assert_eq!(merged.secrets.encrypted_secrets, vec![blob(1)]);
}

/// `verify` does not bound the number of messages a state holds, so a
/// full-state PUT can carry more than one messages delta may. A merge with
/// such a state must not fail on the work bound, in either direction.
#[test]
fn a_state_holding_more_messages_than_one_delta_may_carry_merges() {
    let r = room();
    let mut config = public_config(&r);
    config.max_recent_messages = 100;
    let base = state(&r, config);
    let mut holder = base.clone();
    holder.recent_messages.messages = (0..1_500)
        .map(|t| msg(&r, t, RoomMessageBody::public(format!("m{t}"))))
        .collect();
    let mut other = base.clone();
    other.recent_messages.messages = vec![msg(&r, 2_000, RoomMessageBody::public("x".into()))];
    let merged = assert_merges_agree(&r, &holder, &other);
    assert_eq!(merged.recent_messages.messages.len(), 100);
}

/// Two secret version records at one version, as the UI's rotation (wall
/// clock `created_at`) and the delegate's (`UNIX_EPOCH`) sign them: whole-state
/// merges end on the same record in both directions. A record at a higher
/// version that is not owner-signed fails the merge.
#[test]
fn equal_version_secret_records_merge_the_same_way_and_forgeries_fail() {
    let r = room();
    let base = private_room(&r);
    let record = |created_at: SystemTime, sk: &SigningKey, version: u32| {
        AuthorizedSecretVersionRecord::new(
            SecretVersionRecordV1 {
                version,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at,
            },
            sk,
        )
    };
    let with_v2 = |rec: AuthorizedSecretVersionRecord| {
        let mut s = base.clone();
        s.secrets.versions.push(rec);
        s.secrets.current_version = 2;
        s
    };
    let ui = with_v2(record(at(500), &r.owner_sk, 2));
    let delegate = with_v2(record(SystemTime::UNIX_EPOCH, &r.owner_sk, 2));
    let merged = assert_merges_agree(&r, &ui, &delegate);
    assert_eq!(merged.secrets.versions.len(), 2);

    let forged = with_v2(record(at(500), &SigningKey::from_bytes(&[9; 32]), 2));
    let mut m = base.clone();
    assert!(m.merge(&base, &r.params, &forged).is_err());
}

/// Two upgrade pointers at one version, signed by the owner for different
/// successors: whole-state merges keep the same one in both directions. A
/// pointer that is not owner-signed fails the merge.
#[test]
fn equal_version_upgrade_pointers_merge_the_same_way_and_forgeries_fail() {
    use river_core::room_state::upgrade::{AuthorizedUpgradeV1, OptionalUpgradeV1, UpgradeV1};
    let r = room();
    let base = state(&r, public_config(&r));
    let pointer = |byte: u8, sk: &SigningKey| {
        let mut s = base.clone();
        s.upgrade = OptionalUpgradeV1(Some(AuthorizedUpgradeV1::new(
            UpgradeV1 {
                owner_member_id: r.owner_id,
                version: 1,
                new_chatroom_address: blake3::Hash::from([byte; 32]),
            },
            sk,
        )));
        s
    };
    let (a, b) = (pointer(1, &r.owner_sk), pointer(2, &r.owner_sk));
    assert_merges_agree(&r, &a, &b);
    let forged = pointer(3, &SigningKey::from_bytes(&[9; 32]));
    let mut m = base.clone();
    assert!(m.merge(&base, &r.params, &forged).is_err());
}

/// A pointer or secret version record that outranks the held one but is not
/// owner-signed fails the merge, or at worst is ignored; it is never adopted.
#[test]
fn forged_pointers_never_replace_a_held_pointer() {
    use river_core::room_state::upgrade::{AuthorizedUpgradeV1, OptionalUpgradeV1, UpgradeV1};
    let r = room();
    let base = state(&r, public_config(&r));
    let pointer = |version: u8, byte: u8, sk: &SigningKey| {
        let mut s = base.clone();
        s.upgrade = OptionalUpgradeV1(Some(AuthorizedUpgradeV1::new(
            UpgradeV1 {
                owner_member_id: r.owner_id,
                version,
                new_chatroom_address: blake3::Hash::from([byte; 32]),
            },
            sk,
        )));
        s
    };
    let held = pointer(1, 1, &r.owner_sk);
    let attacker = SigningKey::from_bytes(&[9; 32]);
    let mut errs = 0;
    for byte in 0..32u8 {
        for version in [1u8, 2] {
            let forged = pointer(version, byte, &attacker);
            let mut m = held.clone();
            match m.merge(&held, &r.params, &forged) {
                Err(_) => errs += 1,
                Ok(()) => assert_eq!(m.upgrade, held.upgrade, "forged pointer adopted"),
            }
        }
    }
    assert!(
        errs > 0,
        "some forged pointer must outrank the held one and fail"
    );
}

#[test]
fn forged_secret_records_never_replace_a_held_record() {
    let r = room();
    let base = private_room(&r);
    let record = |created_at: SystemTime, sk: &SigningKey| {
        AuthorizedSecretVersionRecord::new(
            SecretVersionRecordV1 {
                version: 1,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at,
            },
            sk,
        )
    };
    let attacker = SigningKey::from_bytes(&[9; 32]);
    let mut errs = 0;
    for t in 0..32u64 {
        let mut forged = base.clone();
        forged.secrets.versions = vec![record(at(100 + t), &attacker)];
        let mut m = base.clone();
        match m.merge(&base, &r.params, &forged) {
            Err(_) => errs += 1,
            Ok(()) => assert_eq!(
                m.secrets.versions, base.secrets.versions,
                "forged record adopted"
            ),
        }
    }
    assert!(
        errs > 0,
        "some forged record must outrank the held one and fail"
    );
}

/// Records already held do not count toward a delta's work bound: 1001 held
/// members (or messages) re-delivered with one new one still apply.
#[test]
fn redelivered_members_do_not_count_toward_the_work_bound() {
    use river_core::room_state::member::MembersDelta;
    let r = room();
    let mut config = public_config(&r);
    config.max_members = 1000;
    let parent = state(&r, config);
    let mk = |i: u8, j: u8| {
        let sk = SigningKey::from_bytes(&[
            i, j, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
            7, 7, 7,
        ]);
        AuthorizedMember::new(
            Member {
                owner_member_id: r.owner_id,
                invited_by: r.owner_id,
                member_vk: sk.verifying_key(),
            },
            &r.owner_sk,
        )
    };
    let held: Vec<AuthorizedMember> = (0..1001u32)
        .map(|n| mk((n / 250) as u8, (n % 250) as u8))
        .collect();
    let mut members = MembersV1 {
        members: held.clone(),
    };
    let mut delta = held.clone();
    delta.push(mk(200, 1));
    members
        .apply_delta(&parent, &r.params, &Some(MembersDelta::new(delta)))
        .expect("held members re-delivered with one new must apply");
}

#[test]
fn redelivered_messages_do_not_count_toward_the_work_bound() {
    use river_core::room_state::message::MessagesV1;
    let r = room();
    let mut config = public_config(&r);
    config.max_recent_messages = 100;
    let parent = state(&r, config);
    let held: Vec<AuthorizedMessageV1> = (0..1001u64)
        .map(|t| msg(&r, t, RoomMessageBody::public(format!("m{t}"))))
        .collect();
    let mut messages = MessagesV1 {
        messages: held.clone(),
        ..Default::default()
    };
    let mut delta = held.clone();
    delta.push(msg(&r, 5000, RoomMessageBody::public("new".into())));
    messages
        .apply_delta(&parent, &r.params, &Some(delta))
        .expect("held messages re-delivered with one new must apply");
}

// ---------------------------------------------------------------------------
// Normalization of held state (freenet/river#703): a state `verify`
// accepts but no apply would produce converges with its normal form.
// ---------------------------------------------------------------------------

fn member_ban(
    r: &Room,
    target: MemberId,
    t: u64,
) -> river_core::room_state::ban::AuthorizedUserBan {
    river_core::room_state::ban::AuthorizedUserBan::new(
        river_core::room_state::ban::UserBan {
            owner_member_id: r.owner_id,
            banned_at: at(t),
            banned_user: target,
        },
        r.member.member.id(),
        &r.member_sk,
    )
}

#[test]
fn a_duplicated_message_entry_is_normalized_in_both_directions() {
    let r = room();
    let base = state(&r, public_config(&r));
    let m = msg(&r, 1, RoomMessageBody::public("hi".into()));
    let mut holder = base.clone();
    holder.recent_messages.messages = vec![m.clone(), m.clone()];
    let mut other = base.clone();
    other.recent_messages.messages = vec![m.clone()];
    let merged = assert_merges_agree(&r, &holder, &other);
    assert_eq!(merged.recent_messages.messages, vec![m]);
}

#[test]
fn a_duplicated_ban_entry_is_normalized_in_both_directions() {
    let r = room();
    let mut base = state(&r, public_config(&r));
    base.recent_messages.messages = vec![msg(&r, 1, RoomMessageBody::public("hi".into()))];
    let ban = member_ban(
        &r,
        MemberId::from(&SigningKey::from_bytes(&[8; 32]).verifying_key()),
        2,
    );
    let mut holder = base.clone();
    holder.bans.0 = vec![ban.clone(), ban.clone()];
    let mut other = base.clone();
    other.bans.0 = vec![ban.clone()];
    let merged = assert_merges_agree(&r, &holder, &other);
    assert_eq!(merged.bans.0, vec![ban]);
}

/// Two ban entries that share a signature but name different issuers are
/// different records: the bans summary must tell a state holding both from
/// one holding only the genuine one, or anti-entropy never reconciles them.
#[test]
fn ban_records_sharing_a_signature_are_told_apart_by_the_summary() {
    use river_core::room_state::ban::AuthorizedUserBan;
    let r = room();
    let mut base = state(&r, public_config(&r));
    base.recent_messages.messages = vec![msg(&r, 1, RoomMessageBody::public("hi".into()))];
    let ban = member_ban(
        &r,
        MemberId::from(&SigningKey::from_bytes(&[8; 32]).verifying_key()),
        2,
    );
    let unknown = MemberId::from(&SigningKey::from_bytes(&[12; 32]).verifying_key());
    let copy = AuthorizedUserBan::with_signature(ban.ban.clone(), unknown, ban.signature);
    let mut holder = base.clone();
    holder.bans.0 = vec![ban.clone(), copy];
    let mut other = base.clone();
    other.bans.0 = vec![ban.clone()];
    assert_ne!(
        holder.bans.summarize(&holder, &r.params),
        other.bans.summarize(&other, &r.params),
        "the summary must cover the whole ban record"
    );
    let merged = assert_merges_agree(&r, &holder, &other);
    assert_eq!(merged.bans.0, vec![ban]);
}

/// The state format version merges to the higher of the two (both are
/// formats this contract reads).
#[test]
fn the_state_version_merges_to_the_higher() {
    use river_core::room_state::version::StateVersion;
    let r = room();
    let base = state(&r, public_config(&r));
    let mut v0 = base.clone();
    v0.version = StateVersion(0);
    let mut v1 = base.clone();
    v1.version = StateVersion(1);
    let merged = assert_merges_agree(&r, &v0, &v1);
    assert_eq!(merged.version, StateVersion(1));
}

// ---------------------------------------------------------------------------
// Secrets normalization (freenet/river#703)
// ---------------------------------------------------------------------------

fn blob_for(
    r: &Room,
    member: MemberId,
    version: u32,
    ciphertext: Vec<u8>,
) -> river_core::room_state::secret::AuthorizedEncryptedSecretForMember {
    use river_core::room_state::secret::{
        AuthorizedEncryptedSecretForMember, EncryptedSecretForMemberV1,
    };
    AuthorizedEncryptedSecretForMember::new(
        EncryptedSecretForMemberV1 {
            member_id: member,
            secret_version: version,
            ciphertext,
            nonce: [0; 12],
            sender_ephemeral_public_key: [0; 32],
            provider: r.owner_id,
        },
        &r.owner_sk,
    )
}

/// A blob for someone who is not a member is dropped whether or not the
/// apply carries a secrets delta.
#[test]
fn a_blob_for_a_non_member_is_dropped_in_both_directions() {
    let r = room();
    let base = private_room(&r);
    let gone = MemberId::from(&SigningKey::from_bytes(&[13; 32]).verifying_key());
    let mut holder = base.clone();
    holder.secrets.encrypted_secrets = vec![blob_for(&r, gone, 1, vec![1, 2, 3])];
    let merged = assert_merges_agree(&r, &holder, &base);
    assert!(merged.secrets.encrypted_secrets.is_empty());
}

/// Two owner-signed records at one held version: one is kept, the same on
/// both sides.
#[test]
fn two_records_held_at_one_version_are_normalized_in_both_directions() {
    let r = room();
    let base = private_room(&r);
    let mut holder = base.clone();
    let mut second = base.secrets.versions[0].record.clone();
    second.created_at = at(77);
    holder
        .secrets
        .versions
        .push(AuthorizedSecretVersionRecord::new(second, &r.owner_sk));
    let merged = assert_merges_agree(&r, &holder, &base);
    assert_eq!(merged.secrets.versions.len(), 1);
}

/// Two different owner-signed blobs for one (version, member), each held by
/// one peer: both sides end on the same blob.
#[test]
fn different_blobs_for_one_member_and_version_converge() {
    let r = room();
    let base = private_room(&r);
    let mut a = base.clone();
    a.secrets.encrypted_secrets = vec![blob_for(&r, r.member.member.id(), 1, vec![1, 2, 3])];
    let mut b = base.clone();
    b.secrets.encrypted_secrets = vec![blob_for(&r, r.member.member.id(), 1, vec![4, 5, 6])];
    let merged = assert_merges_agree(&r, &a, &b);
    assert_eq!(merged.secrets.encrypted_secrets.len(), 1);
}

/// A held owner-signed configuration that fails the value checks (only a
/// full-state PUT carries one; `verify` checks only the signature) does not
/// compete: both merge directions end on the valid configuration
/// (freenet/river#703).
#[test]
fn an_invalid_held_configuration_yields_to_a_valid_one_in_both_directions() {
    let r = room();
    let base = state(&r, public_config(&r));
    let mut invalid = base.clone();
    let mut c = public_config(&r);
    c.configuration_version = 5;
    c.max_recent_messages = 0;
    invalid.configuration = AuthorizedConfigurationV1::new(c, &r.owner_sk);
    let mut valid = base.clone();
    let mut c = public_config(&r);
    c.configuration_version = 3;
    valid.configuration = AuthorizedConfigurationV1::new(c, &r.owner_sk);
    let merged = assert_merges_agree(&r, &invalid, &valid);
    assert_eq!(merged.configuration, valid.configuration);
}

// ---------------------------------------------------------------------------
// Nickname limit (freenet/river#703)
// ---------------------------------------------------------------------------

/// Lowering `max_nickname_size` drops no record: the room holds every
/// record only to the fixed nickname ceiling, so deputy grants and plain
/// records over the new limit are all kept (freenet/river#703).
#[test]
fn lowering_the_nickname_limit_drops_no_record() {
    use river_core::room_state::ChatRoomStateV1Delta;
    let r = room();
    let e_sk = SigningKey::from_bytes(&[14; 32]);
    let e = AuthorizedMember::new(
        Member {
            owner_member_id: r.owner_id,
            invited_by: r.owner_id,
            member_vk: e_sk.verifying_key(),
        },
        &r.owner_sk,
    );
    let mut s = state(&r, public_config(&r));
    s.members.members.push(e.clone());
    s.recent_messages.messages = vec![
        msg(&r, 1, RoomMessageBody::public("hi".into())),
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: r.owner_id,
                author: e.member.id(),
                time: at(2),
                content: RoomMessageBody::public("hey".into()),
            },
            &e_sk,
        ),
    ];
    let mut grant = MemberInfo::new_public(r.member.member.id(), 1, "moderator".into());
    grant.deputies = vec![e.member.id()];
    s.member_info = MemberInfoV1 {
        member_info: vec![
            AuthorizedMemberInfo::new_with_member_key(grant, &r.member_sk),
            AuthorizedMemberInfo::new_with_member_key(
                MemberInfo::new_public(e.member.id(), 1, "plain long".into()),
                &e_sk,
            ),
        ],
    };
    s.post_apply_cleanup(&r.params).unwrap();
    s.verify(&s, &r.params).unwrap();
    let mut c = s.configuration.configuration.clone();
    c.configuration_version += 1;
    c.max_nickname_size = 3;
    let mut after = s.clone();
    after
        .apply_delta(
            &s,
            &r.params,
            &Some(ChatRoomStateV1Delta {
                configuration: Some(AuthorizedConfigurationV1::new(c, &r.owner_sk)),
                ..Default::default()
            }),
        )
        .unwrap();
    assert_eq!(
        after.member_info.deputies_of(r.member.member.id()),
        &[e.member.id()][..],
        "a deputy grant must survive a lowered nickname limit"
    );
    assert!(after.member_info.canonical(e.member.id()).is_some());
}

/// A peer holding a newer record over the nickname ceiling and one holding
/// an older record within it end on the same record in both merge
/// directions.
#[test]
fn an_over_ceiling_record_and_an_older_valid_one_converge() {
    let r = room();
    let mut base = state(&r, public_config(&r));
    base.recent_messages.messages = vec![msg(&r, 1, RoomMessageBody::public("hi".into()))];
    let info = |version, nick: &str| MemberInfoV1 {
        member_info: vec![AuthorizedMemberInfo::new_with_member_key(
            MemberInfo::new_public(r.member.member.id(), version, nick.into()),
            &r.member_sk,
        )],
    };
    let mut long = base.clone();
    long.member_info = info(
        5,
        &"b".repeat(river_core::room_state::member_info::NICKNAME_CEILING + 1),
    );
    let mut short = base.clone();
    short.member_info = info(1, "ok");
    let merged = assert_merges_agree(&r, &long, &short);
    assert_eq!(merged.member_info, short.member_info);
}

/// Two different owner-signed configurations that both fail the value
/// checks, held by two peers with no valid one between them: the summaries
/// tell them apart, and both merge directions settle on the same one; a
/// valid configuration still outranks both (freenet/river#703 review R3).
#[test]
fn two_invalid_held_configurations_converge_and_a_valid_one_beats_both() {
    let r = room();
    let base = state(&r, public_config(&r));
    let invalid = |version: u32| {
        let mut s = base.clone();
        let mut c = public_config(&r);
        c.configuration_version = version;
        c.max_recent_messages = 0;
        s.configuration = AuthorizedConfigurationV1::new(c, &r.owner_sk);
        s
    };
    let (a, b) = (invalid(4), invalid(6));
    assert_ne!(
        a.summarize(&a, &r.params).configuration,
        b.summarize(&b, &r.params).configuration,
        "the summaries tell two invalid configurations apart"
    );
    let merged = assert_merges_agree(&r, &a, &b);
    assert_eq!(merged.configuration, b.configuration);

    let mut valid = base.clone();
    let mut c = public_config(&r);
    c.configuration_version = 2;
    valid.configuration = AuthorizedConfigurationV1::new(c, &r.owner_sk);
    let merged = assert_merges_agree(&r, &b, &valid);
    assert_eq!(merged.configuration, valid.configuration);
}

/// A state holding TWO blobs for one (version, member): the blob twin of
/// `two_records_held_at_one_version_are_normalized_in_both_directions`.
#[test]
fn two_blobs_held_for_one_member_and_version_are_normalized_in_both_directions() {
    let r = room();
    let base = private_room(&r);
    let mut holder = base.clone();
    holder.secrets.encrypted_secrets = vec![
        blob_for(&r, r.member.member.id(), 1, vec![1, 2, 3]),
        blob_for(&r, r.member.member.id(), 1, vec![4, 5, 6]),
    ];
    let merged = assert_merges_agree(&r, &holder, &base);
    assert_eq!(merged.secrets.encrypted_secrets.len(), 1);
}

/// A blob that does NOT outrank the held one is a no-op, in whichever order
/// the pair arrives.
#[test]
fn a_lower_ranked_blob_never_replaces_a_held_one_in_either_order() {
    use river_core::room_state::secret::SecretsDelta;
    let r = room();
    let base = private_room(&r);
    let a = blob_for(&r, r.member.member.id(), 1, vec![1, 2, 3]);
    let b = blob_for(&r, r.member.member.id(), 1, vec![4, 5, 6]);
    let (low, high) = if a.digest() < b.digest() {
        (a, b)
    } else {
        (b, a)
    };
    for (held, arriving) in [(&low, &high), (&high, &low)] {
        let mut s = base.clone();
        s.secrets.encrypted_secrets = vec![held.clone()];
        let mut secrets = s.secrets.clone();
        secrets
            .apply_delta(
                &s,
                &r.params,
                &Some(SecretsDelta {
                    current_version: None,
                    new_versions: vec![],
                    new_encrypted_secrets: vec![arriving.clone()],
                }),
            )
            .unwrap();
        assert_eq!(secrets.encrypted_secrets, vec![high.clone()]);
    }
}

/// The state version only moves up, and never past what this contract reads.
#[test]
fn the_state_version_ignores_lower_and_unreadable_deltas() {
    use river_core::room_state::version::StateVersion;
    let r = room();
    let s = state(&r, public_config(&r));
    let mut v = StateVersion(1);
    v.apply_delta(&s, &r.params, &Some(0)).unwrap();
    assert_eq!(v, StateVersion(1), "a lower version never lowers it");
    v.apply_delta(&s, &r.params, &Some(99)).unwrap();
    assert_eq!(
        v,
        StateVersion(1),
        "a version this contract does not read is ignored"
    );
    let mut z = StateVersion(0);
    z.apply_delta(&s, &r.params, &Some(1)).unwrap();
    assert_eq!(z, StateVersion(1));
}

/// A ban attributed to a member, with a signature that member's key does not
/// verify, held by a peer that does not know that member (`verify` cannot
/// check it there): a peer that knows the member drops it on arrival instead
/// of failing the merge, so both directions agree (freenet/river#703 review
/// S5).
#[test]
fn a_ban_with_a_bad_signature_from_a_known_issuer_is_dropped_in_both_directions() {
    use river_core::room_state::ban::{AuthorizedUserBan, BansV1, UserBan};
    let r = room();
    let mut knows = state(&r, public_config(&r));
    knows.recent_messages.messages = vec![msg(&r, 1, RoomMessageBody::public("hi".into()))];
    let target = MemberId::from(&SigningKey::from_bytes(&[15; 32]).verifying_key());
    let ban = UserBan {
        owner_member_id: r.owner_id,
        banned_at: at(5),
        banned_user: target,
    };
    let other_sk = SigningKey::from_bytes(&[16; 32]);
    let signed_by_other = AuthorizedUserBan::new(
        ban.clone(),
        MemberId::from(&other_sk.verifying_key()),
        &other_sk,
    );
    let bad =
        AuthorizedUserBan::with_signature(ban, r.member.member.id(), signed_by_other.signature);
    let mut unaware = state(&r, public_config(&r));
    unaware.members.members.clear();
    unaware.bans = BansV1(vec![bad]);
    let merged = assert_merges_agree(&r, &unaware, &knows);
    assert!(merged.bans.0.is_empty());
}

/// A receiver whose configuration is not signed by the room owner (an
/// imported room's placeholder) takes the owner's configuration in a merge
/// even when the placeholder passes the value checks and outranks it at an
/// equal version: a placeholder never survives a merge with the real one.
#[test]
fn an_unsigned_valid_placeholder_yields_to_the_owners_configuration() {
    let r = room();
    let real = state(&r, public_config(&r));
    let impostor = SigningKey::from_bytes(&[17; 32]);
    // Find a placeholder signature whose digest outranks the real one at
    // the same version, so only the placeholder branch can replace it.
    let real_rank = real.configuration.rank(&r.params);
    let placeholder = (0u32..64)
        .map(|i| {
            let mut c = public_config(&r);
            c.max_message_size = 1000 + i as usize;
            AuthorizedConfigurationV1::new(c, &impostor)
        })
        .find(|p| p.rank(&r.params) > real_rank)
        .expect("a higher-ranked placeholder");
    let mut receiver = real.clone();
    receiver.configuration = placeholder;
    let mut merged = receiver.clone();
    merged.merge(&receiver, &r.params, &real).unwrap();
    assert_eq!(merged.configuration, real.configuration);
}

/// A configuration the room owner did not sign (an imported room's
/// placeholder) is never offered: merging a state that holds one into a
/// state holding an owner-signed configuration, even one that fails the
/// value checks, succeeds and keeps the owner's, in both directions
/// (freenet/river#703 review U5).
#[test]
fn an_unsigned_configuration_is_never_offered() {
    let r = room();
    let placeholder = ChatRoomStateV1::default();
    for version in 0..3u32 {
        let mut c = public_config(&r);
        c.configuration_version = version;
        c.max_members = 0;
        let invalid = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(c, &r.owner_sk),
            ..Default::default()
        };
        let mut a = invalid.clone();
        a.merge(&invalid, &r.params, &placeholder)
            .expect("merge(owner-signed, placeholder)");
        let mut b = placeholder.clone();
        b.merge(&placeholder, &r.params, &invalid)
            .expect("merge(placeholder, owner-signed)");
        assert_eq!(a.configuration, invalid.configuration, "version {version}");
        assert_eq!(b.configuration, invalid.configuration, "version {version}");
    }
}

/// A client's local placeholder configuration (not the owner's) is replaced
/// by the owner's configuration whatever their ranks, by the whole-state
/// merge and by the delta a client applies for an incoming state
/// (`merge_through_summary`) alike; two placeholders never replace each
/// other, and merging them does not fail.
#[test]
fn a_placeholder_configuration_is_replaced_only_by_the_owners() {
    let r = room();
    let placeholder = |version: u32, key: u8| {
        let mut c = public_config(&r);
        c.configuration_version = version;
        AuthorizedConfigurationV1::new(c, &SigningKey::from_bytes(&[key; 32]))
    };
    let local = ChatRoomStateV1 {
        configuration: placeholder(9, 0x71),
        ..Default::default()
    };
    let mut c = public_config(&r);
    c.configuration_version = 1;
    let owners = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(c, &r.owner_sk),
        ..Default::default()
    };
    let mut merged = local.clone();
    merged.merge(&local, &r.params, &owners).expect("merge");
    assert_eq!(merged.configuration, owners.configuration);
    let mut applied = local.clone();
    applied
        .merge_through_summary(&r.params, &owners)
        .expect("merges");
    assert_eq!(applied.configuration, owners.configuration);

    let other = ChatRoomStateV1 {
        configuration: placeholder(10, 0x72),
        ..Default::default()
    };
    let mut kept = local.clone();
    kept.merge(&local, &r.params, &other)
        .expect("two placeholders merge");
    assert_eq!(kept.configuration, local.configuration);
}
