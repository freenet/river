//! Integration tests for in-room direct messages (#230 Phase 1).

use ed25519_dalek::{Signer, SigningKey};
use freenet_scaffold::ComposableState;
use rand::rngs::OsRng;
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{
    build_direct_message_signed_bytes, build_recipient_purges_signed_bytes, check_dm_future_skew,
    sign_direct_message, sign_recipient_purges, AuthorizedDirectMessage, AuthorizedRecipientPurges,
    DirectMessage, DirectMessagesDelta, DirectMessagesV1, PurgeToken, RecipientPurges,
    DOMAIN_TAG_MESSAGE, DOMAIN_TAG_PURGES, MAX_DM_CIPHERTEXT_BYTES, MAX_DM_FUTURE_SKEW_SECS,
    MAX_DM_MESSAGES_PER_PAIR, MAX_PURGED_TOMBSTONES_PER_RECIPIENT,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId, MembersV1};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1};
use std::collections::HashSet;
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
    .expect("sign_direct_message")
}

fn tok(n: u8) -> PurgeToken {
    PurgeToken([n; 16])
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

    let mut state = f.state.clone();
    state.direct_messages = dms.clone();
    assert!(
        state.verify(&state, &f.params).is_ok(),
        "verify failed: {:?}",
        state.verify(&state, &f.params)
    );

    let mut buf = Vec::new();
    ciborium::ser::into_writer(&dms, &mut buf).unwrap();
    let decoded: DirectMessagesV1 = ciborium::de::from_reader(buf.as_slice()).unwrap();
    assert_eq!(decoded, dms);
}

// ---------------------------------------------------------------------------
// JSON round-trip (bug-prevention-patterns: HashMap-with-struct-key trap, #3987)
// ---------------------------------------------------------------------------

#[test]
fn json_round_trip_with_populated_purges_does_not_drop_fields() {
    let f = make_fixture();
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hi");

    let purges = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 7,
            purged: vec![tok(0xAA), tok(0xBB)],
        },
    )
    .unwrap();

    let mut dms = DirectMessagesV1::default();
    dms.messages.push(msg);
    dms.purges.push(purges);

    let json = serde_json::to_string(&dms).expect("DM state must JSON-serialize");
    let decoded: DirectMessagesV1 =
        serde_json::from_str(&json).expect("DM state must JSON-deserialize");
    assert_eq!(decoded, dms, "JSON round-trip must preserve all fields");
    assert_eq!(decoded.purges.len(), 1);
    assert_eq!(decoded.purges[0].state.purged.len(), 2);
}

#[test]
fn json_round_trip_of_full_chat_room_state_preserves_dms() {
    let f = make_fixture();
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hi bob");
    let purges = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: vec![tok(0xCC)],
        },
    )
    .unwrap();

    let mut state = f.state.clone();
    state.direct_messages.messages.push(msg);
    state.direct_messages.purges.push(purges);

    let json = serde_json::to_string(&state).expect("ChatRoomStateV1 must JSON-serialize");
    let decoded: ChatRoomStateV1 = serde_json::from_str(&json).expect("must deserialize");
    assert_eq!(
        decoded.direct_messages, state.direct_messages,
        "direct_messages must survive JSON round-trip"
    );
}

// ---------------------------------------------------------------------------
// Backwards-compat: pre-#230 state without direct_messages field
// ---------------------------------------------------------------------------

