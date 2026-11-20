//! What a write to the chat delegate's `outbound_dms` key stores: the value it holds now, with
//! this app's changes made to it and nothing else.
//!
//! The web UI saves the whole key from its own copy ([`super::dm_cache`] has the race), and its
//! `hidden_threads` (the archive list) live in the same value. So a write here is a
//! read-merge-write over the CBOR as the delegate holds it, never a rebuild from the cache:
//!
//! - The value is decoded to a generic CBOR map, and only the `entries` array is edited. Every
//!   other field of the map, known or not, `hidden_threads` included, is carried over value for
//!   value, and so is every entry of the array that is not being removed, with any field this
//!   build does not know. The result is encoded again, canonically, so what is preserved is
//!   the decoded value and not the bytes: an indefinite-length array or map comes back
//!   definite-length and an integer with a non-minimal encoding comes back minimal. Map order,
//!   text, byte strings and integers' values are kept. (The web UI and this app write the
//!   canonical forms, so for a value either wrote the bytes are in fact the same.)
//! - A value that is not a map, whose `entries` or `hidden_threads` is not an array, whose
//!   entries do not decode as [`OutboundDmEntry`], that has trailing bytes, or that is empty
//!   is [`Refusal::Unreadable`]. Nothing is written over what this app cannot read.
//! - A key that was listed when the cache was read and now holds nothing is
//!   [`Refusal::Vanished`]: a delegate that lost the value says the same as one that never had
//!   it, and writing a one-entry store over a lost archive is the loss this module exists to
//!   prevent.
//! - An entry already there for the same room, recipient and token is left alone (a repeated
//!   save, or a save whose answer was lost, changes nothing).
//!
//! **Bounds.** For each (room, sender, recipient) a save adds to, the oldest entries beyond
//! [`MAX_DM_MESSAGES_PER_PAIR`] go, chosen across the whole pair, the ones being added included
//! (by timestamp, then token), as the web UI's `save_outbound_dm_now` does; they are this
//! user's own entries by construction (same sender). The ones dropped are reported, so the
//! cache does not save them again. If the whole value would pass
//! [`MAX_WRITE_BYTES`] (4 MiB, see its note), the oldest entries with the same room and sender as one being added go
//! until it fits, and if it still does not fit the save is [`Refusal::Full`]. Entries of other
//! senders (another identity of the user in the room, or anyone else's) are never evicted.
//!
//! Nothing here prints an entry or puts its text in an error. The decoded values and the buffers
//! this builds are zeroed when dropped, as far as the decoder lets them be reached; see
//! [`super::dm_save`] for what is not.

use std::collections::HashSet;
use std::fmt;

use ciborium::Value;
use river_core::chat_delegate::OutboundDmEntry;
use river_core::room_state::direct_messages::{MAX_DM_MESSAGES_PER_PAIR, PurgeToken};
use river_core::room_state::member::MemberId;

use super::dm_cache::{MAX_STORE_BYTES, Plaintext};

/// The field of the stored map that holds the sent messages.
const ENTRIES: &str = "entries";
/// The archive list, which is never edited here.
const HIDDEN: &str = "hidden_threads";

/// The most bytes a save stores. The node's client API accepts messages of 100 MiB
/// (`WEBSOCKET_MAX_MESSAGE_SIZE`, freenet-core), and this app's own reader (tungstenite's default
/// frame limit) takes 16 MiB, and the delegate has no cap of its own on a value; but a write is
/// a whole value, read and written on the one connection that also carries the user's rooms, so
/// a big one is slow and a failure of it drops the connection. 4 MiB is far beyond what the
/// 100-per-pair cap lets a user accumulate (a message is at most about 32 KiB), and well below
/// every limit above, so a value this app wrote is always one it can read back.
pub const MAX_WRITE_BYTES: usize = 4 * 1024 * 1024;

/// What an entry is found by: the room (its owner's key bytes), the recipient and the
/// message's purge token.
pub type EntryKey = ([u8; 32], MemberId, PurgeToken);

/// One change to the store.
#[derive(Clone, PartialEq, Eq)]
pub enum DmOp {
    /// Add what the user sent.
    Add {
        key: EntryKey,
        /// The user's member id when it was sent.
        sender: MemberId,
        timestamp: u64,
        text: Plaintext,
    },
    /// Remove the entry for a message the user discarded.
    Remove { key: EntryKey },
}

impl fmt::Debug for DmOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DmOp::Add {
                timestamp, text, ..
            } => f
                .debug_struct("Add")
                .field("timestamp", timestamp)
                .field("text", text)
                .finish_non_exhaustive(),
            DmOp::Remove { .. } => f.debug_struct("Remove").finish_non_exhaustive(),
        }
    }
}

