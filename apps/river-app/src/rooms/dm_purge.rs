//! Deleting the direct messages someone sent you: the checks before it, the signed purge
//! envelope and the update that carries it.
//!
//! It follows the web UI's "Delete their messages" (`ui/src/components/direct_messages/
//! dm_thread_modal.rs`, `purge_thread`) and signs the same bytes:
//!
//! - **Only the recipient deletes.** A purge envelope is signed by the recipient of a message, so
//!   only messages the peer sent the user can be named; a message the user sent is never offered
//!   and never named ([`inbound_tokens`] reads only messages whose recipient is the user).
//! - **What a purge does.** The envelope holds the purge tokens (a hash of each message's
//!   signature) of the messages deleted. Every copy of the room drops a message whose token is
//!   listed, and refuses it if it is offered again, so the messages are gone for everyone who
//!   holds the room: the user, the person who sent them (their own thread loses them too) and
//!   everyone else, who only ever held the sealed bytes.
//! - **One envelope per recipient.** The room keeps ONE envelope for the user, and the one with
//!   the higher version (then the greater signature digest) wins, whatever either lists. So a
//!   new envelope must list every token the room's current envelope lists, or it LIFTS those
//!   tombstones and the messages can come back. It is built only by river-core's
//!   `advance_recipient_purges` from the envelope the room holds NOW (never an older copy), at
//!   one version above it, and an envelope that has not gone out yet is built again from the
//!   room as it is when it does ([`refresh`]). **One rule for resending:** an entry is built again
//!   (kept as it is if still right) while it has not gone out and when a refused or unconfirmed
//!   one is retried; one that is out or unanswered is sent again byte for byte.
//! - **The cap.** An envelope lists at most [`MAX_PURGED_TOMBSTONES_PER_RECIPIENT`] tokens
//!   (1000), and the deleted messages' tombstones are never dropped to make room (that would
//!   bring them back). A deletion that would go over is refused whole, with the numbers
//!   ([`Refusal::Cap`]).
//! - **Landing.** Applying the update to a copy of the room can succeed and still leave the
//!   envelope out (the user is not listed, or another envelope outranks it), so a deletion is
//!   queued only if the copy afterwards holds this very envelope and none of the messages
//!   ([`applied`]), as a sent message must land ([`super::dm_send::landed`]).
//!
//! **A race with another device.** One envelope per recipient means an envelope built from a copy
//! of the room that lags the node can be outranked: a device signed in as the user may have signed
//! a higher-version envelope this copy has not seen, or signed at the same version with the greater
//! signature digest. The envelope that wins may omit this device's tokens, and the messages then
//! come back. This app keeps a record of every deletion it sends ([`super::purge_record`], a file
//! per device) and, when the room's winning envelope for the user omits a recorded token, signs
//! the union again at the next version ([`heal`], at most once per winning version, from
//! river-core's `OwnPurges::heal`). That envelope is a [`Work`] with `heal` set and goes through
//! the outbox like any deletion.
//!
//! Nothing here logs, and no error text carries a token, a key or a message. `Debug` of
//! everything here shows counts.

use std::collections::{BTreeSet, HashSet};

use river_core::room_state::ChatRoomStateV1Delta;
use river_core::room_state::direct_messages::{
    AuthorizedRecipientPurges, DirectMessagesDelta, MAX_PURGED_TOMBSTONES_PER_RECIPIENT,
    PurgeToken, advance_recipient_purges,
};
use river_core::room_state::member::MemberId;

use super::outbox::{Blocked, participation};
use super::room::LiveRoom;

/// What is said when the node took a deletion and the room here does not hold it: another device
/// of the user may have deleted recently (the room keeps one list per person, and this copy may
/// lag the node), or the user may no longer be listed as a member.
pub const NOT_KEPT: &str = "The room didn't keep this deletion. Another device of yours may have \
     deleted messages at the same moment, or you may no longer be listed as a member. If \
     another device won, River signs this deletion again by itself; Retry tries it now.";

