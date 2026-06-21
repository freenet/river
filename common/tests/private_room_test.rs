use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use rand::rngs::OsRng;
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
use river_core::room_state::privacy::{
    PrivacyMode, RoomCipherSpec, RoomDisplayMetadata, SealedBytes,
};
use river_core::room_state::secret::{
    AuthorizedEncryptedSecretForMember, AuthorizedSecretVersionRecord, EncryptedSecretForMemberV1,
    RoomSecretsV1, SecretVersionRecordV1, SecretsDelta,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::time::SystemTime;

/// Helper function to generate a random 32-byte secret
fn generate_room_secret() -> [u8; 32] {
    use rand::RngCore;
    let mut secret = [0u8; 32];
    OsRng.fill_bytes(&mut secret);
    secret
}

/// Helper to encrypt a secret for a member using ECIES
/// Returns (ciphertext, nonce, ephemeral_public_key)
fn encrypt_secret_for_member(
    secret: &[u8; 32],
    member_vk: &ed25519_dalek::VerifyingKey,
) -> (Vec<u8>, [u8; 12], [u8; 32]) {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use rand::RngCore;
    use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey};

    // Convert Ed25519 public key to X25519
    let member_x25519_bytes = member_vk.to_montgomery().to_bytes();
    let member_x25519 = X25519PublicKey::from(member_x25519_bytes);

    // Generate ephemeral key pair
    let ephemeral_secret = EphemeralSecret::random_from_rng(OsRng);
    let ephemeral_public = X25519PublicKey::from(&ephemeral_secret);

    // Perform ECDH
    let shared_secret = ephemeral_secret.diffie_hellman(&member_x25519);

    // Derive encryption key from shared secret
    let cipher = Aes256Gcm::new_from_slice(shared_secret.as_bytes()).unwrap();

    // Generate random nonce
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt the secret
    let ciphertext = cipher.encrypt(nonce, secret.as_ref()).unwrap();

    (ciphertext, nonce_bytes, ephemeral_public.to_bytes())
}

#[test]
fn test_private_room_creation_and_encryption() {
    // Create owner signing key
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    // Generate room secret
    let room_secret = generate_room_secret();

    // Create encrypted secret for owner
    let (ciphertext, nonce, ephemeral_key) = encrypt_secret_for_member(&room_secret, &owner_vk);

    let encrypted_secret = EncryptedSecretForMemberV1 {
        member_id: owner_id,
        secret_version: 0,
        ciphertext,
        nonce,
        sender_ephemeral_public_key: ephemeral_key,
        provider: owner_id,
    };

    let auth_encrypted_secret =
        AuthorizedEncryptedSecretForMember::new(encrypted_secret, &owner_sk);

    // Create secret version record
    let secret_version = SecretVersionRecordV1 {
        version: 0,
        cipher_spec: RoomCipherSpec::Aes256Gcm,
        created_at: SystemTime::now(),
    };

    let auth_secret_version = AuthorizedSecretVersionRecord::new(secret_version, &owner_sk);

    // Create room secrets
    let secrets = RoomSecretsV1 {
        current_version: 0,
        versions: vec![auth_secret_version],
        encrypted_secrets: vec![auth_encrypted_secret],
    };

    // Create private room configuration
    let config = Configuration {
        privacy_mode: PrivacyMode::Private,
        display: RoomDisplayMetadata {
            name: SealedBytes::public("Test Private Room".to_string().into_bytes()),
            description: None,
        },
        owner_member_id: owner_id,
        ..Default::default()
    };

    let room_state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(config, &owner_sk),
        secrets,
        ..Default::default()
    };

    let parameters = ChatRoomParametersV1 { owner: owner_vk };

    // Verify the room state
    room_state
        .verify(&room_state, &parameters)
        .expect("Room state should verify");

    // Verify it's a private room
    assert_eq!(
        room_state.configuration.configuration.privacy_mode,
        PrivacyMode::Private
    );

    // Verify secrets are present
    assert_eq!(room_state.secrets.current_version, 0);
    assert_eq!(room_state.secrets.versions.len(), 1);
    assert_eq!(room_state.secrets.encrypted_secrets.len(), 1);
}

#[test]
fn test_private_room_member_addition_with_secrets() {
    // Create owner
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    // Create initial private room
    let mut room_state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                privacy_mode: PrivacyMode::Private,
                display: RoomDisplayMetadata {
                    name: SealedBytes::public("Private Room".to_string().into_bytes()),
                    description: None,
                },
                owner_member_id: owner_id,
                ..Default::default()
            },
            &owner_sk,
        ),
        ..Default::default()
    };

    // Add initial secret
    let room_secret = generate_room_secret();
    let (ciphertext, nonce, ephemeral_key) = encrypt_secret_for_member(&room_secret, &owner_vk);

    let secret_version = SecretVersionRecordV1 {
        version: 0,
        cipher_spec: RoomCipherSpec::Aes256Gcm,
        created_at: SystemTime::now(),
    };

    room_state.secrets = RoomSecretsV1 {
        current_version: 0,
        versions: vec![AuthorizedSecretVersionRecord::new(
            secret_version,
            &owner_sk,
        )],
        encrypted_secrets: vec![AuthorizedEncryptedSecretForMember::new(
            EncryptedSecretForMemberV1 {
                member_id: owner_id,
                secret_version: 0,
                ciphertext,
                nonce,
                sender_ephemeral_public_key: ephemeral_key,
                provider: owner_id,
            },
            &owner_sk,
        )],
    };

    // Add a new member
    let member_sk = SigningKey::generate(&mut OsRng);
    let member_vk = member_sk.verifying_key();
    let member_id = MemberId::from(&member_vk);

    let member = Member {
        owner_member_id: owner_id,
        invited_by: owner_id,
        member_vk,
    };

    let auth_member = AuthorizedMember::new(member, &owner_sk);
    room_state.members.members.push(auth_member);

    // Generate encrypted secret for new member
    let (ciphertext, nonce, ephemeral_key) = encrypt_secret_for_member(&room_secret, &member_vk);

    let encrypted_secret_for_member = EncryptedSecretForMemberV1 {
        member_id,
        secret_version: 0,
        ciphertext,
        nonce,
        sender_ephemeral_public_key: ephemeral_key,
        provider: owner_id,
    };

    room_state
        .secrets
        .encrypted_secrets
        .push(AuthorizedEncryptedSecretForMember::new(
            encrypted_secret_for_member,
            &owner_sk,
        ));

    let parameters = ChatRoomParametersV1 { owner: owner_vk };

    // Verify the room state
    room_state
        .verify(&room_state, &parameters)
        .expect("Operation should succeed");

    // Verify both members have encrypted secrets
    assert_eq!(room_state.secrets.encrypted_secrets.len(), 2);
    assert!(room_state
        .secrets
        .encrypted_secrets
        .iter()
        .any(|s| s.secret.member_id == owner_id));
    assert!(room_state
        .secrets
        .encrypted_secrets
        .iter()
        .any(|s| s.secret.member_id == member_id));
}