/// Why nothing may be written. The words never carry the value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The stored value is not one this app can edit without losing something.
    Unreadable,
    /// The key was there when it was last read and now holds nothing.
    Vanished,
    /// The value would not fit, even with this user's oldest entries dropped.
    Full,
    /// A new entry could not be encoded.
    Encode,
}

impl Refusal {
    /// A word for the log: never anything of the value.
    pub fn kind(self) -> &'static str {
        match self {
            Refusal::Unreadable => "unreadable",
            Refusal::Vanished => "vanished",
            Refusal::Full => "full",
            Refusal::Encode => "encode",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Refusal::Unreadable => "the stored sent messages can't be read by this app",
            Refusal::Vanished => "the stored sent messages went missing",
            Refusal::Full => "the stored sent messages are full",
            Refusal::Encode => "a sent message could not be encoded",
        })
    }
}

/// What a merge decided.
#[derive(Debug, PartialEq, Eq)]
pub struct Merged {
    /// The value to store, or `None` when the store already holds what the ops ask for.
    pub bytes: Option<Vec<u8>>,
    /// Entries of this user dropped to stay inside the bounds.
    pub evicted: Vec<EntryKey>,
}

/// Zero the text and bytes of a decoded value.
fn wipe_value(v: &mut Value) {
    use zeroize::Zeroize;
    match v {
        Value::Text(t) => t.zeroize(),
        Value::Bytes(b) => b.zeroize(),
        Value::Array(a) => a.iter_mut().for_each(wipe_value),
        Value::Map(m) => m.iter_mut().for_each(|(k, v)| {
            wipe_value(k);
            wipe_value(v);
        }),
        Value::Tag(_, inner) => wipe_value(inner),
        _ => {}
    }
}

/// The top-level fields of the stored map, wiped when dropped.
struct Fields(Vec<(Value, Value)>);

impl Drop for Fields {
    fn drop(&mut self) {
        self.0.iter_mut().for_each(|(k, v)| {
            wipe_value(k);
            wipe_value(v);
        });
    }
}

/// One entry of the stored array, kept as read.
struct Item {
    value: Value,
    key: EntryKey,
    sender: MemberId,
    timestamp: u64,
    /// Added by this merge.
    added: bool,
}

impl Drop for Item {
    fn drop(&mut self) {
        wipe_value(&mut self.value);
    }
}

fn item_of(value: Value) -> Result<Item, Refusal> {
    // Through bytes, with the decoder the store's other readers use: `Value::deserialized` does
    // not accept the byte string a purge token is stored as.
    let mut bytes = encode(&value)?;
    let parsed = ciborium::de::from_reader::<OutboundDmEntry, _>(bytes.as_slice());
    zeroize::Zeroize::zeroize(&mut bytes);
    let mut entry = parsed.map_err(|_| Refusal::Unreadable)?;
    zeroize::Zeroize::zeroize(&mut entry.plaintext);
    Ok(Item {
        key: (entry.room_owner_vk, entry.recipient, entry.purge_token),
        sender: entry.sender,
        timestamp: entry.timestamp,
        value,
        added: false,
    })
}

fn encode(value: &Value) -> Result<Vec<u8>, Refusal> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(value, &mut out).map_err(|_| Refusal::Encode)?;
    Ok(out)
}

/// Apply `ops` to `current`, the value the delegate holds (without its generation envelope).
///
/// `listed` says the cache read this key and found a value, so `None` now is [`Refusal::Vanished`].
pub fn merge(current: Option<&[u8]>, listed: bool, ops: &[DmOp]) -> Result<Merged, Refusal> {
    merge_within(current, listed, ops, MAX_WRITE_BYTES)
}

