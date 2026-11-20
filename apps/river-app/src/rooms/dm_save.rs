//! Saving the text of the direct messages the user sent, to the chat delegate's `outbound_dms`
//! key, so it is still there after a restart and on the web UI.
//!
//! A sent message is sealed to its recipient, so the sender can read it back only from the text
//! saved for it ([`super::dm_cache`]). This module is the pacing and bookkeeping of that save.
//! The write itself is a task of [`super::work`] (one request at a time, answers matched by the
//! key awaited), and what it stores is worked out by [`super::dm_store`].
//!
//! **When it saves.** As soon as a message is queued ([`super::session`] `send_dm`), not when
//! the room's node takes it: the text must not depend on the node answering, and a save made
//! first can only leave an entry for a message that never went out (the app quit while it
//! waited in the outbox), which nothing reads. A message the user discards after it was saved is
//! removed again, and only that entry.
//!
//! **What must be true first**, all checked in [`super::session`]'s `dm_save_tick`:
//!
//! - The cache is hydrated: the last read of `outbound_dms` decoded, or listed no such key from
//!   a trustworthy list ([`super::dm_cache::OutboundCache::hydrated`]). A read that was
//!   unreadable unlatches it, so a value this app cannot read is never written over.
//! - River has not moved on (`update_required`) and the connection's write gate is open. A task
//!   already working is failed by [`super::work::Work::halt`] when River moves on.
//! - The stored rooms were read, and the store is there.
//!
//! The write then reads the key again with its generation, merges only the changes asked for
//! into what it finds, and stores it only if the generation is still the one read, reading
//! again after a short wait when another writer got in first (at most
//! [`super::work::CAS_ATTEMPTS`] times). A value it cannot read is not written over.
//!
//! **The web UI's blind save.** The web UI saves the whole `outbound_dms` value from its own
//! copy, with a plain store, so a tab that hydrated before this app saved puts its older copy
//! back and this app's entries vanish from the store (the web UI's own tabs lose each other's
//! the same way). This app cannot prevent that. It notices, because every later read of the
//! key that lacks an entry this app saved moves the entry back to unsaved
//! ([`super::dm_cache::OutboundCache::note_lost`]), and saves it again, at most
//! [`super::dm_cache::MAX_RESAVES`] times for one message (after that its text stays in memory
//! for the session and is not saved again). It does so only while the message is
//! still in the room, was sent by the user's current identity there, was not purged by its
//! recipient, and is not older than the newest [`river_core::room_state::direct_messages::MAX_DM_MESSAGES_PER_PAIR`]
//! of its pair (an entry the web UI dropped by its own cap is not put back): an entry the web UI dropped on purpose (a purge, or replacing the identity) is not
//! put back.
//!
//! **Failure.** A save that fails leaves the text in memory, where the thread keeps showing it,
//! and is tried again after a wait that doubles from [`FIRST_WAIT`] to [`MAX_WAIT`], spread by
//! plus or minus a fifth, at most [`MAX_FAILURES`] times in a row. Sending another message, or
//! discarding a saved one, starts the count again. Nothing about it is shown: the app works
//! without the saved text until the next start. Each failure is said once on stderr with the
//! kind of refusal and the count, never with anything written (`River: saving sent direct
//! messages failed (full), 3 in a row`). A key found empty after it was read with a value
//! ([`super::dm_store::Refusal::Vanished`]) asks for the stored rooms to be read again; if the
//! new list names rooms and not the key, it is gone for good and the next save creates it.
//!
//! **What goes in one write.** Owed removals are written alone and first, so an add that is
//! refused (too big, a value that cannot be read) cannot hold back the removal of a message the
//! user discarded. Adds for a room that is no longer shown are left out when the write is made
//! (the room was left meanwhile), so no entry is orphaned. The text of a room that is not in
//! the list is dropped only after a complete read of the rooms (as the outbox's is): a room whose
//! stored copy could not be read is missing from the list for a while, and its unsaved text waits
//! for it. After [`REMOVAL_TURNS`] failed removal writes in a row the adds go first for a turn.
//!
//! **Archive changes** ([`super::dm_archive`]) are written by the same read-merge-compare-and-swap
//! but never in the same write as sent text or removals, and have a [`DmSaver`] of their own in the
//! session (`archive_saver`): a store that keeps refusing them (an archive entry this build cannot
//! read, a value too big for one more entry) must not refuse a sent text or a removal along with
//! them, nor delay them by its backoff. The text goes first when both are due. A write that finds
//! nothing to change stores nothing and is done.
//!
//! **Size.** A write is a whole value on the connection that also carries the user's rooms, so
//! it is bounded at [`super::dm_store::MAX_WRITE_BYTES`] (4 MiB; see there for the limits of the
//! node's client API, 100 MiB, and of the delegate, none) and a save that goes unanswered costs
//! the session at most one new connection before it fails and waits its turn. A value already
//! over the bound is not added to. Removals are never refused for size.
//!
//! **Copies of the text.** The text is zeroed where this app can reach it: the unsaved and saved
//! texts when dropped, the bytes of a store when its step ends, the decoded CBOR and the buffers
//! the merge builds, the value in the request after it is sent, and the value in an answer the
//! save took. It is not reached in: the WebSocket client's frame buffers, the bincode buffer
//! the request is encoded into, the rooms read's answers (the loader keeps them until it
//! decodes them), the `String`s the CBOR encoder makes along the way, and the web UI's own
//! copies.
//!
//! No text is logged, and nothing here has a `Debug` that prints one.

