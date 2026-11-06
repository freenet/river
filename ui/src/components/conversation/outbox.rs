//! The messages this device has authored but not yet committed to `ROOMS`.
//!
//! Chat messages are signed by the chat delegate (`signing::sign_message_with_fallback`).
//! That round trip is usually ~1 ms but sometimes seconds, because delegate
//! requests queue behind contract WASM on the node. The reader must never wait
//! for it, so a composed message goes into this outbox the moment it is sent,
//! renders from here as a pending row (`pending_sends.rs`), and moves into
//! `ROOMS` when its signature arrives. Unsigned messages live only here: never
//! in `ROOMS`, the delegate's rooms blob, or anything sent to the network.
//!
//! Per room, entries commit in the order they were made. A later entry whose
//! signature arrives first waits for the earlier ones, so a send followed by an
//! edit of an earlier message, or three messages typed quickly, reach `ROOMS`
//! (and so the network) in the order the user made them. A failed entry does
//! not hold back the ones after it: it stays, marked failed, until the user
//! retries or discards it (a retried message then commits after them, keeping
//! its original timestamp). Leaving the room drops what is waiting in it
//! (`outbox_commit::clear_room_now`).
//!
//! The outbox is memory only: a reload while a message is still being signed
//! loses it.
//!
//! Each message keeps the timestamp it was given when the user acted; signing
//! later does not restamp it, so the order on screen is the typed order.

use dioxus::prelude::*;
use ed25519_dalek::{Signature, VerifyingKey};
use river_core::room_state::message::{AuthorizedMessageV1, MessageId, MessageV1};
use std::collections::HashMap;
use std::time::SystemTime;

pub(crate) static OUTBOX: GlobalSignal<Outbox> = Global::new(Outbox::default);

/// What the user did, which decides how a pending entry shows.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum PendingKind {
    /// A message or reply from the composer. Shown as a pending row, with the
    /// typed text, until it commits; kept as a failed row if it cannot.
    Send { text: String, author_name: String },
    /// An edit, a delete or a reaction toggle. Shown while pending by applying
    /// `effects` over the committed messages (`pending_effects`); a failure is
    /// reported with a toast and the entry dropped (`label` names the action
    /// in that toast).
    Action {
        label: &'static str,
        effects: Vec<PendingEffect>,
    },
}

/// What a pending action does to the conversation, in plaintext: the
/// messages themselves may be sealed under the room secret.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum PendingEffect {
    React { target: MessageId, emoji: String },
    Unreact { target: MessageId, emoji: String },
    Edit { target: MessageId, text: String },
    Delete { target: MessageId },
}

#[derive(Clone, PartialEq, Debug)]
pub(crate) enum PendingState {
    /// Waiting for at least one signature.
    Signing,
    /// Signing or committing failed. The reason is for the log and the tooltip.
    Failed(String),
}

/// One message of an entry. A reaction swap is two messages (remove the old
/// reaction, add the new one) that commit together.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct UnsignedMessage {
    pub message: MessageV1,
    /// The CBOR encoding of `message`: the bytes the signature covers.
    pub bytes: Vec<u8>,
    pub signature: Option<Signature>,
}

#[derive(Clone, PartialEq, Debug)]
pub(crate) struct PendingEntry {
    /// Local only, increasing in the order entries were made.
    pub id: u64,
    pub kind: PendingKind,
    /// The author's verifying key: a signature is accepted only under it.
    pub author_vk: VerifyingKey,
    pub messages: Vec<UnsignedMessage>,
    pub state: PendingState,
}

impl PendingEntry {
    fn fully_signed(&self) -> bool {
        self.messages.iter().all(|m| m.signature.is_some())
    }

    /// The signed messages. Only meaningful once `fully_signed`.
    pub fn authorized(&self) -> Vec<AuthorizedMessageV1> {
        self.messages
            .iter()
            .filter_map(|m| {
                m.signature
                    .map(|sig| AuthorizedMessageV1::with_signature(m.message.clone(), sig))
            })
            .collect()
    }

    /// Ids of the signed messages: what the room holds once this commits.
    pub fn message_ids(&self) -> Vec<MessageId> {
        self.authorized().iter().map(|m| m.id()).collect()
    }

    /// When the user acted (the first message's timestamp).
    pub fn time(&self) -> SystemTime {
        self.messages
            .first()
            .map(|m| m.message.time)
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }
}

/// A pending row as the conversation shows it.
#[derive(Clone, PartialEq, Debug)]
pub(crate) struct PendingRow {
    pub id: u64,
    pub text: String,
    pub author_name: String,
    pub time: SystemTime,
    pub failed: Option<String>,
}