#[test]
fn serde_default_lets_pre230_state_decode() {
    // Encode a state, then mutate its CBOR-decoded value-view to drop
    // `direct_messages`, re-encode, and decode again. The decoded state
    // must populate `direct_messages` with `Default::default()` and
    // still verify.
    let f = make_fixture();
    let state = f.state.clone();

    let mut buf = Vec::new();
    ciborium::ser::into_writer(&state, &mut buf).unwrap();
    let mut value: ciborium::Value = ciborium::de::from_reader(buf.as_slice()).unwrap();

    // The encoded ChatRoomStateV1 is a CBOR map; remove the
    // `direct_messages` key if present.
    if let ciborium::Value::Map(ref mut entries) = value {
        entries.retain(|(k, _)| match k {
            ciborium::Value::Text(s) => s != "direct_messages",
            _ => true,
        });
    }

    let mut buf2 = Vec::new();
    ciborium::ser::into_writer(&value, &mut buf2).unwrap();
    let decoded: ChatRoomStateV1 = ciborium::de::from_reader(buf2.as_slice()).unwrap();

    assert_eq!(
        decoded.direct_messages,
        DirectMessagesV1::default(),
        "missing direct_messages must serde-default"
    );
    assert!(
        decoded.verify(&decoded, &f.params).is_ok(),
        "pre-#230 state must still verify"
    );
}

// ---------------------------------------------------------------------------
// Signature failure
// ---------------------------------------------------------------------------

#[test]
fn sender_signature_failure_rejected() {
    let f = make_fixture();
    let mut bad = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1_000, b"hi");
    let bytes = build_direct_message_signed_bytes(
        f.alice_id,
        f.bob_id,
        &f.params.owner,
        bad.message.timestamp,
        &bad.message.ciphertext,
    )
    .unwrap();
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
// Membership checks in verify
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
fn self_dm_rejected_in_verify() {
    let f = make_fixture();
    // sign_direct_message refuses self-DMs at construction time, so
    // build one manually to test the verify-side check.
    let timestamp = 1u64;
    let bytes = build_direct_message_signed_bytes(
        f.alice_id,
        f.alice_id,
        &f.params.owner,
        timestamp,
        b"hi",
    )
    .unwrap();
    let sig = f.alice_sk.sign(&bytes);
    let manual = AuthorizedDirectMessage {
        message: DirectMessage {
            sender: f.alice_id,
            recipient: f.alice_id,
            timestamp,
            ciphertext: b"hi".to_vec(),
        },
        sender_signature: sig,
    };
    let mut state = f.state.clone();
    state.direct_messages.messages.push(manual);
    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(err.contains("must differ"), "got: {err}");
}

