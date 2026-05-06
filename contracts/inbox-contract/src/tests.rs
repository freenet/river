//! Unit tests for the inbox contract (v4).
//!
//! v4 is self-contained: each member-sent message carries a
//! `MembershipProof` with the sender's `AuthorizedMember` plus the
//! invitation chain back to the room owner. The contract verifies
//! the chain locally against `params.room_owner_vk`. Owner-sent
//! messages skip the proof entirely.

use super::*;
use ed25519_dalek::SigningKey;
use river_core::room_state::member::{AuthorizedMember, Member};

const FIXED_NOW: u64 = 1_700_000_000; // 2023-11-14

struct ClockGuard;

impl ClockGuard {
    fn pin(ts: u64) -> Self {
        set_clock_override_for_tests(Some(ts));
        ClockGuard
    }
}

impl Drop for ClockGuard {
    fn drop(&mut self) {
        set_clock_override_for_tests(None);
    }
}

fn sk_from_seed(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn ser<T: Serialize>(t: &T) -> Vec<u8> {
    let mut out = Vec::new();
    into_writer(t, &mut out).unwrap();
    out
}

/// Build an `AuthorizedMember` for `member_sk`, invited by
/// `inviter_sk`, in a room owned by `owner_vk`.
fn auth_member(
    member_sk: &SigningKey,
    inviter_sk: &SigningKey,
    owner_vk: &VerifyingKey,
) -> AuthorizedMember {
    let member = Member {
        owner_member_id: MemberId::from(owner_vk),
        invited_by: MemberId::from(&inviter_sk.verifying_key()),
        member_vk: member_sk.verifying_key(),
    };
    AuthorizedMember::new(member, inviter_sk)
}

/// Convenience: build a one-level proof (sender invited directly by
/// the owner).
fn proof_directly_invited_by_owner(
    sender_sk: &SigningKey,
    owner_sk: &SigningKey,
) -> MembershipProof {
    let owner_vk = owner_sk.verifying_key();
    MembershipProof {
        sender_authorized: auth_member(sender_sk, owner_sk, &owner_vk),
        invitation_chain: Vec::new(),
    }
}

fn mk_inbox_params_bytes(recipient_sk: &SigningKey, owner_vk: &VerifyingKey) -> Vec<u8> {
    ser(&InboxParams {
        recipient_vk: recipient_sk.verifying_key(),
        room_owner_vk: *owner_vk,
    })
}

// ===========================================================================
// validate_state — basic shapes
// ===========================================================================

#[test]
fn empty_inbox_validates() {
    let _g = ClockGuard::pin(FIXED_NOW);
    let recipient_sk = sk_from_seed(99);
    let owner_sk = sk_from_seed(1);
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(
            &recipient_sk,
            &owner_sk.verifying_key(),
        )),
        State::from(Vec::<u8>::new()),
        RelatedContracts::default(),
    )
    .unwrap();
    assert_eq!(res, ValidateResult::Valid);
}

#[test]
fn message_from_member_is_valid() {
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"ct".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };

    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    )
    .unwrap();
    assert_eq!(res, ValidateResult::Valid);
}

#[test]
fn message_from_owner_is_valid() {
    // v4 supports owner-sent messages: sender == fast_hash(owner_vk),
    // member_proof is None, signature against owner_vk directly.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let recipient_sk = sk_from_seed(99);

    let msg = sign_inbox_message_owner(
        &owner_sk,
        &recipient_sk.verifying_key(),
        FIXED_NOW,
        b"hi from owner".to_vec(),
    );
    assert!(msg.member_proof.is_none(), "owner-sent must have no proof");
    assert_eq!(msg.sender, MemberId::from(&owner_vk));

    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    )
    .unwrap();
    assert_eq!(res, ValidateResult::Valid);
}

#[test]
fn owner_sent_message_with_member_proof_rejected() {
    // Suspicious: sender == owner but member_proof is Some. Reject.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let recipient_sk = sk_from_seed(99);

    // Build any valid-looking proof; the contract must reject because
    // an owner-sent message MUST omit the proof.
    let some_member_sk = sk_from_seed(2);
    let proof = proof_directly_invited_by_owner(&some_member_sk, &owner_sk);

    // Sign as owner. Override the proof field after the fact.
    let mut msg = sign_inbox_message_owner(
        &owner_sk,
        &recipient_sk.verifying_key(),
        FIXED_NOW,
        b"x".to_vec(),
    );
    msg.member_proof = Some(proof);

    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(
        res.is_err(),
        "owner-sent message with a member_proof must be rejected"
    );
}

#[test]
fn message_from_non_member_is_rejected() {
    // Outsider with no inviter at all — proof's chain root doesn't
    // terminate at the owner.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let outsider_sk = sk_from_seed(7);
    let other_authority_sk = sk_from_seed(50); // not the owner
    let recipient_sk = sk_from_seed(99);

    // Build a proof where the sender's AuthorizedMember was signed
    // by some random key, NOT the owner.
    let proof = MembershipProof {
        sender_authorized: auth_member(&outsider_sk, &other_authority_sk, &owner_vk),
        invitation_chain: Vec::new(),
    };
    let msg = sign_inbox_message_member(
        &outsider_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"ct".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };

    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(res.is_err(), "non-member should be rejected: {res:?}");
}

#[test]
fn forged_sender_rejected() {
    // Sign the payload with member 2's key but stamp sender as
    // member 3's MemberId. The signature won't verify against
    // member 3's vk.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let m2_sk = sk_from_seed(2);
    let m3_sk = sk_from_seed(3);
    let recipient_sk = sk_from_seed(99);

    let proof_m3 = proof_directly_invited_by_owner(&m3_sk, &owner_sk);

    // Build a payload claiming to be from m3, but signed by m2.
    use ed25519_dalek::Signer;
    let m3_id = MemberId::from(&m3_sk.verifying_key());
    let proof_field = Some(proof_m3);
    let proof_hash = compute_proof_hash(&proof_field);
    let payload = build_signed_payload_bytes(
        m3_id,
        &recipient_sk.verifying_key(),
        &owner_vk,
        &proof_hash,
        FIXED_NOW,
        b"x",
    );
    let signature = m2_sk.sign(&payload);
    let msg = InboxMessage {
        sender: m3_id,
        timestamp: FIXED_NOW,
        ciphertext: b"x".to_vec(),
        signature,
        member_proof: proof_field,
    };

    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(
        res.is_err(),
        "forged sender (signature/sender mismatch) must be rejected"
    );
}

