//! Sending a direct message: the checks before it, how it is composed and signed, and the
//! update that carries it.
//!
//! It follows the web UI's thread composer (`ui/src/components/direct_messages/
//! dm_thread_modal.rs`, `do_send`) and sends the same bytes:
//!
//! - **The body** is the text as raw UTF-8, with no `0x80` structured-body prefix (that form is
//!   only for an invitation). `compose_direct_message` seals it to the recipient's key and signs
//!   it with the room's `self_sk`, the one signing path there is for a direct message; nothing
//!   here signs anything itself. The text is sent as written, as the web UI sends the draft
//!   (blank text is refused; the cap is on the bytes sent).
//! - **The recipient** is named by River's full member id and found among the room's owner and
//!   current members by that id. Never by the id's `Display` label (40 bits, which a member can
//!   grind to match another's).
//! - **The update** carries the message in `direct_messages.new_messages`, with the rejoin
//!   `members` and `member_info` delta ([`super::outbox::rejoin`]) when the room pruned the
//!   sender for inactivity. The contract silently drops a message whose sender is not a member,
//!   and applies the rejoin first (members come before direct messages in the state), so the two
//!   go together.
//! - **The landing check.** Applying the update to a copy of the room can succeed and still not
//!   keep the message (it drops the message of a sender or recipient it does not list, and the
//!   cleanup after an apply can sweep it). A send is queued only if its signature is in the
//!   copy afterwards ([`landed`]); otherwise the user is told and keeps their text.
//!
//! **The sent text is saved to the chat delegate** ([`super::dm_save`]). A sent DM is sealed to
//! its recipient, so the sender can read it back only from the plaintext kept for it; it is kept
//! in memory while the message is queued and written to `outbound_dms` as soon as it is, so after
//! a restart your own sent messages still read as text, here and in the web UI. If that save
//! fails the text stays in memory and the save is tried again later; until it works, a restart
//! loses it. A message still waiting in the outbox when the app quits is lost too (the outbox is
//! not saved), as a room message is; its saved text stays in the store, unread.
//!
//! The message is composed once. ECIES seals with fresh randomness, so composing the same text
//! again gives a different message, and the node may already hold the first: a resend is always
//! the same signed message.
//!
//! Nothing here logs, and no error text carries the message, a key or the ciphertext.

use std::sync::Arc;

use ed25519_dalek::VerifyingKey;
use river_core::room_state::ChatRoomStateV1Delta;
use river_core::room_state::direct_messages::{
    AuthorizedDirectMessage, DirectMessagesDelta, MAX_DM_CIPHERTEXT_BYTES, compose_direct_message,
};
use river_core::room_state::member::MemberId;

use super::dm::{MAX_SHOWN_SECS, token_hex};
use super::outbox::{Blocked, DmRow, participation, rejoin};
use super::room::LiveRoom;
use crate::chat::dm_model::{DmBody, DmMessage, DmThread, Placeholder};
use crate::chat::model::Pending;

/// The most bytes of text a direct message carries, as the web UI caps it
/// (`DM_BODY_BYTE_CAP`): the contract's envelope cap less room for the encryption's overhead.
pub const BODY_CAP: usize = MAX_DM_CIPHERTEXT_BYTES - 256;

/// What the sender is told when the room did not keep the message.
pub const NOT_KEPT: &str = "The room didn't keep this message. You or the person you wrote to may \
     no longer be listed as a member of the room. Send a message in the room first, then send \
     this again.";

/// Why a direct message was refused before anything was queued. The words never quote the
/// message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The user's own state in the room forbids sending: no key on this computer, banned, not a
    /// member, or a private room whose key has not arrived. The words are
    /// [`Blocked::reason`].
    Blocked(Blocked),
    /// Nothing but whitespace.
    Empty,
    TooLong {
        bytes: usize,
    },
    /// The recipient is the user.
    SelfMessage,
    /// The recipient is not the owner or a current member of the room.
    NotMember,
    /// The message could not be composed or signed.
    Compose(String),
    /// The room did not take the update.
    Refused(String),
    /// The update applied and the message is not in the room afterwards.
    NotKept,
    /// An invitation could not be made or sent ([`super::dm_invite`]); the words are fixed ones.
    Invite(String),
}

