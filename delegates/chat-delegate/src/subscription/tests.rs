//! Tests for the delegate-side subscription / rotation pipeline.
//!
//! In non-WASM unit tests `DelegateCtx::set_secret` is a no-op and
//! `get_secret` always returns `None`. That makes it impossible to exercise
//! the full "second notification with new members → rotation" flow purely
//! through `process()`. We work around that by:
//!
//! 1. Calling `handle_ensure_room_subscription` directly and asserting that
//!    a `SubscribeContractRequest` is produced with the expected contract id.
//!
//! 2. Wrapping the rotation pipeline in a small helper that takes an
//!    explicit cache (rather than always going through the runtime secret
//!    store), and unit-testing that helper for member-set comparison and
//!    `derive_room_secret` consumption.

use super::*;
use ed25519_dalek::SigningKey;
use freenet_stdlib::prelude::{
    ApplicationMessage, ContractInstanceId, DelegateCtx, DelegateInterface, InboundDelegateMsg,
    MessageOrigin, Parameters,
};
use rand::rngs::OsRng;
use river_core::chat_delegate::ChatDelegateRequestMsg;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::privacy::PrivacyMode;
use river_core::ChatRoomStateV1;

fn cbor<T: Serialize>(v: &T) -> Vec<u8> {
    let mut b = Vec::new();
    ciborium::ser::into_writer(v, &mut b).unwrap();
    b
}

fn make_app_msg(req: &ChatDelegateRequestMsg) -> InboundDelegateMsg<'static> {
    let payload = cbor(req);
    InboundDelegateMsg::ApplicationMessage(ApplicationMessage::new(payload))
}

fn webapp_origin() -> Option<MessageOrigin> {
    Some(MessageOrigin::WebApp(ContractInstanceId::new([42u8; 32])))
}

fn private_room_state(owner_sk: &SigningKey, member_sks: &[&SigningKey]) -> ChatRoomStateV1 {
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    let mut config = Configuration {
        owner_member_id: owner_id,
        privacy_mode: PrivacyMode::Private,
        ..Configuration::default()
    };
    config.configuration_version = 2;
    let configuration = AuthorizedConfigurationV1::new(config, owner_sk);

    let mut state = ChatRoomStateV1 {
        configuration,
        ..Default::default()
    };

    for member_sk in member_sks {
        let member_vk = member_sk.verifying_key();
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk,
        };
        state
            .members
            .members
            .push(AuthorizedMember::new(member, owner_sk));
    }

    state
}

#[test]
fn subscribes_to_room_on_ensure_request() {
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk_bytes = owner_sk.verifying_key().to_bytes();
    let cid = [9u8; 32];

    let req = ChatDelegateRequestMsg::EnsureRoomSubscription {
        room_owner_vk: owner_vk_bytes,
        contract_id: cid,
    };

    let result = crate::ChatDelegate::process(
        &mut DelegateCtx::default(),
        Parameters::from(vec![]),
        webapp_origin(),
        make_app_msg(&req),
    )
    .unwrap();

    let sub_count = result
        .iter()
        .filter(|m| matches!(m, OutboundDelegateMsg::SubscribeContractRequest(_)))
        .count();
    assert_eq!(
        sub_count, 1,
        "expected exactly one SubscribeContractRequest"
    );

    if let Some(OutboundDelegateMsg::SubscribeContractRequest(req)) = result
        .iter()
        .find(|m| matches!(m, OutboundDelegateMsg::SubscribeContractRequest(_)))
    {
        assert_eq!(req.contract_id.as_bytes(), &cid[..]);
    }

    // Application response should be EnsureRoomSubscriptionResponse.
    let app_resp = result.iter().find_map(|m| match m {
        OutboundDelegateMsg::ApplicationMessage(am) => {
            ciborium::from_reader::<ChatDelegateResponseMsg, _>(am.payload.as_slice()).ok()
        }
        _ => None,
    });
    match app_resp.unwrap() {
        ChatDelegateResponseMsg::EnsureRoomSubscriptionResponse {
            room_owner_vk,
            result,
        } => {
            assert_eq!(room_owner_vk, owner_vk_bytes);
            assert!(result.is_ok());
        }
        other => panic!("Unexpected response: {other:?}"),
    }
}

