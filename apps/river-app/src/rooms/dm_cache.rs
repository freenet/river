//! The plaintext of the direct messages the user sent, read from the chat delegate.
//!
//! A direct message is sealed to its recipient, so the sender cannot open their own. The web UI
//! and `riverctl` keep what they sent in the chat delegate, under the key `outbound_dms`, as the
//! CBOR of [`OutboundDmStore`], and join it to the message in the room by its purge token.
//! [`OutboundCache`] is this app's in-memory copy of that.
//!
//! **This module only reads the delegate.** Nothing here writes `outbound_dms` or any other
//! delegate key (see "Sent this session" for the one in-memory addition): the
//! web UI saves the whole blob with a blind store, so a write made on a stale copy loses
//! entries, and the later change that writes it has to hydrate first and merge. That change
//! gates on [`OutboundCache::hydrated`], so the latch fails safe: it is true only after a read
//! that listed the key and decoded it (or a list that was trustworthy and did not have the
//! key), and a read that proves nothing leaves it as it was.
//!
//! **Sent this session.** A message this app sends is sealed to its recipient, so the sender
//! cannot open it again. [`OutboundCache::remember_sent`] keeps its text in memory so the row
//! shows it for the rest of the run. It is held apart from what the delegate returned
//! (`local`), so a re-read of the delegate does not drop it, and it is not written anywhere:
//! after a restart the row reads "text not available" until the change that saves sent text to
//! `outbound_dms` lands. That change moves these entries into the store and drops them from
//! `local` ([`OutboundCache::forget_sent`]).
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
//! [`OutboundCache::generation`] is 0. The write change needs the generation and must read with
//! `Versioned::Read` (`GetVersionedRequest`) instead. [`OutboundCache::from_bytes`] still accepts
//! the enveloped form and tells the two apart as the delegate does: a CBOR map never starts with
//! the envelope's tag byte.
//!
//! **What the latch trusts.** It trusts any non-empty room list: a list that names rooms is taken
//! to be a read of the whole key index, so a missing `outbound_dms` in it means none is stored.
//! It does not require `rooms_meta`, which is absent for a user who never changed a per-room
//! mode. The basis is that the delegate keeps the index as one stored value, read whole by
//! `get_key_index` (`delegates/chat-delegate/src/handlers.rs`), which errors rather than answer
//! with a part when the value is absent but a key it must list is stored. That is read from the
//! code, not tested against a node, and it does not cover a writer adding the key just after
//! the list was read. So the latch is not what makes a write safe: the writer (PR 4) must read
//! the key again with `Versioned::Read` immediately before writing and refuse when the
//! generation is not the one it read, which makes a wrong latch unable to cause a blind
//! overwrite by itself.
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
type EntryKey = ([u8; 32], MemberId, PurgeToken);

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
}

impl fmt::Debug for OutboundCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboundCache")
            .field("entries", &self.entries.len())
            .field("hidden_threads", &self.hidden.len())
            .field("sent_this_session", &self.local.len())
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
        let epoch = self.epoch.wrapping_add(1);
        match read {
            DmRead::Read(fresh) => {
                let local = std::mem::take(&mut self.local);
                *self = *fresh;
                self.local = local;
                // What the delegate now holds is no longer only local.
                let held = &self.entries;
                self.local.retain(|k, _| !held.contains_key(k));
            }
            DmRead::Unreadable => self.hydrated = false,
            DmRead::NotListed => {
                if !(list_authoritative && !self.hydrated && self.entries.is_empty()) {
                    return;
                }
                self.hydrated = true;
            }
        }
        self.epoch = epoch;
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
        self.entries.get(&key).or_else(|| self.local.get(&key))
    }

    /// Keep the text of a message this app just sent, in memory only (see the module docs).
    /// `sender` is the user's member id and `timestamp` the message's, as the delegate's own
    /// entry would hold. Bounded: past [`MAX_LOCAL`] the oldest is dropped.
    pub fn remember_sent(
        &mut self,
        room: &VerifyingKey,
        recipient: MemberId,
        token: PurgeToken,
        sender: MemberId,
        timestamp: u64,
        text: String,
    ) {
        if self.local.len() >= MAX_LOCAL
            && let Some(oldest) = self
                .local
                .iter()
                .min_by_key(|(_, e)| e.timestamp)
                .map(|(k, _)| *k)
        {
            self.local.remove(&oldest);
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
        if self.local.len() != before {
            self.epoch = self.epoch.wrapping_add(1);
        }
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

    /// The archive list, as stored.
    pub fn hidden_threads(&self) -> &[HiddenDmThreadEntry] {
        &self.hidden
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

    /// The key is the one the web UI reads and the delegate's index holds.
    #[test]
    fn the_key_is_outbound_dms() {
        assert_eq!(KEY, b"outbound_dms");
    }
}