impl Refusal {
    /// What the composer says.
    pub fn text(&self) -> String {
        match self {
            Refusal::Blocked(b) => b.reason().to_owned(),
            Refusal::Empty => "Write something to send.".to_owned(),
            Refusal::TooLong { bytes } => too_long(*bytes),
            Refusal::SelfMessage => "You can't send a direct message to yourself.".to_owned(),
            Refusal::NotMember => {
                "That person is not a member of this room now, so they can't be sent a direct \
                 message."
                    .to_owned()
            }
            Refusal::Compose(why) => format!("The message could not be prepared: {why}"),
            Refusal::Refused(why) => why.clone(),
            Refusal::NotKept => NOT_KEPT.to_owned(),
            Refusal::Invite(why) => why.clone(),
        }
    }
}

/// The words for a message over [`BODY_CAP`].
pub fn too_long(bytes: usize) -> String {
    format!("Message too long: {bytes} bytes (the cap is {BODY_CAP} bytes).")
}

/// The recipient's key, if they are the owner or a current member of the room.
fn peer_vk(room: &LiveRoom, peer: MemberId) -> Option<VerifyingKey> {
    if peer == MemberId::from(&room.stored.owner_vk) {
        return Some(room.stored.owner_vk);
    }
    room.stored
        .room_state
        .members
        .members
        .iter()
        .find(|m| m.member.id() == peer)
        .map(|m| m.member.member_vk)
}

/// Whether `text` may be sent to `peer` in `room`, in the order the web UI checks: the user's
/// own standing, the text, then the recipient. Returns the recipient's key.
///
/// The web UI also refuses a sender who is pruned and holds nothing to re-add themselves with.
/// That case has no separate check here: [`participation`] already refuses a user who is not
/// listed and has no stored admission ([`Blocked::NotMember`]), and [`applied`] checks that the
/// room kept the message whatever the reason.
pub fn preflight(room: &LiveRoom, peer: MemberId, text: &str) -> Result<VerifyingKey, Refusal> {
    check(room, peer, text.len(), text.trim().is_empty())
}

/// [`preflight`] for a body of `len` bytes that is `empty` or not.
fn check(
    room: &LiveRoom,
    peer: MemberId,
    len: usize,
    empty: bool,
) -> Result<VerifyingKey, Refusal> {
    participation(room).map_err(Refusal::Blocked)?;
    // As the web UI: blank text is refused, and what is sent is the text as written, so the cap
    // is on its bytes as they are.
    if empty {
        return Err(Refusal::Empty);
    }
    if len > BODY_CAP {
        return Err(Refusal::TooLong { bytes: len });
    }
    let Some(sk) = &room.stored.self_sk else {
        return Err(Refusal::Blocked(Blocked::NoKey));
    };
    let me = MemberId::from(&sk.verifying_key());
    if me == peer {
        return Err(Refusal::SelfMessage);
    }
    let Some(peer_vk) = peer_vk(room, peer) else {
        return Err(Refusal::NotMember);
    };
    Ok(peer_vk)
}

/// The update that publishes `message` in `room`: the rejoin delta when the room pruned the
/// sender, and the message.
pub fn delta(room: &LiveRoom, message: &AuthorizedDirectMessage) -> ChatRoomStateV1Delta {
    let (members, member_info) = rejoin(room);
    ChatRoomStateV1Delta {
        members,
        member_info,
        direct_messages: Some(DirectMessagesDelta {
            new_messages: vec![message.clone()],
            advanced_purges: Vec::new(),
        }),
        ..Default::default()
    }
}

/// Whether `room` holds `message`: its signature is among the room's direct messages.
pub fn landed(room: &LiveRoom, message: &AuthorizedDirectMessage) -> bool {
    room.stored
        .room_state
        .direct_messages
        .messages
        .iter()
        .any(|m| m.sender_signature == message.sender_signature)
}

/// Whether sending `message` again can never work, whatever the room does next: by the typed
/// refusal, not by its words. Only [`Refusal::NotMember`] (the recipient is gone) is; the others
/// (a key not here yet, an update the contract refuses at this moment, a message not kept after a
/// race) can heal.
pub fn is_permanent(refusal: &Refusal) -> bool {
    matches!(refusal, Refusal::NotMember)
}

