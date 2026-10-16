//! `verify` (what a stored or PUT state may hold) and `apply_delta` (what a
//! merge admits) must agree on every validly signed record: a record that
//! `verify` accepts but `apply_delta` refuses makes every merge with a peer
//! holding it fail, and nothing but its author could ever repair it
//! (freenet/river#703 review). Such records are now handled the same way on
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
fn a_nickname_over_the_room_limit_is_dropped_in_both_directions() {
    let r = room();
    let base = state(&r, public_config(&r));
    let mut holder = base.clone();
    // A message keeps the member from being pruned for inactivity.
    holder.recent_messages.messages = vec![msg(&r, 1, RoomMessageBody::public("hi".into()))];
    holder.member_info = MemberInfoV1 {
        member_info: vec![AuthorizedMemberInfo::new_with_member_key(
            MemberInfo::new_public(r.member.member.id(), 1, "a nickname far over ten".into()),
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
// Normalization of held state (freenet/river#703 review B1): a state `verify`
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
// Secrets normalization (freenet/river#703 review B2)
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
/// (freenet/river#703 review B3).
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
// Nickname limit (freenet/river#703 review B4)
// ---------------------------------------------------------------------------

/// Lowering `max_nickname_size` must not revoke a moderator's deputy grants:
/// a record that lists deputies (or the owner's) is kept whatever the limit;
/// a plain record over the limit is dropped.
#[test]
fn lowering_the_nickname_limit_keeps_records_that_carry_deputies() {
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
    assert!(after.member_info.canonical(e.member.id()).is_none());
}

/// A peer holding a newer record over the limit and one holding an older
/// record within it end on the same record in both merge directions.
#[test]
fn an_over_limit_record_and_an_older_valid_one_converge() {
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
    long.member_info = info(5, "far over ten bytes");
    let mut short = base.clone();
    short.member_info = info(1, "ok");
    let merged = assert_merges_agree(&r, &long, &short);
    assert_eq!(merged.member_info, short.member_info);
}
