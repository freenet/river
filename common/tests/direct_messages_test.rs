//! Integration tests for in-room direct messages (#230 Phase 1).
//!
//! Mirrors the wire-format and merge-semantics tests that protected the
//! reverted inbox-contract (#234), retargeted to room state.

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use rand::rngs::OsRng;
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{
    build_direct_message_signed_bytes, build_recipient_purges_signed_bytes, check_dm_future_skew,
    sign_direct_message, sign_recipient_purges, AuthorizedDirectMessage, AuthorizedRecipientPurges,
    DirectMessagesDelta, DirectMessagesV1, RecipientPurges, MAX_DM_CIPHERTEXT_BYTES,
    MAX_DM_FUTURE_SKEW_SECS, MAX_DM_MESSAGES_PER_PAIR, MAX_PURGED_TOMBSTONES_PER_RECIPIENT,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Fixture builder: owner + 3 members (Alice, Bob, Carol) in a room.
// ---------------------------------------------------------------------------

struct Fixture {
    params: ChatRoomParametersV1,
    owner_sk: SigningKey,
    owner_id: MemberId,
    alice_sk: SigningKey,
    alice_id: MemberId,
    bob_sk: SigningKey,
    bob_id: MemberId,
    #[allow(dead_code)]
    carol_sk: SigningKey,
    #[allow(dead_code)]
    carol_id: MemberId,
    state: ChatRoomStateV1,
}

fn make_fixture() -> Fixture {
    let owner_sk = SigningKey::generate(&mut OsRng);
    let owner_vk = owner_sk.verifying_key();
    let owner_id = MemberId::from(&owner_vk);

    let alice_sk = SigningKey::generate(&mut OsRng);
    let alice_vk = alice_sk.verifying_key();
    let alice_id = MemberId::from(&alice_vk);

    let bob_sk = SigningKey::generate(&mut OsRng);
    let bob_vk = bob_sk.verifying_key();
    let bob_id = MemberId::from(&bob_vk);

    let carol_sk = SigningKey::generate(&mut OsRng);
    let carol_vk = carol_sk.verifying_key();
    let carol_id = MemberId::from(&carol_vk);

    let auth_alice = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: alice_vk,
        },
        &owner_sk,
    );
    let auth_bob = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: bob_vk,
        },
        &owner_sk,
    );
    let auth_carol = AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: carol_vk,
        },
        &owner_sk,
    );

    let config = Configuration {
        max_members: 10,
        max_recent_messages: 100,
        max_user_bans: 10,
        ..Default::default()
    };
    let auth_config = AuthorizedConfigurationV1::new(config, &owner_sk);

    let state = ChatRoomStateV1 {
        configuration: auth_config,
        members: MembersV1 {
            members: vec![auth_alice, auth_bob, auth_carol],
        },
        ..Default::default()
    };

    let params = ChatRoomParametersV1 { owner: owner_vk };

    Fixture {
        params,
        owner_sk,
        owner_id,
        alice_sk,
        alice_id,
        bob_sk,
        bob_id,
        carol_sk,
        carol_id,
        state,
    }
}

fn dm_at(
    f: &Fixture,
    sk: &SigningKey,
    sender: MemberId,
    recipient: MemberId,
    timestamp: u64,
    ct: &[u8],
) -> AuthorizedDirectMessage {
    sign_direct_message(
        sk,
        sender,
        recipient,
        &f.params.owner,
        timestamp,
        ct.to_vec(),
    )
}

// ---------------------------------------------------------------------------
// Round-trip
// ---------------------------------------------------------------------------

#[test]
fn round_trip_send_state_contains_and_serializes() {
    let f = make_fixture();
    let mut dms = DirectMessagesV1::default();
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1_000, b"hello bob");
    dms.messages.push(msg.clone());

    // verify under parent state succeeds
    let mut state = f.state.clone();
    state.direct_messages = dms.clone();
    assert!(
        state.verify(&state, &f.params).is_ok(),
        "verify failed: {:?}",
        state.verify(&state, &f.params)
    );

    // ciborium round-trip preserves equality
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&dms, &mut buf).unwrap();
    let decoded: DirectMessagesV1 = ciborium::de::from_reader(buf.as_slice()).unwrap();
    assert_eq!(decoded, dms);
}

// ---------------------------------------------------------------------------
// Signature failure
// ---------------------------------------------------------------------------