/// Why a deletion was refused before anything was queued. The words never name a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The user's own state in the room forbids writing: no key on this computer, banned, not a
    /// member, or a private room whose key has not arrived.
    Blocked(Blocked),
    /// None of the messages asked for is in the room from that person any more.
    Nothing,
    /// The envelope would list more tokens than the room allows.
    Cap { held: usize, adding: usize },
    /// The envelope could not be built or signed.
    Compose(String),
    /// The room did not take the update.
    Refused(String),
    /// The update applied and the envelope, or its effect, is not in the room afterwards.
    NotKept,
    /// The room holds an envelope this one would lift tombstones of, or one that outranks it.
    /// It is built again from the room as it is ([`refresh`]); it is never sent.
    Stale,
}

impl Refusal {
    /// What the screen says.
    pub fn text(&self) -> String {
        match self {
            Refusal::Blocked(b) => b.reason().to_owned(),
            Refusal::Nothing => "There are no messages from them left to delete.".to_owned(),
            Refusal::Cap { held, adding } => format!(
                "The room records at most {MAX_PURGED_TOMBSTONES_PER_RECIPIENT} deleted \
                 messages for you, and {held} are recorded already, so {adding} more do not \
                 fit. Nothing was deleted. Older records are not dropped to make room, because \
                 that would bring those messages back."
            ),
            Refusal::Compose(why) => format!("The deletion could not be prepared: {why}"),
            Refusal::Refused(why) => why.clone(),
            Refusal::NotKept => NOT_KEPT.to_owned(),
            Refusal::Stale => "Your deleted messages changed in the room while this waited. \
                               Try again."
                .to_owned(),
        }
    }
}

/// A deletion waiting in the outbox: the envelope that goes out as it is, and the tokens it is
/// for, to build it again if it must be built before it first goes out.
#[derive(Clone, PartialEq, Eq)]
pub struct Work {
    pub envelope: AuthorizedRecipientPurges,
    /// The tokens this deletion is for (what it adds to the room's envelope), sorted.
    pub tokens: Vec<PurgeToken>,
    /// Whose messages. For a re-signed envelope ([`Work::heal`]) the user: it is not for one
    /// person's thread.
    pub peer: MemberId,
    /// A re-signed envelope that restores tokens a higher-ranked envelope omitted (see
    /// [`heal`]), not a deletion the user just asked for. `tokens` are the ones it restores,
    /// whose messages may be gone from this copy already.
    pub heal: bool,
    /// Whether this deletion is in the record of deletions ([`super::purge_record`]): set as it
    /// first goes out, cleared when the record is taken back.
    pub recorded: bool,
    /// The tokens its recording newly added; a refusal takes back only these, since the others
    /// were recorded by an earlier deletion that the node may have taken.
    pub added: Vec<PurgeToken>,
}

impl std::fmt::Debug for Work {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Work")
            .field("version", &self.envelope.state.version)
            .field("tokens", &self.tokens.len())
            .field("heal", &self.heal)
            .finish()
    }
}

/// The user's member id in `room`, from the key held.
fn me(room: &LiveRoom) -> Option<MemberId> {
    room.stored
        .self_sk
        .as_ref()
        .map(|sk| MemberId::from(&sk.verifying_key()))
}

/// Whether `id` is the owner or a member the room lists now. A deletion is signed for a
/// recipient the room lists: for anyone else the contract drops it silently.
fn listed(room: &LiveRoom, id: MemberId) -> bool {
    id == MemberId::from(&room.stored.owner_vk)
        || room
            .stored
            .room_state
            .members
            .members
            .iter()
            .any(|m| m.member.id() == id)
}

