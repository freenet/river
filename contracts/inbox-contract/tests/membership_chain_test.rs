//! End-to-end tests for the v4 membership-chain mechanism.
//!
//! v4 dropped the related-contracts integration test (the inbox no
//! longer fetches room state via the host's two-pass protocol). This
//! file replaces it with multi-level chain scenarios that exercise
//! `chain::verify_membership_proof` from the contract-interface
//! boundary.

use ciborium::ser::into_writer;
use ed25519_dalek::SigningKey;
use freenet_stdlib::prelude::*;
use inbox_contract::{
    chain, set_clock_override_for_tests, sign_inbox_message_member, sign_inbox_message_owner,
    Contract, Inbox, InboxParams, MembershipProof, MAX_CHAIN_DEPTH,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};

const FIXED_NOW: u64 = 1_700_000_000;

fn sk_from_seed(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn ser<T: serde::Serialize>(t: &T) -> Vec<u8> {
    let mut out = Vec::new();
    into_writer(t, &mut out).unwrap();
    out
}

fn auth_member(
    member_sk: &SigningKey,
    inviter_sk: &SigningKey,
    owner_vk: &ed25519_dalek::VerifyingKey,
) -> AuthorizedMember {
    let member = Member {
        owner_member_id: MemberId::from(owner_vk),
        invited_by: MemberId::from(&inviter_sk.verifying_key()),
        member_vk: member_sk.verifying_key(),
    };
    AuthorizedMember::new(member, inviter_sk)
}

fn pin_clock(ts: u64) {
    set_clock_override_for_tests(Some(ts));
}

fn unpin_clock() {
    set_clock_override_for_tests(None);
}

/// Helper: build an inbox-state-bytes containing one message and run
/// `validate_state` against it.
fn validate_one_message(
    msg: inbox_contract::InboxMessage,
    recipient_sk: &SigningKey,
    owner_vk: &ed25519_dalek::VerifyingKey,
) -> Result<ValidateResult, ContractError> {
    let inbox = Inbox {
        messages: vec![msg],
        ..Default::default()
    };
    let params = InboxParams {
        recipient_vk: recipient_sk.verifying_key(),
        room_owner_vk: *owner_vk,
    };
    Contract::validate_state(
        Parameters::from(ser(&params)),
        State::from(ser(&inbox)),
        RelatedContracts::default(),
    )
}

#[test]
fn end_to_end_owner_send() {
    pin_clock(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let recipient_sk = sk_from_seed(99);
    let msg = sign_inbox_message_owner(
        &owner_sk,
        &recipient_sk.verifying_key(),
        FIXED_NOW,
        b"hello from owner".to_vec(),
    );
    let res = validate_one_message(msg, &recipient_sk, &owner_sk.verifying_key()).unwrap();
    assert_eq!(res, ValidateResult::Valid);
    unpin_clock();
}

#[test]
fn end_to_end_direct_invite() {
    pin_clock(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let sender_sk = sk_from_seed(2);
    let recipient_sk = sk_from_seed(99);

    let proof = MembershipProof {
        sender_authorized: auth_member(&sender_sk, &owner_sk, &owner_vk),
        invitation_chain: Vec::new(),
    };
    let msg = sign_inbox_message_member(
        &sender_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"direct".to_vec(),
        proof,
    );
    let res = validate_one_message(msg, &recipient_sk, &owner_vk).unwrap();
    assert_eq!(res, ValidateResult::Valid);
    unpin_clock();
}

#[test]
fn end_to_end_two_level_chain() {
    pin_clock(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let alice_sk = sk_from_seed(2); // invited by owner
    let bob_sk = sk_from_seed(3); // invited by alice
    let recipient_sk = sk_from_seed(99);

    let alice_auth = auth_member(&alice_sk, &owner_sk, &owner_vk);
    let bob_auth = auth_member(&bob_sk, &alice_sk, &owner_vk);
    let proof = MembershipProof {
        sender_authorized: bob_auth,
        invitation_chain: vec![alice_auth],
    };
    let msg = sign_inbox_message_member(
        &bob_sk,
        &recipient_sk.verifying_key(),
        &owner_vk,
        FIXED_NOW,
        b"hi".to_vec(),
        proof,
    );
    let res = validate_one_message(msg, &recipient_sk, &owner_vk).unwrap();
    assert_eq!(res, ValidateResult::Valid);
    unpin_clock();
}

#[test]
fn end_to_end_chain_at_max_depth() {
    pin_clock(FIXED_NOW);
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let recipient_sk = sk_from_seed(99);

    // Exactly MAX_CHAIN_DEPTH total links: keys[0] invited by owner,
    // keys[i] invited by keys[i-1]; the sender is keys[last].
    let total = MAX_CHAIN_DEPTH;
    let keys: Vec<SigningKey> = (0..total).map(|i| sk_from_seed(20 + i as u8)).collect();

    let mut chain_auths: Vec<AuthorizedMember> = Vec::new();
    chain_auths.push(auth_member(&keys[0], &owner_sk, &owner_vk));
    for i in 1..total {
        chain_auths.push(auth_member(&keys[i], &keys[i - 1], &owner_vk));
    }
    let sender_auth = chain_auths.pop().unwrap();
    chain_auths.reverse(); // closest-to-sender first

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
        b"deep".to_vec(),
        proof,
    );
    let res = validate_one_message(msg, &recipient_sk, &owner_vk).unwrap();
    assert_eq!(res, ValidateResult::Valid);
    unpin_clock();
}

#[test]
fn chain_module_returns_resolved_vk() {
    // Direct unit test of the chain module: the verifier returns the
    // sender's resolved VK.
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let alice_sk = sk_from_seed(2);
    let bob_sk = sk_from_seed(3);

    let alice_auth = auth_member(&alice_sk, &owner_sk, &owner_vk);
    let bob_auth = auth_member(&bob_sk, &alice_sk, &owner_vk);
    let proof = MembershipProof {
        sender_authorized: bob_auth,
        invitation_chain: vec![alice_auth],
    };

    let resolved = chain::verify_membership_proof(&proof, &owner_vk).unwrap();
    assert_eq!(resolved, bob_sk.verifying_key());
}

#[test]
fn chain_module_rejects_wrong_owner() {
    // Same chain as above, but verified against a different owner_vk.
    let owner_sk = sk_from_seed(1);
    let other_owner_sk = sk_from_seed(50);
    let alice_sk = sk_from_seed(2);
    let bob_sk = sk_from_seed(3);

    let alice_auth = auth_member(&alice_sk, &owner_sk, &owner_sk.verifying_key());
    let bob_auth = auth_member(&bob_sk, &alice_sk, &owner_sk.verifying_key());
    let proof = MembershipProof {
        sender_authorized: bob_auth,
        invitation_chain: vec![alice_auth],
    };

    let res = chain::verify_membership_proof(&proof, &other_owner_sk.verifying_key());
    assert!(
        res.is_err(),
        "chain rooted at owner-1 must NOT verify against owner-2"
    );
}

#[test]
fn chain_module_rejects_self_invitation() {
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let evil_sk = sk_from_seed(7);

    // Self-invited member: invited_by == own id, signed by self.
    let member = Member {
        owner_member_id: MemberId::from(&owner_vk),
        invited_by: MemberId::from(&evil_sk.verifying_key()),
        member_vk: evil_sk.verifying_key(),
    };
    let evil_auth = AuthorizedMember::new(member, &evil_sk);

    let proof = MembershipProof {
        sender_authorized: evil_auth,
        invitation_chain: Vec::new(),
    };
    let res = chain::verify_membership_proof(&proof, &owner_vk);
    assert!(res.is_err(), "self-invitation must be rejected");
}

#[test]
fn chain_module_rejects_owner_in_chain() {
    // Construct a chain where one of the AuthorizedMember entries
    // uses the owner's vk as its `member_vk`. River's room contract
    // forbids this (the owner is intentionally not in the members
    // list); the inbox enforces the same invariant.
    let owner_sk = sk_from_seed(1);
    let owner_vk = owner_sk.verifying_key();
    let some_inviter_sk = sk_from_seed(2);

    // Pretend the owner accepted an invitation from `some_inviter`.
    // (Wouldn't make physical sense in River; we just want to
    // exercise the invariant.)
    let owner_as_member = Member {
        owner_member_id: MemberId::from(&owner_vk),
        invited_by: MemberId::from(&some_inviter_sk.verifying_key()),
        member_vk: owner_vk,
    };
    let owner_authorized = AuthorizedMember::new(owner_as_member, &some_inviter_sk);

    let proof = MembershipProof {
        sender_authorized: owner_authorized,
        invitation_chain: Vec::new(),
    };
    let res = chain::verify_membership_proof(&proof, &owner_vk);
    assert!(
        res.is_err(),
        "AuthorizedMember whose member_vk == owner_vk must be rejected"
    );
}
