//! Archiving a direct-message thread: the rules, as pure functions.
//!
//! "Archive" is the label; the stored word is "hidden" (`hidden_threads` of the chat delegate's
//! `outbound_dms` value, which the web UI shares). An entry `{room_owner_vk, peer, hidden_at_ts}`
//! hides the thread between the user and `peer` in that room until the peer sends something
//! strictly newer than `hidden_at_ts`. The rules are the web UI's, ported exactly
//! (`.claude/rules/direct-messages.md`, "Two coupled invariants"):
//!
//! 1. **The cutoff is the newest INBOUND message time**, from the thread as shown and from the
//!    room as it is now, whichever is larger ([`archive_cutoff`]). Never the user's own clock and
//!    never the time of a message the user sent: a reply stamped by the user's clock would be
//!    compared with a peer's, and a peer's message in flight across the click would vanish.
//! 2. **Inbound sync removes the entry only through the gate** ([`should_unhide_for_inbound`]):
//!    when a message is strictly past the cutoff. Re-landed old messages, which the contract's
//!    membership sweep removes and peers offer again, carry their original times, at or below the
//!    cutoff, and are inert. Explicit user actions (Un-archive, Undo, and sending into the
//!    thread) remove the entry unconditionally ([`ArchiveOp::Unhide`]); routing one through the
//!    gate would make it a no-op, because at that moment the peer's newest message is at or
//!    below the cutoff by construction.
//!
//! **Raw times.** Every time here is the time the peer signed, as in the web UI: the cutoff, the
//! rail's comparison and the gate use [`DmThread::last_inbound_signed`] and the room's own
//! messages, never the clamped time `rooms::dm` shows (`MAX_SHOWN_SECS`, display and read
//! positions only). A clamped cutoff would let a peer who signs a time past 2100 make the web UI's
//! gate (which compares raw times) drop an archive written here. The cost is the web UI's own,
//! an accepted residual: a peer who signs a far-future time keeps their thread archived until
//! real time passes the stamp (the remedy is a block list, freenet/river#461).
//!
//! **Sending.** The unarchive for a send is asked for when the message is queued, not when the
//! room takes it (the web UI does it after the send), and only when this app's copy of the list
//! holds the thread or has not been read yet. A thread another device archived since the copy was
//! read is therefore not unarchived by a send from here (the web UI always asks, at the price of
//! a write per send) and shows archived again at the next read.
//!
//! **Accepted residuals** (as the web UI's): a new inbound message in the same second as the
//! peer's previous one waits for the next; a peer whose clock moves backward is deferred; a peer
//! who stamps the future defers their later messages; a cutoff taken inside a partial re-land
//! window can be lost. None destroys state. Archiving on this device and replying from another
//! leaves this device archived until the peer writes, as in the web UI.
//!
//! Nothing here reads the clock.

use std::collections::HashMap;

use river_core::chat_delegate::HiddenDmThreadEntry;
use river_core::room_state::direct_messages::{DirectMessagesV1, MAX_DM_FUTURE_SKEW_SECS};
use river_core::room_state::member::MemberId;

/// A thread: the room (its owner's key bytes) and the peer's FULL member id, as the stored entry
/// names it. Never the 40-bit label a member can grind to match another's.
pub type ThreadKey = ([u8; 32], MemberId);

/// One change to the archive list.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ArchiveOp {
    /// Archive the thread at this cutoff. Raises an older cutoff; one already at or above it is
    /// left alone (the web UI's `decide_hide_action`).
    Hide {
        thread: ThreadKey,
        hidden_at_ts: u64,
    },
    /// Take the thread out of the archive, whatever its cutoff: the user did it.
    Unhide { thread: ThreadKey },
    /// Take it out only if the stored cutoff is below `past` (a message past the cutoff
    /// arrived). Checked against the value the write reads, so a fresher archive another device
    /// stored meanwhile is not destroyed by a decision made on an older read.
    UnhideIfPast { thread: ThreadKey, past: u64 },
}

/// Prints what the change is and nothing of whom it is about: a debug line of a thread must not
/// name the member or the room.
impl std::fmt::Debug for ArchiveOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArchiveOp::Hide { hidden_at_ts, .. } => f
                .debug_struct("Hide")
                .field("hidden_at_ts", hidden_at_ts)
                .finish_non_exhaustive(),
            ArchiveOp::Unhide { .. } => f.debug_struct("Unhide").finish_non_exhaustive(),
            ArchiveOp::UnhideIfPast { past, .. } => f
                .debug_struct("UnhideIfPast")
                .field("past", past)
                .finish_non_exhaustive(),
        }
    }
}

impl ArchiveOp {
    pub fn thread(&self) -> ThreadKey {
        match self {
            ArchiveOp::Hide { thread, .. }
            | ArchiveOp::Unhide { thread }
            | ArchiveOp::UnhideIfPast { thread, .. } => *thread,
        }
    }
}