#[test]
fn sender_signature_failure_rejected() {
    let f = make_fixture();
    let mut bad = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1_000, b"hi");
    // Replace with a signature from the WRONG key.
    let bytes = build_direct_message_signed_bytes(
        f.alice_id,
        f.bob_id,
        &f.params.owner,
        bad.message.timestamp,
        &bad.message.ciphertext,
    );
    bad.sender_signature = f.bob_sk.sign(&bytes);

    let mut state = f.state.clone();
    state.direct_messages.messages.push(bad);

    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(err.contains("Invalid DM sender signature"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Sender / recipient membership + ban checks
// ---------------------------------------------------------------------------

#[test]
fn sender_not_member_rejected() {
    let f = make_fixture();
    let stranger_sk = SigningKey::generate(&mut OsRng);
    let stranger_id = MemberId::from(&stranger_sk.verifying_key());

    let msg = dm_at(&f, &stranger_sk, stranger_id, f.bob_id, 1, b"hi");
    let mut state = f.state.clone();
    state.direct_messages.messages.push(msg);

    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(
        err.contains("sender") && err.contains("not a current member"),
        "got: {err}"
    );
}

#[test]
fn recipient_not_member_rejected() {
    let f = make_fixture();
    let stranger_sk = SigningKey::generate(&mut OsRng);
    let stranger_id = MemberId::from(&stranger_sk.verifying_key());

    let msg = dm_at(&f, &f.alice_sk, f.alice_id, stranger_id, 1, b"hi");
    let mut state = f.state.clone();
    state.direct_messages.messages.push(msg);

    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(
        err.contains("recipient") && err.contains("not a current member"),
        "got: {err}"
    );
}

#[test]
fn sender_banned_rejected() {
    let f = make_fixture();
    let ban = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: f.owner_id,
            banned_at: SystemTime::now(),
            banned_user: f.alice_id,
        },
        f.owner_id,
        &f.owner_sk,
    );

    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hi");
    let mut state = f.state.clone();
    state.bans.0.push(ban);
    state.direct_messages.messages.push(msg);

    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(
        err.contains("sender") && err.contains("banned"),
        "got: {err}"
    );
}

#[test]
fn recipient_banned_rejected() {
    let f = make_fixture();
    let ban = AuthorizedUserBan::new(
        UserBan {
            owner_member_id: f.owner_id,
            banned_at: SystemTime::now(),
            banned_user: f.bob_id,
        },
        f.owner_id,
        &f.owner_sk,
    );

    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hi");
    let mut state = f.state.clone();
    state.bans.0.push(ban);
    state.direct_messages.messages.push(msg);

    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(
        err.contains("recipient") && err.contains("banned"),
        "got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Tombstone blocks re-add via merge
// ---------------------------------------------------------------------------

#[test]
fn tombstone_blocks_remerge_of_purged_message() {
    let f = make_fixture();
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hi bob");
    let purge_token = msg.purge_token();

    // Bob signs a purge envelope listing this message.
    let purges = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: vec![purge_token],
        },
    );

    // Bob's local state has the purge but does NOT have the message.
    let mut bob_state = f.state.clone();
    bob_state
        .direct_messages
        .purges
        .insert(f.bob_id, purges.clone());

    assert!(
        bob_state.verify(&bob_state, &f.params).is_ok(),
        "bob's purged state should verify"
    );

    // Alice's peer (stale view) sends Bob a delta with the message back.
    let delta = DirectMessagesDelta {
        new_messages: vec![msg.clone()],
        advanced_purges: vec![],
    };
    bob_state
        .direct_messages
        .apply_delta(&bob_state.clone(), &f.params, &Some(delta))
        .expect("apply_delta should succeed");

    // The message must have been silently dropped.
    assert!(
        bob_state.direct_messages.messages.is_empty(),
        "tombstoned message must not be re-installed via merge; got {:?}",
        bob_state.direct_messages.messages
    );
}

// ---------------------------------------------------------------------------
// Purge envelope: signature + signer identity + monotonic version
// ---------------------------------------------------------------------------

#[test]
fn recipient_purges_signature_failure_rejected() {
    let f = make_fixture();
    let purges = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: vec![1, 2, 3],
        },
    );

    // Tamper with the signature: re-sign with the wrong key.
    let mut tampered = purges.clone();
    let bytes = build_recipient_purges_signed_bytes(f.bob_id, &f.params.owner, &tampered.state);
    tampered.recipient_signature = f.alice_sk.sign(&bytes);

    let mut state = f.state.clone();
    state.direct_messages.purges.insert(f.bob_id, tampered);

    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(
        err.contains("Invalid recipient purges signature"),
        "got: {err}"
    );
}