/// Signing work for one entry: `(message index, bytes to sign)`.
pub(crate) type SigningWork = Vec<(usize, Vec<u8>)>;

#[derive(Default, Debug)]
pub(crate) struct Outbox {
    rooms: HashMap<VerifyingKey, Vec<PendingEntry>>,
    next_id: u64,
}

impl Outbox {
    /// Add an entry at the end of `room`'s queue, in the Signing state.
    /// Returns its id and the signing work to start.
    pub fn enqueue(
        &mut self,
        room: VerifyingKey,
        kind: PendingKind,
        author_vk: VerifyingKey,
        messages: Vec<(MessageV1, Vec<u8>)>,
    ) -> (u64, SigningWork) {
        self.next_id += 1;
        let id = self.next_id;
        let work = messages
            .iter()
            .enumerate()
            .map(|(i, (_, bytes))| (i, bytes.clone()))
            .collect();
        self.rooms.entry(room).or_default().push(PendingEntry {
            id,
            kind,
            author_vk,
            messages: messages
                .into_iter()
                .map(|(message, bytes)| UnsignedMessage {
                    message,
                    bytes,
                    signature: None,
                })
                .collect(),
            state: PendingState::Signing,
        });
        (id, work)
    }

    fn entry_mut(&mut self, room: &VerifyingKey, id: u64) -> Option<&mut PendingEntry> {
        self.rooms.get_mut(room)?.iter_mut().find(|e| e.id == id)
    }

    /// Record one message's signature. Ignored for an entry that is gone or
    /// has failed (a late answer to an abandoned attempt). Results are keyed
    /// by entry and index, not by attempt: a late result from an attempt
    /// before a Retry would land on the new attempt. That cannot happen today,
    /// because only sends (one message, failed once its request completed) are
    /// retried.
    pub fn record_signature(&mut self, room: &VerifyingKey, id: u64, index: usize, sig: Signature) {
        if let Some(entry) = self.entry_mut(room, id) {
            if entry.state == PendingState::Signing {
                if let Some(m) = entry.messages.get_mut(index) {
                    m.signature = Some(sig);
                }
            }
        }
    }

    /// Mark an entry failed. Its signatures are dropped, so a retry signs again.
    pub fn record_failure(&mut self, room: &VerifyingKey, id: u64, reason: String) {
        if let Some(entry) = self.entry_mut(room, id) {
            entry.state = PendingState::Failed(reason);
            for m in &mut entry.messages {
                m.signature = None;
            }
        }
    }

    /// Whether `take_ready` would return anything (so a caller can skip the
    /// signal write, and the renders it causes, when it would not).
    pub fn has_ready(&self, room: &VerifyingKey) -> bool {
        self.rooms.get(room).is_some_and(|queue| {
            queue
                .iter()
                .find(|e| e.state == PendingState::Signing)
                .is_some_and(|e| e.fully_signed())
        })
    }

    /// Whether `take_failed_actions` would return anything.
    pub fn has_failed_actions(&self, room: &VerifyingKey) -> bool {
        self.rooms.get(room).is_some_and(|queue| {
            queue.iter().any(|e| {
                matches!(e.kind, PendingKind::Action { .. })
                    && matches!(e.state, PendingState::Failed(_))
            })
        })
    }

    /// Whether anything waits in `room`.
    pub fn has_room(&self, room: &VerifyingKey) -> bool {
        self.rooms.contains_key(room)
    }

    /// Remove and return, in order, the entries ready to commit: from the
    /// front of the queue, every fully signed entry up to the first one still
    /// signing. Failed entries are passed over and stay where they are.
    pub fn take_ready(&mut self, room: &VerifyingKey) -> Vec<PendingEntry> {
        let Some(queue) = self.rooms.get_mut(room) else {
            return Vec::new();
        };
        let mut ready = Vec::new();
        let mut i = 0;
        while i < queue.len() {
            match queue[i].state {
                PendingState::Failed(_) => i += 1,
                PendingState::Signing if queue[i].fully_signed() => ready.push(queue.remove(i)),
                PendingState::Signing => break,
            }
        }
        if queue.is_empty() {
            self.rooms.remove(room);
        }
        ready
    }

    /// Put back an entry that was taken but did not commit, failed, in its
    /// original place.
    pub fn restore_failed(&mut self, room: VerifyingKey, mut entry: PendingEntry, reason: String) {
        entry.state = PendingState::Failed(reason);
        for m in &mut entry.messages {
            m.signature = None;
        }
        let queue = self.rooms.entry(room).or_default();
        let at = queue.partition_point(|e| e.id < entry.id);
        queue.insert(at, entry);
    }