#[test]
fn forged_member_id_with_unrelated_vk() {
    // An attacker fabricates a "root" AuthorizedMember by signing
    // their own Member entry with their own key while claiming
    // `invited_by == owner` so the chain ostensibly terminates at the
    // owner. The chain root signature is then verified against the
    // owner's vk — which fails because it was actually signed by the
    // attacker's key, not the owner's. Exercises the
    // chain-root-signature gate in `chain.rs`; does NOT test
    // MemberId-collision resistance (Ed25519 key collisions are
    // computationally infeasible and out of scope here).
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let attacker_sk = sk_from_seed(123);
    let recipient_sk = sk_from_seed(99);

    // Attacker invents a "root" AuthorizedMember: they sign their own
    // Member entry, claiming `invited_by == owner` so the chain
    // ostensibly terminates at the owner. The chain root signature
    // is then verified against the owner's vk — which fails because
    // it was actually signed by the attacker.
    //
    // AuthorizedMember::new() asserts `member.invited_by` matches
    // the signing key, so we have to construct via with_signature +
    // a manual sign.
    use ed25519_dalek::Signer;
    let fake_root_member = Member {
        owner_member_id: MemberId::from(&owner_vk),
        invited_by: MemberId::from(&owner_vk),
        member_vk: attacker_sk.verifying_key(),
    };
    let mut payload = Vec::new();
    into_writer(&fake_root_member, &mut payload).unwrap();
    let bogus_sig = attacker_sk.sign(&payload);
    let fake_root = AuthorizedMember::with_signature(fake_root_member, bogus_sig);
    let proof = MembershipProof {
        sender_authorized: fake_root,
        invitation_chain: Vec::new(),
    };
    let msg = sign_inbox_message_member(
        &attacker_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"forged".to_vec(),
        proof,
    );

    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(
        res.is_err(),
        "fabricated chain not signed by owner must be rejected"
    );
}

#[test]
fn oversize_ciphertext_rejected() {
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let big = vec![0u8; MAX_CIPHERTEXT_BYTES + 1];
    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        big,
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };

    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(res.is_err(), "oversize ciphertext must be rejected");
}

#[test]
fn far_future_timestamp_rejected() {
    // Future-skew applies to *incoming* messages (update_state
    // path), not to already-stored messages. Use update_state.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let bad_ts = FIXED_NOW + MAX_FUTURE_SKEW_SECS + 60;
    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        bad_ts,
        b"x".to_vec(),
        proof,
    );

    let delta = InboxDelta::AppendMessages(vec![msg]);
    let res = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(Vec::<u8>::new()),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta)))],
    );
    assert!(
        res.is_err(),
        "far-future timestamp on incoming msg must be rejected"
    );
}

#[test]
fn truncated_ciphertext_rejected() {
    // Hand-craft a message where the ciphertext was truncated
    // post-signing. The signature now no longer matches the bytes
    // the verifier reconstructs.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let mut msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"original-ciphertext".to_vec(),
        proof,
    );
    msg.ciphertext.pop();

    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(
        res.is_err(),
        "truncated ciphertext must fail signature verification"
    );
}

#[test]
fn cross_inbox_replay_rejected() {
    // Message signed for inbox A doesn't validate for inbox B,
    // because the signed payload binds the recipient_vk and inbox B
    // has a different recipient_vk in its params.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let alice_sk = sk_from_seed(99); // recipient A
    let bob_sk = sk_from_seed(100); // recipient B

    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let msg = sign_inbox_message_member(
        &sender_sk,
        &alice_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"hi alice".to_vec(),
        proof,
    );

    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&bob_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(
        res.is_err(),
        "message signed for Alice's inbox must not validate against Bob's inbox"
    );
}

#[test]
fn recipient_can_purge_message_from_now_banned_member() {
    // The inbox can't see the room's ban list, so a banned member
    // with a still-valid AuthorizedMember can keep sending. The
    // recipient handles this operationally via the purge primitive.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let banned_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    // Banned member's old AuthorizedMember is still cryptographically
    // valid — the ban happens in the room contract, not here.
    let proof = proof_directly_invited_by_owner(&banned_sk, &owner_sk);
    let msg = sign_inbox_message_member(
        &banned_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"spam".to_vec(),
        proof,
    );

    // Step 1: message lands in the inbox (the contract has no
    // visibility into the ban).
    let inbox = Inbox {
        messages: vec![msg.clone()],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    )
    .unwrap();
    assert_eq!(res, ValidateResult::Valid);

    // Step 2: recipient publishes a purge for the unwanted message.
    let auth = sign_recipient_state(
        &recipient_sk,
        RecipientState {
            version: 1,
            purged: vec![purge_id_for_signature(&msg.signature)],
        },
    );
    let delta = InboxDelta::UpdateRecipientState(auth);
    let res = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta)))],
    )
    .unwrap();
    let purged: Inbox = from_reader(res.new_state.as_ref().unwrap().as_ref()).unwrap();
    assert!(
        purged.messages.is_empty(),
        "recipient purge must remove the spam message"
    );

    // Step 3: replay of the same message is now blocked by the
    // tombstone.
    let delta_replay = InboxDelta::AppendMessages(vec![msg]);
    let res = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&purged)),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta_replay)))],
    );
    assert!(
        res.is_err(),
        "replay after recipient purge must be tombstone-blocked"
    );
}

// ===========================================================================
// validate_state — chain-specific
// ===========================================================================

