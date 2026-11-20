//! The plaintext of the direct messages the user sent, read from the chat delegate.
//!
//! A direct message is sealed to its recipient, so the sender cannot open their own. The web UI
//! and `riverctl` keep what they sent in the chat delegate, under the key `outbound_dms`, as the
//! CBOR of [`OutboundDmStore`], and join it to the message in the room by its purge token.
//! [`OutboundCache`] is this app's in-memory copy of that.
//!
//! **This module holds state and never talks to the delegate.** The write is
//! [`super::dm_save`] (pacing) and [`super::dm_store`] (what is stored), run as a task of
//! [`super::work`]. The web UI saves the whole blob with a blind store, so a write made on a
//! stale copy loses entries; that is why the write re-reads the key with its generation and
//! merges, and why it gates on [`OutboundCache::hydrated`]. The latch fails safe: it is true
//! only after a read that listed the key and decoded it (or a list that was trustworthy and did
//! not have the key), and a read that proves nothing leaves it as it was.
//!
//! **Sent this session, and saving.** A message this app sends is sealed to its recipient, so
//! the sender cannot open it again. [`OutboundCache::remember_sent`] keeps its text in memory so
//! the row shows it, held apart from what the delegate returned (`local`), so a re-read does not
//! drop it. The save moves it on: after the store holds it, [`OutboundCache::note_saved`] moves
//! it from `local` to `saved`. It is not dropped there (the hand-off this module used to promise
//! was [`OutboundCache::forget_sent`]) because the web UI's blind store can overwrite the blob
//! with a copy that lacks it, and the only copy of the text left would then be gone: `saved` is
//! what lets [`OutboundCache::note_read`] notice that and put the entry back
//! ([`MAX_RESAVES`] times; after that the text stays in memory for the session and is not saved
//! again). Which of "a read holds the entry" and "the save's end was taken" comes first does not
//! matter: a read that holds a local entry moves it to `saved` itself, and
//! [`OutboundCache::note_saved`] does nothing for one already there. A read that began before a
//! save ended is not counted as losing that save's entries ([`OutboundCache::note_read_from`]).
//! [`OutboundCache::forget_sent`] and [`OutboundCache::forget_saved`]
//! are for a send the user discarded.
//!
//! **What "not hydrated" and "not listed" mean.** The delegate's key list is the only thing that
//! says whether `outbound_dms` exists, and a list can come back empty for a reason that is not an
//! empty store (a failed read of the index, freenet-core#5857). So [`DmRead::NotListed`] means
//! "no such key was listed", not "an empty store". It latches an empty cache only when the list
//! is authoritative ([`OutboundCache::note_read`]'s `list_authoritative`: the list named rooms, so
//! the index was read) and the cache had not hydrated before; it never clears or unlatches a
//! cache that has. The same distrust the room list applies to a read that lists nothing.
//!
//! **Generation.** The delegate wraps every stored value in a generation envelope (`[1][generation
//! u64 LE][value]`, `delegates/chat-delegate/src/versioning.rs`). The session reads this key with
//! a plain `GetRequest`, which strips it, so on the real path the value is the bare CBOR and
//! [`OutboundCache::generation`] is 0. The write does not use it: it reads with
//! `Versioned::Read` (`GetVersionedRequest`) itself, immediately before it stores. [`OutboundCache::from_bytes`] still accepts
//! the enveloped form and tells the two apart as the delegate does: a CBOR map never starts with
//! the envelope's tag byte.
//!
//! **What the latch trusts.** It trusts any non-empty room list: a list that names rooms is taken
//! to be a read of the whole key index, so a missing `outbound_dms` in it means none is stored.
//! It does not require `rooms_meta`, which is absent for a user who never changed a per-room
//! mode. The basis is that the delegate keeps the index as one stored value, read whole by
//! `get_key_index` (`delegates/chat-delegate/src/handlers.rs`), which errors rather than answer
//! with a part when the value is absent but a key it must list is stored. That is read from the
//! code; the absent-key case (a node River never ran on, listing rooms and no `outbound_dms`) is
//! what the real-node e2e's first save goes through. It does not cover a writer adding the key
//! just after the list was read. So the latch is not what makes a write safe: the writer reads the key
//! again with `Versioned::Read` immediately before writing and stores only if the generation is
//! still the one it read, which makes a wrong latch unable to cause a blind overwrite by
//! itself. A key the cache read with a value ([`OutboundCache::listed`]) that is then found
//! empty is refused too ([`super::dm_store::Refusal::Vanished`]).
//!
//! The entries are what people wrote, so nothing here prints one: [`Plaintext`] and the cache
//! `Debug` show lengths only and decode errors never carry the bytes. A [`Plaintext`] wipes its
//! text when dropped; the input CBOR buffer and the entries a duplicate replaced or skipped are
//! not wiped. Decoding runs on the session thread and is linear in the value, which is
//! why [`MAX_STORE_BYTES`] bounds it.
//!
//! Duplicate entries for one (room, recipient, token) keep the one with the higher timestamp,
//! and the first on a tie, as the web UI's hydration does. The web UI also drops entries whose
//! `sender` is a replaced identity of the room; that is not done here, because a purge token is
//! the hash of one signature, so an entry of another identity never matches a message of this
//! one.

use std::collections::HashMap;
use std::fmt;

use ed25519_dalek::VerifyingKey;
use river_core::chat_delegate::{HiddenDmThreadEntry, OutboundDmEntry, OutboundDmStore};
use river_core::room_state::direct_messages::PurgeToken;
use river_core::room_state::member::MemberId;
use zeroize::Zeroize;

use super::dm_archive::{ArchiveOp, ThreadKey};

pub use river_core::chat_delegate::OUTBOUND_DMS_STORAGE_KEY as KEY;

/// The tag byte of the delegate's generation envelope.
const ENVELOPE_TAG: u8 = 1;
/// The envelope's header: the tag and a little-endian `u64` generation.
const ENVELOPE_HEADER: usize = 1 + 8;
/// The most bytes of stored value read. Far above what the per-pair and room-wide caps allow a
/// user to accumulate; it only stops a corrupt value from being decoded at any length.
pub const MAX_STORE_BYTES: usize = 32 * 1024 * 1024;
/// The most sent texts held only in memory ([`OutboundCache::remember_sent`]). The contract
/// keeps at most 300 direct messages a room, so this is far above what a thread can show.
pub const MAX_LOCAL: usize = 1000;

/// What the cache is looked up by: the room (its owner's key bytes), the recipient and the
/// message's purge token.
pub use super::dm_store::EntryKey;

/// How many times an entry this app saved is put back after a read finds it gone (a web UI tab
/// saved its own copy over it). Past this the entry is left out, so a writer that keeps
/// overwriting is not fought forever.
pub const MAX_RESAVES: u8 = 3;

/// The most saved archive changes kept for a read that began before they were saved
/// ([`OutboundCache::note_archive_saved`]).
pub const MAX_RECENT_ARCHIVE: usize = 64;

/// A sent message's text. `Debug` does not print it, and drop wipes it.
#[derive(Clone, PartialEq, Eq)]
pub struct Plaintext(String);

impl Plaintext {
    pub(crate) fn new(text: String) -> Self {
        Plaintext(text)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Plaintext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Plaintext(<{} bytes>)", self.0.len())
    }
}

impl Drop for Plaintext {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// One entry of the store: what the delegate holds for a sent message, in full, so a writer can
/// put back every field it read. (The type has no field this build does not know; an unknown
/// one in the stored CBOR is dropped by decoding, which is why writing needs a read-merge.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentDm {
    /// The user's member id when it was sent.
    pub sender: MemberId,
    /// Unix seconds, the same as in the message.
    pub timestamp: u64,
    pub plaintext: Plaintext,
}

/// Why a stored value could not be read. The text never contains the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheError(String);

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CacheError {}

/// What one read of `outbound_dms` found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DmRead {
    /// The delegate's key list did not have `outbound_dms`. This is not "an empty store": see
    /// the module docs.
    #[default]
    NotListed,
    /// A value that decoded.
    Read(Box<OutboundCache>),
    /// The key was listed but its value was missing or did not decode.
    Unreadable,
}