    /// Start a failed entry signing again. Returns the author's key and the
    /// work, or `None` if the entry is gone or not failed.
    pub fn retry(&mut self, room: &VerifyingKey, id: u64) -> Option<(VerifyingKey, SigningWork)> {
        let entry = self.entry_mut(room, id)?;
        if !matches!(entry.state, PendingState::Failed(_)) {
            return None;
        }
        entry.state = PendingState::Signing;
        Some((
            entry.author_vk,
            entry
                .messages
                .iter()
                .enumerate()
                .map(|(i, m)| (i, m.bytes.clone()))
                .collect(),
        ))
    }

    /// Drop `room`'s entries written as any identity but `author` (the room's
    /// identity was replaced while they waited). Returns how many.
    pub fn drop_other_identities(&mut self, room: &VerifyingKey, author: &VerifyingKey) -> usize {
        let Some(queue) = self.rooms.get_mut(room) else {
            return 0;
        };
        let before = queue.len();
        queue.retain(|e| e.author_vk == *author);
        let dropped = before - queue.len();
        if queue.is_empty() {
            self.rooms.remove(room);
        }
        dropped
    }

    /// Whether any of `room`'s entries were written as an identity but `author`.
    pub fn has_other_identities(&self, room: &VerifyingKey, author: &VerifyingKey) -> bool {
        self.rooms
            .get(room)
            .is_some_and(|q| q.iter().any(|e| e.author_vk != *author))
    }

    /// Drop everything waiting in `room` (the user left it).
    pub fn clear_room(&mut self, room: &VerifyingKey) -> usize {
        self.rooms.remove(room).map_or(0, |q| q.len())
    }

    /// Drop an entry (the user discarded a failed message).
    pub fn discard(&mut self, room: &VerifyingKey, id: u64) -> Option<PendingEntry> {
        let queue = self.rooms.get_mut(room)?;
        let at = queue.iter().position(|e| e.id == id)?;
        let entry = queue.remove(at);
        if queue.is_empty() {
            self.rooms.remove(room);
        }
        Some(entry)
    }

    /// Remove and return the failed actions (edit, delete, reaction) in
    /// `room`. They have no row to wait on, so they are reported and dropped.
    pub fn take_failed_actions(&mut self, room: &VerifyingKey) -> Vec<PendingEntry> {
        let Some(queue) = self.rooms.get_mut(room) else {
            return Vec::new();
        };
        let (failed, kept): (Vec<_>, Vec<_>) = std::mem::take(queue).into_iter().partition(|e| {
            matches!(e.kind, PendingKind::Action { .. })
                && matches!(e.state, PendingState::Failed(_))
        });
        if kept.is_empty() {
            self.rooms.remove(room);
        } else {
            *queue = kept;
        }
        failed
    }

    /// The effects of `room`'s actions still being signed, in the order they
    /// were made, for the conversation to show before they commit. Only
    /// `author`'s: entries written as an identity the room has since replaced
    /// are never shown under the new one (and never commit, `apply_entry`).
    pub fn pending_effects(
        &self,
        room: &VerifyingKey,
        author: &VerifyingKey,
    ) -> Vec<PendingEffect> {
        self.rooms
            .get(room)
            .into_iter()
            .flatten()
            .filter(|e| e.state == PendingState::Signing && e.author_vk == *author)
            .filter_map(|e| match &e.kind {
                PendingKind::Action { effects, .. } => Some(effects.iter().cloned()),
                PendingKind::Send { .. } => None,
            })
            .flatten()
            .collect()
    }

    /// `author`'s composer messages waiting in `room`, in the order they were
    /// sent (see `pending_effects` for why only `author`'s).
    pub fn pending_rows(&self, room: &VerifyingKey, author: &VerifyingKey) -> Vec<PendingRow> {
        self.rooms
            .get(room)
            .into_iter()
            .flatten()
            .filter(|e| e.author_vk == *author)
            .filter_map(|e| match &e.kind {
                PendingKind::Send { text, author_name } => Some(PendingRow {
                    id: e.id,
                    text: text.clone(),
                    author_name: author_name.clone(),
                    time: e.time(),
                    failed: match &e.state {
                        PendingState::Failed(reason) => Some(reason.clone()),
                        PendingState::Signing => None,
                    },
                }),
                PendingKind::Action { .. } => None,
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "outbox_tests.rs"]
mod tests;