#[test]
fn chain_directly_invited_by_owner() {
    // invitation_chain is empty; sender's AuthorizedMember signed
    // directly by owner. (Same shape as
    // `message_from_member_is_valid` — kept for clarity.)
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    assert!(proof.invitation_chain.is_empty());

    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"ct".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    )
    .unwrap();
    assert_eq!(res, ValidateResult::Valid);
}

#[test]
fn chain_one_level_deep() {
    // owner -> X -> sender
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let x_sk = sk_from_seed(2);
    let sender_sk = sk_from_seed(3);
    let recipient_sk = sk_from_seed(99);

    let x_auth = auth_member(&x_sk, &owner_sk, &owner_vk);
    let sender_auth = auth_member(&sender_sk, &x_sk, &owner_vk);
    let proof = MembershipProof {
        sender_authorized: sender_auth,
        invitation_chain: vec![x_auth],
    };
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"hi".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    )
    .unwrap();
    assert_eq!(res, ValidateResult::Valid);
}

#[test]
fn chain_three_levels_deep() {
    // owner -> a -> b -> c -> sender (4 hops, depth = 4)
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let a_sk = sk_from_seed(2);
    let b_sk = sk_from_seed(3);
    let c_sk = sk_from_seed(4);
    let sender_sk = sk_from_seed(5);
    let recipient_sk = sk_from_seed(99);

    let a_auth = auth_member(&a_sk, &owner_sk, &owner_vk);
    let b_auth = auth_member(&b_sk, &a_sk, &owner_vk);
    let c_auth = auth_member(&c_sk, &b_sk, &owner_vk);
    let sender_auth = auth_member(&sender_sk, &c_sk, &owner_vk);
    // invitation_chain order: closest to sender first.
    let proof = MembershipProof {
        sender_authorized: sender_auth,
        invitation_chain: vec![c_auth, b_auth, a_auth],
    };
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"hi".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    )
    .unwrap();
    assert_eq!(res, ValidateResult::Valid);
}

#[test]
fn chain_too_deep_rejected() {
    // Build a chain with depth = MAX_CHAIN_DEPTH + 1.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let recipient_sk = sk_from_seed(99);

    // Generate MAX_CHAIN_DEPTH + 1 keys: keys[0] is invited by owner,
    // keys[i] invited by keys[i-1], keys[last] is the sender.
    let total = MAX_CHAIN_DEPTH + 1;
    let keys: Vec<SigningKey> = (0..total).map(|i| sk_from_seed(10 + i as u8)).collect();

    let mut chain_auths: Vec<AuthorizedMember> = Vec::new();
    chain_auths.push(auth_member(&keys[0], &owner_sk, &owner_vk));
    for i in 1..total {
        chain_auths.push(auth_member(&keys[i], &keys[i - 1], &owner_vk));
    }
    let sender_auth = chain_auths.pop().unwrap();
    // Chain order closest-to-sender first: reverse.
    chain_auths.reverse();

    let proof = MembershipProof {
        sender_authorized: sender_auth,
        invitation_chain: chain_auths,
    };
    assert_eq!(1 + proof.invitation_chain.len(), total);

    let sender_sk = &keys[total - 1];
    let msg = sign_inbox_message_member(
        sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"x".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(
        res.is_err(),
        "chain at depth {} must be rejected (limit = {})",
        total,
        MAX_CHAIN_DEPTH
    );
}

#[test]
fn chain_with_broken_link_rejected() {
    // owner -> a -> b -> sender, but b's `invited_by` is set to a
    // bogus MemberId (not a's id). Fabricate the link so that the
    // signature still verifies but the invited_by field is wrong.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let a_sk = sk_from_seed(2);
    let b_sk = sk_from_seed(3);
    let sender_sk = sk_from_seed(4);
    let recipient_sk = sk_from_seed(99);

    let a_auth = auth_member(&a_sk, &owner_sk, &owner_vk);
    let b_auth = auth_member(&b_sk, &a_sk, &owner_vk);
    let sender_auth = auth_member(&sender_sk, &b_sk, &owner_vk);

    // Tamper: replace b's `invited_by` with a totally unrelated
    // MemberId. The signature still verifies on the original Member
    // but invited_by no longer matches a.id().
    let bogus_id = MemberId::from(&sk_from_seed(99).verifying_key());
    let mut tampered_b_member = b_auth.member.clone();
    tampered_b_member.invited_by = bogus_id;
    // This produces an AuthorizedMember whose stored signature is a
    // signature over the *original* (untampered) Member by a — so
    // signature verification will also fail. Either the broken-link
    // check or the signature check will catch it; both are valid.
    let tampered_b_auth = AuthorizedMember::with_signature(tampered_b_member, b_auth.signature);

    let proof = MembershipProof {
        sender_authorized: sender_auth,
        invitation_chain: vec![tampered_b_auth, a_auth],
    };
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"x".to_vec(),
        proof,
    );

    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(res.is_err(), "broken-link chain must be rejected: {res:?}");
}

#[test]
fn chain_not_terminating_at_owner_rejected() {
    // Sender's AuthorizedMember claims to be invited by the owner
    // (invited_by == owner_id), but is actually signed by a non-owner
    // key. Signature against owner's vk will fail.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let other_sk = sk_from_seed(50);
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    // Build sender-as-a-direct-invitee but signed by `other_sk`,
    // claiming invited_by = owner.
    let member = Member {
        owner_member_id: MemberId::from(&owner_vk),
        invited_by: MemberId::from(&owner_vk),
        member_vk: sender_sk.verifying_key(),
    };
    // Forge: signature must be over `member` by `other_sk`. We can't
    // use AuthorizedMember::new because it asserts invited_by ==
    // signing_key. Use util::sign_struct directly via a manual
    // construction. The simplest way is to construct it using
    // AuthorizedMember::with_signature plus a hand-rolled signature.
    // But sign_struct isn't exported here, so use the "wrong key"
    // trick: build with a Member whose invited_by is the OWNER's
    // id, signed by other_sk. AuthorizedMember::new would panic, so
    // we sign manually.

    use ed25519_dalek::Signer;
    // Reproduce sign_struct's wire format: ciborium serialise the
    // Member, sign the bytes.
    let mut payload = Vec::new();
    into_writer(&member, &mut payload).unwrap();
    let signature = other_sk.sign(&payload);
    let sender_auth = AuthorizedMember::with_signature(member, signature);

    let proof = MembershipProof {
        sender_authorized: sender_auth,
        invitation_chain: Vec::new(),
    };
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"x".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(
        res.is_err(),
        "chain root not signed by owner must be rejected"
    );
}

