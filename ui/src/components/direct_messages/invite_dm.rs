//! Shared "send an invitation card as a DM" machinery.
//!
//! Two surfaces produce an invitation-card DM, from opposite starting
//! points:
//!
//! * [`super::invite_via_dm_picker_modal`] — we know the PERSON (a member
//!   of the room being viewed) and the user picks which of their OTHER
//!   rooms to invite them to (#252).
//! * [`super::invite_contact_picker_modal`] — we know the ROOM (the one
//!   being viewed) and the user picks WHICH person they already share a
//!   room with to send it to (#566).
//!
//! Both reduce to the same operation: sign a fresh membership claim
//! against the *target* room, wrap it in a
//! [`DirectMessageBody::Invite`], and DM it to a peer inside a *carrier*
//! room the local user shares with them. That single operation lives here
//! so the two pickers cannot drift — an invitation the recipient can't
//! accept is indistinguishable from a working one until they try.

use crate::components::direct_messages::{send_structured_dm, SendDmOutcome};
use crate::components::members::{collect_invitation_secrets, Invitation};
use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::room_state::dm_body::{DirectMessageBody, InvitePayload};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};

/// Mint an invitation for `target_room`, then deliver it to `peer` as a
/// structured `Invite` DM sent inside `carrier_room`.
///
/// `carrier_room` is the room whose DM channel carries the message — the
/// local user and `peer` must both be members of it. `target_room` is the
/// room the invitation grants access to; it supplies the signing identity
/// (`self_sk`) and, for a private room, the secrets the invitee needs to
/// read it immediately on join.
///
/// The two may not be the same room: inviting someone to a room they are
/// already in is a no-op, and the callers both exclude that case when
/// building their candidate lists.
///
/// Returns a user-facing error string on failure — every arm is safe to
/// render verbatim in a modal.
pub(super) async fn compose_and_send_invite_dm(
    carrier_room: VerifyingKey,
    peer: MemberId,
    target_room: crate::room_data::RoomData,
    personal_message: Option<String>,
) -> Result<(), String> {
    // A fresh identity per invitation — this is the bearer credential the
    // recipient becomes. Never reused across invitations (two people
    // holding one identity is exactly the failure the link flow keeps
    // producing in the wild).
    let invitee_signing_key = SigningKey::generate(&mut rand::thread_rng());
    let member = Member {
        owner_member_id: target_room.owner_vk.into(),
        invited_by: target_room.self_sk.verifying_key().into(),
        member_vk: invitee_signing_key.verifying_key(),
    };

    // Sign the member-claim via the delegate-backed signing path. Same
    // semantics as the link flow in `invite_member_modal.rs`.
    let mut member_bytes = Vec::new();
    if ciborium::ser::into_writer(&member, &mut member_bytes).is_err() {
        return Err("Couldn't serialize membership claim. Try again.".into());
    }
    let signature = crate::signing::sign_member_with_fallback(
        target_room.room_key(),
        member_bytes,
        &target_room.self_sk,
    )
    .await;
    let authorized = AuthorizedMember::with_signature(member, signature);

    // For a private room, embed the room secrets the inviter holds so the
    // invitee can decrypt the room immediately on join. Empty for a public
    // room or an empty secrets map.
    let room_secrets = if target_room.is_private() {
        collect_invitation_secrets(&target_room.secrets)
    } else {
        Vec::new()
    };
    let invitation = Invitation {
        room: target_room.owner_vk,
        invitee_signing_key,
        invitee: authorized,
        room_secrets,
    };

    // Encode the Invitation as CBOR — same bytes the URL form base58-
    // encodes. The recipient decodes these bytes back to `Invitation`.
    let mut invitation_payload = Vec::new();
    ciborium::ser::into_writer(&invitation, &mut invitation_payload)
        .map_err(|e| format!("Couldn't encode invitation: {}", e))?;

    let body = DirectMessageBody::Invite(Box::new(InvitePayload {
        room_owner_vk: target_room.owner_vk,
        invitation_payload,
        personal_message,
    }));

    match send_structured_dm(carrier_room, peer, body).await {
        SendDmOutcome::Sent => Ok(()),
        SendDmOutcome::RoomGone => Err("The room you're DM'ing in is no longer loaded.".into()),
        SendDmOutcome::RecipientNotMember => {
            Err("The recipient is no longer a member of this room.".into())
        }
        SendDmOutcome::SelfDm => Err("Cannot send a DM to yourself.".into()),
        SendDmOutcome::SenderMissingRejoin => Err(
            "You're not currently in this room's member list and no rejoin \
             credentials are stored locally. Reload the room or re-accept your \
             invitation before sending an invite DM."
                .into(),
        ),
        SendDmOutcome::BodyTooLargeOrEncodeFailed(e) => Err(format!(
            "Couldn't send invite — body too large or encode failed: {}",
            e
        )),
        SendDmOutcome::DeltaFailed(e) => Err(format!(
            "Couldn't send invite — local apply_delta failed: {}",
            e
        )),
        SendDmOutcome::SilentDrop => Err(
            "Invite couldn't be added to the room (your member entry may be \
             missing). Try posting a message in the room first, then retry."
                .into(),
        ),
    }
}