/// The envelope the room holds for `me` now.
pub fn held_envelope(room: &LiveRoom, me: MemberId) -> Option<&AuthorizedRecipientPurges> {
    room.stored
        .room_state
        .direct_messages
        .purges
        .iter()
        .find(|p| p.recipient_id == me)
}

/// The purge tokens of the messages `peer` sent the user that the room holds, sorted, each once.
/// A message the user sent, or one between other people, is never here.
pub fn inbound_tokens(room: &LiveRoom, me: MemberId, peer: MemberId) -> Vec<PurgeToken> {
    let held: BTreeSet<PurgeToken> = room
        .stored
        .room_state
        .direct_messages
        .messages
        .iter()
        .filter(|m| m.message.recipient == me && m.message.sender == peer)
        .map(|m| m.purge_token())
        .collect();
    held.into_iter().collect()
}

/// The tokens of `wanted` that name a message `peer` sent the user and the room still holds,
/// sorted, each once. What a deletion is asked for beyond that is not part of it.
pub fn deletable(
    room: &LiveRoom,
    me: MemberId,
    peer: MemberId,
    wanted: &[PurgeToken],
) -> Vec<PurgeToken> {
    let held: HashSet<PurgeToken> = inbound_tokens(room, me, peer).into_iter().collect();
    let wanted: BTreeSet<PurgeToken> = wanted
        .iter()
        .copied()
        .filter(|t| held.contains(t))
        .collect();
    wanted.into_iter().collect()
}

/// Build the envelope that adds `tokens` to the room's envelope for the user: river-core's
/// `advance_recipient_purges`, from the envelope the room holds now, signed with the room's
/// key. Over the cap it is refused, with the numbers.
pub fn build(room: &LiveRoom, tokens: &[PurgeToken]) -> Result<AuthorizedRecipientPurges, Refusal> {
    let Some(sk) = &room.stored.self_sk else {
        return Err(Refusal::Blocked(Blocked::NoKey));
    };
    let me = MemberId::from(&sk.verifying_key());
    let previous = held_envelope(room, me);
    let held: HashSet<&PurgeToken> = previous
        .iter()
        .flat_map(|p| p.state.purged.iter())
        .collect();
    let adding = tokens
        .iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .filter(|t| !held.contains(t))
        .count();
    if held.len() + adding > MAX_PURGED_TOMBSTONES_PER_RECIPIENT {
        return Err(Refusal::Cap {
            held: held.len(),
            adding,
        });
    }
    advance_recipient_purges(sk, &room.stored.owner_vk, previous, tokens.iter().copied())
        .map_err(Refusal::Compose)
}

/// The update that publishes `envelope`.
pub fn delta(envelope: &AuthorizedRecipientPurges) -> ChatRoomStateV1Delta {
    ChatRoomStateV1Delta {
        direct_messages: Some(DirectMessagesDelta {
            new_messages: Vec::new(),
            advanced_purges: vec![envelope.clone()],
        }),
        ..Default::default()
    }
}

/// Whether the room's envelope for the user is this very one.
pub fn landed(room: &LiveRoom, envelope: &AuthorizedRecipientPurges) -> bool {
    held_envelope(room, envelope.recipient_id)
        .is_some_and(|h| h.recipient_signature == envelope.recipient_signature)
}

/// Whether the room's envelope for the user lists every one of `tokens`: they are deleted,
/// whoever signed the envelope that does it.
pub fn covers(room: &LiveRoom, me: MemberId, tokens: &[PurgeToken]) -> bool {
    let Some(held) = held_envelope(room, me) else {
        return tokens.is_empty();
    };
    let held: HashSet<&PurgeToken> = held.state.purged.iter().collect();
    tokens.iter().all(|t| held.contains(t))
}