#[test]
fn test_secret_rotation() {
    // Create owner
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    // Create member
    let member_sk = SigningKey::generate(&mut OsRng);
    let member_vk = member_sk.verifying_key();
    let member_id = MemberId::from(&member_vk);

    // Create initial private room with both members
    let mut room_state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                privacy_mode: PrivacyMode::Private,
                display: RoomDisplayMetadata {
                    name: SealedBytes::public("Private Room".to_string().into_bytes()),
                    description: None,
                },
                owner_member_id: owner_id,
                ..Default::default()
            },
            &owner_sk,
        ),
        ..Default::default()
    };

    // Add member
    room_state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk,
        },
        &owner_sk,
    ));

    // Initial secret (version 0)
    let secret_v0 = generate_room_secret();
    let (ct1, n1, ek1) = encrypt_secret_for_member(&secret_v0, &owner_vk);
    let (ct2, n2, ek2) = encrypt_secret_for_member(&secret_v0, &member_vk);

    room_state.secrets = RoomSecretsV1 {
        current_version: 0,
        versions: vec![AuthorizedSecretVersionRecord::new(
            SecretVersionRecordV1 {
                version: 0,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at: SystemTime::now(),
            },
            &owner_sk,
        )],
        encrypted_secrets: vec![
            AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id: owner_id,
                    secret_version: 0,
                    ciphertext: ct1,
                    nonce: n1,
                    sender_ephemeral_public_key: ek1,
                    provider: owner_id,
                },
                &owner_sk,
            ),
            AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id,
                    secret_version: 0,
                    ciphertext: ct2,
                    nonce: n2,
                    sender_ephemeral_public_key: ek2,
                    provider: owner_id,
                },
                &owner_sk,
            ),
        ],
    };

    // Rotate to version 1
    let secret_v1 = generate_room_secret();
    let (ct1_v1, n1_v1, ek1_v1) = encrypt_secret_for_member(&secret_v1, &owner_vk);
    let (ct2_v1, n2_v1, ek2_v1) = encrypt_secret_for_member(&secret_v1, &member_vk);

    let rotation_delta = SecretsDelta {
        current_version: Some(1),
        new_versions: vec![AuthorizedSecretVersionRecord::new(
            SecretVersionRecordV1 {
                version: 1,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at: SystemTime::now(),
            },
            &owner_sk,
        )],
        new_encrypted_secrets: vec![
            AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id: owner_id,
                    secret_version: 1,
                    ciphertext: ct1_v1,
                    nonce: n1_v1,
                    sender_ephemeral_public_key: ek1_v1,
                    provider: owner_id,
                },
                &owner_sk,
            ),
            AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id,
                    secret_version: 1,
                    ciphertext: ct2_v1,
                    nonce: n2_v1,
                    sender_ephemeral_public_key: ek2_v1,
                    provider: owner_id,
                },
                &owner_sk,
            ),
        ],
    };

    let parameters = ChatRoomParametersV1 { owner: owner_vk };
    let current_state = room_state.clone();

    // Apply rotation delta
    room_state
        .secrets
        .apply_delta(&current_state, &parameters, &Some(rotation_delta))
        .expect("Operation should succeed");

    // Verify rotation succeeded
    assert_eq!(room_state.secrets.current_version, 1);
    assert_eq!(room_state.secrets.versions.len(), 2); // v0 and v1
    assert_eq!(room_state.secrets.encrypted_secrets.len(), 4); // 2 members × 2 versions

    // Verify both members have secrets for both versions
    assert!(room_state
        .secrets
        .encrypted_secrets
        .iter()
        .any(|s| s.secret.member_id == owner_id && s.secret.secret_version == 0));
    assert!(room_state
        .secrets
        .encrypted_secrets
        .iter()
        .any(|s| s.secret.member_id == owner_id && s.secret.secret_version == 1));
    assert!(room_state
        .secrets
        .encrypted_secrets
        .iter()
        .any(|s| s.secret.member_id == member_id && s.secret.secret_version == 0));
    assert!(room_state
        .secrets
        .encrypted_secrets
        .iter()
        .any(|s| s.secret.member_id == member_id && s.secret.secret_version == 1));
}