/// `room` with the update for `message` applied, or why the room would not keep it.
pub fn applied(room: &LiveRoom, message: &AuthorizedDirectMessage) -> Result<LiveRoom, Refusal> {
    // A recipient the room no longer lists is the one refusal that cannot heal: no later state
    // of this room takes a message to them (a member who left is re-invited as someone new).
    if peer_vk(room, message.message.recipient).is_none() {
        return Err(Refusal::NotMember);
    }
    let mut next = room.clone();
    next.apply_delta(delta(room, message))
        .map_err(|e| Refusal::Refused(format!("The room refused it: {e}")))?;
    if landed(&next, message) {
        Ok(next)
    } else {
        Err(Refusal::NotKept)
    }
}

/// Check, compose and sign a message of `text` to `peer`, and check that the room would keep it.
/// `now` is unix seconds. The checks are [`preflight`]'s.
pub fn prepare(
    room: &LiveRoom,
    peer: MemberId,
    text: &str,
    now: u64,
) -> Result<AuthorizedDirectMessage, Refusal> {
    let peer_vk = preflight(room, peer, text)?;
    seal(room, &peer_vk, text.as_bytes(), now)
}

/// [`prepare`] for a structured body already encoded as `body` (an invitation,
/// [`super::dm_invite`]): the same checks, with the cap on the encoded bytes.
pub fn prepare_body(
    room: &LiveRoom,
    peer: MemberId,
    body: &[u8],
    now: u64,
) -> Result<AuthorizedDirectMessage, Refusal> {
    let peer_vk = check(room, peer, body.len(), false)?;
    seal(room, &peer_vk, body, now)
}

/// Seal `body` to `peer_vk`, sign it with the room's key, and check the room would keep it.
fn seal(
    room: &LiveRoom,
    peer_vk: &VerifyingKey,
    body: &[u8],
    now: u64,
) -> Result<AuthorizedDirectMessage, Refusal> {
    let Some(sk) = &room.stored.self_sk else {
        return Err(Refusal::Blocked(Blocked::NoKey));
    };
    let message = compose_direct_message(sk, peer_vk, &room.stored.owner_vk, now, now, body)
        .map_err(Refusal::Compose)?;
    applied(room, &message)?;
    Ok(message)
}

/// `threads` (as [`super::dm::threads`] built them) with the direct messages still in the
/// outbox shown in them: a message not held by the room yet, or not sent, is a row of its thread
/// with its state ([`DmMessage::pending`]), and one the room holds already only gains the state.
/// A thread the user has no other messages in is made. A thread the outbox does not touch is
/// handed back as it was, so it is not copied.
pub fn with_pending(
    room: &LiveRoom,
    threads: &[Arc<DmThread>],
    rows: &[DmRow],
) -> Vec<Arc<DmThread>> {
    let mut out: Vec<Arc<DmThread>> = threads.to_vec();
    for row in rows {
        let key = token_hex(&row.token);
        let peer_key = super::dm::peer_key(row.recipient);
        let message = |pending: Pending| DmMessage {
            key: key.clone(),
            outgoing: true,
            timestamp: row.timestamp.min(MAX_SHOWN_SECS),
            body: match &row.text {
                Some(t) => super::dm::sent_body(t.as_str()),
                None => DmBody::Placeholder(Placeholder::CiphertextOnly),
            },
            pending: Some(pending),
        };
        match out.iter().position(|t| t.peer_key == peer_key) {
            Some(i) => {
                let thread = Arc::make_mut(&mut out[i]);
                if let Some(m) = thread.messages.iter_mut().find(|m| m.key == key) {
                    m.pending = Some(row.pending.clone());
                    continue;
                }
                let m = message(row.pending.clone());
                thread.last_ts = thread.last_ts.max(m.timestamp);
                thread.messages.push(m);
                // As `dm::threads` orders them: by time, and within one second by purge token, so
                // a row keeps its place when the room holds it.
                thread
                    .messages
                    .sort_by_key(|m| (m.timestamp, m.key.clone()));
            }
            None => {
                let m = message(row.pending.clone());
                out.push(Arc::new(DmThread {
                    room: room.key(),
                    room_name: room.name(),
                    peer_key,
                    peer: row.recipient,
                    peer_name: room.member_name(row.recipient),
                    peer_is_member: peer_vk(room, row.recipient).is_some(),
                    last_ts: m.timestamp,
                    last_inbound_ts: 0,
                    last_inbound_signed: 0,
                    messages: vec![m],
                }));
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "dm_send_tests.rs"]
mod tests;