#[test]
fn non_recipient_signing_purges_rejected() {
    let f = make_fixture();
    // Alice signs a "purges" envelope claiming to be from Bob.
    // We construct this by manually building the signed bytes for
    // recipient_id = Bob but signing with Alice's key.
    let state_purges = RecipientPurges {
        version: 1,
        purged: vec![42],
    };
    let bytes = build_recipient_purges_signed_bytes(f.bob_id, &f.params.owner, &state_purges);
    let alice_sig = f.alice_sk.sign(&bytes);
    let bogus = AuthorizedRecipientPurges {
        recipient_id: f.bob_id,
        state: state_purges,
        recipient_signature: alice_sig,
    };

    let mut state = f.state.clone();
    state.direct_messages.purges.insert(f.bob_id, bogus);

    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(
        err.contains("Invalid recipient purges signature"),
        "got: {err}"
    );
}

#[test]
fn key_recipient_mismatch_rejected() {
    let f = make_fixture();
    let purges = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: vec![],
        },
    );

    // Install Bob's signed envelope under Alice's key — verify must reject.
    let mut state = f.state.clone();
    state.direct_messages.purges.insert(f.alice_id, purges);

    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(
        err.contains("does not match signed recipient_id"),
        "got: {err}"
    );
}

#[test]
fn purge_envelope_monotonic_version_apply_delta() {
    let f = make_fixture();
    let mut state = f.state.clone();

    let v2 = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 2,
            purged: vec![10],
        },
    );
    state.direct_messages.purges.insert(f.bob_id, v2.clone());

    // Attempt to "go backwards" to v1 — must NOT replace v2.
    let v1 = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: vec![10, 20],
        },
    );
    let delta = DirectMessagesDelta {
        new_messages: vec![],
        advanced_purges: vec![v1],
    };
    state
        .direct_messages
        .apply_delta(&state.clone(), &f.params, &Some(delta))
        .expect("apply_delta succeeds (older version silently ignored)");

    // v2 is still installed.
    assert_eq!(state.direct_messages.purges.get(&f.bob_id).unwrap(), &v2);
}

#[test]
fn purge_envelope_same_version_different_content_rejected() {
    let f = make_fixture();
    let mut state = f.state.clone();

    let env_a = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 2,
            purged: vec![10],
        },
    );
    state.direct_messages.purges.insert(f.bob_id, env_a);

    let env_b = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 2,
            purged: vec![20],
        },
    );
    let delta = DirectMessagesDelta {
        new_messages: vec![],
        advanced_purges: vec![env_b],
    };
    let err = state
        .direct_messages
        .apply_delta(&state.clone(), &f.params, &Some(delta))
        .expect_err("apply_delta must reject conflicting envelopes at same version");
    assert!(err.contains("conflicting envelopes"), "got: {err}");
}

