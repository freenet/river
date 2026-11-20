//! What a direct-message thread shows, as plain data.
//!
//! Built by `rooms::dm` from a room's `direct_messages` and the owner's plaintext cache. These
//! types hold decrypted text, so none of them derives `Debug` over a body: a stray `{:?}` of a
//! thread must not print what someone wrote. The `Debug` impls here print lengths only.
//!
//! Everything a peer controls in a body (`Invite.note`, `Invite.room_label` when it falls back
//! to a key, and `Text`) is untrusted text: render it as plain text or as the room's markdown,
//! never as markup of the app's own.

use std::fmt;

use river_core::room_state::member::MemberId as RiverMemberId;

/// A thread of direct messages between the local user and one peer, in one room. The same two
/// people in two rooms are two threads.
#[derive(Clone, PartialEq, Eq)]
pub struct DmThread {
    /// The room's key, as `Room::key` names it.
    pub room: String,
    pub room_name: String,
    /// The peer's River member id, as `Member::uid` shows it for the same member.
    pub peer_uid: String,
    /// The same id, to address a message to them.
    pub peer: RiverMemberId,
    pub peer_name: String,
    /// The peer is the owner or a current member, so a message to them would be accepted.
    pub peer_is_member: bool,
    /// Oldest first.
    pub messages: Vec<DmMessage>,
    /// The time (unix seconds) of the newest message of either direction.
    pub last_ts: u64,
    /// The time of the newest message the peer sent, 0 when they sent none. What the first
    /// read position is seeded from.
    pub last_inbound_ts: u64,
}

impl fmt::Debug for DmThread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DmThread")
            .field("room", &self.room)
            .field("peer_uid", &self.peer_uid)
            .field("messages", &self.messages.len())
            .field("last_ts", &self.last_ts)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DmMessage {
    /// The message's purge token in hex (the hash of its signature): what a delete names.
    ///
    /// Unique among the messages shown. A copy of a message with the genuine signature but an
    /// altered body fails the signature check and is dropped before any row is built
    /// (`Threads::unverifiable`), and an identical copy is shown once, so no two rows share a
    /// token.
    pub key: String,
    /// The local user sent it.
    pub outgoing: bool,
    /// Unix seconds, as the sender signed it.
    pub timestamp: u64,
    pub body: DmBody,
}

impl fmt::Debug for DmMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DmMessage")
            .field("key", &self.key)
            .field("outgoing", &self.outgoing)
            .field("timestamp", &self.timestamp)
            .field("body", &self.body)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum DmBody {
    /// What was written, to render as markdown.
    Text(String),
    /// An invitation the local user received. It carries no invitation: that is a bearer
    /// credential, and reading it again from the message is the accepting step's business.
    Invite {
        /// The target room's name when this computer has it, else a short form of its key.
        room_label: String,
        note: Option<String>,
    },
    /// An invitation the local user sent, from the plaintext cache's summary.
    SentInvite { note: Option<String> },
    /// A row that cannot be shown. Rendered as plain text, never as markdown.
    Placeholder(Placeholder),
}

impl fmt::Debug for DmBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DmBody::Text(t) => write!(f, "Text(<{} bytes>)", t.len()),
            DmBody::Invite { note, .. } => write!(f, "Invite(note: {})", note.is_some()),
            DmBody::SentInvite { note } => write!(f, "SentInvite(note: {})", note.is_some()),
            DmBody::Placeholder(p) => write!(f, "Placeholder({p:?})"),
        }
    }
}

/// Why a row has no body to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placeholder {
    /// This device holds no signing key for the room, so nothing sent to the user opens.
    NoKey,
    /// The envelope did not open: not sealed to this key, or altered.
    CannotDecrypt,
    /// It opened, but what is inside is not a body this app reads.
    CannotDecode,
    /// Sent by the local user, and the plaintext cache has no copy of it.
    CiphertextOnly,
}

impl Placeholder {
    /// The words shown in the row.
    pub fn text(self) -> &'static str {
        match self {
            Placeholder::NoKey => "unable to decrypt: no signing key for this room on this device",
            Placeholder::CannotDecrypt => "unable to decrypt: this message could not be opened",
            Placeholder::CannotDecode => "unable to read: this message is not in a known format",
            Placeholder::CiphertextOnly => "sent - ciphertext only",
        }
    }
}
