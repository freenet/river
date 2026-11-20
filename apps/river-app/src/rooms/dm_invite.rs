//! Invitations sent and accepted in a direct message.
//!
//! It follows the web UI's "Share an invite via DM" picker (`ui/src/components/
//! direct_messages/invite_via_dm_picker_modal.rs`, `drive_send`) and its invitation card
//! (`dm_thread_modal.rs`), and sends and reads the same bytes:
//!
//! - **Sending.** The user's own invitation to a second room (the same `Invitation` the invite
//!   link carries) is encoded as CBOR into an [`InvitePayload`], the payload into a
//!   [`DirectMessageBody::Invite`], and that with `encode_body` (`0x80` + CBOR) into the body of
//!   an ordinary direct message to the person, sent by the one send path there is
//!   ([`super::dm_send`], [`super::outbox::Outbox::send_invite_dm`]): composed and signed once,
//!   checked to land, queued, and resent byte for byte.
//! - **What is saved** for the sender's own row is only the web UI's summary string:
//!   `[Invitation]`, then the note ([`summary`]). Never the invitation.
//! - **Accepting.** The invitation is read again from the message the room holds, at the moment
//!   the user presses Accept ([`open`]), and is not kept anywhere in between: the thread's view
//!   model has no invitation (`rooms::dm` wipes it when it builds a row). The invitation is then
//!   joined through the same join work a pasted invite code uses.
//!
//! **An invitation is a bearer credential**: it carries the invitee's signing key and, for a
//! private room, the room's secrets. It exists in the clear only in the sealed envelope on the
//! wire and, briefly, in memory while sending or accepting. Here it is never logged, never
//! printed (nothing in this module derives or writes a `Debug` of one, and the words of every
//! refusal are fixed ones), never saved, and its byte buffers are wiped when done with.
//!
//! **What an invitation in a message proves.** Anything sealed to a user's key can be put in a
//! message by any member, and the envelope has no sender binding, so a member can re-sign a
//! ciphertext that was sealed to the user as their own message. The signed sender of the message
//! is the one fact the thread can name. The invitation says who invited the invitee, and the card
//! says only that the person sent the message, never that they made the invitation. The
//! invitation is also untrusted input: its size is bounded before it is decoded, it must name the
//! room the message names, and it must agree with itself ([`super::create::invitation_refusal`]).
//! Accepting is never automatic.

use ciborium::de::from_reader;
use ciborium::ser::into_writer;
use river_core::invitation::Invitation;
use river_core::room_state::direct_messages::{
    MAX_DM_CIPHERTEXT_BYTES, PurgeToken, open_direct_message,
};
use river_core::room_state::dm_body::{DirectMessageBody, InvitePayload, decode_body, encode_body};
use river_core::room_state::member::MemberId;
use zeroize::Zeroize;

use super::create;
use super::dm::INVITE_SENTINEL;
use super::dm_send::{self, Refusal};
use super::outbox::participation;
use super::room::{LiveRoom, is_hidden};

/// The most characters of a personal note the picker takes: the web UI's cap
/// (`PERSONAL_MESSAGE_CHAR_CAP`).
pub const MAX_NOTE_CHARS: usize = 4_000;

/// The most bytes of an encoded invitation that are decoded. A real one is a few hundred bytes,
/// and a private room's adds 36 for each secret version it has had; the message itself is at most
/// [`MAX_DM_CIPHERTEXT_BYTES`]. Anything longer is refused unread.
pub const MAX_INVITATION_BYTES: usize = 24 * 1024;

/// Why a note can't be sent, if it can't: too long, or holding characters that draw nothing or
/// reorder text (line breaks are allowed).
pub fn note_refusal(note: &str) -> Option<String> {
    if note.chars().count() > MAX_NOTE_CHARS {
        return Some(format!(
            "The note is too long (the limit is {MAX_NOTE_CHARS} characters)."
        ));
    }
    note.chars()
        .any(|c| c != '\n' && is_hidden(c))
        .then(|| "The note has characters that don't show. Type it again.".to_owned())
}

/// What is saved for an invitation the user sent, as the web UI saves it
/// (`summarise_body_for_outbound_cache`): the sentinel, then the note trimmed, or the sentinel
/// alone.
pub fn summary(note: Option<&str>) -> String {
    match note.map(str::trim) {
        Some(n) if !n.is_empty() => format!("{INVITE_SENTINEL} {n}"),
        _ => INVITE_SENTINEL.to_owned(),
    }
}

/// Whether `target` can be invited to from `carrier`: another room the user is a current member
/// of, as `participation` decides, and listed in it (an invitation is a membership the inviter
/// signs, and the room takes it only from a member it lists). The web UI offers every other room
/// and finds out when the invitee joins.
pub fn can_invite_to(carrier: &LiveRoom, target: &LiveRoom) -> Result<(), String> {
    if carrier.stored.owner_vk == target.stored.owner_vk {
        return Err("Pick a different room: the person is in this one already.".to_owned());
    }
    participation(target).map_err(|b| b.reason().to_owned())?;
    let Some(sk) = &target.stored.self_sk else {
        return Err("This computer doesn't hold your key for that room.".to_owned());
    };
    let me = sk.verifying_key();
    let listed = me == target.stored.owner_vk
        || target
            .stored
            .room_state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == me);
    if !listed {
        return Err(
            "You are not listed as a member of that room right now, so an invitation from you \
             would not be accepted. Send a message there first."
                .to_owned(),
        );
    }
    Ok(())
}

