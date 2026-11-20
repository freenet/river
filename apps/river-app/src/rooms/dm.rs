//! Direct-message threads, built from a room's `direct_messages` and the user's plaintext cache.
//!
//! Pure logic, no I/O, no writing. It follows the web UI's thread memo
//! (`ui/src/components/direct_messages/dm_thread_modal.rs`):
//!
//! - A thread is the messages between the local user and one peer in one room. A message is the
//!   user's when the user sent it and the peer's when the user is its recipient, decided only by
//!   the user's own member id. **With no known identity there are no threads**: direction is
//!   never guessed, because a guess shows the user's own words as someone else's.
//! - A message to someone else, from someone else, or from the user to the user, is in no thread.
//! - What the peer sent is opened with the room's signing key (`open_direct_message`). What the
//!   user sent is sealed to the peer, so it comes from the plaintext cache by the message's purge
//!   token ([`OutboundCache`]). A row that cannot be shown says why in fixed words
//!   ([`Placeholder`]): the reason never quotes the envelope or the decoder.
//! - Bodies decode as `decode_body` does: legacy raw UTF-8 (what the web composer sends for
//!   text), `0x80` + CBOR text, and an invitation.
//!
//! Differences from the web UI, all in the direction of caution:
//!
//! - **A message is verified before it is counted.** The envelope says nothing of who sealed it
//!   (anyone can seal to the user's public key), so the sender's signature is checked first,
//!   against the owner's key or a current member's, as the contract resolves it
//!   (`resolve_member_vk`: the user is not special-cased, so a user the room no longer lists
//!   cannot be verified). A message that fails (no key for the sender, an envelope over the
//!   contract's size, a bad signature) is dropped and counted in [`Threads::unverifiable`]: it
//!   makes no thread, no row, no time and no count toward the bounds, and is never drawn as the
//!   user's own.
//! - Duplicates (the same message twice) show once.
//! - The work is bounded ([`MAX_THREADS`], [`MAX_THREAD_MESSAGES`], [`MAX_TOTAL_MESSAGES`])
//!   whatever the room state holds. The read-position time ([`DmThread::last_inbound_ts`]) is
//!   taken before any bound is applied.
//!
//! Secrets: the signing key is borrowed, never copied, and nothing is logged. A decrypted
//! envelope's buffer is wiped once decoded (the sealed cache entries wipe on drop; see
//! [`super::dm_cache`] for what is not wiped), and an invitation's payload (a bearer credential) is
//! wiped and dropped at once: it never reaches the view model. The text of a body is not wiped;
//! it is what the user is looking at.
//!
//! A [`DmThreadsCache`] keeps the result between frames so a view does not verify and open up to
//! a thousand messages every time it draws.

use std::collections::{HashMap, HashSet};

use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::room_state::direct_messages::{
    AuthorizedDirectMessage, DirectMessagesV1, MAX_DM_CIPHERTEXT_BYTES, PurgeToken,
    open_direct_message,
};
use river_core::room_state::dm_body::{DirectMessageBody, decode_body};
use river_core::room_state::member::{MemberId, MembersV1};
use zeroize::Zeroize;

use super::dm_cache::OutboundCache;
use crate::chat::dm_model::{DmBody, DmMessage, DmThread, Placeholder};

/// The most threads built. The newest are kept.
pub const MAX_THREADS: usize = 100;
/// The most messages in one thread: twice what the contract allows one sender to a peer. The
/// newest are kept.
pub const MAX_THREAD_MESSAGES: usize = 200;
/// The most messages opened in one build, across threads. The room-wide cap the contract
/// enforces is 300 by default.
pub const MAX_TOTAL_MESSAGES: usize = 1000;

/// The latest time (unix seconds, the start of 2100) a thread shows or reports.
///
/// A message's timestamp is signed by its sender and the contract does not bound it, so a member
/// can sign `u64::MAX`. Everything above this ceiling is reported as the ceiling, here, once, so
/// no consumer of a thread (the rail, the unread count, a clock, a day heading) ever converts an
/// out-of-range time. Messages are still ordered by the signed value.
pub const MAX_SHOWN_SECS: u64 = 4_102_444_800;

fn shown_secs(ts: u64) -> u64 {
    ts.min(MAX_SHOWN_SECS)
}