/// Whether `envelope` is still one to send against `room`: it is the room's own, or it outranks
/// the room's and lists everything the room's lists. Anything else would be refused as older, or
/// would lift a tombstone the room holds.
pub fn is_current(room: &LiveRoom, envelope: &AuthorizedRecipientPurges) -> bool {
    let Some(held) = held_envelope(room, envelope.recipient_id) else {
        return true;
    };
    if held.recipient_signature == envelope.recipient_signature {
        return true;
    }
    let ours: HashSet<&PurgeToken> = envelope.state.purged.iter().collect();
    envelope.state.version > held.state.version
        && held.state.purged.iter().all(|t| ours.contains(t))
}

/// `room` with the update for `envelope` applied, or why the room would not keep it. `tokens`
/// are the messages it is for: none of them may remain afterwards.
pub fn applied(
    room: &LiveRoom,
    envelope: &AuthorizedRecipientPurges,
    tokens: &[PurgeToken],
) -> Result<LiveRoom, Refusal> {
    participation(room).map_err(Refusal::Blocked)?;
    let Some(me) = me(room) else {
        return Err(Refusal::Blocked(Blocked::NoKey));
    };
    // Signed by the user's own key, for the user.
    if envelope.recipient_id != me {
        return Err(Refusal::Compose("it is not for you".to_owned()));
    }
    if !listed(room, me) {
        return Err(Refusal::Blocked(Blocked::NotMember));
    }
    if !is_current(room, envelope) {
        return Err(Refusal::Stale);
    }
    let mut next = room.clone();
    next.apply_delta(delta(envelope))
        .map_err(|e| Refusal::Refused(format!("The room refused it: {e}")))?;
    let gone = {
        let named: HashSet<&PurgeToken> = tokens.iter().collect();
        !next
            .stored
            .room_state
            .direct_messages
            .messages
            .iter()
            .any(|m| m.message.recipient == me && named.contains(&m.purge_token()))
    };
    if landed(&next, envelope) && gone {
        Ok(next)
    } else {
        Err(Refusal::NotKept)
    }
}

/// Check a deletion of the messages `wanted` that `peer` sent the user, build its envelope and
/// check that the room would keep it. What is asked for beyond what the room holds from `peer`
/// is left out; nothing left is [`Refusal::Nothing`].
pub fn prepare(room: &LiveRoom, peer: MemberId, wanted: &[PurgeToken]) -> Result<Work, Refusal> {
    participation(room).map_err(Refusal::Blocked)?;
    let Some(me) = me(room) else {
        return Err(Refusal::Blocked(Blocked::NoKey));
    };
    if !listed(room, me) {
        return Err(Refusal::Blocked(Blocked::NotMember));
    }
    if peer == me {
        return Err(Refusal::Nothing);
    }
    let tokens = deletable(room, me, peer, wanted);
    if tokens.is_empty() {
        return Err(Refusal::Nothing);
    }
    let envelope = build(room, &tokens)?;
    applied(room, &envelope, &tokens)?;
    Ok(Work {
        envelope,
        tokens,
        peer,
        heal: false,
        recorded: false,
        added: Vec::new(),
    })
}

/// A deletion that has not gone out, checked against the room as it is now. `Ok(None)` when the
/// envelope is still the one to send, byte for byte. Otherwise the room's envelope changed (a
/// deletion from another device, or a bigger version) and a new one is built from it, for the
/// tokens that still need deleting: a message that went meanwhile, or whose token the room's
/// envelope lists already, is left out. Nothing left to delete is [`Refusal::Nothing`].
pub fn refresh(room: &LiveRoom, work: &Work) -> Result<Option<Work>, Refusal> {
    if work.heal {
        return refresh_heal(room, work);
    }
    // Already deleted (the room took this envelope from the network, or another device's lists
    // these tokens): nothing to build or send, and no failure. The caller leaves the entry for
    // `Outbox::reconcile` to settle as done.
    if let Some(me) = me(room)
        && covers(room, me, &work.tokens)
    {
        return Ok(None);
    }
    // The messages went (the room's own cleanup, or a cap), and none is left to delete.
    if let Some(me) = me(room)
        && deletable(room, me, work.peer, &work.tokens).is_empty()
    {
        return Err(Refusal::Nothing);
    }
    match applied(room, &work.envelope, &work.tokens) {
        Ok(_) => return Ok(None),
        Err(Refusal::Stale) => {}
        Err(other) => return Err(other),
    }
    let Some(me) = me(room) else {
        return Err(Refusal::Blocked(Blocked::NoKey));
    };
    let tokens = deletable(room, me, work.peer, &work.tokens);
    if tokens.is_empty() {
        return Err(Refusal::Nothing);
    }
    let envelope = build(room, &tokens)?;
    applied(room, &envelope, &tokens)?;
    Ok(Some(Work {
        envelope,
        tokens,
        peer: work.peer,
        heal: false,
        recorded: work.recorded,
        added: work.added.clone(),
    }))
}