#[test]
fn chain_with_invalid_signature_rejected() {
    // Build a valid 1-level chain, then corrupt the sender's
    // AuthorizedMember signature.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let mut sender_auth = auth_member(&sender_sk, &owner_sk, &owner_vk);
    // Corrupt the signature.
    sender_auth.signature = Signature::from_bytes(&[0u8; 64]);

    let proof = MembershipProof {
        sender_authorized: sender_auth,
        invitation_chain: Vec::new(),
    };
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"x".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(
        res.is_err(),
        "chain link with invalid signature must be rejected"
    );
}

#[test]
fn chain_proof_sender_id_mismatch_rejected() {
    // Build a legit proof for member A, but stamp the InboxMessage's
    // `sender` field with a different MemberId. Since
    // `sender_authorized.member.id() != msg.sender`, reject.
    //
    // Note: this test still has to produce a forward-consistent
    // signature. We sign as A, but force `msg.sender = B`. The
    // signature will then fail to verify because A signed bytes
    // that include `sender=A_id`, but the contract reconstructs with
    // `sender=B_id`. The earlier proof-id check fires first.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let a_sk = sk_from_seed(2);
    let b_sk = sk_from_seed(3);
    let recipient_sk = sk_from_seed(99);

    let proof_a = proof_directly_invited_by_owner(&a_sk, &owner_sk);
    let mut msg = sign_inbox_message_member(
        &a_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"x".to_vec(),
        proof_a,
    );
    msg.sender = MemberId::from(&b_sk.verifying_key());

    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(
        res.is_err(),
        "sender/proof mismatch must be rejected: {res:?}"
    );
}

#[test]
fn signature_against_proof_resolved_vk() {
    // Sanity check: the contract verifies the InboxMessage signature
    // against the VK pulled from sender_authorized, not against
    // anything derived from msg.sender directly. Construct a case
    // where the signature was made by a key that produces the same
    // MemberId as `sender_authorized.member.member_vk` (impossible
    // in practice; we cheat by having the proof's vk and the signer
    // vk be the same key — i.e. the happy path).
    //
    // The negative version (proof-vk mismatch) is covered by
    // `forged_member_id_with_unrelated_vk`. This test is the
    // positive "signature verifies against proof-resolved vk"
    // assertion.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let resolved_vk = proof.sender_authorized.member.member_vk;
    assert_eq!(resolved_vk, sender_sk.verifying_key());

    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"x".to_vec(),
        proof,
    );
    // Direct verify-call against the proof-resolved vk should
    // succeed.
    msg.verify_signature(&resolved_vk, &recipient_sk.verifying_key(), &owner_vk)
        .unwrap();

    // Whole-state validation succeeds too.
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    )
    .unwrap();
    assert_eq!(res, ValidateResult::Valid);
}

// ===========================================================================
// update_state
// ===========================================================================

#[test]
fn append_grows_state() {
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"hi".to_vec(),
        proof,
    );
    let delta = InboxDelta::AppendMessages(vec![msg]);

    let res = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(Vec::<u8>::new()),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta)))],
    )
    .unwrap();
    let new_state: Inbox = from_reader(res.new_state.as_ref().unwrap().as_ref()).unwrap();
    assert_eq!(new_state.messages.len(), 1);
}

#[test]
fn update_enforces_max_messages() {
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);

    let mut messages = Vec::with_capacity(MAX_INBOX_MESSAGES);
    for i in 0..MAX_INBOX_MESSAGES {
        messages.push(sign_inbox_message_member(
            &sender_sk,
            &recipient_sk.verifying_key(),
            &owner_vk,
            FIXED_NOW + i as u64,
            vec![i as u8],
            proof.clone(),
        ));
    }
    let inbox = Inbox {
        messages,
        ..Default::default()
    };
    let state_bytes = ser(&inbox);

    let extra = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW + MAX_INBOX_MESSAGES as u64 + 1,
        b"over".to_vec(),
        proof,
    );
    let delta = InboxDelta::AppendMessages(vec![extra]);

    let res = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(state_bytes),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta)))],
    );
    assert!(res.is_err(), "must reject >MAX_INBOX_MESSAGES");
}

#[test]
fn update_recipient_state_removes_purged_messages() {
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let m1 = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"a".to_vec(),
        proof.clone(),
    );
    let m2 = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW + 1,
        b"b".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![m1.clone(), m2.clone()],
        ..Default::default()
    };

    // Recipient signs a state purging m1.
    let new_state = RecipientState {
        version: 1,
        purged: vec![purge_id_for_signature(&m1.signature)],
    };
    let auth = sign_recipient_state(&recipient_sk, new_state);
    let delta = InboxDelta::UpdateRecipientState(auth);

    let res = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta)))],
    )
    .unwrap();
    let new_state: Inbox = from_reader(res.new_state.as_ref().unwrap().as_ref()).unwrap();
    assert_eq!(new_state.messages.len(), 1);
    assert_eq!(new_state.messages[0].signature, m2.signature);
    assert_eq!(new_state.recipient_state.unwrap().state.version, 1);
}

#[test]
fn update_recipient_state_rejected_by_non_recipient() {
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);
    let imposter_sk = sk_from_seed(50);

    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let m1 = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"a".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![m1.clone()],
        ..Default::default()
    };

    // Imposter signs a recipient_state — the contract verifies
    // against `params.recipient_vk`, which is the legitimate
    // recipient.
    let new_state = RecipientState {
        version: 1,
        purged: vec![purge_id_for_signature(&m1.signature)],
    };
    let auth = sign_recipient_state(&imposter_sk, new_state);
    let delta = InboxDelta::UpdateRecipientState(auth);

    let res = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta)))],
    );
    assert!(
        res.is_err(),
        "imposter-signed recipient_state must be rejected"
    );
}

