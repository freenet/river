//! Membership-chain verification.
//!
//! A [`MembershipProof`](crate::MembershipProof) is a self-contained
//! certificate: the sender's [`AuthorizedMember`] plus a path of
//! [`AuthorizedMember`] entries leading back to (but not including)
//! the room owner. The contract verifies the chain locally against
//! `params.room_owner_vk`. This mirrors the chain-walking logic in
//! `river_core::room_state::member::MembersV1::get_invite_chain` —
//! the inbox just enforces the invariants on a self-contained slice
//! rather than walking a global members map.
//!
//! # Invariants enforced
//!
//! Given a proof
//! `(sender_authorized, invitation_chain[0..n])`:
//!
//! 1. Total chain depth `1 + n` is at most
//!    [`MAX_CHAIN_DEPTH`](crate::MAX_CHAIN_DEPTH).
//! 2. For each `i` in `0..n`, the (i+1)th link signs the i-th link's
//!    `Member` payload — i.e.
//!    `invitation_chain[i].verify_signature(&invitation_chain[i+1].member.member_vk)`
//!    succeeds. The sender's own `AuthorizedMember`
//!    (`sender_authorized`) is verified against `invitation_chain[0]`
//!    (or against `params.room_owner_vk` if `n == 0`).
//! 3. Chain links are consistent: each member's `invited_by` matches
//!    the next link's `MemberId`. The chain root's `invited_by` is
//!    the room owner's `MemberId`.
//! 4. The chain root signature verifies against
//!    `params.room_owner_vk`.
//! 5. No member in the chain may use the room owner's `member_vk`
//!    (the owner is intentionally not in the room's members list).
//! 6. No member may self-invite (`member.id() == member.invited_by`).
//!
//! Returns the resolved sender [`VerifyingKey`] (i.e.
//! `sender_authorized.member.member_vk`) on success, so the caller
//! can verify the signature on the [`InboxMessage`].

use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::{AuthorizedMember, MemberId};

use crate::{MembershipProof, MAX_CHAIN_DEPTH};

/// Verify a [`MembershipProof`] against a known room owner VK.
///
/// On success, returns the sender's resolved [`VerifyingKey`] (taken
/// from `proof.sender_authorized.member.member_vk`). The caller
/// uses this VK to verify the [`InboxMessage`] signature.
pub fn verify_membership_proof(
    proof: &MembershipProof,
    room_owner_vk: &VerifyingKey,
) -> Result<VerifyingKey, String> {
    // Total chain depth.
    let depth = 1 + proof.invitation_chain.len();
    if depth > MAX_CHAIN_DEPTH {
        return Err(format!(
            "membership chain has depth {}, exceeds MAX_CHAIN_DEPTH ({})",
            depth, MAX_CHAIN_DEPTH
        ));
    }

    let owner_member_id = MemberId::from(room_owner_vk);

    // Per-member self-consistency: no member may use the owner's vk
    // and no member may self-invite. (River's MembersV1::verify
    // enforces the same invariants on a global members list; we
    // enforce them per-link here.)
    let all_links: Vec<&AuthorizedMember> = std::iter::once(&proof.sender_authorized)
        .chain(proof.invitation_chain.iter())
        .collect();
    for link in &all_links {
        if &link.member.member_vk == room_owner_vk {
            return Err("member cannot have the same verifying key as the room owner".to_string());
        }
        if link.member.invited_by == link.member.id() {
            return Err(format!(
                "self-invitation detected for member {:?}",
                link.member.id()
            ));
        }
    }

    // Walk from the sender up the chain, verifying each link's
    // signature against the next link's vk. The "next link" for the
    // last member is the room owner itself.
    //
    // Chain layout reminder:
    //   sender_authorized        <- the sender's own AuthorizedMember
    //   invitation_chain[0]      <- inviter of the sender
    //   invitation_chain[1]      <- inviter of invitation_chain[0]
    //   ...
    //   invitation_chain[n-1]    <- the chain root; invited_by == owner
    let chain_len = proof.invitation_chain.len();
    for i in 0..=chain_len {
        // `current` is the link being verified; `inviter_vk_or_owner`
        // is what its signature must verify against.
        let current = if i == 0 {
            &proof.sender_authorized
        } else {
            &proof.invitation_chain[i - 1]
        };

        // Determine the expected inviter id and the vk to verify
        // against.
        let (expected_inviter_id, inviter_vk) = if i == chain_len {
            // `current` is the chain root — it must have been invited
            // by the room owner.
            (owner_member_id, *room_owner_vk)
        } else {
            let inviter = &proof.invitation_chain[i];
            (inviter.member.id(), inviter.member.member_vk)
        };

        if current.member.invited_by != expected_inviter_id {
            return Err(format!(
                "chain link broken at depth {}: member.invited_by ({:?}) does not match next link's id ({:?})",
                i, current.member.invited_by, expected_inviter_id
            ));
        }

        current
            .verify_signature(&inviter_vk)
            .map_err(|e| format!("invalid signature at chain depth {}: {}", i, e))?;
    }

    Ok(proof.sender_authorized.member.member_vk)
}