/// The cutoff to archive a thread at: the newest inbound time of the thread as shown
/// (`row_inbound_ts`) or of the room as it is now (`live_inbound_ts`, `None` when it could not be
/// read), whichever is larger, 0 for a peer who never wrote. Raw, and clamped at nothing:
/// clamping at the clock (or at 2100) would put the cutoff below an existing inbound message,
/// which turns Archive into a no-op, lets a re-landed message destroy the entry
/// (freenet/river#526), and lets the web UI's own gate drop it.
pub fn archive_cutoff(row_inbound_ts: u64, live_inbound_ts: Option<u64>) -> u64 {
    row_inbound_ts.max(live_inbound_ts.unwrap_or(0))
}

/// Whether a thread is hidden: it has an entry with cutoff `cutoff` and the peer's newest
/// message is not strictly past it.
pub fn is_hidden(cutoff: Option<u64>, last_inbound_ts: u64) -> bool {
    cutoff.is_some_and(|c| last_inbound_ts <= c)
}

/// Whether an inbound message stamped `inbound_ts` removes the entry whose cutoff is `cutoff`
/// (`None`: the thread is not archived, or the list has not been read, which must not be told
/// apart: nothing to revive). Strictly past, with the stamp bounded at `now + the contract's
/// future skew`: the bound can only make the gate more conservative, and keeps a peer who has
/// already inflated the cutoff from escalating to a larger forgery. A first forgery can still
/// revive a normally archived thread.
pub fn should_unhide_for_inbound(cutoff: Option<u64>, inbound_ts: u64, now_secs: u64) -> bool {
    let effective = inbound_ts.min(now_secs.saturating_add(MAX_DM_FUTURE_SKEW_SECS));
    matches!(cutoff, Some(c) if effective > c)
}

/// The op that drops a stored entry for a message stamped `newest_inbound_ts`, when the gate
/// says so. The stamp is bounded as [`should_unhide_for_inbound`] does, and that bounded value is
/// what the write compares with the cutoff it finds.
pub fn revival_op(
    thread: ThreadKey,
    cutoff: u64,
    newest_inbound_ts: u64,
    now_secs: u64,
) -> Option<ArchiveOp> {
    should_unhide_for_inbound(Some(cutoff), newest_inbound_ts, now_secs).then(|| {
        ArchiveOp::UnhideIfPast {
            thread,
            past: newest_inbound_ts.min(now_secs.saturating_add(MAX_DM_FUTURE_SKEW_SECS)),
        }
    })
}

/// The newest time of a message `peer` sent `me` in this room's direct messages, as signed, 0 when
/// there is none. Not verified: a message in the room's state was checked by
/// the contract, and a cutoff that is too high only defers one peer's next message.
pub fn live_inbound_ts(messages: &DirectMessagesV1, me: MemberId, peer: MemberId) -> u64 {
    messages
        .messages
        .iter()
        .filter(|m| m.message.sender == peer && m.message.recipient == me)
        .map(|m| m.message.timestamp)
        .max()
        .unwrap_or(0)
}

/// [`live_inbound_ts`] for every peer at once: one pass over the messages, for a caller that asks
/// about many threads of one room.
pub fn live_inbound_by_peer(messages: &DirectMessagesV1, me: MemberId) -> HashMap<MemberId, u64> {
    let mut out: HashMap<MemberId, u64> = HashMap::new();
    for m in &messages.messages {
        if m.message.recipient == me {
            let t = out.entry(m.message.sender).or_insert(0);
            *t = (*t).max(m.message.timestamp);
        }
    }
    out
}

/// The cutoff stored for a thread, when it has entries: the largest, as the web UI's hydration
/// keeps when a value holds the same thread twice.
pub fn stored_cutoff(list: &[HiddenDmThreadEntry], thread: &ThreadKey) -> Option<u64> {
    list.iter()
        .filter(|h| h.room_owner_vk == thread.0 && h.peer == thread.1)
        .map(|h| h.hidden_at_ts)
        .max()
}

/// What an op does to a list of entries, in the same way the write does it to the stored value
/// (`dm_store`): the in-memory view and the store agree. True when the list changed.
pub fn apply(list: &mut Vec<HiddenDmThreadEntry>, op: &ArchiveOp) -> bool {
    let thread = op.thread();
    let held = stored_cutoff(list, &thread);
    let same = |h: &HiddenDmThreadEntry| h.room_owner_vk == thread.0 && h.peer == thread.1;
    match (op, held) {
        (ArchiveOp::Hide { hidden_at_ts, .. }, None) => {
            list.push(HiddenDmThreadEntry {
                room_owner_vk: thread.0,
                peer: thread.1,
                hidden_at_ts: *hidden_at_ts,
            });
            true
        }
        (ArchiveOp::Hide { hidden_at_ts, .. }, Some(at)) if at < *hidden_at_ts => {
            for h in list.iter_mut().filter(|h| same(h)) {
                h.hidden_at_ts = *hidden_at_ts;
            }
            true
        }
        (ArchiveOp::Unhide { .. }, Some(_)) => {
            list.retain(|h| !same(h));
            true
        }
        (ArchiveOp::UnhideIfPast { past, .. }, Some(at)) if at < *past => {
            list.retain(|h| !same(h));
            true
        }
        _ => false,
    }
}

#[cfg(test)]
#[path = "dm_archive_tests.rs"]
mod tests;