#[test]
fn update_recipient_state_rejected_by_old_version() {
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let recipient_sk = sk_from_seed(99);

    let v5 = sign_recipient_state(
        &recipient_sk,
        RecipientState {
            version: 5,
            purged: Vec::new(),
        },
    );
    let inbox = Inbox {
        messages: Vec::new(),
        recipient_state: Some(v5),
    };

    let v3 = sign_recipient_state(
        &recipient_sk,
        RecipientState {
            version: 3,
            purged: Vec::new(),
        },
    );
    let delta = InboxDelta::UpdateRecipientState(v3);

    let res = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta)))],
    );
    assert!(res.is_err(), "older version must be rejected");
}

#[test]
fn update_recipient_state_must_be_monotonic() {
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let recipient_sk = sk_from_seed(99);

    let v5 = sign_recipient_state(
        &recipient_sk,
        RecipientState {
            version: 5,
            purged: Vec::new(),
        },
    );
    let inbox = Inbox {
        messages: Vec::new(),
        recipient_state: Some(v5),
    };

    let v5b = sign_recipient_state(
        &recipient_sk,
        RecipientState {
            version: 5,
            purged: vec![123],
        },
    );
    let delta = InboxDelta::UpdateRecipientState(v5b);

    let res = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta)))],
    );
    assert!(
        res.is_err(),
        "equal version must be rejected (strictly greater required)"
    );
}

#[test]
fn tombstone_blocks_replay_after_purge() {
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let m1 = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"a".to_vec(),
        proof,
    );

    // Recipient pre-publishes a tombstone for m1 BEFORE m1 ever
    // lands.
    let auth = sign_recipient_state(
        &recipient_sk,
        RecipientState {
            version: 1,
            purged: vec![purge_id_for_signature(&m1.signature)],
        },
    );
    let inbox = Inbox {
        messages: Vec::new(),
        recipient_state: Some(auth),
    };

    // Sender retries m1.
    let delta = InboxDelta::AppendMessages(vec![m1]);
    let res = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta)))],
    );
    assert!(
        res.is_err(),
        "replay of a tombstoned message must be rejected"
    );
}

#[test]
fn tombstone_eviction_via_recipient_state_replacement() {
    // The bundled-signature pattern means the recipient can replace
    // their RecipientState with one that has a smaller `purged`
    // list. Previously-tombstoned messages can then re-enter. This
    // is by design — the recipient is the authority on what's in
    // their tombstone set.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let m1 = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"a".to_vec(),
        proof,
    );
    let m1_purge = purge_id_for_signature(&m1.signature);

    // v1: m1 is tombstoned.
    let v1 = sign_recipient_state(
        &recipient_sk,
        RecipientState {
            version: 1,
            purged: vec![m1_purge],
        },
    );
    let inbox = Inbox {
        messages: Vec::new(),
        recipient_state: Some(v1),
    };

    // v2: m1 is no longer tombstoned (purged list shrinks).
    let v2 = sign_recipient_state(
        &recipient_sk,
        RecipientState {
            version: 2,
            purged: Vec::new(),
        },
    );
    let delta_v2 = InboxDelta::UpdateRecipientState(v2);
    let res = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta_v2)))],
    )
    .unwrap();
    let after_v2: Inbox = from_reader(res.new_state.as_ref().unwrap().as_ref()).unwrap();
    assert!(after_v2
        .recipient_state
        .as_ref()
        .unwrap()
        .state
        .purged
        .is_empty());

    // Now the sender re-submits m1; it should land.
    let delta_m1 = InboxDelta::AppendMessages(vec![m1.clone()]);
    let res = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&after_v2)),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta_m1)))],
    )
    .unwrap();
    let final_state: Inbox = from_reader(res.new_state.as_ref().unwrap().as_ref()).unwrap();
    assert_eq!(final_state.messages.len(), 1);
    assert_eq!(final_state.messages[0].signature, m1.signature);
}

// ===========================================================================
// CRDT convergence
// ===========================================================================

#[test]
fn append_order_does_not_matter() {
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let s1_sk = sk_from_seed(2);
    let s2_sk = sk_from_seed(3);
    let recipient_sk = sk_from_seed(99);
    let recipient_vk = recipient_sk.verifying_key();

    let proof1 = proof_directly_invited_by_owner(&s1_sk, &owner_sk);
    let proof2 = proof_directly_invited_by_owner(&s2_sk, &owner_sk);

    let m1 = sign_inbox_message_member(
        &s1_sk,
        &recipient_vk,
        &owner_vk,
        FIXED_NOW,
        b"a".to_vec(),
        proof1.clone(),
    );
    let m2 = sign_inbox_message_member(
        &s2_sk,
        &recipient_vk,
        &owner_vk,
        FIXED_NOW + 1,
        b"b".to_vec(),
        proof2,
    );
    let m3 = sign_inbox_message_member(
        &s1_sk,
        &recipient_vk,
        &owner_vk,
        FIXED_NOW + 2,
        b"c".to_vec(),
        proof1,
    );

    let recipient_params = mk_inbox_params_bytes(&recipient_sk, &owner_vk);
    let apply = |msgs: Vec<InboxMessage>| -> Vec<u8> {
        let mut state_bytes: Vec<u8> = Vec::new();
        for m in msgs {
            let delta = InboxDelta::AppendMessages(vec![m]);
            let res = Contract::update_state(
                Parameters::from(recipient_params.clone()),
                State::from(state_bytes.clone()),
                vec![UpdateData::Delta(StateDelta::from(ser(&delta)))],
            )
            .unwrap();
            state_bytes = res.new_state.as_ref().unwrap().as_ref().to_vec();
        }
        state_bytes
    };

    let bytes_a = apply(vec![m1.clone(), m2.clone(), m3.clone()]);
    let bytes_b = apply(vec![m3, m1, m2]);
    // Compare SERIALISED bytes — stronger than struct equality;
    // catches any non-canonical ordering that creeps into wire
    // output.
    assert_eq!(
        bytes_a, bytes_b,
        "merge result must be byte-identical regardless of insertion order"
    );
}