#[test]
fn test_ban_member_excludes_from_new_secrets() {
    // Create owner
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    // Create two members
    let member1_sk = SigningKey::generate(&mut OsRng);
    let member1_vk = member1_sk.verifying_key();
    let member1_id = MemberId::from(&member1_vk);

    let member2_sk = SigningKey::generate(&mut OsRng);
    let member2_vk = member2_sk.verifying_key();
    let member2_id = MemberId::from(&member2_vk);

    // Create initial private room
    let mut room_state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                privacy_mode: PrivacyMode::Private,
                display: RoomDisplayMetadata {
                    name: SealedBytes::public("Private Room".to_string().into_bytes()),
                    description: None,
                },
                owner_member_id: owner_id,
                ..Default::default()
            },
            &owner_sk,
        ),
        ..Default::default()
    };

    // Add both members
    room_state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member1_vk,
        },
        &owner_sk,
    ));
    room_state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member2_vk,
        },
        &owner_sk,
    ));

    // Initial secret with all three (owner + 2 members)
    let secret_v0 = generate_room_secret();
    let (ct1, n1, ek1) = encrypt_secret_for_member(&secret_v0, &owner_vk);
    let (ct2, n2, ek2) = encrypt_secret_for_member(&secret_v0, &member1_vk);
    let (ct3, n3, ek3) = encrypt_secret_for_member(&secret_v0, &member2_vk);

    room_state.secrets = RoomSecretsV1 {
        current_version: 0,
        versions: vec![AuthorizedSecretVersionRecord::new(
            SecretVersionRecordV1 {
                version: 0,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at: SystemTime::now(),
            },
            &owner_sk,
        )],
        encrypted_secrets: vec![
            AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id: owner_id,
                    secret_version: 0,
                    ciphertext: ct1,
                    nonce: n1,
                    sender_ephemeral_public_key: ek1,
                    provider: owner_id,
                },
                &owner_sk,
            ),
            AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id: member1_id,
                    secret_version: 0,
                    ciphertext: ct2,
                    nonce: n2,
                    sender_ephemeral_public_key: ek2,
                    provider: owner_id,
                },
                &owner_sk,
            ),
            AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id: member2_id,
                    secret_version: 0,
                    ciphertext: ct3,
                    nonce: n3,
                    sender_ephemeral_public_key: ek3,
                    provider: owner_id,
                },
                &owner_sk,
            ),
        ],
    };

    // Ban member1
    let ban = UserBan {
        owner_member_id: owner_id,
        banned_at: SystemTime::now(),
        banned_user: member1_id,
    };

    room_state
        .bans
        .0
        .push(AuthorizedUserBan::new(ban, owner_id, &owner_sk));

    // Rotate secret (version 1) - should only include owner and member2, NOT member1
    let secret_v1 = generate_room_secret();
    let (ct1_v1, n1_v1, ek1_v1) = encrypt_secret_for_member(&secret_v1, &owner_vk);
    let (ct3_v1, n3_v1, ek3_v1) = encrypt_secret_for_member(&secret_v1, &member2_vk);

    let rotation_delta = SecretsDelta {
        current_version: Some(1),
        new_versions: vec![AuthorizedSecretVersionRecord::new(
            SecretVersionRecordV1 {
                version: 1,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at: SystemTime::now(),
            },
            &owner_sk,
        )],
        new_encrypted_secrets: vec![
            AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id: owner_id,
                    secret_version: 1,
                    ciphertext: ct1_v1,
                    nonce: n1_v1,
                    sender_ephemeral_public_key: ek1_v1,
                    provider: owner_id,
                },
                &owner_sk,
            ),
            // NOTE: member1 is NOT included here
            AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id: member2_id,
                    secret_version: 1,
                    ciphertext: ct3_v1,
                    nonce: n3_v1,
                    sender_ephemeral_public_key: ek3_v1,
                    provider: owner_id,
                },
                &owner_sk,
            ),
        ],
    };

    let parameters = ChatRoomParametersV1 { owner: owner_vk };
    let current_state = room_state.clone();

    // Apply rotation delta
    room_state
        .secrets
        .apply_delta(&current_state, &parameters, &Some(rotation_delta))
        .expect("Operation should succeed");

    // Verify rotation succeeded
    assert_eq!(room_state.secrets.current_version, 1);

    // Verify member1 does NOT have a secret for version 1 (forward secrecy)
    assert!(!room_state
        .secrets
        .encrypted_secrets
        .iter()
        .any(|s| s.secret.member_id == member1_id && s.secret.secret_version == 1));

    // Verify owner and member2 DO have secrets for version 1
    assert!(room_state
        .secrets
        .encrypted_secrets
        .iter()
        .any(|s| s.secret.member_id == owner_id && s.secret.secret_version == 1));
    assert!(room_state
        .secrets
        .encrypted_secrets
        .iter()
        .any(|s| s.secret.member_id == member2_id && s.secret.secret_version == 1));

    // Verify member1 still has secret for version 0 (can decrypt old messages)
    assert!(room_state
        .secrets
        .encrypted_secrets
        .iter()
        .any(|s| s.secret.member_id == member1_id && s.secret.secret_version == 0));
}