/// What a room hands over to be read as threads. Everything is borrowed: nothing is copied out
/// of the room, least of all the key.
pub struct Source<'a> {
    /// The room's key as `Room::key` names it, and its name.
    pub room_key: &'a str,
    pub room_name: &'a str,
    pub owner: &'a VerifyingKey,
    pub members: &'a MembersV1,
    pub messages: &'a DirectMessagesV1,
    /// The user's signing key for the room. `None` when this device holds none.
    pub self_sk: Option<&'a SigningKey>,
    /// The public half when the signing key is not held (`StoredRoom::self_verifying_key`).
    pub self_vk: Option<VerifyingKey>,
    pub cache: &'a OutboundCache,
    /// A member's name as the room shows it.
    pub name_of: &'a dyn Fn(MemberId) -> String,
    /// The name of the room an invitation is for, when this computer has that room.
    pub room_label: &'a dyn Fn(&VerifyingKey) -> Option<String>,
}

/// The threads of one room.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Threads {
    /// Newest first.
    pub threads: Vec<DmThread>,
    /// The user's identity in this room is known. When false the empty list does not mean there
    /// are no messages, and the screen must not say so.
    pub identity_known: bool,
    /// Messages or threads were left out to stay within the bounds.
    pub truncated: bool,
    /// Messages between the user and someone that were dropped because they could not be
    /// verified (see the module docs).
    pub unverifiable: usize,
}

/// One verified message with who it is with.
struct Row<'a> {
    outgoing: bool,
    message: &'a AuthorizedDirectMessage,
}

#[derive(Default)]
struct PeerRows<'a> {
    rows: Vec<Row<'a>>,
    /// The newest message the peer sent, before any bound.
    last_inbound_ts: u64,
}

/// Build the threads of a room.
pub fn threads(src: &Source<'_>) -> Threads {
    let me_vk = src.self_sk.map(SigningKey::verifying_key).or(src.self_vk);
    let Some(me_vk) = me_vk else {
        return Threads::default();
    };
    let me = MemberId::from(&me_vk);
    let owner_id = MemberId::from(src.owner);

    // Whose key signs what, as the contract resolves it: the owner, then current members.
    let mut keys: HashMap<MemberId, VerifyingKey> =
        HashMap::with_capacity(src.members.members.len() + 1);
    keys.insert(owner_id, *src.owner);
    for m in &src.members.members {
        keys.entry(m.member.id()).or_insert(m.member.member_vk);
    }

    // The same message twice is one. One that merely shares a signature with another but differs
    // is not the same message; it fails verification below and is dropped.
    let mut seen: HashSet<(PurgeToken, u64, MemberId, MemberId, [u8; 32])> = HashSet::new();
    let mut by_peer: HashMap<MemberId, PeerRows<'_>> = HashMap::new();
    let mut unverifiable = 0;
    for m in &src.messages.messages {
        let (sender, recipient) = (m.message.sender, m.message.recipient);
        if sender == recipient {
            continue;
        }
        let (peer, outgoing) = if sender == me {
            (recipient, true)
        } else if recipient == me {
            (sender, false)
        } else {
            continue;
        };
        let identity = (
            m.purge_token(),
            m.message.timestamp,
            sender,
            recipient,
            *blake3::hash(&m.message.ciphertext).as_bytes(),
        );
        if !seen.insert(identity) {
            continue;
        }
        // Verified before anything of it is used: its direction, time or count.
        let verified = m.message.ciphertext.len() <= MAX_DM_CIPHERTEXT_BYTES
            && keys
                .get(&sender)
                .is_some_and(|vk| m.verify_signature(vk, src.owner).is_ok());
        if !verified {
            unverifiable += 1;
            continue;
        }
        let entry = by_peer.entry(peer).or_default();
        if !outgoing {
            entry.last_inbound_ts = entry.last_inbound_ts.max(m.message.timestamp);
        }
        entry.rows.push(Row {
            outgoing,
            message: m,
        });
    }

    let mut truncated = false;
    // Oldest first within a thread, newest kept. Messages in the same second are ordered by their
    // purge token: the same on every device and every build, and not dependent on the order the
    // room happens to hold them in (which differs between peers).
    let mut grouped: Vec<(MemberId, Vec<Row<'_>>, u64, u64)> = by_peer
        .into_iter()
        .map(|(peer, mut p)| {
            p.rows
                .sort_by_key(|r| (r.message.message.timestamp, r.message.purge_token()));
            let last = p.rows.last().map_or(0, |r| r.message.message.timestamp);
            if p.rows.len() > MAX_THREAD_MESSAGES {
                truncated = true;
                p.rows.drain(..p.rows.len() - MAX_THREAD_MESSAGES);
            }
            (peer, p.rows, last, p.last_inbound_ts)
        })
        .collect();
    // Newest thread first; the member id keeps equal times in a fixed order.
    grouped.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.0.0.cmp(&b.0.0.0)));
    if grouped.len() > MAX_THREADS {
        truncated = true;
        grouped.truncate(MAX_THREADS);
    }
    let mut budget = MAX_TOTAL_MESSAGES;
    for (_, rows, _, _) in &mut grouped {
        if rows.len() > budget {
            truncated = true;
            rows.drain(..rows.len() - budget);
        }
        budget -= rows.len();
    }
    grouped.retain(|(_, rows, _, _)| !rows.is_empty());

    let threads = grouped
        .into_iter()
        .map(|(peer, rows, last_ts, last_inbound_ts)| DmThread {
            room: src.room_key.to_string(),
            room_name: src.room_name.to_string(),
            peer_key: peer_key(peer),
            peer,
            peer_name: (src.name_of)(peer),
            peer_is_member: peer == owner_id
                || src.members.members.iter().any(|m| m.member.id() == peer),
            messages: rows.iter().map(|r| message(src, peer, r)).collect(),
            last_ts: shown_secs(last_ts),
            last_inbound_ts: shown_secs(last_inbound_ts),
        })
        .collect();
    Threads {
        threads,
        identity_known: true,
        truncated,
        unverifiable,
    }
}