// ===========================================================================
// Wire-format
// ===========================================================================

/// Build a deterministic test inbox used by the wire-format lock
/// test.
fn canonical_test_inbox() -> (Inbox, SigningKey, VerifyingKey) {
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);
    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        1_700_000_000,
        b"hi".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    (inbox, recipient_sk, owner_vk)
}

/// Lock the on-the-wire encoding so accidental serde / ciborium
/// changes are caught. The hex constant below was captured from the
/// first run of this test; if it ever changes, intentional
/// state-format migration is required.
#[test]
fn inbox_wire_format_locked() {
    let (inbox, _recipient_sk, _owner_vk) = canonical_test_inbox();
    let bytes = ser(&inbox);
    // Captured 2026-04-27 against ciborium 0.2.x with the v5 schema
    // (sender: MemberId, recipient_state: Option<...>, member_proof:
    // Option<MembershipProof>, signed payload binding
    // sender+recipient+room_owner+proof_hash+timestamp+ciphertext).
    // The signed payload now includes a 32-byte proof_hash, so v5
    // signatures differ from v4 even for the same message contents.
    // Any change to this string requires a deliberate state-format
    // migration.
    const EXPECTED_HEX: &str = "a2686d6573736167657381a56673656e6465723b6d9aeebb4641956f6974696d657374616d701a6553f1006a636970686572746578748218681869697369676e61747572659840187a18f318c818791824185d184818ea1894186018ae18aa183f18f6184f18ce0918b20c18760118f518a7185e1878183018e118b418c318e718bd1834188f181a18ba188918c30818e818c01829182418fe18c9183d18b518841888189a183e18d618b518c718841822189818501893186a189218c60218fa006c6d656d6265725f70726f6f66a27173656e6465725f617574686f72697a6564a2666d656d626572a36f6f776e65725f6d656d6265725f69641b20340a250a45d4606a696e76697465645f62791b20340a250a45d460696d656d6265725f766b58208139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394697369676e6174757265984018e918c318c218621868187918c51018f218a6187318a118770418cc188c18d2183718a018d318dc183e185a18d3182218ec186118d7188718f9188318dd189318cf18e11826189d18ce18371819185c01186d18451883182c18e118cb18de182b18b51418e018d6185a183d186018e1181918b8183e18ba18530770696e7669746174696f6e5f636861696e806f726563697069656e745f7374617465f6";
    assert_eq!(hex::encode(&bytes), EXPECTED_HEX, "wire format drift");

    // Round-trip too — catches deserialiser asymmetry.
    let parsed: Inbox = from_reader(bytes.as_slice()).unwrap();
    assert_eq!(parsed, inbox);
}

#[test]
fn signed_payload_is_canonical_per_axis() {
    // Each component of (sender, recipient_vk, room_owner_vk,
    // proof_hash, timestamp, ciphertext) must alter the signed bytes.
    let sender1 = MemberId::from(&sk_from_seed(10).verifying_key());
    let sender2 = MemberId::from(&sk_from_seed(11).verifying_key());
    let recip1 = sk_from_seed(20).verifying_key();
    let recip2 = sk_from_seed(21).verifying_key();
    let owner1 = sk_from_seed(30).verifying_key();
    let owner2 = sk_from_seed(31).verifying_key();
    let ph_a = [0u8; 32];
    let mut ph_b = [0u8; 32];
    ph_b[0] = 1;

    let base = build_signed_payload_bytes(sender1, &recip1, &owner1, &ph_a, 100, b"abc");
    let diff_sender = build_signed_payload_bytes(sender2, &recip1, &owner1, &ph_a, 100, b"abc");
    let diff_recip = build_signed_payload_bytes(sender1, &recip2, &owner1, &ph_a, 100, b"abc");
    let diff_owner = build_signed_payload_bytes(sender1, &recip1, &owner2, &ph_a, 100, b"abc");
    let diff_proof_hash = build_signed_payload_bytes(sender1, &recip1, &owner1, &ph_b, 100, b"abc");
    let diff_ts = build_signed_payload_bytes(sender1, &recip1, &owner1, &ph_a, 101, b"abc");
    let diff_ct = build_signed_payload_bytes(sender1, &recip1, &owner1, &ph_a, 100, b"abd");

    let all = [
        &base,
        &diff_sender,
        &diff_recip,
        &diff_owner,
        &diff_proof_hash,
        &diff_ts,
        &diff_ct,
    ];
    for i in 0..all.len() {
        for j in (i + 1)..all.len() {
            assert_ne!(
                all[i], all[j],
                "signed-payload byte axes #{} and #{} must differ",
                i, j
            );
        }
    }
}

// ===========================================================================
// summarise / get_state_delta
// ===========================================================================

#[test]
fn summarize_then_delta_yields_missing_messages() {
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let m1 = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"a".to_vec(),
        proof.clone(),
    );
    let m2 = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW + 1,
        b"b".to_vec(),
        proof,
    );
    let server_inbox = Inbox {
        messages: vec![m1.clone(), m2.clone()],
        ..Default::default()
    };

    let client_inbox = Inbox {
        messages: vec![m1],
        ..Default::default()
    };
    let summary_bytes = Contract::summarize_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&client_inbox)),
    )
    .unwrap();

    let delta_bytes = Contract::get_state_delta(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&server_inbox)),
        summary_bytes,
    )
    .unwrap();

    let delta: InboxDelta = from_reader(delta_bytes.as_ref()).unwrap();
    match delta {
        InboxDelta::AppendMessages(msgs) => {
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].signature, m2.signature);
        }
        other => panic!("expected AppendMessages, got {other:?}"),
    }
}

// ===========================================================================
// v5: cycle detection, mid-chain owner-vk / self-invitation rejection,
// max-depth happy-path, replay dedup, full-state asymmetry, proof
// substitution
// ===========================================================================