/// Make the user's invitation to `target`, and the direct message to `peer` in `carrier` that
/// carries it, for `note`. Returns the signed message and what to save for the sender's own row
/// ([`summary`]). Nothing is queued here; the checks are the text message's
/// ([`dm_send::preflight`]) and [`can_invite_to`]'s. `now` is unix seconds.
pub fn prepare(
    carrier: &LiveRoom,
    target: &LiveRoom,
    peer: MemberId,
    note: Option<&str>,
    now: u64,
) -> Result<
    (
        river_core::room_state::direct_messages::AuthorizedDirectMessage,
        String,
    ),
    Refusal,
> {
    if let Some(why) = note.and_then(note_refusal) {
        return Err(Refusal::Invite(why));
    }
    can_invite_to(carrier, target).map_err(Refusal::Invite)?;
    // The recipient and the sender are checked before an invitation is made, so a refusal leaves
    // no credential minted.
    dm_send::preflight(carrier, peer, "invitation")?;
    let invitation = create::mint_invitation(target).map_err(Refusal::Invite)?;
    let mut cbor = Vec::new();
    let encoded = into_writer(&invitation, &mut cbor);
    wipe_invitation(invitation);
    if encoded.is_err() {
        cbor.zeroize();
        return Err(Refusal::Invite(
            "The invitation could not be prepared.".to_owned(),
        ));
    }
    let personal_message = note
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned);
    let body = DirectMessageBody::Invite(Box::new(InvitePayload {
        room_owner_vk: target.stored.owner_vk,
        invitation_payload: cbor,
        personal_message,
    }));
    let bytes = encode_body(&body);
    wipe(body);
    let mut bytes =
        bytes.map_err(|_| Refusal::Invite("The message could not be prepared.".into()))?;
    let message = dm_send::prepare_body(carrier, peer, &bytes, now);
    bytes.zeroize();
    Ok((message?, summary(note)))
}

/// Wipe an invitation's secrets and drop it. Its signing key wipes itself when dropped; the room
/// secrets of a private room's invitation do not, so they are wiped here.
pub(super) fn wipe_invitation(mut invitation: Invitation) {
    for (_, secret) in &mut invitation.room_secrets {
        secret.zeroize();
    }
}

/// Wipe the invitation a body carries.
fn wipe(body: DirectMessageBody) {
    if let DirectMessageBody::Invite(mut p) = body {
        p.invitation_payload.zeroize();
    }
}

/// The invitation in the message `token` that `peer` sent the user in `room`, read from the room
/// as it is now. Refused, with fixed words, unless the message is the room's, was signed by
/// `peer`, is to the user, opens with the user's key, holds an invitation of a bounded size that
/// names the room the message names, and agrees with itself. The result is a bearer credential:
/// the caller joins with it and drops it.
pub fn open(room: &LiveRoom, peer: MemberId, token: &PurgeToken) -> Result<Invitation, String> {
    const GONE: &str = "That invitation is no longer in this conversation.";
    const DAMAGED: &str = "This invitation is damaged and can't be used.";
    let Some(sk) = &room.stored.self_sk else {
        return Err(
            "This computer doesn't hold your key for this room, so it can't open the \
                    invitation."
                .to_owned(),
        );
    };
    let me = MemberId::from(&sk.verifying_key());
    let owner = room.stored.owner_vk;
    let state = &room.stored.room_state;
    let Some(m) = state.direct_messages.messages.iter().find(|m| {
        m.purge_token() == *token && m.message.sender == peer && m.message.recipient == me
    }) else {
        return Err(GONE.to_owned());
    };
    let sender_vk = if peer == MemberId::from(&owner) {
        Some(owner)
    } else {
        state
            .members
            .members
            .iter()
            .find(|x| x.member.id() == peer)
            .map(|x| x.member.member_vk)
    };
    // As the thread does: a message is the sender's only when their signature says so.
    let verified = m.message.ciphertext.len() <= MAX_DM_CIPHERTEXT_BYTES
        && sender_vk.is_some_and(|vk| m.verify_signature(&vk, &owner).is_ok());
    if !verified {
        return Err(GONE.to_owned());
    }
    let Ok(mut opened) = open_direct_message(sk, m) else {
        return Err(DAMAGED.to_owned());
    };
    let decoded = decode_body(&opened);
    opened.zeroize();
    let payload = match decoded {
        Ok(DirectMessageBody::Invite(p)) => *p,
        _ => return Err("That message is not an invitation.".to_owned()),
    };
    let InvitePayload {
        room_owner_vk,
        mut invitation_payload,
        ..
    } = payload;
    let decoded = if invitation_payload.len() > MAX_INVITATION_BYTES {
        Err(())
    } else {
        from_reader::<Invitation, _>(&invitation_payload[..]).map_err(|_| ())
    };
    invitation_payload.zeroize();
    let Ok(invitation) = decoded else {
        return Err(DAMAGED.to_owned());
    };
    // The room the message names is the room the invitation is for: the card names the first and
    // joining uses the second.
    if invitation.room != room_owner_vk {
        wipe_invitation(invitation);
        return Err(
            "This invitation is damaged: it is for a different room than the message says."
                .to_owned(),
        );
    }
    match create::invitation_refusal(&invitation) {
        Some(why) => {
            wipe_invitation(invitation);
            Err(why)
        }
        None => Ok(invitation),
    }
}

#[cfg(test)]
#[path = "dm_invite_tests.rs"]
mod tests;