#[test]
fn test_encrypted_messages_in_private_room() {
    // Create owner
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    // Create private room
    let mut room_state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                privacy_mode: PrivacyMode::Private,
                display: RoomDisplayMetadata {
                    name: SealedBytes::public("Private Room".to_string().into_bytes()),
                    description: None,
                },
                owner_member_id: owner_id,
                ..Default::default()
            },
            &owner_sk,
        ),
        ..Default::default()
    };

    // Add encrypted message
    let message = MessageV1 {
        room_owner: owner_id,
        author: owner_id,
        content: RoomMessageBody::private_text(
            vec![1, 2, 3, 4, 5], // Mock encrypted content
            [0u8; 12],
            0,
        ),
        time: SystemTime::now(),
    };

    room_state
        .recent_messages
        .messages
        .push(AuthorizedMessageV1::new(message, &owner_sk));

    let parameters = ChatRoomParametersV1 { owner: owner_vk };

    // Verify the room state with encrypted message
    room_state
        .verify(&room_state, &parameters)
        .expect("Operation should succeed");

    // Verify message is encrypted
    assert_eq!(room_state.recent_messages.messages.len(), 1);
    match &room_state.recent_messages.messages[0].message.content {
        RoomMessageBody::Private { secret_version, .. } => {
            assert_eq!(*secret_version, 0);
        }
        RoomMessageBody::Public { .. } => {
            panic!("Expected encrypted message in private room");
        }
    }
}

/// Join event messages must be accepted in private rooms even though they are
/// public (they contain no sensitive content). Without this exemption, no one
/// can join a private room.
#[test]
fn test_join_event_accepted_in_private_room() {
    use river_core::room_state::member::MembersDelta;
    use river_core::room_state::ChatRoomStateV1Delta;

    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);
    let params = ChatRoomParametersV1 { owner: owner_vk };

    // Create a minimal private room with secrets
    let room_secret = generate_room_secret();
    let (ciphertext, nonce, ephemeral_key) = encrypt_secret_for_member(&room_secret, &owner_vk);
    let encrypted_secret = EncryptedSecretForMemberV1 {
        member_id: owner_id,
        secret_version: 0,
        ciphertext,
        nonce,
        sender_ephemeral_public_key: ephemeral_key,
        provider: owner_id,
    };
    let secrets = RoomSecretsV1 {
        current_version: 0,
        versions: vec![AuthorizedSecretVersionRecord::new(
            SecretVersionRecordV1 {
                version: 0,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at: SystemTime::now(),
            },
            &owner_sk,
        )],
        encrypted_secrets: vec![AuthorizedEncryptedSecretForMember::new(
            encrypted_secret,
            &owner_sk,
        )],
    };

    let mut room_state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                privacy_mode: PrivacyMode::Private,
                owner_member_id: owner_id,
                ..Default::default()
            },
            &owner_sk,
        ),
        secrets,
        ..Default::default()
    };

    // A new member joins with a public join event
    let joiner_sk = SigningKey::generate(&mut OsRng);
    let joiner_vk = joiner_sk.verifying_key();
    let joiner_id = MemberId::from(&joiner_vk);

    let authorized_member = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: joiner_vk,
        },
        &owner_sk,
    );

    let join_message = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: joiner_id,
            content: RoomMessageBody::join_event(),
            time: SystemTime::now(),
        },
        &joiner_sk,
    );

    let delta = ChatRoomStateV1Delta {
        recent_messages: Some(vec![join_message]),
        members: Some(MembersDelta::new(vec![authorized_member])),
        ..Default::default()
    };

    let old_state = room_state.clone();
    room_state
        .apply_delta(&old_state, &params, &Some(delta))
        .expect("Join event should be accepted in private room");

    assert!(
        room_state
            .members
            .members
            .iter()
            .any(|m| m.member.id() == joiner_id),
        "Joiner should be in members list"
    );
    assert!(
        room_state
            .recent_messages
            .messages
            .iter()
            .any(|m| m.message.content.is_event()),
        "Join event should be in messages"
    );
}