/// [`merge`] with the byte bound named, so a test need not build sixteen megabytes.
fn merge_within(
    current: Option<&[u8]>,
    listed: bool,
    ops: &[DmOp],
    limit: usize,
) -> Result<Merged, Refusal> {
    let mut top = Fields(match current {
        None if listed => return Err(Refusal::Vanished),
        None => vec![
            (Value::Text(ENTRIES.into()), Value::Array(Vec::new())),
            (Value::Text(HIDDEN.into()), Value::Array(Vec::new())),
        ],
        Some(bytes) => {
            if bytes.is_empty() || bytes.len() > MAX_STORE_BYTES {
                return Err(Refusal::Unreadable);
            }
            let mut rest = bytes;
            let value = ciborium::de::from_reader::<Value, _>(&mut rest)
                .map_err(|_| Refusal::Unreadable)?;
            if !rest.is_empty() {
                return Err(Refusal::Unreadable);
            }
            match value {
                Value::Map(map) => map,
                mut other => {
                    wipe_value(&mut other);
                    return Err(Refusal::Unreadable);
                }
            }
        }
    });
    if top
        .0
        .iter()
        .any(|(k, v)| k.as_text() == Some(HIDDEN) && !matches!(v, Value::Array(_)))
    {
        return Err(Refusal::Unreadable);
    }
    let at = match top.0.iter().position(|(k, _)| k.as_text() == Some(ENTRIES)) {
        Some(i) => i,
        None => {
            top.0
                .push((Value::Text(ENTRIES.into()), Value::Array(Vec::new())));
            top.0.len() - 1
        }
    };
    let Value::Array(stored) = std::mem::replace(&mut top.0[at].1, Value::Null) else {
        return Err(Refusal::Unreadable);
    };
    let mut items: Vec<Item> = stored.into_iter().map(item_of).collect::<Result<_, _>>()?;

    let mut changed = false;
    // Removals first, so an entry to remove goes whatever is asked of the adds.
    let removals = ops.iter().filter(|o| matches!(o, DmOp::Remove { .. }));
    let adds = ops.iter().filter(|o| matches!(o, DmOp::Add { .. }));
    for op in removals.chain(adds) {
        match op {
            DmOp::Remove { key } => {
                let before = items.len();
                items.retain(|i| i.key != *key);
                changed |= items.len() != before;
            }
            DmOp::Add {
                key,
                sender,
                timestamp,
                text,
            } => {
                if items.iter().any(|i| i.key == *key) {
                    continue;
                }
                let entry = OutboundDmEntry {
                    room_owner_vk: key.0,
                    sender: *sender,
                    recipient: key.1,
                    purge_token: key.2,
                    timestamp: *timestamp,
                    plaintext: text.as_str().to_owned(),
                };
                let value = Value::serialized(&entry).map_err(|_| Refusal::Encode)?;
                items.push(Item {
                    value,
                    key: *key,
                    sender: *sender,
                    timestamp: *timestamp,
                    added: true,
                });
                changed = true;
            }
        }
    }
    if !changed {
        return Ok(Merged {
            bytes: None,
            evicted: Vec::new(),
        });
    }

    let mut evicted = Vec::new();
    // The per-pair cap: the same pair (room, sender, recipient) the web UI trims, the oldest
    // across the whole pair, what is being added included.
    let pairs: HashSet<([u8; 32], MemberId, MemberId)> = items
        .iter()
        .filter(|i| i.added)
        .map(|i| (i.key.0, i.sender, i.key.1))
        .collect();
    for pair in pairs {
        let in_pair = |i: &Item| (i.key.0, i.sender, i.key.1) == pair;
        while items.iter().filter(|i| in_pair(i)).count() > MAX_DM_MESSAGES_PER_PAIR {
            let Some(oldest) = items
                .iter()
                .enumerate()
                .filter(|(_, i)| in_pair(i))
                .min_by_key(|(_, i)| (i.timestamp, i.key.2.0))
                .map(|(n, _)| n)
            else {
                break;
            };
            evicted.push(items.remove(oldest).key);
        }
    }

    let build = |items: &[Item], top: &Fields| -> Result<Vec<u8>, Refusal> {
        let mut value = Value::Map(top.0.clone());
        if let Value::Map(map) = &mut value {
            map[at].1 = Value::Array(items.iter().map(|i| i.value.clone()).collect());
        }
        let out = encode(&value);
        wipe_value(&mut value);
        out
    };
    let mut bytes = build(&items, &top)?;
    // A removal only makes the value smaller, so it is never refused for size.
    if bytes.len() > limit && items.iter().any(|i| i.added) {
        // This user's oldest entries in the rooms and under the ids being added to, oldest
        // first, until it fits.
        let ours: HashSet<([u8; 32], MemberId)> = items
            .iter()
            .filter(|i| i.added)
            .map(|i| (i.key.0, i.sender))
            .collect();
        let mut candidates: Vec<(u64, EntryKey)> = items
            .iter()
            .filter(|i| !i.added && ours.contains(&(i.key.0, i.sender)))
            .map(|i| (i.timestamp, i.key))
            .collect();
        candidates.sort_by_key(|(ts, _)| *ts);
        let mut size = bytes.len();
        for (_, key) in candidates {
            if size <= limit {
                break;
            }
            if let Some(n) = items.iter().position(|i| i.key == key) {
                let mut one = encode(&items[n].value)?;
                size = size.saturating_sub(one.len());
                zeroize::Zeroize::zeroize(&mut one);
                evicted.push(items.remove(n).key);
            }
        }
        bytes = build(&items, &top)?;
        if bytes.len() > limit {
            return Err(Refusal::Full);
        }
    }
    Ok(Merged {
        bytes: Some(bytes),
        evicted,
    })
}

#[cfg(test)]
#[path = "dm_store_tests.rs"]
mod tests;