#[test]
fn self_dm_rejected_at_signing_time() {
    let f = make_fixture();
    let err = sign_direct_message(
        &f.alice_sk,
        f.alice_id,
        f.alice_id,
        &f.params.owner,
        1,
        b"hi".to_vec(),
    )
    .expect_err("self-DM at signing time must be rejected");
    assert!(err.contains("must differ"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Banned participants: `verify` is STABLE (bans not enforced in verify);
// sweep in post_apply_cleanup is what drops them.
// ---------------------------------------------------------------------------

#[test]
fn ban_then_existing_dm_state_still_verifies() {
    let f = make_fixture();
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hi");
    let mut state = f.state.clone();
    state.direct_messages.messages.push(msg);
    assert!(state.verify(&state, &f.params).is_ok(), "pre-ban verify");

    // Owner bans Alice while her DM is still in state.
    state.bans.0.push(AuthorizedUserBan::new(
        UserBan {
            owner_member_id: f.owner_id,
            banned_at: SystemTime::now(),
            banned_user: f.alice_id,
        },
        f.owner_id,
        &f.owner_sk,
    ));
    // verify MUST stay green - the sweep in post_apply_cleanup runs
    // after apply_delta and removes banned-participant DMs.
    assert!(
        state.verify(&state, &f.params).is_ok(),
        "verify must remain stable after ban"
    );
}

#[test]
fn post_apply_cleanup_sweeps_banned_sender_dms() {
    let f = make_fixture();
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hi");
    let mut state = f.state.clone();
    state.direct_messages.messages.push(msg);

    // Add a single recent message from Alice so she's retained as a
    // member through normal cleanup, then ban her.
    state.bans.0.push(AuthorizedUserBan::new(
        UserBan {
            owner_member_id: f.owner_id,
            banned_at: SystemTime::now(),
            banned_user: f.alice_id,
        },
        f.owner_id,
        &f.owner_sk,
    ));
    state.post_apply_cleanup(&f.params).unwrap();

    assert!(
        state.direct_messages.messages.is_empty(),
        "banned-sender DM must be swept; got {:?}",
        state.direct_messages.messages
    );
}

#[test]
fn post_apply_cleanup_sweeps_banned_recipient_dms() {
    let f = make_fixture();
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hi");
    let mut state = f.state.clone();
    state.direct_messages.messages.push(msg);

    state.bans.0.push(AuthorizedUserBan::new(
        UserBan {
            owner_member_id: f.owner_id,
            banned_at: SystemTime::now(),
            banned_user: f.bob_id,
        },
        f.owner_id,
        &f.owner_sk,
    ));
    state.post_apply_cleanup(&f.params).unwrap();

    assert!(
        state.direct_messages.messages.is_empty(),
        "banned-recipient DM must be swept"
    );
}

#[test]
fn post_apply_cleanup_retains_dm_participants_as_members() {
    let f = make_fixture();
    // Alice DMs Bob. Neither has any recent_messages, so without the
    // DM-participant retention they'd both be pruned, and the DM
    // would then orphan into an unverifiable state. With retention,
    // they stay.
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hi bob");
    let mut state = f.state.clone();
    state.direct_messages.messages.push(msg);

    state.post_apply_cleanup(&f.params).unwrap();

    let member_ids: HashSet<MemberId> = state
        .members
        .members
        .iter()
        .map(|m| m.member.id())
        .collect();
    assert!(
        member_ids.contains(&f.alice_id),
        "Alice (DM sender) must be retained as a member"
    );
    assert!(
        member_ids.contains(&f.bob_id),
        "Bob (DM recipient) must be retained as a member"
    );
    assert!(
        !state.direct_messages.messages.is_empty(),
        "DM must be retained when its participants are members"
    );
    assert!(state.verify(&state, &f.params).is_ok());
}

#[test]
fn cleanup_drops_purge_envelope_for_non_member() {
    let f = make_fixture();
    let purges = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: vec![tok(0x11)],
        },
    )
    .unwrap();
    let mut state = f.state.clone();
    state.direct_messages.purges.push(purges);

    // Ban Bob. With no DMs referencing him, the participant set is
    // empty, but his purge envelope was attached to him.
    state.bans.0.push(AuthorizedUserBan::new(
        UserBan {
            owner_member_id: f.owner_id,
            banned_at: SystemTime::now(),
            banned_user: f.bob_id,
        },
        f.owner_id,
        &f.owner_sk,
    ));
    state.post_apply_cleanup(&f.params).unwrap();
    assert!(
        state.direct_messages.purges.is_empty(),
        "banned recipient's purge envelope must be swept"
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

    let purges = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: vec![purge_token],
        },
    )
    .unwrap();

    let mut bob_state = f.state.clone();
    bob_state.direct_messages.purges.push(purges.clone());

    assert!(
        bob_state.verify(&bob_state, &f.params).is_ok(),
        "bob's purged state should verify"
    );

    // Stale peer sends Bob a delta with the message back.
    let delta = DirectMessagesDelta {
        new_messages: vec![msg.clone()],
        advanced_purges: vec![],
    };
    bob_state
        .direct_messages
        .apply_delta(&bob_state.clone(), &f.params, &Some(delta))
        .expect("apply_delta should succeed");

    assert!(
        bob_state.direct_messages.messages.is_empty(),
        "tombstoned message must not be re-installed via merge"
    );
}

#[test]
fn purge_advance_retroactively_drops_already_installed_message() {
    let f = make_fixture();
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hi bob");
    let token = msg.purge_token();

    // Bob's state already contains the message.
    let mut state = f.state.clone();
    state.direct_messages.messages.push(msg.clone());
    assert!(state.verify(&state, &f.params).is_ok());

    // Bob signs a purge for that token.
    let purges = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: vec![token],
        },
    )
    .unwrap();
    let delta = DirectMessagesDelta {
        new_messages: vec![],
        advanced_purges: vec![purges],
    };
    state
        .direct_messages
        .apply_delta(&state.clone(), &f.params, &Some(delta))
        .expect("apply_delta should succeed");

    assert!(
        state.direct_messages.messages.is_empty(),
        "retroactive tombstone retain should drop the message"
    );
}