/// Regression test for the delegate-driven secret rotation pipeline added in
/// #228 PR 2: a SecretsDelta produced by the rotation pipeline (signed
/// externally via `with_signature`) must apply cleanly to the room state and
/// land in `secrets.current_version` / `secrets.versions` /
/// `secrets.encrypted_secrets` exactly as expected.
///
/// This intentionally lives in `common/` (not in `chat-delegate/`) because it
/// validates the contract-level expectations of the wire format the delegate
/// produces. If the contract's `apply_delta` ever rejects this shape, the
/// delegate will silently fail to rotate in production.
#[test]
fn delegate_driven_rotation_round_trip() {
    use ed25519_dalek::Signer;
    use river_core::key_derivation::derive_room_secret;
    use river_core::room_state::secret::{
        AuthorizedEncryptedSecretForMember, AuthorizedSecretVersionRecord,
        EncryptedSecretForMemberV1, SecretVersionRecordV1, SecretsDelta,
    };

    fn cbor_bytes<T: serde::Serialize>(v: &T) -> Vec<u8> {
        let mut b = Vec::new();
        ciborium::ser::into_writer(v, &mut b).unwrap();
        b
    }

    // Owner + 2 members, private room.
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);
    let alice_sk = SigningKey::generate(&mut OsRng);
    let bob_sk = SigningKey::generate(&mut OsRng);

    let configuration = AuthorizedConfigurationV1::new(
        Configuration {
            owner_member_id: owner_id,
            privacy_mode: PrivacyMode::Private,
            ..Configuration::default()
        },
        &owner_sk,
    );

    let mut state = ChatRoomStateV1 {
        configuration,
        ..Default::default()
    };

    let alice_vk = alice_sk.verifying_key();
    let bob_vk = bob_sk.verifying_key();
    state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: alice_vk,
        },
        &owner_sk,
    ));
    state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: bob_vk,
        },
        &owner_sk,
    ));

    let params = ChatRoomParametersV1 { owner: owner_vk };

    // Simulate the delegate's rotation pipeline: derive secret, sign records
    // externally, build SecretsDelta.
    let new_version: u32 = state.secrets.current_version + 1;
    let secret = derive_room_secret(&owner_sk.to_bytes(), &owner_vk, new_version);

    let record = SecretVersionRecordV1 {
        version: new_version,
        cipher_spec: RoomCipherSpec::Aes256Gcm,
        created_at: SystemTime::UNIX_EPOCH,
    };
    let record_sig = owner_sk.sign(&cbor_bytes(&record));
    let authorized_record = AuthorizedSecretVersionRecord::with_signature(record, record_sig);

    let mut new_encrypted_secrets = Vec::new();
    for (member_id, member_vk) in [
        (owner_id, owner_vk),
        (MemberId::from(&alice_vk), alice_vk),
        (MemberId::from(&bob_vk), bob_vk),
    ] {
        let (ciphertext, nonce, ephemeral_pub) = encrypt_secret_for_member(&secret, &member_vk);
        let s = EncryptedSecretForMemberV1 {
            member_id,
            secret_version: new_version,
            ciphertext,
            nonce,
            sender_ephemeral_public_key: ephemeral_pub,
            provider: owner_id,
        };
        let sig = owner_sk.sign(&cbor_bytes(&s));
        new_encrypted_secrets.push(AuthorizedEncryptedSecretForMember::with_signature(s, sig));
    }

    let secrets_delta = SecretsDelta {
        current_version: Some(new_version),
        new_versions: vec![authorized_record],
        new_encrypted_secrets,
    };

    // Wrap in a ChatRoomStateV1Delta and apply via the secret state directly
    // (the room contract's `apply_delta` propagates secrets through the
    // composable macro; here we exercise the per-field apply_delta to keep
    // the test focused on the secrets delta surface).
    let old_state = state.clone();
    state
        .secrets
        .apply_delta(&old_state, &params, &Some(secrets_delta))
        .expect("delegate-produced SecretsDelta must apply cleanly");

    assert_eq!(state.secrets.current_version, new_version);
    assert_eq!(state.secrets.versions.len(), 1);
    // Owner + 2 members.
    assert_eq!(state.secrets.encrypted_secrets.len(), 3);
    // Verify the version record signature.
    assert!(state.secrets.versions[0]
        .verify_signature(&owner_vk)
        .is_ok());
    // Verify each encrypted-secret signature.
    for s in &state.secrets.encrypted_secrets {
        assert!(s.verify_signature(&owner_vk).is_ok());
    }
}

/// freenet/river#318 contract-level invariant pin.
///
/// The UI nickname-save flow (`ui/.../nickname_field.rs`) is the privacy gate
/// that keeps a plaintext `SealedBytes::Public` nickname out of a private
/// room: it reads `is_private` and seals the nickname atomically with
/// `apply_delta` so a public→private reconfiguration can't slip a stale
/// plaintext delta through. That UI guard is load-bearing *because the
/// contract does NOT enforce the same rule for `member_info`*.
///
/// This test pins both halves of that assumption so a future change can't
/// silently invalidate it:
///
/// 1. `MemberInfoV1::apply_delta` ACCEPTS a `SealedBytes::Public`
///    `preferred_nickname` even when the room is `PrivacyMode::Private`
///    (there is no contract-level privacy guard for member_info — only
///    nickname length + signature + membership are checked). If someone
///    adds such a guard here, this assertion fails first and forces them
///    to treat it as the coordinated room-contract WASM migration it is
///    (the guard would move the contract key AND, because composed
///    `apply_delta` propagates a sub-delta `Err` via `?`, reject the WHOLE
///    delta from any existing room that already carries a public nickname —
///    a CRDT-divergence / backwards-compat break per AGENTS.md). The robust
///    place to enforce this without a migration is the UI, which #318 does.
///
/// 2. The analogous `Configuration` write IS rejected at the contract level
///    (`configuration.rs::apply_delta`), which is why the leak window only
///    ever affected member_info — room name / description writes fail to
///    apply locally and never reach `mark_needs_sync`. This is the existing
///    guard #318's UI fix mirrors in spirit.
#[test]
fn issue_318_member_info_apply_delta_has_no_contract_privacy_guard() {
    use river_core::room_state::member::MembersDelta;
    use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo, MemberInfoV1};
    use river_core::room_state::ChatRoomStateV1Delta;

    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);
    let params = ChatRoomParametersV1 { owner: owner_vk };

    // A private room with one non-owner member.
    let member_sk = SigningKey::generate(&mut OsRng);
    let member_vk = member_sk.verifying_key();
    let member_id = MemberId::from(&member_vk);

    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                privacy_mode: PrivacyMode::Private,
                owner_member_id: owner_id,
                ..Default::default()
            },
            &owner_sk,
        ),
        ..Default::default()
    };
    state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk,
        },
        &owner_sk,
    ));

    // Part 1: a PUBLIC (plaintext) nickname delta is accepted by the
    // member_info contract path even though the room is private. This is the
    // gap #318's UI fix closes; it is NOT closed at the contract level.
    let public_nickname = MemberInfo {
        member_id,
        version: 1,
        preferred_nickname: SealedBytes::public(b"PlaintextNick".to_vec()),
    };
    let authorized = AuthorizedMemberInfo::new_with_member_key(public_nickname, &member_sk);

    let mut member_info_state = MemberInfoV1::default();
    let apply_result =
        member_info_state.apply_delta(&state, &params, &Some(vec![authorized.clone()]));
    assert!(
        apply_result.is_ok(),
        "member_info::apply_delta currently accepts a public nickname in a \
         private room (no contract guard). If this now errors, a contract-level \
         privacy guard was added — see this test's doc comment: that is a \
         room-contract WASM migration with CRDT-divergence implications, not a \
         drop-in fix. Update legacy_room_contracts.toml and reconsider whether \
         the UI-level guard (#318) is the intended enforcement point.",
    );
    assert_eq!(
        member_info_state.member_info.len(),
        1,
        "the public nickname entry should have been stored as-is"
    );
    assert!(
        member_info_state.member_info[0]
            .member_info
            .preferred_nickname
            .is_public(),
        "stored nickname is the plaintext public variant the UI must prevent reaching here"
    );

    // Part 2: the analogous Configuration write (public display metadata into
    // a private room) IS rejected at the contract level. This is why the leak
    // only ever affected member_info, and is the guard the UI fix mirrors.
    let bad_config = Configuration {
        privacy_mode: PrivacyMode::Private,
        owner_member_id: owner_id,
        // Public (plaintext) display name in a private room — illegal.
        display: RoomDisplayMetadata {
            name: SealedBytes::public(b"Plaintext Room Name".to_vec()),
            description: None,
        },
        configuration_version: state.configuration.configuration.configuration_version + 1,
        ..Default::default()
    };
    let config_delta = ChatRoomStateV1Delta {
        configuration: Some(AuthorizedConfigurationV1::new(bad_config, &owner_sk)),
        // include an unrelated member delta to prove it's the config that's rejected
        members: Some(MembersDelta::new(vec![])),
        ..Default::default()
    };
    let mut state_for_config = state.clone();
    let old = state_for_config.clone();
    let config_result = state_for_config.apply_delta(&old, &params, &Some(config_delta));
    assert!(
        config_result.is_err(),
        "Configuration::apply_delta MUST reject public display metadata in a \
         private room (the existing contract-level guard #318's UI fix mirrors). \
         If this no longer errors, the configuration privacy guard regressed.",
    );
}

