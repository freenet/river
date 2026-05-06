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
    let payload = build_signed_payload_bytes(
        m3_id,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"x",
    );
    let signature = m2_sk.sign(&payload);
    let msg = InboxMessage {
        sender: m3_id,
        timestamp: FIXED_NOW,
        ciphertext: b"x".to_vec(),
        signature,
        member_proof: Some(proof_m3),
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
    // An attacker with a totally unrelated key fabricates an
    // AuthorizedMember chain claiming to be a legitimate member's
    // MemberId. The chain's signatures won't verify against the
    // owner's vk.
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
    // Captured 2026-04-27 against ciborium 0.2.x with the v4 schema
    // (sender: MemberId, recipient_state: Option<...>, member_proof:
    // Option<MembershipProof>, signed payload binding
    // sender+recipient+room_owner+timestamp+ciphertext). Any change
    // to this string requires a deliberate state-format migration.
    const EXPECTED_HEX: &str = "a2686d6573736167657381a56673656e6465723b6d9aeebb4641956f6974696d657374616d701a6553f1006a636970686572746578748218681869697369676e6174757265984018fe186a0618fa187518fb1822189e1857182218ec18cb18c8188b1823186518cb188918bf18da1318a418b118c9186d181e185e18e418a818701860185c186118c50d1891182518831886183618b9188a18311848182c1846181f18471884183218c4187e1853189208188318c418df18b809186e18f6189d016c6d656d6265725f70726f6f66a27173656e6465725f617574686f72697a6564a2666d656d626572a36f6f776e65725f6d656d6265725f69641b20340a250a45d4606a696e76697465645f62791b20340a250a45d460696d656d6265725f766b58208139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394697369676e6174757265984018e918c318c218621868187918c51018f218a6187318a118770418cc188c18d2183718a018d318dc183e185a18d3182218ec186118d7188718f9188318dd189318cf18e11826189d18ce18371819185c01186d18451883182c18e118cb18de182b18b51418e018d6185a183d186018e1181918b8183e18ba18530770696e7669746174696f6e5f636861696e806f726563697069656e745f7374617465f6";
    assert_eq!(hex::encode(&bytes), EXPECTED_HEX, "wire format drift");

    // Round-trip too — catches deserialiser asymmetry.
    let parsed: Inbox = from_reader(bytes.as_slice()).unwrap();
    assert_eq!(parsed, inbox);
}

#[test]
fn signed_payload_is_canonical_per_axis() {
    // Each component of (sender, recipient_vk, room_owner_vk,
    // timestamp, ciphertext) must alter the signed bytes.
    let sender1 = MemberId::from(&sk_from_seed(10).verifying_key());
    let sender2 = MemberId::from(&sk_from_seed(11).verifying_key());
    let recip1 = sk_from_seed(20).verifying_key();
    let recip2 = sk_from_seed(21).verifying_key();
    let owner1 = sk_from_seed(30).verifying_key();
    let owner2 = sk_from_seed(31).verifying_key();

    let base = build_signed_payload_bytes(sender1, &recip1, &owner1, 100, b"abc");
    let diff_sender = build_signed_payload_bytes(sender2, &recip1, &owner1, 100, b"abc");
    let diff_recip = build_signed_payload_bytes(sender1, &recip2, &owner1, 100, b"abc");
    let diff_owner = build_signed_payload_bytes(sender1, &recip1, &owner2, 100, b"abc");
    let diff_ts = build_signed_payload_bytes(sender1, &recip1, &owner1, 101, b"abc");
    let diff_ct = build_signed_payload_bytes(sender1, &recip1, &owner1, 100, b"abd");

    let all = [
        &base,
        &diff_sender,
        &diff_recip,
        &diff_owner,
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