#[test]
fn chain_with_cycle_rejected() {
    // Construct a depth-2 cycle A→B→A→...→root. The per-link signature
    // and invited_by checks all pass up to MAX_CHAIN_DEPTH; only the
    // explicit cycle-detection HashSet catches the duplicate
    // MemberId. Mirrors River's
    // MembersV1::get_invite_chain_with_lookup behaviour.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let a_sk = sk_from_seed(2);
    let b_sk = sk_from_seed(3);
    let recipient_sk = sk_from_seed(99);

    // Build A invited by B and B invited by A. Both are also signed
    // such that their signatures are valid against each other. Then
    // place the root B-invited-by-owner so the chain ostensibly
    // terminates at the owner — but with a duplicate of B (and A)
    // in the middle.
    //
    //   sender_authorized = A_sk invited by B_sk
    //   invitation_chain[0] = B_sk invited by A_sk      <- cycle starts
    //   invitation_chain[1] = A_sk invited by B_sk      <- cycle continues
    //   invitation_chain[2] = B_sk invited by owner     <- root
    //
    // Per-link signatures are valid; invited_by chains line up; only
    // the visited-set check rejects this.
    let a_by_b = auth_member(&a_sk, &b_sk, &owner_vk);
    let b_by_a = auth_member(&b_sk, &a_sk, &owner_vk);
    let a_by_b_again = auth_member(&a_sk, &b_sk, &owner_vk);
    let b_by_owner = auth_member(&b_sk, &owner_sk, &owner_vk);

    let proof = MembershipProof {
        sender_authorized: a_by_b,
        invitation_chain: vec![b_by_a, a_by_b_again, b_by_owner],
    };
    // Confirm the chain depth is well within MAX_CHAIN_DEPTH so it's
    // really the cycle gate that fires.
    assert!(proof.invitation_chain.len() < MAX_CHAIN_DEPTH);

    let msg = sign_inbox_message_member(
        &a_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"cycle".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(
        res.is_err(),
        "cyclic membership chain must be rejected: {res:?}"
    );
}

#[test]
fn chain_at_max_depth_accepted_in_unit_suite() {
    // A chain of EXACTLY MAX_CHAIN_DEPTH levels (i.e.
    // sender + (MAX_CHAIN_DEPTH - 1) invitation_chain entries) must
    // be accepted. Boundary case for the depth gate.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let recipient_sk = sk_from_seed(99);

    let total = MAX_CHAIN_DEPTH;
    let keys: Vec<SigningKey> = (0..total).map(|i| sk_from_seed(10 + i as u8)).collect();

    let mut chain_auths: Vec<AuthorizedMember> = Vec::new();
    chain_auths.push(auth_member(&keys[0], &owner_sk, &owner_vk));
    for i in 1..total {
        chain_auths.push(auth_member(&keys[i], &keys[i - 1], &owner_vk));
    }
    let sender_auth = chain_auths.pop().unwrap();
    chain_auths.reverse();

    let proof = MembershipProof {
        sender_authorized: sender_auth,
        invitation_chain: chain_auths,
    };
    assert_eq!(1 + proof.invitation_chain.len(), MAX_CHAIN_DEPTH);

    let sender_sk = &keys[total - 1];
    let msg = sign_inbox_message_member(
        sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"max-depth".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    )
    .unwrap();
    assert_eq!(res, ValidateResult::Valid);
}

#[test]
fn chain_with_mid_chain_owner_vk_rejected() {
    // A mid-chain link (not sender, not root) carries the owner's
    // member_vk. Catches an attempt to splice the owner into the
    // middle of a chain. The chain.rs check applies to ALL links,
    // not just sender_authorized.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let a_sk = sk_from_seed(2);
    let sender_sk = sk_from_seed(3);
    let recipient_sk = sk_from_seed(99);

    // Build a normal chain owner -> a -> sender, then splice a
    // forged middle link whose member_vk == owner_vk. The forged
    // link claims to be invited by `a`, and "invites" the sender.
    //
    // Chain layout:
    //   sender_authorized   = sender_sk invited by FORGED (member_vk = owner)
    //   invitation_chain[0] = FORGED (member_vk = owner) invited by a
    //   invitation_chain[1] = a invited by owner
    //
    // We cannot use AuthorizedMember::new because member.invited_by
    // doesn't match the signing key for FORGED (it's signed by `a`,
    // but its member_vk is owner_vk). Instead construct manually
    // with a hand-rolled signature.
    use ed25519_dalek::Signer;
    let a_auth = auth_member(&a_sk, &owner_sk, &owner_vk);

    // Forged middle: member_vk = owner_vk; invited_by = a; signed by a.
    let forged_middle_member = Member {
        owner_member_id: MemberId::from(&owner_vk),
        invited_by: MemberId::from(&a_sk.verifying_key()),
        member_vk: owner_vk,
    };
    let mut forged_middle_payload = Vec::new();
    into_writer(&forged_middle_member, &mut forged_middle_payload).unwrap();
    let forged_middle_sig = a_sk.sign(&forged_middle_payload);
    let forged_middle = AuthorizedMember::with_signature(forged_middle_member, forged_middle_sig);

    // Sender invited by FORGED (so we need a key for FORGED to sign
    // the sender's AuthorizedMember; but FORGED's member_vk is owner,
    // so the sender's signature must verify against owner's vk). We
    // sign with owner_sk.
    let sender_member = Member {
        owner_member_id: MemberId::from(&owner_vk),
        invited_by: forged_middle.member.id(),
        member_vk: sender_sk.verifying_key(),
    };
    let mut sender_payload = Vec::new();
    into_writer(&sender_member, &mut sender_payload).unwrap();
    let sender_sig = owner_sk.sign(&sender_payload);
    let sender_auth = AuthorizedMember::with_signature(sender_member, sender_sig);

    let proof = MembershipProof {
        sender_authorized: sender_auth,
        invitation_chain: vec![forged_middle, a_auth],
    };
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"x".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(
        res.is_err(),
        "mid-chain link with owner's member_vk must be rejected: {res:?}"
    );
}

#[test]
fn chain_with_mid_chain_self_invitation_rejected() {
    // A mid-chain link (not sender, not root) has
    // member.invited_by == member.id(). The chain.rs self-invitation
    // check applies to ALL links.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let a_sk = sk_from_seed(2);
    let mid_sk = sk_from_seed(3);
    let sender_sk = sk_from_seed(4);
    let recipient_sk = sk_from_seed(99);

    // Build chain: owner -> a -> mid -> sender.
    // Tamper `mid`'s `invited_by` to equal mid's own MemberId.
    let a_auth = auth_member(&a_sk, &owner_sk, &owner_vk);
    let mid_auth = auth_member(&mid_sk, &a_sk, &owner_vk);
    let sender_auth = auth_member(&sender_sk, &mid_sk, &owner_vk);

    let mut tampered_mid_member = mid_auth.member.clone();
    tampered_mid_member.invited_by = mid_auth.member.id();
    let tampered_mid = AuthorizedMember::with_signature(tampered_mid_member, mid_auth.signature);

    let proof = MembershipProof {
        sender_authorized: sender_auth,
        invitation_chain: vec![tampered_mid, a_auth],
    };
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"x".to_vec(),
        proof,
    );
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    assert!(
        res.is_err(),
        "mid-chain self-invitation must be rejected: {res:?}"
    );
}