// =============================================================================
// Bug #3 regression tests (Ivvor's 2026-05-17 Matrix report)
//
// Symptom: in a freshly-created private room, the owner sends messages but
// invitees never see them (not even as ciphertext). The owner's local
// state has advanced to a higher secret version (e.g. v3 after membership
// churn), but the invitees are still at v0 — and the previous strict
// `secret_version == current_version` check in `MessagesV1::apply_delta`
// dropped the entire `ChatRoomStateV1Delta` whenever an invitee's
// secrets-state hadn't caught up yet. The message was therefore never
// stored on the invitee's contract instance, so back-fill from a peer
// later was also impossible (nothing to back-fill).
//
// PR A fixes the room-contract validation: messages at any
// owner-signed secret version are accepted, and `RoomSecretsV1::apply_delta`
// is now transactional so a failing sub-check no longer leaves the state
// half-mutated (which would otherwise silently corrupt the room and
// break CRDT convergence). PR B will follow with the UI back-fill path.
// =============================================================================

/// Bug #3 regression: a private message at `secret_version = v_new`
/// must be accepted when the invitee's local state still has
/// `current_version = v_old`, as long as a signed version record for
/// `v_new` exists in `parent_state.secrets.versions`.
///
/// Pre-fix behavior: `apply_delta` returned `Err("Private message secret
/// version 1 does not match current version 0")`, the composable macro
/// short-circuited via `?`, and the entire delta (including the message)
/// was dropped — invitees never saw the encrypted message.
#[test]
fn message_at_older_or_newer_known_secret_version_is_accepted() {
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    // Private room with TWO signed version records (v0 and v1), but
    // `current_version` is still 0. This simulates an invitee that has
    // received both version records but hasn't yet processed the
    // current_version bump for whatever reason (out-of-order delta).
    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                privacy_mode: PrivacyMode::Private,
                owner_member_id: owner_id,
                ..Default::default()
            },
            &owner_sk,
        ),
        ..Default::default()
    };

    let secret_v0 = generate_room_secret();
    let secret_v1 = generate_room_secret();
    let (ct0, n0, ek0) = encrypt_secret_for_member(&secret_v0, &owner_vk);
    let (ct1, n1, ek1) = encrypt_secret_for_member(&secret_v1, &owner_vk);

    state.secrets = RoomSecretsV1 {
        current_version: 0,
        versions: vec![
            AuthorizedSecretVersionRecord::new(
                SecretVersionRecordV1 {
                    version: 0,
                    cipher_spec: RoomCipherSpec::Aes256Gcm,
                    created_at: SystemTime::now(),
                },
                &owner_sk,
            ),
            AuthorizedSecretVersionRecord::new(
                SecretVersionRecordV1 {
                    version: 1,
                    cipher_spec: RoomCipherSpec::Aes256Gcm,
                    created_at: SystemTime::now(),
                },
                &owner_sk,
            ),
        ],
        encrypted_secrets: vec![
            AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id: owner_id,
                    secret_version: 0,
                    ciphertext: ct0,
                    nonce: n0,
                    sender_ephemeral_public_key: ek0,
                    provider: owner_id,
                },
                &owner_sk,
            ),
            AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id: owner_id,
                    secret_version: 1,
                    ciphertext: ct1,
                    nonce: n1,
                    sender_ephemeral_public_key: ek1,
                    provider: owner_id,
                },
                &owner_sk,
            ),
        ],
    };

    let params = ChatRoomParametersV1 { owner: owner_vk };

    // Owner sends a message encrypted at v1 (the "newer" version), while
    // the invitee's local `current_version` is still 0.
    let msg_at_v1 = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: SystemTime::now(),
            content: RoomMessageBody::private_text(vec![9, 9, 9, 9], [0u8; 12], 1),
        },
        &owner_sk,
    );

    // Apply via MessagesV1::apply_delta directly — exercising the relaxation.
    let result =
        state
            .recent_messages
            .apply_delta(&state.clone(), &params, &Some(vec![msg_at_v1.clone()]));
    assert!(
        result.is_ok(),
        "message at known secret_version 1 (with current_version=0) should be accepted, got: {:?}",
        result.err()
    );
    assert!(
        state
            .recent_messages
            .messages
            .iter()
            .any(|m| m.id() == msg_at_v1.id()),
        "message at v1 should be stored even though current_version is still 0"
    );

    // Conversely, a message at a version that does NOT have a signed
    // record must still be rejected (defense in depth: an attacker could
    // not otherwise be ruled out from injecting ciphertext at a fabricated
    // version).
    let msg_at_v99 = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: SystemTime::now(),
            content: RoomMessageBody::private_text(vec![7, 7, 7, 7], [0u8; 12], 99),
        },
        &owner_sk,
    );
    let result =
        state
            .recent_messages
            .apply_delta(&state.clone(), &params, &Some(vec![msg_at_v99.clone()]));
    assert!(
        result.is_err(),
        "message at unknown secret_version 99 must be rejected"
    );
    assert!(
        result.unwrap_err().contains("unknown secret version"),
        "error should mention unknown secret version"
    );
}