// ---------------------------------------------------------------------------
// Purge envelope: signature + signer identity + monotonic version + content
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
            purged: vec![tok(1), tok(2), tok(3)],
        },
    )
    .unwrap();

    let mut tampered = purges.clone();
    let bytes =
        build_recipient_purges_signed_bytes(f.bob_id, &f.params.owner, &tampered.state).unwrap();
    tampered.recipient_signature = f.alice_sk.sign(&bytes);

    let mut state = f.state.clone();
    state.direct_messages.purges.push(tampered);

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
    let state_purges = RecipientPurges {
        version: 1,
        purged: vec![tok(42)],
    };
    let bytes =
        build_recipient_purges_signed_bytes(f.bob_id, &f.params.owner, &state_purges).unwrap();
    let alice_sig = f.alice_sk.sign(&bytes);
    let bogus = AuthorizedRecipientPurges {
        recipient_id: f.bob_id,
        state: state_purges,
        recipient_signature: alice_sig,
    };

    let mut state = f.state.clone();
    state.direct_messages.purges.push(bogus);

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
fn purge_envelope_version_zero_rejected_in_verify() {
    let f = make_fixture();
    let bogus = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 0,
            purged: vec![],
        },
    )
    .unwrap();
    let mut state = f.state.clone();
    state.direct_messages.purges.push(bogus);
    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(err.contains("version 0 is reserved"), "got: {err}");
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
            purged: vec![tok(10)],
        },
    )
    .unwrap();
    state.direct_messages.purges.push(v2.clone());

    let v1 = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: vec![tok(10), tok(20)],
        },
    )
    .unwrap();
    let delta = DirectMessagesDelta {
        new_messages: vec![],
        advanced_purges: vec![v1],
    };
    state
        .direct_messages
        .apply_delta(&state.clone(), &f.params, &Some(delta))
        .expect("older version is silently ignored");

    assert_eq!(state.direct_messages.purges.len(), 1);
    assert_eq!(state.direct_messages.purges[0], v2);
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
            purged: vec![tok(10)],
        },
    )
    .unwrap();
    state.direct_messages.purges.push(env_a);

    let env_b = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 2,
            purged: vec![tok(20)],
        },
    )
    .unwrap();
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
fn purge_version_bump_must_be_superset() {
    let f = make_fixture();
    let mut state = f.state.clone();

    let v1 = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: vec![tok(10), tok(20)],
        },
    )
    .unwrap();
    state.direct_messages.purges.push(v1);

    // v2 drops tok(20) - must be rejected.
    let v2 = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 2,
            purged: vec![tok(10)],
        },
    )
    .unwrap();
    let delta = DirectMessagesDelta {
        new_messages: vec![],
        advanced_purges: vec![v2],
    };
    let err = state
        .direct_messages
        .apply_delta(&state.clone(), &f.params, &Some(delta))
        .expect_err("dropping a previously-purged token must be rejected");
    assert!(
        err.contains("drops") && err.contains("previously-purged"),
        "got: {err}"
    );
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
    )
    .unwrap();
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
fn per_pair_count_cap_at_limit_accepted() {
    let f = make_fixture();
    let mut state = f.state.clone();
    for i in 0..MAX_DM_MESSAGES_PER_PAIR as u64 {
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
    assert!(state.verify(&state, &f.params).is_ok());
}

#[test]
fn per_pair_count_cap_just_over_rejected() {
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
fn ciphertext_size_cap_at_limit_accepted() {
    let f = make_fixture();
    let at_limit = vec![0u8; MAX_DM_CIPHERTEXT_BYTES];
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, &at_limit);
    let mut state = f.state.clone();
    state.direct_messages.messages.push(msg);
    assert!(state.verify(&state, &f.params).is_ok());
}

#[test]
fn ciphertext_size_cap_just_over_rejected() {
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
fn purge_list_cap_just_over_rejected() {
    let f = make_fixture();
    let huge: Vec<PurgeToken> = (0..(MAX_PURGED_TOMBSTONES_PER_RECIPIENT as u32 + 1))
        .map(|i| {
            let mut t = [0u8; 16];
            t[0..4].copy_from_slice(&i.to_le_bytes());
            PurgeToken(t)
        })
        .collect();
    let purges = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: huge,
        },
    )
    .unwrap();
    let mut state = f.state.clone();
    state.direct_messages.purges.push(purges);
    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(err.contains("exceed cap"), "got: {err}");
}