/// The user's sent messages and hidden threads, as last read.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct OutboundCache {
    entries: HashMap<EntryKey, SentDm>,
    hidden: Vec<HiddenDmThreadEntry>,
    /// The generation of the stored value, 0 when the bytes had no envelope (always, on the
    /// session's plain read).
    generation: u64,
    /// The last read that proved anything succeeded, so the entries are the delegate's.
    hydrated: bool,
    /// Bumped by every read that changes what this holds, so a view built from it can tell.
    epoch: u64,
    /// What this app sent since it started and has not saved to the delegate: the text of its
    /// own sent messages. Kept across reads (see the module docs).
    local: HashMap<EntryKey, SentDm>,
    /// What this app saved to the delegate since it started, kept in memory so an entry a web
    /// UI tab overwrites can be put back (see "Saving"). Bounded by [`MAX_LOCAL`].
    saved: HashMap<EntryKey, SentDm>,
    /// How many times each key was put back after being found gone.
    resaves: HashMap<EntryKey, u8>,
    /// Bumped by every save that ends well, so a read can be told from one that began before
    /// the save ([`OutboundCache::note_read_from`]).
    save_seq: u64,
    /// The last read that decoded found the key with a value. A later read of nothing is then
    /// a value that went missing, which a write must not paper over.
    listed: bool,
    /// Archive changes the user made that the delegate does not hold yet, at most one per
    /// thread (the newest wins), oldest first. Shown over `hidden` at once, kept across reads
    /// (a read made before the write ended does not have them), and dropped when a write that
    /// carried them ends well.
    archive_owed: Vec<ArchiveOp>,
    /// Archive changes a write saved, each with the save count it ended at, kept until a read
    /// that began after it (one that began before may not hold them: a stale read must not put
    /// an archive back or take one away). Bounded by [`MAX_RECENT_ARCHIVE`].
    archive_recent: Vec<(u64, ArchiveOp)>,
    /// Threads whose conditional removal the store skipped (it holds a higher cutoff than this
    /// copy): not looked at for revival until the next read shows what the store holds, so the
    /// same removal is not asked for again against a stale copy.
    revival_held: std::collections::HashSet<ThreadKey>,
    /// Bumped whenever [`OutboundCache::archived`] changes, so a view built from it can tell
    /// without the thread rebuild that `epoch` causes.
    archive_rev: u64,
}

impl fmt::Debug for OutboundCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboundCache")
            .field("entries", &self.entries.len())
            .field("hidden_threads", &self.hidden.len())
            .field("archive_owed", &self.archive_owed.len())
            .field("sent_this_session", &self.local.len())
            .field("saved_this_session", &self.saved.len())
            .field("hydrated", &self.hydrated)
            .finish()
    }
}