/// Bug #3 regression: a single member missing an encrypted blob at the
/// current secret version must not freeze the entire room for messages.
///
/// Pre-fix behavior: `has_complete_distribution` returned false, and the
/// gate at the top of `MessagesV1::apply_delta` rejected ANY private
/// message — even ones from members who DO have a blob. The room would
/// stay frozen until the missing member came online and the owner
/// re-issued their blob (or the missing member was removed and pruned).
#[test]
fn single_member_missing_blob_does_not_freeze_room() {
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    let alice_sk = SigningKey::generate(&mut OsRng);
    let alice_vk = alice_sk.verifying_key();
    let alice_id = MemberId::from(&alice_vk);

    let bob_sk = SigningKey::generate(&mut OsRng);
    let bob_vk = bob_sk.verifying_key();

    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                privacy_mode: PrivacyMode::Private,
                owner_member_id: owner_id,
                ..Default::default()
            },
            &owner_sk,
        ),
        ..Default::default()
    };

    state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: alice_vk,
        },
        &owner_sk,
    ));
    state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: bob_vk,
        },
        &owner_sk,
    ));

    let secret = generate_room_secret();
    let (ct_o, n_o, ek_o) = encrypt_secret_for_member(&secret, &owner_vk);
    let (ct_a, n_a, ek_a) = encrypt_secret_for_member(&secret, &alice_vk);

    // Owner and Alice have v1 blobs; Bob does NOT (simulates the
    // partial-distribution case Ivvor hit). current_version=1 is critical
    // here — `has_complete_distribution` short-circuits to `true` when
    // current_version == 0, so we need to test at v1+ to exercise the
    // distribution gate that was previously freezing the room.
    state.secrets = RoomSecretsV1 {
        current_version: 1,
        versions: vec![AuthorizedSecretVersionRecord::new(
            SecretVersionRecordV1 {
                version: 1,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at: SystemTime::now(),
            },
            &owner_sk,
        )],
        encrypted_secrets: vec![
            AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id: owner_id,
                    secret_version: 1,
                    ciphertext: ct_o,
                    nonce: n_o,
                    sender_ephemeral_public_key: ek_o,
                    provider: owner_id,
                },
                &owner_sk,
            ),
            AuthorizedEncryptedSecretForMember::new(
                EncryptedSecretForMemberV1 {
                    member_id: alice_id,
                    secret_version: 1,
                    ciphertext: ct_a,
                    nonce: n_a,
                    sender_ephemeral_public_key: ek_a,
                    provider: owner_id,
                },
                &owner_sk,
            ),
        ],
    };

    let params = ChatRoomParametersV1 { owner: owner_vk };

    // Alice sends a message at v1 — she has her blob, so she could
    // decrypt outbound. Bob being blob-less must not block this.
    let msg = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: alice_id,
            time: SystemTime::now(),
            content: RoomMessageBody::private_text(vec![1, 2, 3, 4], [0u8; 12], 1),
        },
        &alice_sk,
    );

    let result =
        state
            .recent_messages
            .apply_delta(&state.clone(), &params, &Some(vec![msg.clone()]));
    assert!(
        result.is_ok(),
        "message should not be blocked by a single member's missing blob, got: {:?}",
        result.err()
    );
    assert!(
        state
            .recent_messages
            .messages
            .iter()
            .any(|m| m.id() == msg.id()),
        "message must be stored despite incomplete distribution"
    );
}