#[test]
fn duplicate_recipient_purges_envelope_rejected() {
    let f = make_fixture();
    let env1 = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: vec![tok(1)],
        },
    )
    .unwrap();
    let env2 = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 2,
            purged: vec![tok(1), tok(2)],
        },
    )
    .unwrap();
    let mut state = f.state.clone();
    state.direct_messages.purges.push(env1);
    state.direct_messages.purges.push(env2);
    let err = state
        .direct_messages
        .verify(&state, &f.params)
        .expect_err("verify should fail");
    assert!(err.contains("duplicate envelope"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Future-skew helper
// ---------------------------------------------------------------------------

#[test]
fn future_skew_boundary() {
    let now = 1_700_000_000u64;
    assert!(check_dm_future_skew(now, now).is_ok());
    assert!(check_dm_future_skew(now + MAX_DM_FUTURE_SKEW_SECS, now).is_ok());
    let err = check_dm_future_skew(now + MAX_DM_FUTURE_SKEW_SECS + 1, now)
        .expect_err("far-future must be rejected");
    assert!(err.contains("ahead of now"), "got: {err}");
    assert!(check_dm_future_skew(0, now).is_ok());
}

// ---------------------------------------------------------------------------
// Deterministic wire format (locked hex)
// ---------------------------------------------------------------------------

#[test]
fn direct_messages_wire_format_locked() {
    let sender_sk = SigningKey::from_bytes(&[7u8; 32]);
    let recipient_sk = SigningKey::from_bytes(&[11u8; 32]);
    let owner_sk = SigningKey::from_bytes(&[42u8; 32]);
    let room_owner_vk = owner_sk.verifying_key();

    let sender_id = MemberId::from(&sender_sk.verifying_key());
    let recipient_id = MemberId::from(&recipient_sk.verifying_key());

    let msg = sign_direct_message(
        &sender_sk,
        sender_id,
        recipient_id,
        &room_owner_vk,
        1_700_000_000,
        b"deterministic ciphertext".to_vec(),
    )
    .unwrap();

    let purges = sign_recipient_purges(
        &recipient_sk,
        recipient_id,
        &room_owner_vk,
        RecipientPurges {
            version: 5,
            purged: vec![PurgeToken([0xAA; 16]), PurgeToken([0xBB; 16])],
        },
    )
    .unwrap();

    let mut dms = DirectMessagesV1::default();
    dms.messages.push(msg);
    dms.purges.push(purges);

    let mut buf = Vec::new();
    ciborium::ser::into_writer(&dms, &mut buf).unwrap();

    let decoded: DirectMessagesV1 = ciborium::de::from_reader(buf.as_slice()).unwrap();
    assert_eq!(decoded, dms);

    let hex_actual = data_encoding::HEXLOWER.encode(&buf);
    let expected_hex_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("direct_messages_wire_format.hex");

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
// Domain separation: DM signed bytes start with 'M', purge with 'P'
// ---------------------------------------------------------------------------

#[test]
fn signed_bytes_carry_domain_tag() {
    let f = make_fixture();
    let dm_bytes =
        build_direct_message_signed_bytes(f.alice_id, f.bob_id, &f.params.owner, 0, b"hi").unwrap();
    assert_eq!(dm_bytes[0], DOMAIN_TAG_MESSAGE);

    let p_bytes = build_recipient_purges_signed_bytes(
        f.bob_id,
        &f.params.owner,
        &RecipientPurges {
            version: 1,
            purged: vec![tok(0xAB)],
        },
    )
    .unwrap();
    assert_eq!(p_bytes[0], DOMAIN_TAG_PURGES);
    assert_ne!(dm_bytes[0], p_bytes[0]);
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

// ---------------------------------------------------------------------------
// CRDT convergence: commutativity + idempotency of apply_delta
// ---------------------------------------------------------------------------

#[test]
fn apply_delta_commutativity_two_messages() {
    let f = make_fixture();
    let m1 = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hello");
    let m2 = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 2, b"world");

    let mut a = f.state.clone();
    let mut b = f.state.clone();

    let delta_a = DirectMessagesDelta {
        new_messages: vec![m1.clone()],
        advanced_purges: vec![],
    };
    let delta_b = DirectMessagesDelta {
        new_messages: vec![m2.clone()],
        advanced_purges: vec![],
    };

    a.direct_messages
        .apply_delta(&a.clone(), &f.params, &Some(delta_a.clone()))
        .unwrap();
    a.direct_messages
        .apply_delta(&a.clone(), &f.params, &Some(delta_b.clone()))
        .unwrap();

    b.direct_messages
        .apply_delta(&b.clone(), &f.params, &Some(delta_b))
        .unwrap();
    b.direct_messages
        .apply_delta(&b.clone(), &f.params, &Some(delta_a))
        .unwrap();

    assert_eq!(
        a.direct_messages, b.direct_messages,
        "applying deltas in different orders must converge"
    );
}

#[test]
fn apply_delta_commutativity_message_then_purge_vs_purge_then_message() {
    let f = make_fixture();
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hi bob");
    let purge = sign_recipient_purges(
        &f.bob_sk,
        f.bob_id,
        &f.params.owner,
        RecipientPurges {
            version: 1,
            purged: vec![msg.purge_token()],
        },
    )
    .unwrap();

    let m_delta = DirectMessagesDelta {
        new_messages: vec![msg.clone()],
        advanced_purges: vec![],
    };
    let p_delta = DirectMessagesDelta {
        new_messages: vec![],
        advanced_purges: vec![purge.clone()],
    };

    let mut a = f.state.clone();
    a.direct_messages
        .apply_delta(&a.clone(), &f.params, &Some(m_delta.clone()))
        .unwrap();
    a.direct_messages
        .apply_delta(&a.clone(), &f.params, &Some(p_delta.clone()))
        .unwrap();

    let mut b = f.state.clone();
    b.direct_messages
        .apply_delta(&b.clone(), &f.params, &Some(p_delta))
        .unwrap();
    b.direct_messages
        .apply_delta(&b.clone(), &f.params, &Some(m_delta))
        .unwrap();

    assert_eq!(a.direct_messages, b.direct_messages, "must converge");
    assert!(
        a.direct_messages.messages.is_empty(),
        "tombstone must win regardless of order"
    );
}

#[test]
fn apply_delta_idempotency() {
    let f = make_fixture();
    let m = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hi");
    let delta = DirectMessagesDelta {
        new_messages: vec![m.clone()],
        advanced_purges: vec![],
    };

    let mut once = f.state.clone();
    once.direct_messages
        .apply_delta(&once.clone(), &f.params, &Some(delta.clone()))
        .unwrap();

    let mut twice = once.clone();
    twice
        .direct_messages
        .apply_delta(&twice.clone(), &f.params, &Some(delta))
        .unwrap();

    assert_eq!(
        once.direct_messages, twice.direct_messages,
        "applying the same delta twice must be idempotent"
    );
    assert_eq!(once.direct_messages.messages.len(), 1);
}

// ---------------------------------------------------------------------------
// Intra-delta duplicate-signature dedup
// ---------------------------------------------------------------------------

#[test]
fn intra_delta_duplicate_signature_only_pushes_once() {
    let f = make_fixture();
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, b"hi");
    // Same message twice in the same delta.
    let delta = DirectMessagesDelta {
        new_messages: vec![msg.clone(), msg.clone()],
        advanced_purges: vec![],
    };
    let mut state = f.state.clone();
    state
        .direct_messages
        .apply_delta(&state.clone(), &f.params, &Some(delta))
        .unwrap();
    assert_eq!(
        state.direct_messages.messages.len(),
        1,
        "duplicate within delta must not double-push"
    );
}

// ---------------------------------------------------------------------------
// apply_delta silent-drop branches
// ---------------------------------------------------------------------------

#[test]
fn apply_delta_silently_drops_non_member_sender() {
    let f = make_fixture();
    let stranger_sk = SigningKey::generate(&mut OsRng);
    let stranger_id = MemberId::from(&stranger_sk.verifying_key());
    let msg = sign_direct_message(
        &stranger_sk,
        stranger_id,
        f.bob_id,
        &f.params.owner,
        1,
        b"hi".to_vec(),
    )
    .unwrap();
    let delta = DirectMessagesDelta {
        new_messages: vec![msg],
        advanced_purges: vec![],
    };
    let mut state = f.state.clone();
    state
        .direct_messages
        .apply_delta(&state.clone(), &f.params, &Some(delta))
        .expect("apply_delta succeeds");
    assert!(
        state.direct_messages.messages.is_empty(),
        "stranger-sender must be silently dropped"
    );
}

#[test]
fn apply_delta_silently_drops_self_dm() {
    let f = make_fixture();
    // Construct a self-DM manually since sign_direct_message rejects it.
    let timestamp = 1u64;
    let bytes = build_direct_message_signed_bytes(
        f.alice_id,
        f.alice_id,
        &f.params.owner,
        timestamp,
        b"hi",
    )
    .unwrap();
    let sig = f.alice_sk.sign(&bytes);
    let manual = AuthorizedDirectMessage {
        message: DirectMessage {
            sender: f.alice_id,
            recipient: f.alice_id,
            timestamp,
            ciphertext: b"hi".to_vec(),
        },
        sender_signature: sig,
    };
    let delta = DirectMessagesDelta {
        new_messages: vec![manual],
        advanced_purges: vec![],
    };
    let mut state = f.state.clone();
    state
        .direct_messages
        .apply_delta(&state.clone(), &f.params, &Some(delta))
        .expect("apply_delta succeeds (drops self-DM silently)");
    assert!(state.direct_messages.messages.is_empty());
}

#[test]
fn apply_delta_silently_drops_oversize_ciphertext() {
    let f = make_fixture();
    let too_big = vec![0u8; MAX_DM_CIPHERTEXT_BYTES + 1];
    let msg = dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, 1, &too_big);
    let delta = DirectMessagesDelta {
        new_messages: vec![msg],
        advanced_purges: vec![],
    };
    let mut state = f.state.clone();
    state
        .direct_messages
        .apply_delta(&state.clone(), &f.params, &Some(delta))
        .expect("apply_delta succeeds");
    assert!(state.direct_messages.messages.is_empty());
}

#[test]
fn apply_delta_silently_drops_per_pair_overflow() {
    let f = make_fixture();
    let mut state = f.state.clone();
    // Pre-fill to the cap.
    for i in 0..MAX_DM_MESSAGES_PER_PAIR as u64 {
        state
            .direct_messages
            .messages
            .push(dm_at(&f, &f.alice_sk, f.alice_id, f.bob_id, i, b"x"));
    }
    // One more in a delta - must be dropped, not error out the merge.
    let overflow = dm_at(
        &f,
        &f.alice_sk,
        f.alice_id,
        f.bob_id,
        MAX_DM_MESSAGES_PER_PAIR as u64 + 100,
        b"over",
    );
    let delta = DirectMessagesDelta {
        new_messages: vec![overflow],
        advanced_purges: vec![],
    };
    state
        .direct_messages
        .apply_delta(&state.clone(), &f.params, &Some(delta))
        .expect("apply_delta must NOT fail on per-pair overflow");
    assert_eq!(
        state.direct_messages.messages.len(),
        MAX_DM_MESSAGES_PER_PAIR,
        "overflow message must be silently dropped"
    );
}