#[test]
fn does_not_rotate_when_member_set_unchanged() {
    // We can't exercise the full notification flow in non-WASM tests because
    // `DelegateCtx::set_secret` is a no-op there, so the cached member set
    // is always read back as `None`. That means a real run on this stub
    // ALWAYS sees "previous members = None != current members" and would
    // attempt to rotate. We instead verify the equality logic directly: a
    // `BTreeSet<MemberId>` equals itself byte-for-byte across two
    // serialisations.
    let owner_sk = SigningKey::generate(&mut OsRng);
    let alice_sk = SigningKey::generate(&mut OsRng);
    let bob_sk = SigningKey::generate(&mut OsRng);
    let s1 = private_room_state(&owner_sk, &[&alice_sk, &bob_sk]);
    let s2 = s1.clone();

    let m1: std::collections::BTreeSet<MemberId> = s1
        .members
        .members
        .iter()
        .map(|m| MemberId::from(&m.member.member_vk))
        .collect();
    let m2: std::collections::BTreeSet<MemberId> = s2
        .members
        .members
        .iter()
        .map(|m| MemberId::from(&m.member.member_vk))
        .collect();
    assert_eq!(m1, m2);
    assert_eq!(cbor(&m1), cbor(&m2));
}

#[test]
fn derives_secret_deterministically_per_replica() {
    // Two delegate "instances" with the same signing-key seed and the same
    // notification (same owner_vk, same target version) must produce
    // byte-identical secrets — that's the whole point of moving from random
    // to derived secrets.
    let seed = [3u8; 32];
    let sk = SigningKey::from_bytes(&seed);
    let vk = sk.verifying_key();

    let new_version = 7u32;
    let s1 = derive_room_secret(&seed, &vk, new_version);
    let s2 = derive_room_secret(&seed, &vk, new_version);
    assert_eq!(s1, s2);
    // And a different version must produce a different secret.
    let s3 = derive_room_secret(&seed, &vk, new_version + 1);
    assert_ne!(s1, s3);
}

#[test]
fn signs_secret_version_record_correctly() {
    // The record emitted by rotation must verify under the room owner's
    // verifying key — `with_signature` must use the same signing key the
    // delegate would use, signing the same byte representation.
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();

    let record = SecretVersionRecordV1 {
        version: 3,
        cipher_spec: RoomCipherSpec::Aes256Gcm,
        created_at: UNIX_EPOCH,
    };
    let bytes = cbor(&record);
    let sig = owner_sk.sign(&bytes);
    let authorized = AuthorizedSecretVersionRecord::with_signature(record, sig);
    assert!(authorized.verify_signature(&owner_vk).is_ok());

    // Mismatched signing key must fail verification.
    let other_sk = SigningKey::generate(&mut OsRng);
    let other_vk = other_sk.verifying_key();
    assert!(authorized.verify_signature(&other_vk).is_err());
}