impl OutboundCache {
    /// Decode a stored value: the CBOR of an [`OutboundDmStore`], bare or in the delegate's
    /// generation envelope. **No bytes is an error**, not an empty store: a listed key whose
    /// value is empty is a failed read, and nothing may treat it as nothing sent. The result is
    /// hydrated.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CacheError> {
        if bytes.len() > MAX_STORE_BYTES {
            return Err(CacheError(format!(
                "the stored sent messages are {} bytes, over the {MAX_STORE_BYTES} read limit",
                bytes.len()
            )));
        }
        let (generation, payload) = strip_envelope(bytes);
        if payload.is_empty() {
            return Err(CacheError("the stored sent messages hold no value".into()));
        }
        let store: OutboundDmStore =
            ciborium::de::from_reader(payload).map_err(|e| CacheError(describe(&e)))?;
        let mut entries: HashMap<EntryKey, SentDm> = HashMap::with_capacity(store.entries.len());
        for OutboundDmEntry {
            room_owner_vk,
            sender,
            recipient,
            purge_token,
            timestamp,
            plaintext,
        } in store.entries
        {
            let key = (room_owner_vk, recipient, purge_token);
            // The higher timestamp wins; the first of two equal ones stays.
            if entries.get(&key).is_some_and(|e| e.timestamp >= timestamp) {
                continue;
            }
            entries.insert(
                key,
                SentDm {
                    sender,
                    timestamp,
                    plaintext: Plaintext(plaintext),
                },
            );
        }
        Ok(OutboundCache {
            entries,
            hidden: store.hidden_threads,
            generation,
            hydrated: true,
            epoch: 0,
            local: HashMap::new(),
            saved: HashMap::new(),
            resaves: HashMap::new(),
            save_seq: 0,
            listed: true,
            archive_owed: Vec::new(),
            archive_recent: Vec::new(),
            revival_held: Default::default(),
            archive_rev: 0,
        })
    }

    /// Take the outcome of a read.
    ///
    /// - A decoded value replaces everything and latches.
    /// - An unreadable one keeps the entries (they still label rows) and unlatches.
    /// - A key that was not listed proves little: it latches an empty cache only when
    ///   `list_authoritative` and nothing had been read before, and otherwise changes nothing. It
    ///   never clears a cache that has hydrated, or unlatches it.
    ///
    /// `list_authoritative` trusts any non-empty room list (see the module docs: why that is
    /// judged safe, and why a writer must re-read before writing anyway).
    ///
    /// A cache that was unreadable keeps its entries, so a later `NotListed` does not hydrate it
    /// (that needs `entries.is_empty()`); it stays unhydrated, and the next `Read` fixes it.
    pub fn note_read(&mut self, read: DmRead, list_authoritative: bool) {
        self.note_read_from(read, list_authoritative, self.save_seq);
    }

    /// [`OutboundCache::note_read`] for a read that began when [`OutboundCache::save_seq`] was
    /// `began_at`. A save that ended since then may not be in what the read returned, so a
    /// saved entry the read lacks says nothing: it is not counted as lost.
    pub fn note_read_from(&mut self, read: DmRead, list_authoritative: bool, began_at: u64) {
        let epoch = self.epoch.wrapping_add(1);
        match read {
            DmRead::Read(fresh) => {
                let before = self.archived();
                let local = std::mem::take(&mut self.local);
                let saved = std::mem::take(&mut self.saved);
                let resaves = std::mem::take(&mut self.resaves);
                let save_seq = self.save_seq;
                let archive_owed = std::mem::take(&mut self.archive_owed);
                let archive_recent = std::mem::take(&mut self.archive_recent);
                let archive_rev = self.archive_rev;
                *self = *fresh;
                // A read shows what the store holds: nothing is held back from revival now.
                self.revival_held.clear();
                self.local = local;
                self.saved = saved;
                self.resaves = resaves;
                self.save_seq = save_seq;
                self.archive_owed = archive_owed;
                self.archive_recent = archive_recent;
                self.archive_rev = archive_rev;
                // A read that began before an archive write ended may not hold what it saved:
                // those are laid over it again. One that began after holds them, and they go.
                let late: Vec<ArchiveOp> = self
                    .archive_recent
                    .iter()
                    .filter(|(seq, _)| *seq > began_at)
                    .map(|(_, op)| *op)
                    .collect();
                for op in &late {
                    super::dm_archive::apply(&mut self.hidden, op);
                }
                self.archive_recent.retain(|(seq, _)| *seq > began_at);
                // What the delegate holds now may differ (another device archived, or its blind
                // save put an older list back): the view changes, and the rail with it.
                if self.archived() != before {
                    self.archive_rev = self.archive_rev.wrapping_add(1);
                }
                // What the delegate now holds is no longer only local: it is saved, whichever
                // of the read and the end of the save came first.
                let confirmed: Vec<EntryKey> = self
                    .local
                    .keys()
                    .filter(|k| self.entries.contains_key(*k))
                    .copied()
                    .collect();
                for key in confirmed {
                    if let Some(entry) = self.local.remove(&key) {
                        self.keep_saved(key, entry);
                    }
                }
                if began_at == self.save_seq {
                    self.note_lost();
                }
            }
            DmRead::Unreadable => self.hydrated = false,
            DmRead::NotListed => {
                // A trustworthy list without the key says it is gone, which a value read as
                // empty ([`super::dm_store::Refusal::Vanished`]) waits on.
                if list_authoritative {
                    self.listed = false;
                }
                if !(list_authoritative && !self.hydrated && self.entries.is_empty()) {
                    return;
                }
                self.hydrated = true;
            }
        }
        self.epoch = epoch;
    }

    /// Changes whenever a save ends well.
    pub fn save_seq(&self) -> u64 {
        self.save_seq
    }

    /// Hold a text the delegate has, bounded by [`MAX_LOCAL`].
    fn keep_saved(&mut self, key: EntryKey, entry: SentDm) {
        if self.saved.len() >= MAX_LOCAL && !self.saved.contains_key(&key) {
            // Prefer dropping the oldest text the delegate holds (it is not the only copy);
            // else the oldest of all.
            let entries = &self.entries;
            let oldest = |held: bool| {
                self.saved
                    .iter()
                    .filter(|(k, _)| !held || entries.contains_key(*k))
                    .min_by_key(|(_, e)| e.timestamp)
                    .map(|(k, _)| *k)
            };
            if let Some(oldest) = oldest(true).or_else(|| oldest(false)) {
                self.saved.remove(&oldest);
                self.resaves.remove(&oldest);
            }
        }
        self.saved.insert(key, entry);
    }

    /// True while the last read that proved anything succeeded. A writer must hydrate first and
    /// refuse to write while this is false.
    pub fn hydrated(&self) -> bool {
        self.hydrated
    }

    /// Changes whenever a read changes what the cache holds.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The generation the stored value had, when the bytes carried it. 0 on the session's read,
    /// which does not return it (see the module docs).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// What the local user sent as the message with `token` to `recipient` in the room owned by
    /// `room`.
    pub fn entry(
        &self,
        room: &VerifyingKey,
        recipient: MemberId,
        token: &PurgeToken,
    ) -> Option<&SentDm> {
        let key = (room.to_bytes(), recipient, *token);
        self.entries
            .get(&key)
            .or_else(|| self.local.get(&key))
            .or_else(|| self.saved.get(&key))
    }

    /// Keep the text of a message this app just sent, in memory only (see the module docs).
    /// `sender` is the user's member id and `timestamp` the message's, as the delegate's own
    /// entry would hold. Bounded: past [`MAX_LOCAL`] the oldest is dropped, which is what the
    /// return value says (the caller logs it: this module never prints).
    pub fn remember_sent(
        &mut self,
        room: &VerifyingKey,
        recipient: MemberId,
        token: PurgeToken,
        sender: MemberId,
        timestamp: u64,
        text: String,
    ) -> bool {
        let mut dropped = false;
        if self.local.len() >= MAX_LOCAL
            && let Some(oldest) = self
                .local
                .iter()
                .min_by_key(|(_, e)| e.timestamp)
                .map(|(k, _)| *k)
        {
            self.local.remove(&oldest);
            self.resaves.remove(&oldest);
            dropped = true;
        }
        self.local.insert(
            (room.to_bytes(), recipient, token),
            SentDm {
                sender,
                timestamp,
                plaintext: Plaintext(text),
            },
        );
        self.epoch = self.epoch.wrapping_add(1);
        dropped
    }

    /// Drop what [`OutboundCache::remember_sent`] kept for this message (a send the user
    /// discarded). True when there was one.
    pub fn forget_sent(
        &mut self,
        room: &VerifyingKey,
        recipient: MemberId,
        token: &PurgeToken,
    ) -> bool {
        let gone = self
            .local
            .remove(&(room.to_bytes(), recipient, *token))
            .is_some();
        if gone {
            self.epoch = self.epoch.wrapping_add(1);
        }
        gone
    }

    /// Drop the texts kept in memory for rooms `keep` says are no longer here (a room the user
    /// left): what was written there is not kept past it.
    pub fn keep_sent_rooms(&mut self, keep: impl Fn(&[u8; 32]) -> bool) {
        let before = self.local.len();
        self.local.retain(|(room, _, _), _| keep(room));
        self.saved.retain(|(room, _, _), _| keep(room));
        self.resaves.retain(|(room, _, _), _| keep(room));
        if self.local.len() != before {
            self.epoch = self.epoch.wrapping_add(1);
        }
    }

    /// Whether the delegate was found to hold a value under the key at the last read that
    /// decoded one.
    pub fn listed(&self) -> bool {
        self.listed
    }

    /// The texts held only in memory, oldest first, each with whether it is one being put back
    /// after a read found it gone from the delegate (rather than a message just sent).
    pub fn unsaved(&self) -> Vec<(EntryKey, &SentDm, bool)> {
        let mut out: Vec<_> = self
            .local
            .iter()
            .map(|(k, e)| (*k, e, self.resaves.contains_key(k)))
            .collect();
        out.sort_by_key(|(k, e, _)| (e.timestamp, k.0, k.1.0.0, k.2.0));
        out
    }

    /// Whether this message's text is in the delegate's store, as far as the last read and this
    /// app's own saves say. A text that was lost and given up on (see [`MAX_RESAVES`]) is held
    /// in memory only and is not saved. (Nothing outside the tests asks yet: the thread view
    /// does not show a saved state.)
    pub fn is_saved(&self, room: &VerifyingKey, recipient: MemberId, token: &PurgeToken) -> bool {
        let key = (room.to_bytes(), recipient, *token);
        if self.entries.contains_key(&key) {
            return true;
        }
        let given_up = self.resaves.get(&key).is_some_and(|n| *n >= MAX_RESAVES);
        self.saved.contains_key(&key) && !given_up
    }

    /// A save stored `added` (they were in the delegate's store when it ended) and dropped
    /// `evicted` to stay in bounds. The saved texts leave [`OutboundCache::remember_sent`]'s
    /// set and are kept apart, so a later read that does not find one puts it back
    /// ([`OutboundCache::unsaved`]). The text shown does not change.
    pub fn note_saved(&mut self, added: &[EntryKey], evicted: &[EntryKey]) {
        self.save_seq = self.save_seq.wrapping_add(1);
        for key in added {
            // Idempotent: a read may have confirmed the entry first and moved it already.
            if let Some(entry) = self.local.remove(key) {
                self.keep_saved(*key, entry);
            }
        }
        for key in evicted {
            self.local.remove(key);
            self.saved.remove(key);
            self.resaves.remove(key);
        }
    }

    /// Stop putting an entry back: its text stays in memory, held as saved, for the session.
    pub fn stop_resaving(&mut self, key: &EntryKey) {
        if let Some(entry) = self.local.remove(key) {
            self.keep_saved(*key, entry);
            self.resaves.insert(*key, MAX_RESAVES);
        }
    }

    /// Drop what [`OutboundCache::note_saved`] kept for this message (a send the user
    /// discarded). True when there was one.
    pub fn forget_saved(
        &mut self,
        room: &VerifyingKey,
        recipient: MemberId,
        token: &PurgeToken,
    ) -> bool {
        let key = (room.to_bytes(), recipient, *token);
        self.resaves.remove(&key);
        self.saved.remove(&key).is_some()
    }

    /// Drop every text `named` names (the messages their recipient deleted), wherever it is
    /// held, and so never put one back. Returns each key dropped, with whether the delegate's
    /// store holds it or was given it (`entries` or `saved`), which still has to be removed from
    /// it ([`super::dm_save`]). A key held only as unsaved text may still be in a write now
    /// working: the caller checks that. **Only what a purge names**: a message missing from the
    /// room says nothing, because an early read finds the room's messages not yet caught up.
    pub fn prune_named(&mut self, named: impl Fn(&EntryKey) -> bool) -> Vec<(EntryKey, bool)> {
        let keys: std::collections::HashSet<EntryKey> = self
            .entries
            .keys()
            .chain(self.saved.keys())
            .chain(self.local.keys())
            .filter(|k| named(k))
            .copied()
            .collect();
        let mut dropped = Vec::new();
        for key in &keys {
            let stored = self.entries.remove(key).is_some();
            let saved = self.saved.remove(key).is_some();
            self.local.remove(key);
            self.resaves.remove(key);
            dropped.push((*key, stored || saved));
        }
        if !keys.is_empty() {
            self.epoch = self.epoch.wrapping_add(1);
        }
        dropped
    }

    /// After a read: what this app saved and the delegate no longer holds was overwritten (the
    /// web UI saves its whole copy blind). It goes back to being unsaved, to be saved again, up
    /// to [`MAX_RESAVES`] times. After that the text stays in memory for the session (the row
    /// keeps showing it) and is not saved again.
    fn note_lost(&mut self) {
        let lost: Vec<EntryKey> = self
            .saved
            .keys()
            .filter(|k| !self.entries.contains_key(*k))
            .copied()
            .collect();
        for key in lost {
            let times = self.resaves.entry(key).or_insert(0);
            if *times >= MAX_RESAVES {
                continue;
            }
            *times += 1;
            if let Some(entry) = self.saved.remove(&key) {
                self.local.insert(key, entry);
            }
        }
    }

    /// How many entries of the same pair (room, sender, recipient) this cache holds that are
    /// newer than the message with this timestamp and token: what says whether the message is
    /// inside the pair's cap window ([`MAX_DM_MESSAGES_PER_PAIR`]).
    pub fn newer_in_pair(&self, key: &EntryKey, sender: MemberId, timestamp: u64) -> usize {
        let mut seen: std::collections::HashSet<EntryKey> = std::collections::HashSet::new();
        for (k, e) in self.entries.iter().chain(&self.local).chain(&self.saved) {
            if k.0 == key.0
                && k.1 == key.1
                && e.sender == sender
                && (e.timestamp, k.2.0) > (timestamp, key.2.0)
            {
                seen.insert(*k);
            }
        }
        seen.len()
    }

    /// How many sent texts are held only in memory.
    pub fn sent_this_session(&self) -> usize {
        self.local.len()
    }

    /// The text of [`OutboundCache::entry`].
    pub fn plaintext(
        &self,
        room: &VerifyingKey,
        recipient: MemberId,
        token: &PurgeToken,
    ) -> Option<&str> {
        self.entry(room, recipient, token)
            .map(|e| e.plaintext.as_str())
    }

    /// The archive list, as stored (as last read, and as this app's own writes since changed
    /// it). What the user asked for and the delegate does not hold yet is not in it: see
    /// [`OutboundCache::archived`].
    pub fn hidden_threads(&self) -> &[HiddenDmThreadEntry] {
        &self.hidden
    }

    /// The archive list as the user should see it: [`OutboundCache::hidden_threads`] with the
    /// changes still owed applied, one cutoff per thread (the largest, as the web UI keeps for a
    /// value that holds a thread twice), in a fixed order. **Recomputed on every call, from the
    /// last read**, so the list is the delegate's, not a copy of what this app once wrote: an
    /// entry the web UI added shows, and one it removed does not.
    pub fn archived(&self) -> Vec<(ThreadKey, u64)> {
        let mut list = self.hidden.clone();
        for op in &self.archive_owed {
            super::dm_archive::apply(&mut list, op);
        }
        let mut by_thread: HashMap<ThreadKey, u64> = HashMap::with_capacity(list.len());
        for h in &list {
            let ts = by_thread.entry((h.room_owner_vk, h.peer)).or_insert(0);
            *ts = (*ts).max(h.hidden_at_ts);
        }
        let mut out: Vec<(ThreadKey, u64)> = by_thread.into_iter().collect();
        out.sort_by_key(|((room, peer), _)| (*room, peer.0.0));
        out
    }

    /// The cutoff the thread is archived at, as [`OutboundCache::archived`] holds it.
    pub fn archived_at(&self, thread: &ThreadKey) -> Option<u64> {
        let mut list = self.hidden.clone();
        for op in &self.archive_owed {
            super::dm_archive::apply(&mut list, op);
        }
        super::dm_archive::stored_cutoff(&list, thread)
    }

    /// The user archived or unarchived a thread. It shows at once, and is owed to the delegate
    /// ([`OutboundCache::archive_owed`]). A later change to the same thread replaces this one
    /// (the last thing the user did is what stays). Bounded: one op per thread, and the list
    /// cannot outgrow the threads the user has.
    pub fn archive(&mut self, op: ArchiveOp) {
        let before = self.archived();
        let thread = op.thread();
        // The user acts on it again: revival may look at it afresh.
        self.revival_held.remove(&thread);
        self.archive_owed.retain(|o| o.thread() != thread);
        self.archive_owed.push(op);
        if self.archived() != before {
            self.archive_rev = self.archive_rev.wrapping_add(1);
        }
    }

    /// The archive changes the delegate does not hold yet, for the next write.
    pub fn archive_owed(&self) -> &[ArchiveOp] {
        &self.archive_owed
    }

    /// A write that carried `done` ended well: the delegate holds them. They leave the owed list
    /// (only the same ones: a newer change to a thread stays owed) and are applied to the list as
    /// stored, so the view does not change under the user.
    ///
    /// `skipped` are the ones among `done` that found the store already as asked or at a higher
    /// cutoff. A skipped conditional removal is not applied to the list (the store still holds an
    /// entry this copy then lacks); the read that follows shows what it holds. The others are
    /// applied: they leave the store as the copy now has it, or higher.
    pub fn note_archive_saved(&mut self, done: &[ArchiveOp], skipped: &[ArchiveOp]) {
        let before = self.archived();
        // A save ended: a read that began earlier says nothing about it ([`Self::note_read_from`]).
        self.save_seq = self.save_seq.wrapping_add(1);
        for op in done {
            if self.archive_owed.contains(op) {
                self.archive_owed.retain(|o| o != op);
            }
            if matches!(op, ArchiveOp::UnhideIfPast { .. }) && skipped.contains(op) {
                self.revival_held.insert(op.thread());
                continue;
            }
            super::dm_archive::apply(&mut self.hidden, op);
            self.archive_recent.push((self.save_seq, *op));
        }
        if self.archive_recent.len() > MAX_RECENT_ARCHIVE {
            let extra = self.archive_recent.len() - MAX_RECENT_ARCHIVE;
            self.archive_recent.drain(..extra);
        }
        if self.archived() != before {
            self.archive_rev = self.archive_rev.wrapping_add(1);
        }
    }

    /// A write left these out because their room is not shown: nothing was written for them, and
    /// they are not owed any more (and not applied: the room is gone).
    pub fn note_archive_dropped(&mut self, dropped: &[ArchiveOp]) {
        let before = self.archived();
        self.archive_owed.retain(|o| !dropped.contains(o));
        if self.archived() != before {
            self.archive_rev = self.archive_rev.wrapping_add(1);
        }
    }

    /// Whether revival leaves this thread alone until the next read ([`Self::note_archive_saved`]).
    pub fn revival_held(&self, thread: &ThreadKey) -> bool {
        self.revival_held.contains(thread)
    }

    /// Changes whenever [`OutboundCache::archived`] does.
    pub fn archive_rev(&self) -> u64 {
        self.archive_rev
    }

    /// Add an entry, for tests that build a cache without the delegate.
    #[cfg(test)]
    pub(crate) fn insert_for_test(
        &mut self,
        room: &VerifyingKey,
        recipient: MemberId,
        token: PurgeToken,
        text: &str,
    ) {
        self.entries.insert(
            (room.to_bytes(), recipient, token),
            SentDm {
                sender: recipient,
                timestamp: 0,
                plaintext: Plaintext(text.to_string()),
            },
        );
        self.hydrated = true;
        self.epoch = self.epoch.wrapping_add(1);
    }
}

