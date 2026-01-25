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
    let mut room_state = ChatRoomStateV1::default();
    room_state.configuration = AuthorizedConfigurationV1::new(
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
    );

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
        member_vk: member_vk.clone(),
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
    let mut room_state = ChatRoomStateV1::default();
    room_state.configuration = AuthorizedConfigurationV1::new(
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
    );

    // Add member
    room_state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member_vk.clone(),
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
    let mut room_state = ChatRoomStateV1::default();
    room_state.configuration = AuthorizedConfigurationV1::new(
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
    );

    // Add both members
    room_state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member1_vk.clone(),
        },
        &owner_sk,
    ));
    room_state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member2_vk.clone(),
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
    let mut room_state = ChatRoomStateV1::default();
    room_state.configuration = AuthorizedConfigurationV1::new(
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
    );

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