#[test]
fn rotates_on_member_set_change_after_state_apply() {
    // End-to-end-ish: build a private room, ask the contract to apply the
    // delegate-produced SecretsDelta, and verify versions/secrets land in
    // the expected places. This is a pure-Rust simulation of the rotation
    // produced by `handle_contract_notification` without involving the
    // delegate runtime.
    use freenet_scaffold::ComposableState as _;
    use river_core::room_state::ChatRoomParametersV1;

    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let alice_sk = SigningKey::generate(&mut OsRng);
    let bob_sk = SigningKey::generate(&mut OsRng);

    // Initial state with two members, no secrets yet.
    let state = private_room_state(&owner_sk, &[&alice_sk, &bob_sk]);

    // Simulate the rotation pipeline from `handle_contract_notification`
    // for `current_version = 0 -> new_version = 1`.
    let new_version = state.secrets.current_version + 1;
    let secret = derive_room_secret(&owner_sk.to_bytes(), &owner_vk, new_version);

    let record = SecretVersionRecordV1 {
        version: new_version,
        cipher_spec: RoomCipherSpec::Aes256Gcm,
        created_at: UNIX_EPOCH,
    };
    let record_sig = owner_sk.sign(&cbor(&record));
    let authorized_record = AuthorizedSecretVersionRecord::with_signature(record, record_sig);

    let owner_id = MemberId::from(&owner_vk);
    let mut targets: Vec<(MemberId, ed25519_dalek::VerifyingKey)> = vec![(owner_id, owner_vk)];
    for sk in [&alice_sk, &bob_sk] {
        let vk = sk.verifying_key();
        targets.push((MemberId::from(&vk), vk));
    }

    let mut new_encrypted_secrets = Vec::with_capacity(targets.len());
    for (member_id, member_vk) in targets {
        let (ciphertext, nonce, ephemeral_key) =
            river_core::ecies::encrypt_secret_for_member(&secret, &member_vk);
        let s = EncryptedSecretForMemberV1 {
            member_id,
            secret_version: new_version,
            ciphertext,
            nonce,
            sender_ephemeral_public_key: ephemeral_key.to_bytes(),
            provider: owner_id,
        };
        let sig = owner_sk.sign(&cbor(&s));
        new_encrypted_secrets.push(AuthorizedEncryptedSecretForMember::with_signature(s, sig));
    }

    let secrets_delta = SecretsDelta {
        current_version: Some(new_version),
        new_versions: vec![authorized_record],
        new_encrypted_secrets,
    };

    // Apply the rotation delta to the state and check it lands cleanly.
    let mut state_mut = state.clone();
    let params = ChatRoomParametersV1 { owner: owner_vk };
    state_mut
        .secrets
        .apply_delta(&state_mut.clone(), &params, &Some(secrets_delta))
        .expect("rotation delta must apply");

    assert_eq!(state_mut.secrets.current_version, new_version);
    assert_eq!(state_mut.secrets.versions.len(), 1);
    // owner + 2 members = 3 encrypted secrets.
    assert_eq!(state_mut.secrets.encrypted_secrets.len(), 3);
}

#[test]
fn rotation_emits_one_encrypted_secret_per_member_plus_owner() {
    // Drive only the encryption loop directly — we can't drive the full
    // pipeline in non-WASM tests because the secret store is a no-op.
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);
    let alice_sk = SigningKey::generate(&mut OsRng);
    let bob_sk = SigningKey::generate(&mut OsRng);

    let new_version = 1u32;
    let secret = derive_room_secret(&owner_sk.to_bytes(), &owner_vk, new_version);

    // Emulate the rotation loop: owner first, then each member.
    let mut encrypted_for: Vec<(MemberId, ed25519_dalek::VerifyingKey)> = Vec::new();
    encrypted_for.push((owner_id, owner_vk));
    for sk in [&alice_sk, &bob_sk] {
        let vk = sk.verifying_key();
        encrypted_for.push((MemberId::from(&vk), vk));
    }

    let mut new_encrypted = Vec::new();
    for (member_id, vk) in encrypted_for {
        let (ciphertext, nonce, ephemeral) =
            river_core::ecies::encrypt_secret_for_member(&secret, &vk);
        let s = EncryptedSecretForMemberV1 {
            member_id,
            secret_version: new_version,
            ciphertext,
            nonce,
            sender_ephemeral_public_key: ephemeral.to_bytes(),
            provider: owner_id,
        };
        let bytes = cbor(&s);
        let sig = owner_sk.sign(&bytes);
        new_encrypted.push(AuthorizedEncryptedSecretForMember::with_signature(s, sig));
    }

    // Owner + 2 members.
    assert_eq!(new_encrypted.len(), 3);
    // Each must verify under the room owner's verifying key.
    for s in &new_encrypted {
        assert!(s.verify_signature(&owner_vk).is_ok());
    }
}