fn message(src: &Source<'_>, peer: MemberId, row: &Row<'_>) -> DmMessage {
    let m = row.message;
    DmMessage {
        key: token_hex(&m.purge_token()),
        outgoing: row.outgoing,
        timestamp: shown_secs(m.message.timestamp),
        body: if row.outgoing {
            sent(src, peer, m)
        } else {
            received(src, m)
        },
        pending: None,
    }
}

/// What the peer sent: opened with the room's signing key.
fn received(src: &Source<'_>, m: &AuthorizedDirectMessage) -> DmBody {
    let Some(sk) = src.self_sk else {
        return DmBody::Placeholder(Placeholder::NoKey);
    };
    let Ok(mut bytes) = open_direct_message(sk, m) else {
        return DmBody::Placeholder(Placeholder::CannotDecrypt);
    };
    let decoded = decode_body(&bytes);
    bytes.zeroize();
    match decoded {
        Ok(DirectMessageBody::Text { text }) => DmBody::Text(text),
        Ok(DirectMessageBody::Invite(payload)) => {
            let mut payload = *payload;
            // The credential goes nowhere: wiped now, not carried to the view.
            payload.invitation_payload.zeroize();
            let room_label = (src.room_label)(&payload.room_owner_vk)
                .unwrap_or_else(|| short_key(&payload.room_owner_vk));
            DmBody::Invite {
                room_label,
                note: note_of(payload.personal_message.as_deref()),
            }
        }
        Err(_) => DmBody::Placeholder(Placeholder::CannotDecode),
    }
}

/// A message the user sent: from the cache, by the recipient and the purge token.
fn sent(src: &Source<'_>, recipient: MemberId, m: &AuthorizedDirectMessage) -> DmBody {
    let Some(text) = src.cache.plaintext(src.owner, recipient, &m.purge_token()) else {
        return DmBody::Placeholder(Placeholder::CiphertextOnly);
    };
    match text.strip_prefix(INVITE_SENTINEL) {
        Some(rest) => DmBody::SentInvite {
            note: note_of(Some(rest)),
        },
        None => DmBody::Text(text.to_string()),
    }
}

/// What the web UI and `riverctl` cache for an invitation the user sent
/// (`OUTBOUND_INVITE_SENTINEL`), followed by the personal note. Text the user typed that starts
/// the same way reads as an invitation too, as it does in the web UI.
const INVITE_SENTINEL: &str = "[Invitation]";