#[test]
fn replay_into_same_inbox_silently_deduped() {
    // Submitting the same valid message twice into the same inbox is
    // idempotent — the second submission silently dedups (does NOT
    // error). The contract is still expected to produce a state with
    // exactly one copy of the message.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let proof = proof_directly_invited_by_owner(&sender_sk, &owner_sk);
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"hello".to_vec(),
        proof,
    );

    // First submission.
    let delta = InboxDelta::AppendMessages(vec![msg.clone()]);
    let res1 = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(Vec::<u8>::new()),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta)))],
    )
    .unwrap();
    let after1: Inbox = from_reader(res1.new_state.as_ref().unwrap().as_ref()).unwrap();
    assert_eq!(after1.messages.len(), 1);

    // Second submission of the SAME message — must not error and
    // must not duplicate.
    let delta2 = InboxDelta::AppendMessages(vec![msg]);
    let res2 = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&after1)),
        vec![UpdateData::Delta(StateDelta::from(ser(&delta2)))],
    )
    .expect("replay of the same message must be silently deduped, not errored");
    let after2: Inbox = from_reader(res2.new_state.as_ref().unwrap().as_ref()).unwrap();
    assert_eq!(
        after2.messages.len(),
        1,
        "duplicate message must be deduped"
    );
}

#[test]
fn apply_full_state_rejects_stale_recipient_state() {
    // Symmetry guarantee: a malicious peer must not be able to mask
    // an old-version replay by sending it as `UpdateData::State`
    // instead of as a delta. v4 silently ignored stale recipient_state
    // in apply_full_state; v5 rejects it just like apply_delta.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let recipient_sk = sk_from_seed(99);

    let v5_state = sign_recipient_state(
        &recipient_sk,
        RecipientState {
            version: 5,
            purged: Vec::new(),
        },
    );
    let inbox = Inbox {
        messages: Vec::new(),
        recipient_state: Some(v5_state),
    };

    // Stale full-state push: version 3 < current 5.
    let v3_state = sign_recipient_state(
        &recipient_sk,
        RecipientState {
            version: 3,
            purged: vec![42],
        },
    );
    let stale_full = Inbox {
        messages: Vec::new(),
        recipient_state: Some(v3_state),
    };

    let res = Contract::update_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        vec![UpdateData::State(State::from(ser(&stale_full)))],
    );
    assert!(
        res.is_err(),
        "apply_full_state must reject stale recipient_state symmetrically with apply_delta: {res:?}"
    );
}

#[test]
fn proof_substitution_breaks_signature() {
    // The proof_hash in the signed payload commits the signature to
    // a specific member_proof value. Substituting a different valid
    // proof for the same sender breaks the signature.
    let _g = ClockGuard::pin(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let intermediary_sk = sk_from_seed(3);
    let recipient_sk = sk_from_seed(99);

    // Two distinct valid proofs for the same sender:
    //   proof_v1 = sender invited directly by owner
    //   proof_v2 = owner -> intermediary -> sender (different chain)
    // Both produce a valid sender_authorized that resolves to the
    // same `sender_sk.verifying_key()`, so the signature would
    // verify if the proof weren't bound to it.
    let proof_v1 = proof_directly_invited_by_owner(&sender_sk, &owner_sk);

    let intermediary_auth = auth_member(&intermediary_sk, &owner_sk, &owner_vk);
    let sender_via_intermediary = auth_member(&sender_sk, &intermediary_sk, &owner_vk);
    let proof_v2 = MembershipProof {
        sender_authorized: sender_via_intermediary,
        invitation_chain: vec![intermediary_auth],
    };

    // Sign the message with proof_v1's hash committed.
    let msg_v1 = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"hi".to_vec(),
        proof_v1,
    );

    // Sanity: as-signed, verification succeeds.
    Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&Inbox {
            messages: vec![msg_v1.clone()],
            ..Default::default()
        })),
        RelatedContracts::default(),
    )
    .expect("as-signed message must validate");

    // Now substitute proof_v2 onto the v1-signed message. The
    // signature was computed over v1's proof_hash; the contract
    // recomputes proof_hash from the message's current member_proof
    // (= v2) and the verification fails.
    let mut tampered = msg_v1;
    tampered.member_proof = Some(proof_v2);

    let inbox = Inbox {
        messages: vec![tampered],
        ..Default::default()
    };
    let res = Contract::validate_state(
        Parameters::from(mk_inbox_params_bytes(&recipient_sk, &owner_vk)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    );
    // Pin the failure to the signature path specifically; a different
    // error (e.g. chain-validation) here would mean the proof_hash
    // binding isn't actually what's catching the substitution.
    let err = res.expect_err("proof substitution must invalidate the signature");
    let err_str = format!("{err:?}");
    assert!(
        err_str.contains("invalid inbox-message signature"),
        "expected signature-failure error, got: {err_str}"
    );
}
