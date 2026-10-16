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