use std::time::{Duration, Instant};

use super::dm_store::EntryKey;
use super::keys::wait_after;

/// The wait after the first failed save, doubling after each.
pub const FIRST_WAIT: Duration = Duration::from_secs(20);
/// The longest wait between two tries.
pub const MAX_WAIT: Duration = Duration::from_secs(10 * 60);
/// How many removal writes may fail in a row before the adds are tried.
pub const REMOVAL_TURNS: u32 = 3;
/// How many saves in a row may fail before the app stops trying until the next message sent.
pub const MAX_FAILURES: u32 = 10;

/// What the session keeps between saves.
#[derive(Clone, Debug)]
pub struct DmSaver {
    failures: u32,
    not_before: Option<Instant>,
    /// Entries to remove from the store (a saved message the user discarded).
    removals: Vec<EntryKey>,
    /// Removal writes that failed in a row. Past [`REMOVAL_TURNS`] the adds go first for a turn,
    /// so a removal that keeps failing cannot hold them back for good.
    removal_failures: u32,
    /// The entries the write now working adds and removes.
    adding: Vec<EntryKey>,
    removing: Vec<EntryKey>,
    first: Duration,
    max: Duration,
    /// A fixed point of the jitter's range (0 to 1) for tests; random otherwise.
    unit: Option<f64>,
}

impl Default for DmSaver {
    fn default() -> Self {
        DmSaver {
            failures: 0,
            not_before: None,
            removals: Vec::new(),
            removal_failures: 0,
            adding: Vec::new(),
            removing: Vec::new(),
            first: FIRST_WAIT,
            max: MAX_WAIT,
            unit: None,
        }
    }
}

impl DmSaver {
    /// For tests: waits with no jitter (the middle of its range).
    #[cfg(test)]
    pub fn with_waits(first: Duration, max: Duration) -> Self {
        DmSaver {
            first,
            max,
            unit: Some(0.5),
            ..DmSaver::default()
        }
    }

    /// Whether a save may start now.
    pub fn due(&self, now: Instant) -> bool {
        self.failures < MAX_FAILURES && self.not_before.is_none_or(|at| now >= at)
    }

    /// The user sent something: whatever failed before is tried again as soon as it can be.
    pub fn new_work(&mut self) {
        self.removal_failures = 0;
        self.failures = 0;
        self.not_before = None;
    }

    /// The entries to remove, as a write is started for them.
    pub fn removals(&self) -> &[EntryKey] {
        &self.removals
    }

    /// Whether a write of removals goes now: some are owed, and they have not failed
    /// [`REMOVAL_TURNS`] times in a row.
    pub fn removals_first(&self) -> bool {
        !self.removals.is_empty() && self.removal_failures < REMOVAL_TURNS
    }

    /// A save of a message the user discarded is owed, when it was saved or is being saved.
    /// Owing one also starts the count of failures again, so a removal that was held back by
    /// failures is tried as soon as the user asks for another.
    pub fn owes_removal(&mut self, key: EntryKey) {
        if !self.removals.contains(&key) {
            self.removals.push(key);
        }
        self.new_work();
    }

    /// Whether a write now working adds this entry, so a discard must remove it afterwards.
    pub fn is_being_added(&self, key: &EntryKey) -> bool {
        self.adding.contains(key)
    }

    /// A write was started for these.
    pub fn started(&mut self, adding: Vec<EntryKey>, removing: Vec<EntryKey>) {
        self.adding = adding;
        self.removing = removing;
    }

    /// The write ended. A failed one is tried again later, and its removals stay owed.
    pub fn ended(&mut self, ok: bool, now: Instant) {
        let removed = std::mem::take(&mut self.removing);
        self.adding.clear();
        if !removed.is_empty() {
            self.removal_failures = if ok { 0 } else { self.removal_failures + 1 };
        } else if ok {
            // The adds had their turn: the removals are tried again.
            self.removal_failures = 0;
        }
        if ok {
            self.failures = 0;
            self.not_before = None;
            self.removals.retain(|k| !removed.contains(k));
        } else {
            self.failures += 1;
            // Plus or minus a fifth, so nothing that failed together tries together.
            let unit = self
                .unit
                .unwrap_or_else(rand::random::<f64>)
                .clamp(0.0, 1.0);
            let wait = wait_after(self.failures, self.first, self.max).mul_f64(0.8 + 0.4 * unit);
            self.not_before = Some(now + wait);
        }
    }

    /// Consecutive failed saves.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// For tests: the jitter's fixed point.
    #[cfg(test)]
    pub fn fix_jitter(&mut self, unit: f64) {
        self.unit = Some(unit);
    }
}

#[cfg(test)]
#[path = "dm_save_tests.rs"]
mod tests;