#[test]
fn purge_envelope_version_zero_rejected_in_apply_delta() {
    let f = make_fixture();
    let bogus = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 0,
            purged: vec![],
        },
    );
    let mut state = f.state.clone();
    let delta = DirectMessagesDelta {
        new_messages: vec![],
        advanced_purges: vec![bogus],
    };
    let err = state
        .direct_messages
        .apply_delta(&state.clone(), &f.params, &Some(delta))
        .expect_err("apply_delta must reject version 0");
    assert!(err.contains("version 0 is reserved"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Caps
// ---------------------------------------------------------------------------

#[test]
fn per_pair_count_cap_enforced_in_verify() {
    let f = make_fixture();
    let mut state = f.state.clone();
    for i in 0..(MAX_DM_MESSAGES_PER_PAIR as u64 + 1) {
        let msg = dm_at(
            &f,
            &f.alice_sk,
            f.alice_id,
            f.bob_id,
            1_000 + i,
            format!("msg {i}").as_bytes(),
        );
        state.direct_messages.messages.push(msg);
    }
    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(err.contains("exceeds cap"), "got: {err}");
}

#[test]
fn ciphertext_size_cap_enforced() {
    let f = make_fixture();
    let too_big = vec![0u8; MAX_DM_CIPHERTEXT_BYTES + 1];
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, &too_big);
    let mut state = f.state.clone();
    state.direct_messages.messages.push(msg);
    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(err.contains("ciphertext too large"), "got: {err}");
}

#[test]
fn purge_list_cap_enforced() {
    let f = make_fixture();
    let huge: Vec<u32> = (0..(MAX_PURGED_TOMBSTONES_PER_RECIPIENT as u32 + 1)).collect();
    let purges = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: huge,
        },
    );
    let mut state = f.state.clone();
    state.direct_messages.purges.insert(f.bob_id, purges);
    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(err.contains("exceed cap"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Future-skew helper
// ---------------------------------------------------------------------------

#[test]
fn future_skew_far_future_rejected() {
    let now = 1_700_000_000u64;
    assert!(check_dm_future_skew(now, now).is_ok());
    assert!(check_dm_future_skew(now + MAX_DM_FUTURE_SKEW_SECS, now).is_ok());
    let err = check_dm_future_skew(now + MAX_DM_FUTURE_SKEW_SECS + 1, now)
        .expect_err("far-future must be rejected");
    assert!(err.contains("ahead of now"), "got: {err}");
    // Past timestamps are always accepted (no past-skew bound).
    assert!(check_dm_future_skew(0, now).is_ok());
}

// ---------------------------------------------------------------------------
// Deterministic wire format (locked hex)
// ---------------------------------------------------------------------------

/// Build a `DirectMessagesV1` from deterministic inputs and check the
/// hex of its ciborium encoding. Any change to the wire format will
/// flip this value and require deliberate review.
#[test]
fn direct_messages_wire_format_locked() {
    // Deterministic, fixed-seed signing keys so the captured hex is stable.
    let sender_sk = SigningKey::from_bytes(&[7u8; 32]);
    let recipient_sk = SigningKey::from_bytes(&[11u8; 32]);
    let room_owner_vk =
        VerifyingKey::from_bytes(&[42u8; 32]).unwrap_or_else(|_| sender_sk.verifying_key());

    let sender_id = MemberId::from(&sender_sk.verifying_key());
    let recipient_id = MemberId::from(&recipient_sk.verifying_key());

    let msg = sign_direct_message(
        &sender_sk,
        sender_id,
        recipient_id,
        &room_owner_vk,
        1_700_000_000,
        b"deterministic ciphertext".to_vec(),
    );

    let purges = sign_recipient_purges(
        &recipient_sk,
        recipient_id,
        &room_owner_vk,
        RecipientPurges {
            version: 5,
            purged: vec![0xDEADBEEF, 0xCAFEF00D],
        },
    );

    let mut dms = DirectMessagesV1::default();
    dms.messages.push(msg);
    dms.purges.insert(recipient_id, purges);

    let mut buf = Vec::new();
    ciborium::ser::into_writer(&dms, &mut buf).unwrap();

    // Round-trip first — guarantees the value is stably decodable.
    let decoded: DirectMessagesV1 = ciborium::de::from_reader(buf.as_slice()).unwrap();
    assert_eq!(decoded, dms);

    // Hex of the encoding. Update this only with deliberate review.
    let hex_actual = data_encoding::HEXLOWER.encode(&buf);

    // The expected hex was captured on first run. If this value drifts
    // we have a wire-format change — review carefully before updating.
    let expected_hex_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("direct_messages_wire_format.hex");

    // Allow the captured-on-first-run flow: if the env override is set,
    // write the actual hex out and pass. Otherwise compare against the
    // committed value.
    if std::env::var("RIVER_DM_WIRE_CAPTURE").is_ok() {
        std::fs::write(&expected_hex_path, &hex_actual).unwrap();
        eprintln!("captured wire format to {}", expected_hex_path.display());
    }

    let expected_hex = std::fs::read_to_string(&expected_hex_path).unwrap_or_else(|e| {
        panic!(
            "missing wire-format snapshot at {}: {e}. \
             Run `RIVER_DM_WIRE_CAPTURE=1 cargo test --test direct_messages_test \
             direct_messages_wire_format_locked` once to capture, then commit \
             the file.",
            expected_hex_path.display()
        )
    });
    let expected_hex = expected_hex.trim();
    assert_eq!(
        expected_hex, hex_actual,
        "wire format drifted!\nold: {expected_hex}\nnew: {hex_actual}"
    );
}

// ---------------------------------------------------------------------------
// Owner can DM members + members can DM owner
// ---------------------------------------------------------------------------

#[test]
fn owner_can_send_dm_to_member() {
    let f = make_fixture();
    let msg = dm_at(&f, &f.owner_sk, f.owner_id, f.bob_id, 1, b"from owner");
    let mut state = f.state.clone();
    state.direct_messages.messages.push(msg);
    assert!(state.verify(&state, &f.params).is_ok());
}

#[test]
fn member_can_send_dm_to_owner() {
    let f = make_fixture();
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.owner_id, 1, b"to owner");
    let mut state = f.state.clone();
    state.direct_messages.messages.push(msg);
    assert!(state.verify(&state, &f.params).is_ok());
}