/// The envelope that restores the recorded tokens `own` holds for the user and the room's winning
/// envelope omits, signed at the next version, or `None` when nothing is missing or this device
/// already re-signed against the winning version in this run (`own` carries that guard and
/// records it; it is [`river_core::own_purges::OwnPurges::heal`]). Not for a room whose copy has
/// not taken the network's state: the caller checks that.
pub fn heal(
    room: &LiveRoom,
    own: &mut river_core::own_purges::OwnPurges,
) -> Option<Result<Work, Refusal>> {
    let sk = room.stored.self_sk.as_ref()?;
    let me = MemberId::from(&sk.verifying_key());
    let held: HashSet<PurgeToken> = held_envelope(room, me)
        .map(|h| h.state.purged.iter().copied().collect())
        .unwrap_or_default();
    let envelope = match own.heal(sk, &room.stored.owner_vk, &room.stored.room_state)? {
        Ok(envelope) => envelope,
        Err(e) => return Some(Err(Refusal::Compose(e))),
    };
    let tokens: Vec<PurgeToken> = envelope
        .state
        .purged
        .iter()
        .copied()
        .filter(|t| !held.contains(t))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let work = Work {
        envelope,
        tokens,
        peer: me,
        heal: true,
        recorded: false,
        added: Vec::new(),
    };
    Some(applied(room, &work.envelope, &work.tokens).map(|_| work))
}

/// [`refresh`] for a re-signed envelope. It is for tokens whose messages may be gone, so the
/// "nothing left to delete" checks do not apply; it is done when the room lists every token.
fn refresh_heal(room: &LiveRoom, work: &Work) -> Result<Option<Work>, Refusal> {
    let Some(me) = me(room) else {
        return Err(Refusal::Blocked(Blocked::NoKey));
    };
    if covers(room, me, &work.tokens) {
        return Ok(None);
    }
    match applied(room, &work.envelope, &work.tokens) {
        Ok(_) => return Ok(None),
        Err(Refusal::Stale) => {}
        Err(other) => return Err(other),
    }
    // The room's envelope changed since this was signed: sign again from it, for the tokens it
    // still omits, as many as it has room for.
    let held: HashSet<PurgeToken> = held_envelope(room, me)
        .map(|h| h.state.purged.iter().copied().collect())
        .unwrap_or_default();
    let room_left = MAX_PURGED_TOMBSTONES_PER_RECIPIENT.saturating_sub(held.len());
    let tokens: Vec<PurgeToken> = work
        .tokens
        .iter()
        .copied()
        .filter(|t| !held.contains(t))
        .take(room_left)
        .collect();
    if tokens.is_empty() {
        return Err(Refusal::Nothing);
    }
    let envelope = build(room, &tokens)?;
    applied(room, &envelope, &tokens)?;
    Ok(Some(Work {
        envelope,
        tokens,
        peer: work.peer,
        heal: true,
        recorded: false,
        added: Vec::new(),
    }))
}

#[cfg(test)]
#[path = "dm_purge_tests.rs"]
mod tests;