/// Bug #3 regression: `RoomSecretsV1::apply_delta` must be transactional.
/// Pre-fix behavior pushed `new_versions[0]` onto `self.versions` before
/// running later checks; if any later check returned `Err`, the
/// half-mutated state survived and was used as the new baseline by the
/// composable `apply_delta`. That violated CRDT idempotence — re-applying
/// the same failing delta would now succeed (because the version was
/// already there) and could leave the state inconsistent.
///
/// Post-fix behavior: the entire delta either applies or leaves the state
/// byte-identical to its pre-call value.
#[test]
fn secrets_apply_delta_is_transactional_on_failure() {
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    let state = ChatRoomStateV1::default();
    let params = ChatRoomParametersV1 { owner: owner_vk };

    let mut secrets = RoomSecretsV1::default();

    // Build a delta where:
    //   - new_versions: [v1]                            (passes)
    //   - new_encrypted_secrets: [secret_for_owner_at_v99]  (FAILS: version 99 not in working.versions)
    //
    // Pre-fix: versions.push(v1) succeeds, then the v99 check returns
    // Err — but `self.versions` is now `[v1]`, a partial mutation.
    let v1_record = SecretVersionRecordV1 {
        version: 1,
        cipher_spec: RoomCipherSpec::Aes256Gcm,
        created_at: SystemTime::now(),
    };
    let auth_v1 = AuthorizedSecretVersionRecord::new(v1_record, &owner_sk);

    let bad_secret = EncryptedSecretForMemberV1 {
        member_id: owner_id,
        secret_version: 99, // version 99 not in `new_versions`
        ciphertext: vec![1, 2, 3],
        nonce: [0u8; 12],
        sender_ephemeral_public_key: [0u8; 32],
        provider: owner_id,
    };
    let auth_bad_secret = AuthorizedEncryptedSecretForMember::new(bad_secret, &owner_sk);

    let bad_delta = SecretsDelta {
        current_version: None,
        new_versions: vec![auth_v1],
        new_encrypted_secrets: vec![auth_bad_secret],
    };

    // Snapshot pre-call state.
    let before = secrets.clone();

    let result = secrets.apply_delta(&state, &params, &Some(bad_delta));
    assert!(result.is_err(), "bad delta must fail");
    assert!(
        result.unwrap_err().contains("non-existent version"),
        "expected the v99 check to be the one that fires"
    );

    // POST-FIX REQUIREMENT: `secrets` is unchanged. Pre-fix this would have
    // contained `versions = [v1]` (partial mutation).
    assert_eq!(
        secrets, before,
        "apply_delta must leave state byte-identical on failure (transactional)"
    );
    assert!(
        secrets.versions.is_empty(),
        "no version should be pushed when a later check fails"
    );
}

/// Composability regression: an Ivvor-shaped delta carrying BOTH a
/// rotation (`new_versions = [v_new]` + `current_version = v_new`) AND
/// a message encrypted at `v_new` must apply atomically — even from a
/// baseline where `current_version = 0` and `versions = [v0]`.
///
/// This exercises the composable-macro field ordering: secrets is
/// applied before recent_messages, so by the time the message's
/// `apply_delta` runs, `parent_state.secrets.versions` already
/// contains `v_new`. The message check (relaxed in PR A) accepts it.
#[test]
fn delta_with_rotation_plus_message_at_new_version_applies_atomically() {
    use river_core::room_state::ChatRoomStateV1Delta;

    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                privacy_mode: PrivacyMode::Private,
                owner_member_id: owner_id,
                ..Default::default()
            },
            &owner_sk,
        ),
        ..Default::default()
    };

    // Baseline: secrets at v0 only.
    let secret_v0 = generate_room_secret();
    let (ct0, n0, ek0) = encrypt_secret_for_member(&secret_v0, &owner_vk);
    state.secrets = RoomSecretsV1 {
        current_version: 0,
        versions: vec![AuthorizedSecretVersionRecord::new(
            SecretVersionRecordV1 {
                version: 0,
                cipher_spec: RoomCipherSpec::Aes256Gcm,
                created_at: SystemTime::now(),
            },
            &owner_sk,
        )],
        encrypted_secrets: vec![AuthorizedEncryptedSecretForMember::new(
            EncryptedSecretForMemberV1 {
                member_id: owner_id,
                secret_version: 0,
                ciphertext: ct0,
                nonce: n0,
                sender_ephemeral_public_key: ek0,
                provider: owner_id,
            },
            &owner_sk,
        )],
    };

    let params = ChatRoomParametersV1 { owner: owner_vk };

    // Build a combined delta: rotation to v1 + message encrypted at v1.
    let secret_v1 = generate_room_secret();
    let (ct1, n1, ek1) = encrypt_secret_for_member(&secret_v1, &owner_vk);
    let v1_record = AuthorizedSecretVersionRecord::new(
        SecretVersionRecordV1 {
            version: 1,
            cipher_spec: RoomCipherSpec::Aes256Gcm,
            created_at: SystemTime::now(),
        },
        &owner_sk,
    );
    let v1_secret_for_owner = AuthorizedEncryptedSecretForMember::new(
        EncryptedSecretForMemberV1 {
            member_id: owner_id,
            secret_version: 1,
            ciphertext: ct1,
            nonce: n1,
            sender_ephemeral_public_key: ek1,
            provider: owner_id,
        },
        &owner_sk,
    );
    let secrets_delta = SecretsDelta {
        current_version: Some(1),
        new_versions: vec![v1_record],
        new_encrypted_secrets: vec![v1_secret_for_owner],
    };

    let msg_at_v1 = AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: owner_id,
            author: owner_id,
            time: SystemTime::now(),
            content: RoomMessageBody::private_text(vec![5, 5, 5, 5], [0u8; 12], 1),
        },
        &owner_sk,
    );

    let delta = ChatRoomStateV1Delta {
        secrets: Some(secrets_delta),
        recent_messages: Some(vec![msg_at_v1.clone()]),
        ..Default::default()
    };

    let old_state = state.clone();
    state
        .apply_delta(&old_state, &params, &Some(delta))
        .expect("combined rotation+message delta should apply atomically");

    // Post-conditions: secrets rotated to v1, message stored.
    assert_eq!(state.secrets.current_version, 1);
    assert!(state.secrets.versions.iter().any(|v| v.record.version == 1));
    assert!(
        state
            .recent_messages
            .messages
            .iter()
            .any(|m| m.id() == msg_at_v1.id()),
        "message at v1 must be stored after combined rotation+message delta"
    );
}