fn note_of(note: Option<&str>) -> Option<String> {
    let note = note?.trim();
    (!note.is_empty()).then(|| note.to_string())
}

/// A room's key shortened to name it when its name is not known.
fn short_key(vk: &VerifyingKey) -> String {
    let full = bs58::encode(vk.as_bytes()).into_string();
    format!("Room {}", full.chars().take(8).collect::<String>())
}

/// What names a peer in a thread: River's full member id, in decimal.
///
/// Never the id's `Display` form, which is only the first eight base32 characters (a label for
/// the screen, 40 bits): a member can grind a key whose label matches another's, and two such
/// members must never share a thread or receive each other's messages.
pub fn peer_key(peer: MemberId) -> String {
    peer.0.0.to_string()
}

/// The member id [`peer_key`] names.
pub fn peer_from_key(key: &str) -> Option<MemberId> {
    key.parse::<i64>()
        .ok()
        .map(|n| MemberId(freenet_scaffold::util::FastHash(n)))
}

/// A purge token as lowercase hex.
pub fn token_hex(token: &PurgeToken) -> String {
    token.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// The purge token a message's key names: [`token_hex`] the other way. `None` for anything that is
/// not 32 hex digits.
pub fn token_from_hex(key: &str) -> Option<PurgeToken> {
    if key.len() != 32 || !key.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&key[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(PurgeToken(out))
}

/// What decides whether the threads built last time are still the threads to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmKey([u8; 32]);

impl DmKey {
    /// A fingerprint of everything [`threads`] reads except what its two closures read: the
    /// room's key and name, who the user is, the members, each message's identity (its
    /// signature, time, ends and length; the signature covers the ciphertext, and a state that
    /// took an altered one would not have verified), and the cache's epoch.
    ///
    /// The key cannot see through `name_of` and `room_label`, so `names` must be a hash of what
    /// they read: **this room's nicknames** (what `name_of` shows for the user's peers) **and the
    /// name of every other room `room_label` can answer for** (the rooms an invitation row may
    /// name). Leave either out and a rename shows stale until something else changes.
    pub fn of(src: &Source<'_>, names: u64) -> DmKey {
        let mut h = blake3::Hasher::new();
        // Length-prefixed, so two different splits of the same bytes cannot collide.
        for text in [src.room_key, src.room_name] {
            h.update(&(text.len() as u64).to_le_bytes());
            h.update(text.as_bytes());
        }
        h.update(src.owner.as_bytes());
        let me = src.self_sk.map(SigningKey::verifying_key).or(src.self_vk);
        h.update(&[u8::from(src.self_sk.is_some()), u8::from(me.is_some())]);
        if let Some(me) = me {
            h.update(me.as_bytes());
        }
        h.update(&(src.members.members.len() as u64).to_le_bytes());
        for m in &src.members.members {
            h.update(m.member.member_vk.as_bytes());
        }
        h.update(&(src.messages.messages.len() as u64).to_le_bytes());
        for m in &src.messages.messages {
            h.update(&m.purge_token().0);
            h.update(&m.message.timestamp.to_le_bytes());
            h.update(&m.message.sender.0.0.to_le_bytes());
            h.update(&m.message.recipient.0.0.to_le_bytes());
            h.update(&(m.message.ciphertext.len() as u64).to_le_bytes());
        }
        h.update(&src.cache.epoch().to_le_bytes());
        h.update(&[u8::from(src.cache.hydrated())]);
        h.update(&names.to_le_bytes());
        DmKey(*h.finalize().as_bytes())
    }
}

/// The last threads built and what they were built from, so a view that draws often rebuilds
/// only when something changed.
#[derive(Default)]
pub struct DmThreadsCache {
    last: Option<(DmKey, Threads)>,
    /// How many times the threads were built, for tests.
    builds: usize,
}

impl DmThreadsCache {
    /// The threads for `src`, built only if `key` differs from the last call's.
    pub fn get(&mut self, key: DmKey, build: impl FnOnce() -> Threads) -> &Threads {
        if self.last.as_ref().is_none_or(|(k, _)| *k != key) {
            self.builds += 1;
            self.last = Some((key, build()));
        }
        &self.last.as_ref().expect("just set").1
    }

    pub fn builds(&self) -> usize {
        self.builds
    }
}