/// What went wrong decoding, without the decoder's own text: serde's messages for a value of
/// the wrong type quote the value, and these values are what people wrote.
fn describe(e: &ciborium::de::Error<std::io::Error>) -> String {
    use ciborium::de::Error;
    let what = match e {
        Error::Io(_) => "could not be read".to_string(),
        Error::Syntax(at) => format!("is not valid CBOR (at byte {at})"),
        Error::Semantic(at, _) => match at {
            Some(at) => format!("is not a sent-messages store (at byte {at})"),
            None => "is not a sent-messages store".to_string(),
        },
        Error::RecursionLimitExceeded => "is nested too deeply".to_string(),
    };
    format!("the stored sent messages {what}")
}

/// The generation and the payload of stored bytes: the delegate's `decode_versioned`.
fn strip_envelope(bytes: &[u8]) -> (u64, &[u8]) {
    if bytes.len() >= ENVELOPE_HEADER && bytes[0] == ENVELOPE_TAG {
        let mut generation = [0u8; 8];
        generation.copy_from_slice(&bytes[1..ENVELOPE_HEADER]);
        (u64::from_le_bytes(generation), &bytes[ENVELOPE_HEADER..])
    } else {
        (0, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::chat_delegate::OutboundDmEntry;

    fn vk(n: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[n; 32]).verifying_key()
    }

    fn id(n: u8) -> MemberId {
        MemberId::from(&vk(n))
    }

    fn entry(room: u8, recipient: u8, token: u8, text: &str) -> OutboundDmEntry {
        OutboundDmEntry {
            room_owner_vk: vk(room).to_bytes(),
            sender: id(99),
            recipient: id(recipient),
            purge_token: PurgeToken([token; 16]),
            timestamp: 1_700_000_000,
            plaintext: text.to_string(),
        }
    }

    fn encode(store: &OutboundDmStore) -> Vec<u8> {
        let mut out = Vec::new();
        ciborium::ser::into_writer(store, &mut out).unwrap();
        out
    }

    fn enveloped(generation: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![ENVELOPE_TAG];
        out.extend_from_slice(&generation.to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn a_bare_and_an_enveloped_value_decode_to_the_same_cache() {
        let store = OutboundDmStore {
            entries: vec![entry(1, 2, 7, "hello"), entry(1, 3, 8, "again")],
            hidden_threads: vec![HiddenDmThreadEntry {
                room_owner_vk: vk(1).to_bytes(),
                peer: id(2),
                hidden_at_ts: 5,
            }],
        };
        let bytes = encode(&store);
        let bare = OutboundCache::from_bytes(&bytes).unwrap();
        let wrapped = OutboundCache::from_bytes(&enveloped(41, &bytes)).unwrap();
        assert_eq!(bare.len(), 2);
        assert_eq!(
            bare.plaintext(&vk(1), id(2), &PurgeToken([7; 16])),
            Some("hello")
        );
        assert_eq!(
            bare.plaintext(&vk(1), id(3), &PurgeToken([8; 16])),
            Some("again")
        );
        // A different room, recipient or token is a miss.
        assert_eq!(bare.plaintext(&vk(9), id(2), &PurgeToken([7; 16])), None);
        assert_eq!(bare.plaintext(&vk(1), id(3), &PurgeToken([7; 16])), None);
        assert_eq!(bare.hidden_threads().len(), 1);
        assert!(bare.hydrated() && wrapped.hydrated());
        assert_eq!((bare.generation(), wrapped.generation()), (0, 41));
        assert_eq!(bare.entries, wrapped.entries);
    }

    #[test]
    fn a_text_sent_this_session_shows_and_survives_a_read_of_the_store() {
        let mut cache = OutboundCache::default();
        let before = cache.epoch();
        cache.remember_sent(
            &vk(1),
            id(2),
            PurgeToken([7; 16]),
            id(99),
            5,
            "just sent".to_owned(),
        );
        assert_eq!(
            cache.plaintext(&vk(1), id(2), &PurgeToken([7; 16])),
            Some("just sent")
        );
        assert_ne!(cache.epoch(), before, "a view built from it is rebuilt");
        assert!(!cache.hydrated(), "it says nothing of what the store holds");
        assert_eq!(cache.sent_this_session(), 1);
        // Not what a read of the store returned, so a read does not take it away.
        let store = OutboundDmStore {
            entries: vec![entry(1, 2, 8, "from the store")],
            hidden_threads: vec![],
        };
        let read = OutboundCache::from_bytes(&encode(&store)).unwrap();
        cache.note_read(DmRead::Read(Box::new(read)), true);
        assert!(cache.hydrated());
        assert_eq!(
            cache.plaintext(&vk(1), id(2), &PurgeToken([7; 16])),
            Some("just sent")
        );
        assert_eq!(
            cache.plaintext(&vk(1), id(2), &PurgeToken([8; 16])),
            Some("from the store")
        );
        // Once the store holds it, the store's copy is the one and the local one is dropped.
        let store = OutboundDmStore {
            entries: vec![entry(1, 2, 7, "saved")],
            hidden_threads: vec![],
        };
        let read = OutboundCache::from_bytes(&encode(&store)).unwrap();
        cache.note_read(DmRead::Read(Box::new(read)), true);
        assert_eq!(cache.sent_this_session(), 0);
        assert_eq!(
            cache.plaintext(&vk(1), id(2), &PurgeToken([7; 16])),
            Some("saved")
        );
        // Forgetting is for a discarded message, and only of what this session held.
        cache.remember_sent(&vk(1), id(2), PurgeToken([9; 16]), id(99), 6, "x".into());
        assert!(cache.forget_sent(&vk(1), id(2), &PurgeToken([9; 16])));
        assert!(!cache.forget_sent(&vk(1), id(2), &PurgeToken([9; 16])));
        assert!(!cache.forget_sent(&vk(1), id(2), &PurgeToken([8; 16])));
        // A room the user left takes the texts written in it.
        cache.remember_sent(
            &vk(5),
            id(2),
            PurgeToken([11; 16]),
            id(99),
            8,
            "gone".into(),
        );
        let epoch = cache.epoch();
        cache.keep_sent_rooms(|room| room != &vk(5).to_bytes());
        assert!(
            cache
                .plaintext(&vk(5), id(2), &PurgeToken([11; 16]))
                .is_none()
        );
        assert_ne!(cache.epoch(), epoch);
        cache.keep_sent_rooms(|_| true);
        // Debug shows counts only.
        cache.remember_sent(
            &vk(1),
            id(2),
            PurgeToken([10; 16]),
            id(99),
            7,
            "SECRET".into(),
        );
        assert!(!format!("{cache:?}").contains("SECRET"));
    }

    #[test]
    fn the_texts_held_only_in_memory_are_bounded() {
        let mut cache = OutboundCache::default();
        for n in 0..(MAX_LOCAL + 50) {
            let mut token = [0u8; 16];
            token[..8].copy_from_slice(&(n as u64).to_le_bytes());
            cache.remember_sent(
                &vk(1),
                id(2),
                PurgeToken(token),
                id(99),
                n as u64,
                "t".into(),
            );
        }
        assert_eq!(cache.sent_this_session(), MAX_LOCAL);
        // The oldest went.
        let mut oldest = [0u8; 16];
        oldest[..8].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(cache.plaintext(&vk(1), id(2), &PurgeToken(oldest)), None);
    }

    #[test]
    fn no_bytes_is_an_error_and_a_store_from_before_archiving_decodes() {
        // A listed key with no value is a failed read, not a store with nothing in it.
        assert!(OutboundCache::from_bytes(&[]).is_err());
        assert!(OutboundCache::from_bytes(&enveloped(3, &[])).is_err());
        // Written before `hidden_threads` existed: the field is absent.
        #[derive(serde::Serialize)]
        struct Old {
            entries: Vec<OutboundDmEntry>,
        }
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(
            &Old {
                entries: vec![entry(1, 2, 7, "x")],
            },
            &mut bytes,
        )
        .unwrap();
        let old = OutboundCache::from_bytes(&bytes).unwrap();
        assert_eq!(old.len(), 1);
        assert!(old.hidden_threads().is_empty());
        // A store that decodes and holds nothing is fine.
        let none = OutboundCache::from_bytes(&encode(&OutboundDmStore::default())).unwrap();
        assert!(none.is_empty() && none.hydrated());
    }

    #[test]
    fn the_full_entry_is_kept_and_a_duplicate_keeps_the_later_one() {
        let mut older = entry(1, 2, 7, "older");
        older.timestamp = 10;
        let mut newer = entry(1, 2, 7, "newer");
        newer.timestamp = 20;
        let mut tie = entry(1, 2, 8, "first");
        tie.timestamp = 5;
        let mut tie_later = entry(1, 2, 8, "second");
        tie_later.timestamp = 5;
        for order in [
            vec![older.clone(), newer.clone()],
            vec![newer.clone(), older.clone()],
        ] {
            let store = OutboundDmStore {
                entries: [order, vec![tie.clone(), tie_later.clone()]].concat(),
                hidden_threads: vec![],
            };
            let cache = OutboundCache::from_bytes(&encode(&store)).unwrap();
            let kept = cache.entry(&vk(1), id(2), &PurgeToken([7; 16])).unwrap();
            assert_eq!((kept.timestamp, kept.sender), (20, id(99)));
            assert_eq!(kept.plaintext.as_str(), "newer");
            let kept = cache.entry(&vk(1), id(2), &PurgeToken([8; 16])).unwrap();
            assert_eq!(kept.plaintext.as_str(), "first");
        }
    }

    #[test]
    fn a_value_that_does_not_decode_is_an_error_that_does_not_echo_it() {
        // A string where an entry is expected: serde's own message would quote it.
        #[derive(serde::Serialize)]
        struct Wrong {
            entries: Vec<&'static str>,
        }
        let mut secret = Vec::new();
        ciborium::ser::into_writer(
            &Wrong {
                entries: vec!["THE-SECRET-TEXT"],
            },
            &mut secret,
        )
        .unwrap();
        let err = OutboundCache::from_bytes(&secret).unwrap_err().to_string();
        assert!(err.contains("not a sent-messages store"), "{err}");
        assert!(!err.contains("THE-SECRET-TEXT"), "{err}");
        let huge = vec![0u8; MAX_STORE_BYTES + 1];
        assert!(OutboundCache::from_bytes(&huge).is_err());
    }

    /// Hostile or corrupt CBOR: whatever it declares, it is an error or a store, never a panic
    /// and never an allocation of the declared size.
    #[test]
    fn adversarial_cbor_is_an_error_or_a_store_and_never_a_panic() {
        let mut cases: Vec<Vec<u8>> = vec![
            // A map whose `entries` array declares 2^64-1 items, then ends.
            [
                vec![0xa1, 0x67],
                b"entries".to_vec(),
                vec![0x9b],
                vec![0xff; 8],
            ]
            .concat(),
            // An array of 2^32 items.
            [
                vec![0xa1, 0x67],
                b"entries".to_vec(),
                vec![0x9a, 0xff, 0xff, 0xff, 0xff],
            ]
            .concat(),
            // A byte string and a text string declaring 2^64-1 bytes.
            [
                vec![0xa1, 0x67],
                b"entries".to_vec(),
                vec![0x5b],
                vec![0xff; 8],
            ]
            .concat(),
            [
                vec![0xa1, 0x67],
                b"entries".to_vec(),
                vec![0x7b],
                vec![0xff; 8],
            ]
            .concat(),
            // Deep nesting: 100000 arrays inside one another.
            [vec![0xa1, 0x67], b"entries".to_vec(), vec![0x81; 100_000]].concat(),
            // The same key twice.
            [
                vec![0xa2, 0x67],
                b"entries".to_vec(),
                vec![0x80, 0x67],
                b"entries".to_vec(),
                vec![0x80],
            ]
            .concat(),
            // An indefinite-length map that never ends.
            vec![0xbf, 0x67, b'e', b'n', b't', b'r', b'i', b'e', b's'],
            // Only a tag.
            vec![0xc0],
            vec![0xff],
        ];
        // The duplicate key with a real entry in each.
        let one = |text: &str| {
            let mut v = Vec::new();
            ciborium::ser::into_writer(&vec![entry(1, 2, 7, text)], &mut v).unwrap();
            v
        };
        cases.push(
            [
                vec![0xa2, 0x67],
                b"entries".to_vec(),
                one("first"),
                vec![0x67],
                b"entries".to_vec(),
                one("second"),
            ]
            .concat(),
        );
        for case in &cases {
            let _ = OutboundCache::from_bytes(case);
            let _ = OutboundCache::from_bytes(&enveloped(1, case));
        }
        // Never panics on arbitrary short bytes either.
        for len in 0..40usize {
            let junk: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let _ = OutboundCache::from_bytes(&junk);
            let _ = OutboundCache::from_bytes(&enveloped(1, &junk));
        }
    }

    fn hydrated_with(text: &str) -> OutboundCache {
        let store = OutboundDmStore {
            entries: vec![entry(1, 2, 7, text)],
            hidden_threads: vec![],
        };
        OutboundCache::from_bytes(&encode(&store)).unwrap()
    }

    #[test]
    fn the_latch_follows_reads_that_prove_something() {
        let mut cache = OutboundCache::default();
        assert!(!cache.hydrated(), "a new cache has read nothing");
        // Not listed, and the list not to be trusted: nothing is known.
        cache.note_read(DmRead::NotListed, false);
        assert!(!cache.hydrated());
        cache.note_read(DmRead::Read(Box::new(hydrated_with("kept"))), false);
        assert!(cache.hydrated());
        // An unreadable value keeps the entries for labelling rows but unlatches.
        cache.note_read(DmRead::Unreadable, true);
        assert!(!cache.hydrated());
        assert_eq!(
            cache.plaintext(&vk(1), id(2), &PurgeToken([7; 16])),
            Some("kept")
        );
        cache.note_read(DmRead::Read(Box::new(hydrated_with("again"))), false);
        assert!(cache.hydrated());
    }

    #[test]
    fn a_key_not_listed_never_wipes_or_unlatches_a_hydrated_cache() {
        let mut cache = OutboundCache::default();
        cache.note_read(DmRead::Read(Box::new(hydrated_with("kept"))), true);
        let epoch = cache.epoch();
        for authoritative in [false, true] {
            cache.note_read(DmRead::NotListed, authoritative);
            assert!(cache.hydrated());
            assert_eq!(cache.len(), 1);
            assert_eq!(cache.epoch(), epoch, "nothing changed");
        }
    }

    #[test]
    fn a_key_not_listed_by_a_trustworthy_list_is_an_empty_hydrated_cache() {
        let mut cache = OutboundCache::default();
        cache.note_read(DmRead::NotListed, true);
        assert!(cache.hydrated() && cache.is_empty());
        // An unreadable read, then a trustworthy absence, does not hydrate over kept entries.
        let mut cache = OutboundCache::default();
        cache.note_read(DmRead::Read(Box::new(hydrated_with("x"))), true);
        cache.note_read(DmRead::Unreadable, true);
        cache.note_read(DmRead::NotListed, true);
        assert!(!cache.hydrated());
    }

    #[test]
    fn debug_output_never_contains_what_was_written() {
        let cache = hydrated_with("TOP-SECRET-WORDS");
        let kept = cache.entry(&vk(1), id(2), &PurgeToken([7; 16])).unwrap();
        let shown = format!(
            "{cache:?} {kept:?} {:?}",
            Plaintext("TOP-SECRET-WORDS".into())
        );
        assert!(!shown.contains("TOP-SECRET"), "{shown}");
        assert!(shown.contains("entries: 1"));
    }

    fn key_of(room: u8, recipient: u8, token: u8) -> EntryKey {
        (vk(room).to_bytes(), id(recipient), PurgeToken([token; 16]))
    }

    fn sent(cache: &mut OutboundCache, room: u8, recipient: u8, token: u8, ts: u64, text: &str) {
        cache.remember_sent(
            &vk(room),
            id(recipient),
            PurgeToken([token; 16]),
            id(99),
            ts,
            text.to_string(),
        );
    }

    /// A cache that read an empty store, as the delegate returned it.
    fn read_empty() -> OutboundCache {
        let mut cache = OutboundCache::default();
        let store = OutboundDmStore::default();
        let read = OutboundCache::from_bytes(&encode(&store)).unwrap();
        cache.note_read(DmRead::Read(Box::new(read)), true);
        cache
    }

    #[test]
    fn a_saved_text_leaves_the_unsaved_set_and_is_still_shown() {
        let mut cache = read_empty();
        sent(&mut cache, 1, 2, 7, 10, "mine");
        sent(&mut cache, 1, 2, 8, 20, "also mine");
        assert_eq!(cache.sent_this_session(), 2);
        assert!(!cache.is_saved(&vk(1), id(2), &PurgeToken([7; 16])));
        let epoch = cache.epoch();

        cache.note_saved(&[key_of(1, 2, 7)], &[]);
        assert_eq!(cache.sent_this_session(), 1);
        assert!(cache.is_saved(&vk(1), id(2), &PurgeToken([7; 16])));
        assert!(!cache.is_saved(&vk(1), id(2), &PurgeToken([8; 16])));
        assert_eq!(
            cache.plaintext(&vk(1), id(2), &PurgeToken([7; 16])),
            Some("mine")
        );
        assert_eq!(cache.epoch(), epoch, "what is shown did not change");
        let unsaved = cache.unsaved();
        assert_eq!(unsaved.len(), 1);
        assert_eq!(unsaved[0].0, key_of(1, 2, 8));
        assert!(!unsaved[0].2, "a message just sent is not a put-back");
    }

    #[test]
    fn unsaved_texts_are_listed_oldest_first() {
        let mut cache = read_empty();
        sent(&mut cache, 1, 2, 7, 30, "c");
        sent(&mut cache, 1, 2, 8, 10, "a");
        sent(&mut cache, 1, 2, 9, 20, "b");
        let texts: Vec<&str> = cache
            .unsaved()
            .iter()
            .map(|(_, e, _)| e.plaintext.as_str())
            .collect();
        assert_eq!(texts, ["a", "b", "c"]);
    }

    #[test]
    fn a_read_that_lost_a_saved_entry_makes_it_unsaved_again_a_bounded_number_of_times() {
        let mut cache = read_empty();
        sent(&mut cache, 1, 2, 7, 10, "mine");
        let k = key_of(1, 2, 7);
        let empty = || {
            DmRead::Read(Box::new(
                OutboundCache::from_bytes(&encode(&OutboundDmStore::default())).unwrap(),
            ))
        };
        let with = || {
            DmRead::Read(Box::new(
                OutboundCache::from_bytes(&encode(&OutboundDmStore {
                    entries: vec![entry(1, 2, 7, "mine")],
                    hidden_threads: vec![],
                }))
                .unwrap(),
            ))
        };
        for round in 1..=MAX_RESAVES {
            cache.note_saved(&[k], &[]);
            assert_eq!(cache.sent_this_session(), 0);
            // A read that holds it: nothing to do, and the count stays.
            cache.note_read(with(), true);
            assert_eq!(cache.sent_this_session(), 0, "round {round}");
            // A read that does not: put back, as a put-back.
            cache.note_read(empty(), true);
            assert_eq!(cache.sent_this_session(), 1, "round {round}");
            assert!(cache.unsaved()[0].2);
            assert_eq!(
                cache.plaintext(&vk(1), id(2), &PurgeToken([7; 16])),
                Some("mine")
            );
        }
        // The next loss is the last put-back: the text stays in memory for the session, shown, and
        // is not saved again.
        cache.note_saved(&[k], &[]);
        cache.note_read(empty(), true);
        assert_eq!(cache.sent_this_session(), 0);
        assert_eq!(
            cache.plaintext(&vk(1), id(2), &PurgeToken([7; 16])),
            Some("mine")
        );
        cache.note_read(empty(), true);
        assert_eq!(cache.sent_this_session(), 0, "and still not");
    }

    #[test]
    fn an_unreadable_or_unlisted_read_loses_nothing_that_was_saved() {
        let mut cache = read_empty();
        sent(&mut cache, 1, 2, 7, 10, "mine");
        cache.note_saved(&[key_of(1, 2, 7)], &[]);
        cache.note_read(DmRead::Unreadable, true);
        cache.note_read(DmRead::NotListed, true);
        assert_eq!(
            cache.sent_this_session(),
            0,
            "not put back for a read that proved nothing"
        );
        assert!(cache.is_saved(&vk(1), id(2), &PurgeToken([7; 16])));
    }

    #[test]
    fn evicted_and_discarded_entries_are_forgotten_everywhere() {
        let mut cache = read_empty();
        sent(&mut cache, 1, 2, 7, 10, "a");
        sent(&mut cache, 1, 2, 8, 20, "b");
        sent(&mut cache, 1, 2, 9, 30, "c");
        cache.note_saved(&[key_of(1, 2, 7), key_of(1, 2, 8)], &[key_of(1, 2, 7)]);
        assert_eq!(cache.plaintext(&vk(1), id(2), &PurgeToken([7; 16])), None);
        assert!(cache.forget_saved(&vk(1), id(2), &PurgeToken([8; 16])));
        assert!(!cache.forget_saved(&vk(1), id(2), &PurgeToken([8; 16])));
        assert_eq!(cache.plaintext(&vk(1), id(2), &PurgeToken([8; 16])), None);
        // Stopped being put back: the text stays, held as saved.
        cache.stop_resaving(&key_of(1, 2, 9));
        assert_eq!(cache.sent_this_session(), 0);
        assert_eq!(
            cache.plaintext(&vk(1), id(2), &PurgeToken([9; 16])),
            Some("c")
        );
    }

    #[test]
    fn a_room_that_is_gone_takes_its_saved_texts_with_it() {
        let mut cache = read_empty();
        sent(&mut cache, 1, 2, 7, 10, "a");
        sent(&mut cache, 3, 2, 8, 20, "b");
        cache.note_saved(&[key_of(1, 2, 7), key_of(3, 2, 8)], &[]);
        cache.keep_sent_rooms(|room| *room == vk(1).to_bytes());
        assert!(cache.is_saved(&vk(1), id(2), &PurgeToken([7; 16])));
        assert!(!cache.is_saved(&vk(3), id(2), &PurgeToken([8; 16])));
    }

    #[test]
    fn the_cache_knows_whether_the_key_held_a_value() {
        let mut cache = OutboundCache::default();
        assert!(!cache.listed());
        cache.note_read(DmRead::NotListed, true);
        assert!(
            !cache.listed(),
            "hydrated by an absent key: nothing was listed"
        );
        cache.note_read(DmRead::Read(Box::new(hydrated_with("x"))), true);
        assert!(cache.listed());
        // A later unreadable read, or a list that cannot be trusted, does not say it went away.
        cache.note_read(DmRead::Unreadable, true);
        cache.note_read(DmRead::NotListed, false);
        assert!(cache.listed());
        // A trustworthy list without the key does: a write may create it again.
        cache.note_read(DmRead::NotListed, true);
        assert!(!cache.listed());
    }

    fn with_entry() -> DmRead {
        DmRead::Read(Box::new(
            OutboundCache::from_bytes(&encode(&OutboundDmStore {
                entries: vec![entry(1, 2, 7, "mine")],
                hidden_threads: vec![],
            }))
            .unwrap(),
        ))
    }

    fn without_entry() -> DmRead {
        DmRead::Read(Box::new(
            OutboundCache::from_bytes(&encode(&OutboundDmStore::default())).unwrap(),
        ))
    }

    /// The read that shows a save landed can come before or after the save's end is taken, and
    /// the text is tracked as saved either way.
    #[test]
    fn a_read_that_confirms_an_entry_before_the_save_ends_tracks_it_as_saved() {
        let k = key_of(1, 2, 7);
        let lost_later = |cache: &mut OutboundCache| {
            cache.note_read(without_entry(), true);
            assert_eq!(cache.sent_this_session(), 1, "put back after being lost");
            assert_eq!(
                cache.plaintext(&vk(1), id(2), &PurgeToken([7; 16])),
                Some("mine")
            );
        };

        // The read first: the entry was in `local`, the read holds it.
        let mut cache = read_empty();
        sent(&mut cache, 1, 2, 7, 10, "mine");
        cache.note_read(with_entry(), true);
        assert_eq!(cache.sent_this_session(), 0);
        assert!(cache.is_saved(&vk(1), id(2), &PurgeToken([7; 16])));
        // Then the save's end arrives: nothing changes, and it is not an error.
        cache.note_saved(&[k], &[]);
        assert_eq!(cache.sent_this_session(), 0);
        // A web overwrite afterwards is noticed, and the entry put back.
        lost_later(&mut cache);

        // The end first, then the read: the same.
        let mut cache = read_empty();
        sent(&mut cache, 1, 2, 7, 10, "mine");
        cache.note_saved(&[k], &[]);
        cache.note_read(with_entry(), true);
        assert!(cache.is_saved(&vk(1), id(2), &PurgeToken([7; 16])));
        lost_later(&mut cache);

        // The end alone, twice: idempotent.
        let mut cache = read_empty();
        sent(&mut cache, 1, 2, 7, 10, "mine");
        cache.note_saved(&[k], &[]);
        cache.note_saved(&[k], &[]);
        assert_eq!(
            cache.plaintext(&vk(1), id(2), &PurgeToken([7; 16])),
            Some("mine")
        );
    }

    #[test]
    fn a_read_that_began_before_a_save_ended_does_not_count_that_save_as_lost() {
        let k = key_of(1, 2, 7);
        let mut cache = read_empty();
        sent(&mut cache, 1, 2, 7, 10, "mine");
        // The read begins; the save ends; the read, which cannot hold the entry, is applied.
        let began = cache.save_seq();
        cache.note_saved(&[k], &[]);
        assert_ne!(cache.save_seq(), began);
        cache.note_read_from(without_entry(), true, began);
        assert_eq!(cache.sent_this_session(), 0, "no put-back for a stale read");
        assert!(cache.is_saved(&vk(1), id(2), &PurgeToken([7; 16])));
        // The count was not spent: all of the puts-back are still there.
        for round in 1..=MAX_RESAVES {
            cache.note_read(without_entry(), true);
            assert_eq!(cache.sent_this_session(), 1, "round {round}");
            cache.note_saved(&[k], &[]);
        }
        cache.note_read(without_entry(), true);
        assert_eq!(cache.sent_this_session(), 0);
    }

    #[test]
    fn the_saved_set_is_bounded_and_forgets_the_counts_of_what_it_drops() {
        let mut cache = read_empty();
        for n in 0..=MAX_LOCAL as u64 {
            let token = (n % 200) as u8;
            let room = 1 + (n / 200) as u8;
            sent(&mut cache, room, 2, token, n, "x");
            cache.resaves.insert(key_of(room, 2, token), 1);
            cache.note_saved(&[key_of(room, 2, token)], &[]);
        }
        assert!(cache.saved.len() <= MAX_LOCAL);
        assert!(cache.resaves.keys().all(|k| cache.saved.contains_key(k)));
        // The oldest went, and its text with it.
        assert_eq!(cache.plaintext(&vk(1), id(2), &PurgeToken([0; 16])), None);
    }

    #[test]
    fn newer_entries_of_a_pair_are_counted_across_what_the_cache_holds() {
        let mut cache = read_empty();
        for token in 1..=5u8 {
            sent(&mut cache, 1, 2, token, 100 + u64::from(token), "x");
        }
        cache.note_saved(&[key_of(1, 2, 1), key_of(1, 2, 2)], &[]);
        // Another recipient and another room are other pairs, and so is another sender.
        sent(&mut cache, 1, 3, 9, 500, "other recipient");
        sent(&mut cache, 4, 2, 9, 500, "other room");
        let k = key_of(1, 2, 1);
        assert_eq!(cache.newer_in_pair(&k, id(99), 101), 4);
        assert_eq!(cache.newer_in_pair(&k, id(98), 101), 0);
        assert_eq!(cache.newer_in_pair(&key_of(1, 2, 5), id(99), 105), 0);
    }

    #[test]
    fn a_text_given_up_on_is_not_reported_saved_and_the_full_set_drops_held_texts_first() {
        let mut cache = read_empty();
        sent(&mut cache, 1, 2, 7, 10, "mine");
        for _ in 0..MAX_RESAVES {
            cache.note_saved(&[key_of(1, 2, 7)], &[]);
            cache.note_read(without_entry(), true);
        }
        cache.note_saved(&[key_of(1, 2, 7)], &[]);
        cache.note_read(without_entry(), true);
        // Held in memory only, shown, and not saved.
        assert_eq!(
            cache.plaintext(&vk(1), id(2), &PurgeToken([7; 16])),
            Some("mine")
        );
        assert!(!cache.is_saved(&vk(1), id(2), &PurgeToken([7; 16])));
        // A read that holds it again makes it saved.
        cache.note_read(with_entry(), true);
        assert!(cache.is_saved(&vk(1), id(2), &PurgeToken([7; 16])));

        // At the bound the oldest text the delegate holds goes before an older one that is the
        // only copy.
        let mut cache = read_empty();
        sent(&mut cache, 9, 2, 1, 1, "only copy");
        cache.note_saved(&[key_of(9, 2, 1)], &[]);
        cache.resaves.insert(key_of(9, 2, 1), MAX_RESAVES);
        sent(&mut cache, 1, 2, 7, 500, "held");
        cache.note_saved(&[key_of(1, 2, 7)], &[]);
        cache.note_read(with_entry(), true);
        let fill = |cache: &mut OutboundCache, n: u64| {
            let (room, token) = (20 + (n / 200) as u8, (n % 200) as u8);
            sent(cache, room, 2, token, 1000 + n, "x");
            cache.note_saved(&[key_of(room, 2, token)], &[]);
        };
        for n in 0..MAX_LOCAL as u64 - 2 {
            fill(&mut cache, n);
        }
        assert_eq!(cache.saved.len(), MAX_LOCAL);
        fill(&mut cache, MAX_LOCAL as u64);
        assert!(
            !cache.saved.contains_key(&key_of(1, 2, 7)),
            "the held one went"
        );
        assert_eq!(
            cache.plaintext(&vk(9), id(2), &PurgeToken([1; 16])),
            Some("only copy")
        );
        assert!(cache.saved.len() <= MAX_LOCAL);
    }

    #[test]
    fn debug_output_never_contains_a_saved_text() {
        let mut cache = read_empty();
        sent(&mut cache, 1, 2, 7, 10, "TOP-SECRET-WORDS");
        cache.note_saved(&[key_of(1, 2, 7)], &[]);
        let shown = format!("{cache:?}");
        assert!(!shown.contains("TOP-SECRET"));
        assert!(shown.contains("saved_this_session: 1"));
    }

    // The archive list ------------------------------------------------------------------------

    fn thread(room: u8, peer: u8) -> ThreadKey {
        (vk(room).to_bytes(), id(peer))
    }

    fn read_hidden(list: &[(u8, u8, u64)]) -> OutboundCache {
        let store = OutboundDmStore {
            entries: vec![],
            hidden_threads: list
                .iter()
                .map(|(r, p, ts)| HiddenDmThreadEntry {
                    room_owner_vk: vk(*r).to_bytes(),
                    peer: id(*p),
                    hidden_at_ts: *ts,
                })
                .collect(),
        };
        OutboundCache::from_bytes(&encode(&store)).unwrap()
    }

    #[test]
    fn the_archive_list_is_what_the_delegate_holds_with_the_users_changes_over_it() {
        let mut cache = read_hidden(&[(1, 2, 100), (1, 3, 200)]);
        assert_eq!(
            cache.archived(),
            vec![(thread(1, 2), 100), (thread(1, 3), 200)]
        );
        let rev = cache.archive_rev();
        // An archive shows at once, an unarchive too.
        cache.archive(ArchiveOp::Hide {
            thread: thread(1, 4),
            hidden_at_ts: 300,
        });
        cache.archive(ArchiveOp::Unhide {
            thread: thread(1, 2),
        });
        assert_eq!(
            cache.archived(),
            vec![(thread(1, 3), 200), (thread(1, 4), 300)]
        );
        assert_ne!(cache.archive_rev(), rev);
        // The stored list is untouched until a write ends.
        assert_eq!(cache.hidden_threads().len(), 2);
        assert_eq!(cache.archived_at(&thread(1, 4)), Some(300));
        assert_eq!(cache.archived_at(&thread(2, 4)), None);
    }

    #[test]
    fn one_change_is_owed_per_thread_and_the_last_wins() {
        let mut cache = read_hidden(&[]);
        for ts in [5, 6, 7] {
            cache.archive(ArchiveOp::Hide {
                thread: thread(1, 2),
                hidden_at_ts: ts,
            });
        }
        cache.archive(ArchiveOp::Unhide {
            thread: thread(1, 2),
        });
        cache.archive(ArchiveOp::Hide {
            thread: thread(1, 3),
            hidden_at_ts: 1,
        });
        assert_eq!(cache.archive_owed().len(), 2);
        assert_eq!(cache.archived(), vec![(thread(1, 3), 1)]);
    }

    #[test]
    fn a_read_replaces_the_list_and_keeps_what_is_still_owed() {
        let mut cache = read_hidden(&[(1, 2, 100)]);
        cache.archive(ArchiveOp::Hide {
            thread: thread(1, 4),
            hidden_at_ts: 300,
        });
        let rev = cache.archive_rev();
        // The web UI archived another thread and unarchived the first: a read shows exactly
        // that, plus what this app still owes.
        cache.note_read(DmRead::Read(Box::new(read_hidden(&[(1, 3, 50)]))), true);
        assert_eq!(
            cache.archived(),
            vec![(thread(1, 3), 50), (thread(1, 4), 300)]
        );
        assert_ne!(cache.archive_rev(), rev, "the view changed");
        // A read that changes nothing in the view does not bump the revision.
        let rev = cache.archive_rev();
        cache.note_read(DmRead::Read(Box::new(read_hidden(&[(1, 3, 50)]))), true);
        assert_eq!(cache.archive_rev(), rev);
        // An unreadable one or a key not listed leaves the list as it was.
        cache.note_read(DmRead::Unreadable, true);
        cache.note_read(DmRead::NotListed, true);
        assert_eq!(cache.archived().len(), 2);
    }

    #[test]
    fn a_write_that_ended_well_moves_its_changes_into_the_list_and_leaves_newer_ones_owed() {
        let mut cache = read_hidden(&[]);
        let first = ArchiveOp::Hide {
            thread: thread(1, 2),
            hidden_at_ts: 100,
        };
        cache.archive(first);
        let shown = cache.archived();
        // The user changes the same thread while that write is out.
        let second = ArchiveOp::Hide {
            thread: thread(1, 2),
            hidden_at_ts: 150,
        };
        cache.archive(second);
        cache.note_archive_saved(&[first], &[]);
        assert_eq!(
            cache.archive_owed(),
            &[second],
            "the newer change stays owed"
        );
        assert_eq!(cache.archived(), vec![(thread(1, 2), 150)]);
        cache.note_archive_saved(&[second], &[]);
        assert!(cache.archive_owed().is_empty());
        assert_eq!(cache.hidden_threads().len(), 1);
        assert_eq!(cache.hidden_threads()[0].hidden_at_ts, 150);
        assert_ne!(shown, cache.archived());
    }

    #[test]
    fn a_read_that_began_before_an_archive_write_ended_does_not_undo_it() {
        let mut cache = read_hidden(&[]);
        let began = cache.save_seq();
        // The user archives; the write ends; and only then does the read that began before it
        // return, without the entry (it was built from the value as it was).
        let hide = ArchiveOp::Hide {
            thread: thread(1, 2),
            hidden_at_ts: 100,
        };
        cache.archive(hide);
        cache.note_archive_saved(&[hide], &[]);
        assert_eq!(cache.archived(), vec![(thread(1, 2), 100)]);
        cache.note_read_from(DmRead::Read(Box::new(read_hidden(&[]))), true, began);
        assert_eq!(
            cache.archived(),
            vec![(thread(1, 2), 100)],
            "a stale read does not unarchive what was just saved"
        );
        // A read that began after the write holds it, or the web UI took it away: believed.
        let began = cache.save_seq();
        cache.note_read_from(DmRead::Read(Box::new(read_hidden(&[]))), true, began);
        assert!(cache.archived().is_empty());
        // And it is not laid over again.
        let began = cache.save_seq();
        cache.note_read_from(DmRead::Read(Box::new(read_hidden(&[]))), true, began);
        assert!(cache.archived().is_empty());
    }

    #[test]
    fn a_stale_read_does_not_bring_back_an_entry_that_was_just_removed() {
        let mut cache = read_hidden(&[(1, 2, 100)]);
        let began = cache.save_seq();
        let unhide = ArchiveOp::Unhide {
            thread: thread(1, 2),
        };
        cache.archive(unhide);
        cache.note_archive_saved(&[unhide], &[]);
        assert!(cache.archived().is_empty());
        cache.note_read_from(
            DmRead::Read(Box::new(read_hidden(&[(1, 2, 100)]))),
            true,
            began,
        );
        assert!(cache.archived().is_empty());
    }

    #[test]
    fn the_saved_changes_kept_for_a_stale_read_are_bounded() {
        let mut cache = read_hidden(&[]);
        for n in 0..(MAX_RECENT_ARCHIVE as u8 + 10) {
            let op = ArchiveOp::Hide {
                thread: thread(1, n),
                hidden_at_ts: 1,
            };
            cache.archive(op);
            cache.note_archive_saved(&[op], &[]);
        }
        assert!(cache.archive_recent.len() <= MAX_RECENT_ARCHIVE);
    }

    #[test]
    fn dropping_an_owed_entry_changes_the_view_and_its_revision() {
        let mut cache = read_hidden(&[]);
        let hide = ArchiveOp::Hide {
            thread: thread(1, 2),
            hidden_at_ts: 5,
        };
        cache.archive(hide);
        let rev = cache.archive_rev();
        cache.note_archive_dropped(&[hide]);
        assert!(cache.archived().is_empty());
        assert_ne!(cache.archive_rev(), rev, "the window is told");
    }

    #[test]
    fn archiving_again_lets_revival_look_at_a_held_thread_afresh() {
        let mut cache = read_hidden(&[(1, 2, 100)]);
        let op = ArchiveOp::UnhideIfPast {
            thread: thread(1, 2),
            past: 150,
        };
        cache.archive(op);
        cache.note_archive_saved(&[op], &[op]);
        assert!(cache.revival_held(&thread(1, 2)));
        cache.archive(ArchiveOp::Hide {
            thread: thread(1, 2),
            hidden_at_ts: 200,
        });
        assert!(!cache.revival_held(&thread(1, 2)));
    }

    #[test]
    fn a_thread_the_delegate_holds_twice_shows_once_at_its_largest_cutoff() {
        let cache = read_hidden(&[(1, 2, 100), (1, 2, 300), (1, 2, 200)]);
        assert_eq!(cache.archived(), vec![(thread(1, 2), 300)]);
    }

    #[test]
    fn the_archive_list_is_never_printed() {
        let mut cache = read_hidden(&[(1, 2, 100)]);
        cache.archive(ArchiveOp::Unhide {
            thread: thread(1, 2),
        });
        let shown = format!("{cache:?}");
        assert!(shown.contains("hidden_threads: 1"));
        assert!(shown.contains("archive_owed: 1"));
        assert!(!shown.contains("hidden_at_ts"));
    }

    /// The key is the one the web UI reads and the delegate's index holds.
    #[test]
    fn the_key_is_outbound_dms() {
        assert_eq!(KEY, b"outbound_dms");
    }
}
