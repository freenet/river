//! Sending from this app: a message, a reply, an edit or deletion of your own message, or a
//! reaction, built and signed the way the web UI builds and signs them, shown at once, and kept
//! here until the node takes it.
//!
//! What this mirrors in the web UI (`ui/src/components/conversation.rs` and
//! `conversation/outbox_commit.rs`):
//! - **Bodies.** A public room gets `RoomMessageBody::public` / `reply` and a public action
//!   body (`action_body`: `CONTENT_TYPE_ACTION` with the encoded `ActionContentV1`). A private
//!   room seals the encoded `TextContentV1` / `ReplyContentV1` / `ActionContentV1` under the
//!   room's CURRENT secret (`RoomData::get_secret`: the version the room state names as
//!   current) with `encrypt_with_symmetric_key`, as `SealInput::seal` (`conversation/seal.rs`)
//!   does. A private room without that secret refuses, as the web UI's `missing_room_secret`.
//! - **Signing.** `MessageV1 { room_owner, author, content, time }`, signed over its CBOR
//!   bytes with the room's stored `self_sk` (`AuthorizedMessageV1::new`). The web UI asks the
//!   chat delegate (`SignMessage`) and falls back to the same `self_sk`
//!   (`signing::sign_message_with_fallback`); Ed25519 is deterministic, so both give the same
//!   signature. This app does not ask the delegate: it has the key, and a room with no key
//!   gets no composer.
//! - **The delta.** `ChatRoomStateV1Delta { recent_messages, members, member_info }`, where
//!   `members` and `member_info` re-add a member the room pruned for inactivity
//!   (`RoomData::build_rejoin_delta`), as `apply_entry_to` builds it. It goes to the node as
//!   `ContractRequest::Update { key, data: UpdateData::Delta(cbor) }` under the room's contract
//!   key, as the web UI's synchronizer sends a room's changes.
//! - **Reactions.** One per member per message: the same emoji again removes it, a different
//!   one replaces it, counting reactions still on their way (`handle_toggle_reaction`).
//! - **Admission.** A delta the room would take without adding the message (a cap, a ban) is a
//!   failure, not a success (`apply_entry_to`'s "the room did not admit the message").
//! - **A rotated secret.** A message waiting to go out for the first time, sealed under a
//!   secret the room has since moved on from, is sealed again under the current one, keeping
//!   its time (`take_committable`), and never under an older one (`sealed_at_or_below`).
//!   Here that holds only until it first goes out: from then on it is sent again unchanged,
//!   since the node may already hold it and a resealed copy would show twice. The trade is
//!   that a message resent after a rotation can reach a member the rotation excluded, which
//!   the room itself allows (it admits any version it has a record of).
//!   A message written as an identity the room no longer uses is never sent under the new one.
//!
//! - **Your nickname.** A new signed `member_info` record (`Action::SetNickname`), as the web UI's
//!   `NicknameField` builds it: sealed under the room's current secret in a private room (a
//!   private room with no secret here refuses, freenet/river#299), signed with your key, at one
//!   above the higher of the room's record and the cached one (and above any nickname still
//!   waiting), with your deputy grants carried over (#410). It rides the same queue, in order
//!   with messages. A newer change replaces one never sent. A member the room removed for
//!   inactivity is told to send a message first: the room drops them again in the same apply,
//!   along with a record sent with their re-adding. The web UI does not compare the name with
//!   other members' (it warns readers), and neither does this.
//!
//! - **The room's details.** A new signed `configuration` (`Action::SetRoomDetails`), as the web
//!   UI's room details dialog builds it (`sign_and_apply_configuration`): the held configuration
//!   with the owner's changes, its version one above the held one, normalized and checked the way
//!   `riverctl` does (`Configuration::normalize_for_signing`, `signing_refusal`), and signed with
//!   the owner's key. A private room's name and description are sealed under the room's current
//!   secret; with none here it refuses (freenet/river#299). Only the fields the owner changed
//!   are kept, and applied to whatever the room holds when it is built, so an edit waiting while
//!   another device of the owner changed something else does not undo that. It rides the same
//!   queue, in order with messages. Two owner devices at one version are settled by the
//!   contract's rank (version, then signature digest): the loser is told it was replaced.
//!   The web UI saves one field at a time as each loses focus; this sends one change for the
//!   whole dialog, on Save.
//!
//!
//! - **A direct message** (`Action::SendDm`, [`super::dm_send`]): composed and signed once with the
//!   room's key, checked to land in a copy of the room, and then queued like the rest. It is not
//!   a room message: it has no row in the conversation (the thread shows it, with its state, from
//!   [`Outbox::dm_rows`]) and it is never built again. A retry, a resend after a lost connection
//!   and a late answer all send the same signed message, because composing again seals with new
//!   randomness and the node, which may hold the first, would show both.
//!
//! - **Deleting their messages** (`Action::PurgeDm`, [`super::dm_purge`]): one signed purge
//!   envelope, queued like the rest (one update per room, in order, the same Retry and Discard,
//!   held when River has moved on). It is not a room message and has no row in the conversation:
//!   the thread shows it ([`Outbox::purge_rows`]). The room keeps one purge list per person, and
//!   a new envelope that omits a token the room holds lifts that tombstone, so the envelope is
//!   built from the room's current one and **one rule** decides when it is built again: while it
//!   has not gone out, and when a refused or unconfirmed one is retried (kept as it is when still
//!   right); one that is out or unanswered is sent again byte for byte.
//!
//! Where this differs: the web UI marks a room to sync and never hears whether the node took
//! the update. Here one update per room is out at a time, and its answer decides: an
//! `UpdateResponse` means sent (if the room then holds the messages), an update error means
//! failed, and no answer within [`super::session::Pace::update_wait`] is shown as not
//! confirmed. An answer names only the contract, not the request, so it is matched to the
//! oldest update of that room still owed one, and nothing more goes out for a room while it
//! owes an answer, whatever became of the entry meanwhile (settled by the network, discarded):
//! otherwise an owed answer would be taken for the next update's. A new connection owes
//! nothing, so Retry of an unanswered update reconnects, and so does an answer owed for
//! [`super::session::Pace::owed_limit`]; an update that has cost [`MAX_FORCED_RECONNECTS`]
//! connections that way is left unconfirmed. Once sent, messages are only ever sent again
//! unchanged (a retry of an unanswered update, a resend after a reconnect), so the node never
//! gets two copies under two ids; only a refused one is built again. One the node took but the
//! room here does not show stays unconfirmed, with why, and is never sent again from here. A
//! send not taken stays on screen marked "Not sent" with Retry and Discard; an edit, deletion
//! or reaction not taken is said above the composer. Whatever the network later brings in with these messages' ids counts as sent, whatever the
//! answer was. Sending writes nothing to the chat delegate's storage, so the web UI's
//! after-send bookkeeping (its read position, its stored copy of the room) is not done: the
//! web UI picks the message up from the network.
//!
//! One deviation in rejoining: when the user has no published nickname record and never chose
//! a nickname, the web UI publishes a generated one; this app re-adds the member with no
//! nickname record, as the web UI itself does for a private room whose secret is missing, and
//! the web UI's own repair publishes one later.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant, SystemTime};

use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::ContractInstanceId;
use river_core::ecies::{encrypt_with_symmetric_key, seal_bytes};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::content::{
    ACTION_CONTENT_VERSION, ActionContentV1, CONTENT_TYPE_ACTION, CONTENT_TYPE_REPLY,
    CONTENT_TYPE_TEXT, REPLY_CONTENT_VERSION, ReplyContentV1, TEXT_CONTENT_VERSION, TextContentV1,
};
use river_core::room_state::direct_messages::{AuthorizedDirectMessage, PurgeToken};
use river_core::room_state::member::{AuthorizedMember, MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{AuthorizedMessageV1, MessageId, MessageV1, RoomMessageBody};
use river_core::room_state::privacy::SealedBytes;
use river_core::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};

use super::room::{LiveRoom, message_key};
use crate::chat::model::{DeputyChange, Notice, Pending, ReplyWrap, Room};

/// Something the user did in a room, sent from the chat screen to the background work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The room's key (`Room::key`).
    pub room: String,
    pub action: Action,
}

/// Messages are named by their key (`Message::key`). `Debug` does not print a nickname.
#[derive(Clone, PartialEq, Eq)]
pub enum Action {
    Send {
        text: String,
        reply_to: Option<String>,
    },
    Edit {
        target: String,
        text: String,
    },
    Delete {
        target: String,
    },
    /// Toggle: the same emoji as yours removes it, another replaces yours.
    React {
        target: String,
        emoji: String,
    },
    Retry {
        entry: u64,
    },
    Discard {
        entry: u64,
    },
    /// Change the room's details (owner only): a new signed configuration with these changes.
    SetRoomDetails {
        edit: ConfigEdit,
    },
    /// Change your own nickname in the room: a new signed `member_info` record at the next
    /// version, sealed under the room's current secret in a private room.
    SetNickname {
        nickname: String,
    },
    /// Make the member `target` (River's member id, as `Member::uid`) one of your deputies, or
    /// end that: your own `member_info` record again, with that member added to or removed from
    /// its `deputies`, as the web UI's `DeputyButton` does.
    SetDeputy {
        target: String,
        grant: bool,
    },
    /// Ban the member `target` (River's member id, as `Member::uid`): a signed ban, and in a
    /// private room the rotated secret in the same update ([`super::bans`]).
    Ban {
        target: String,
    },
    /// Ask what banning `target` (`Member::uid`) would do to the room's other bans: the next
    /// view says, for that member alone. Sends nothing.
    PreviewBan {
        target: String,
    },
    /// Rotate the room's secret (a private room's owner): the next secret version for every
    /// member the room holds, as the web UI's Room Details "Rotate" does ([`super::bans`]).
    RotateSecret,
    /// Make an invitation to the room, for its link to be copied ([`super::create`]).
    Invite,
    /// Create a room. The command's `room` is empty: this is not about a room shown.
    CreateRoom {
        name: String,
        nickname: String,
        private: bool,
    },
    /// Join the room a pasted invite code or link invites to. `room` is empty.
    Join {
        code: InviteCode,
        nickname: String,
    },
    /// Try a failed create or join again ([`super::work`]). `room` is empty.
    RetryWork {
        id: u64,
    },
    /// Drop a failed create or join. `room` is empty.
    DismissWork {
        id: u64,
    },
    /// Set the room's notification mode, stored in `rooms_meta` beside the web UI's
    /// ([`super::modes`]).
    SetNotificationMode {
        mode: super::modes::NotificationMode,
    },
    /// Leave the room, as the web UI does ([`super::leave`]): it is removed from the stored
    /// rooms and does not come back.
    LeaveRoom,
    /// Make the identity export token for the room ([`super::identity`]). Nothing is sent to
    /// the node; the token comes back as [`super::RoomsUpdate::Export`].
    ExportId,
    /// Import the identity a pasted token holds ([`super::identity`]): add its room, or with
    /// `replace` swap the identity of the room already stored. `room` is empty.
    ImportId {
        token: super::identity::IdentityToken,
        replace: bool,
    },
    /// Send a direct message to the member `peer` of the command's room ([`super::dm_send`]).
    /// `peer` is River's FULL member id, never the `uid` label (40 bits, which a member can
    /// grind to match another's). The session answers it, by `ticket`, with
    /// [`super::session::RoomsUpdate::DmSend`]: whether the message was queued, or why not.
    SendDm {
        peer: MemberId,
        text: String,
        ticket: u64,
    },
    /// Delete the direct messages the member `peer` sent the user in the command's room, the
    /// ones with these purge tokens ([`super::dm_purge`]): what the thread showed when the user
    /// confirmed. The session answers it, by `ticket`, with
    /// [`super::session::RoomsUpdate::DmPurge`]: whether the deletion was queued, or why not.
    /// Only messages `peer` sent the user and the room still holds are deleted, whatever the
    /// tokens name.
    PurgeDm {
        peer: MemberId,
        tokens: Vec<PurgeToken>,
        ticket: u64,
    },
    /// Send the member `peer` of the command's room an invitation to the room keyed `target` in
    /// a direct message, with an optional `note` ([`super::dm_invite`]). The session answers it,
    /// by `ticket`, with [`super::session::RoomsUpdate::DmInvite`]: queued, or why not.
    SendInviteDm {
        peer: MemberId,
        target: String,
        note: Option<String>,
        ticket: u64,
    },
    /// Accept the invitation in the message `token` that `peer` sent the user in the command's
    /// room ([`super::dm_invite`]): the invitation is read from the room now and joined. The
    /// session answers it, by `ticket`, with [`super::session::RoomsUpdate::DmInvite`]:
    /// whether the join was started, or why not. Nothing carries the invitation.
    AcceptInviteDm {
        peer: MemberId,
        token: PurgeToken,
        ticket: u64,
    },
    /// Archive the thread with `peer` in the command's room ([`super::dm_archive`]).
    /// `row_inbound_ts` is the newest time of a message `peer` sent the user in the thread as the
    /// window shows it, 0 when they sent none; the cutoff is that or the room's own newest,
    /// whichever is larger. Not answered: the thread is archived on screen at once and saved
    /// like the sent text is.
    ArchiveDm {
        peer: MemberId,
        row_inbound_ts: u64,
    },
    /// Take the thread with `peer` out of the archive, whatever its cutoff (the user asked).
    UnarchiveDm {
        peer: MemberId,
    },
    /// The window now shows the command's room. Not something the user sends: the background
    /// work reads that room again from the node when it has heard nothing about it for a while
    /// ([`super::resync`]). Never kept in the outbox.
    Viewing,
    /// The window came to the front: read again from the node every room not heard about
    /// lately ([`super::resync`]). `room` is empty. Never kept in the outbox.
    Resync,
}

impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::Send { text, reply_to } => f
                .debug_struct("Send")
                .field("text", text)
                .field("reply_to", reply_to)
                .finish(),
            Action::Edit { target, text } => f
                .debug_struct("Edit")
                .field("target", target)
                .field("text", text)
                .finish(),
            Action::Delete { target } => f.debug_struct("Delete").field("target", target).finish(),
            Action::React { target, emoji } => f
                .debug_struct("React")
                .field("target", target)
                .field("emoji", emoji)
                .finish(),
            Action::Retry { entry } => f.debug_struct("Retry").field("entry", entry).finish(),
            Action::Discard { entry } => f.debug_struct("Discard").field("entry", entry).finish(),
            Action::SetNickname { .. } => f.write_str("SetNickname { nickname: <hidden> }"),
            Action::SetDeputy { target, grant } => f
                .debug_struct("SetDeputy")
                .field("target", target)
                .field("grant", grant)
                .finish(),
            Action::Ban { target } => f.debug_struct("Ban").field("target", target).finish(),
            Action::RotateSecret => f.write_str("RotateSecret"),
            Action::PreviewBan { target } => f
                .debug_struct("PreviewBan")
                .field("target", target)
                .finish(),
            Action::SetRoomDetails { edit } => f
                .debug_struct("SetRoomDetails")
                .field("edit", edit)
                .finish(),
            Action::Invite => f.write_str("Invite"),
            Action::CreateRoom { name, private, .. } => f
                .debug_struct("CreateRoom")
                .field("name", name)
                .field("nickname", &"<hidden>")
                .field("private", private)
                .finish(),
            Action::Join { code, .. } => f
                .debug_struct("Join")
                .field("code", code)
                .field("nickname", &"<hidden>")
                .finish(),
            Action::RetryWork { id } => f.debug_struct("RetryWork").field("id", id).finish(),
            Action::DismissWork { id } => f.debug_struct("DismissWork").field("id", id).finish(),
            Action::SetNotificationMode { mode } => f
                .debug_struct("SetNotificationMode")
                .field("mode", mode)
                .finish(),
            Action::LeaveRoom => f.write_str("LeaveRoom"),
            Action::Viewing => f.write_str("Viewing"),
            Action::Resync => f.write_str("Resync"),
            Action::ExportId => f.write_str("ExportId"),
            Action::ImportId { replace, .. } => f
                .debug_struct("ImportId")
                .field("token", &"<hidden>")
                .field("replace", replace)
                .finish(),
            Action::SendDm { peer, ticket, .. } => f
                .debug_struct("SendDm")
                .field("peer", &super::dm::peer_key(*peer))
                .field("text", &"<hidden>")
                .field("ticket", ticket)
                .finish(),
            Action::PurgeDm {
                peer,
                tokens,
                ticket,
            } => f
                .debug_struct("PurgeDm")
                .field("peer", &super::dm::peer_key(*peer))
                .field("tokens", &tokens.len())
                .field("ticket", ticket)
                .finish(),
            // Neither prints the note, nor a token (a name for a message), and nothing here
            // holds an invitation.
            Action::SendInviteDm {
                peer,
                target,
                ticket,
                ..
            } => f
                .debug_struct("SendInviteDm")
                .field("peer", &super::dm::peer_key(*peer))
                .field("target", target)
                .field("note", &"<hidden>")
                .field("ticket", ticket)
                .finish(),
            Action::AcceptInviteDm { peer, ticket, .. } => f
                .debug_struct("AcceptInviteDm")
                .field("peer", &super::dm::peer_key(*peer))
                .field("token", &"<hidden>")
                .field("ticket", ticket)
                .finish(),
            Action::ArchiveDm { peer, .. } => f
                .debug_struct("ArchiveDm")
                .field("peer", &super::dm::peer_key(*peer))
                .finish_non_exhaustive(),
            Action::UnarchiveDm { peer } => f
                .debug_struct("UnarchiveDm")
                .field("peer", &super::dm::peer_key(*peer))
                .finish(),
        }
    }
}

/// The changes to a room's details the owner asked for: only the fields that changed. The name
/// and description are plain text here and sealed when built; an empty description clears it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ConfigEdit {
    pub name: Option<String>,
    pub description: Option<String>,
    pub max_members: Option<usize>,
    pub max_recent_messages: Option<usize>,
    pub max_message_size: Option<usize>,
    pub max_user_bans: Option<usize>,
    pub max_nickname_size: Option<usize>,
    pub max_room_name: Option<usize>,
    pub max_room_description: Option<usize>,
}

impl ConfigEdit {
    /// Whether it changes nothing.
    pub fn is_empty(&self) -> bool {
        *self == ConfigEdit::default()
    }

    /// This edit made after `older`: what this sets wins, the rest is `older`'s.
    fn over(self, older: &ConfigEdit) -> ConfigEdit {
        ConfigEdit {
            name: self.name.or_else(|| older.name.clone()),
            description: self.description.or_else(|| older.description.clone()),
            max_members: self.max_members.or(older.max_members),
            max_recent_messages: self.max_recent_messages.or(older.max_recent_messages),
            max_message_size: self.max_message_size.or(older.max_message_size),
            max_user_bans: self.max_user_bans.or(older.max_user_bans),
            max_nickname_size: self.max_nickname_size.or(older.max_nickname_size),
            max_room_name: self.max_room_name.or(older.max_room_name),
            max_room_description: self.max_room_description.or(older.max_room_description),
        }
    }

    /// `held` with the limits this sets.
    fn apply_limits(&self, held: &mut Configuration) {
        let set = |field: &mut usize, new: Option<usize>| {
            if let Some(n) = new {
                *field = n;
            }
        };
        set(&mut held.max_members, self.max_members);
        set(&mut held.max_recent_messages, self.max_recent_messages);
        set(&mut held.max_message_size, self.max_message_size);
        set(&mut held.max_user_bans, self.max_user_bans);
        set(&mut held.max_nickname_size, self.max_nickname_size);
        set(&mut held.max_room_name, self.max_room_name);
        set(&mut held.max_room_description, self.max_room_description);
    }
}

/// The limits' names as the web UI's room details dialog labels them.
pub const LIMIT_LABELS: [&str; 7] = [
    "Members",
    "Max Recent Messages",
    "Max Message Size (bytes)",
    "Max User Bans",
    "Max Nickname Size",
    "Max Room Name Size",
    "Max Room Description Size",
];

/// The most each limit may be set to in this app, in [`LIMIT_LABELS`] order (the nickname limit
/// has the room's own ceiling instead). Generous against the web UI's defaults (200 members,
/// 100 messages, 1000 bytes, 10 bans, 100 and 500 bytes): they only stop a slipped digit, and
/// they are not rules of the room contract, whose only bound is the u32 range.
pub const LIMIT_SANITY_CEILINGS: [usize; 7] = [
    100_000,
    100_000,
    1_000_000,
    100_000,
    river_core::room_state::member_info::NICKNAME_CEILING,
    10_000,
    100_000,
];

/// What lowering a limit does to what the room holds, said in plain words, for each limit the
/// edit lowers below the current usage. These are irreversible: the room removes the excess
/// members, stops enforcing the excess bans and drops the oldest messages as soon as it applies
/// the new configuration.
pub fn lowering_warnings(details: &crate::chat::model::Details, edit: &ConfigEdit) -> Vec<String> {
    let u = &details.usage;
    let mut out = Vec::new();
    if let Some(n) = edit.max_members
        && u.members > n
    {
        out.push(format!(
            "Lowering Members to {n} removes {} members, starting with the longest invite chains. \
             This cannot be undone.",
            u.members - n
        ));
    }
    if let Some(n) = edit.max_user_bans
        && u.bans > n
    {
        out.push(format!(
            "Lowering Max User Bans to {n} stops {} of the room's {} bans from applying. \
             This cannot be undone.",
            u.bans - n,
            u.bans
        ));
    }
    if let Some(n) = edit.max_recent_messages
        && u.messages > n
    {
        out.push(format!(
            "Lowering Max Recent Messages to {n} deletes the oldest {} of the room's {} messages. \
             This cannot be undone.",
            u.messages - n,
            u.messages
        ));
    }
    out
}

/// Why `edit` can't be made to a room with these `held` limits and `name` and `description` (as
/// stored, empty when unreadable), if it can't: what the web UI refuses before signing
/// (`Configuration::signing_refusal`) said in the dialog's terms, and what its create dialog
/// refuses in a name (empty, characters that don't show).
pub fn details_refusal(
    held: &crate::chat::model::Limits,
    name: &str,
    description: &str,
    edit: &ConfigEdit,
) -> Option<String> {
    let new_name = edit.name.as_deref().unwrap_or(name).trim();
    if edit.name.is_some() {
        if new_name.is_empty() {
            return Some("Give the room a name.".into());
        }
        if new_name.chars().any(super::room::is_hidden) {
            return Some("The room name has characters that don't show. Type it again.".into());
        }
    }
    let limits = [
        edit.max_members.unwrap_or(held.max_members),
        edit.max_recent_messages.unwrap_or(held.max_recent_messages),
        edit.max_message_size.unwrap_or(held.max_message_size),
        edit.max_user_bans.unwrap_or(held.max_user_bans),
        edit.max_nickname_size.unwrap_or(held.max_nickname_size),
        edit.max_room_name.unwrap_or(held.max_room_name),
        edit.max_room_description
            .unwrap_or(held.max_room_description),
    ];
    for (label, value) in LIMIT_LABELS.iter().zip(limits) {
        if value == 0 {
            return Some(format!("{label} must be at least 1."));
        }
    }
    // The room contract is wasm32, where a limit is a u32: a larger one is signed and then
    // refused by the node with no reason. The sanity ceilings are this app's own guard against
    // a slip of the keyboard, not a rule of the room.
    for (i, (label, set)) in LIMIT_LABELS
        .iter()
        .zip([
            edit.max_members,
            edit.max_recent_messages,
            edit.max_message_size,
            edit.max_user_bans,
            edit.max_nickname_size,
            edit.max_room_name,
            edit.max_room_description,
        ])
        .enumerate()
    {
        let Some(v) = set else { continue };
        if v as u64 > u64::from(u32::MAX) {
            return Some(format!(
                "{label} can be at most {} (the largest number a room can hold).",
                u32::MAX
            ));
        }
        if i != 4 && v > LIMIT_SANITY_CEILINGS[i] {
            return Some(format!(
                "{label} can be at most {} here. That is far more than a room needs.",
                LIMIT_SANITY_CEILINGS[i]
            ));
        }
    }
    let ceiling = river_core::room_state::member_info::NICKNAME_CEILING;
    // A held limit over the ceiling is lowered by `normalize_for_signing`; only one set
    // here is refused.
    if edit.max_nickname_size.is_some_and(|n| n > ceiling) {
        return Some(format!("Max nickname size can be at most {ceiling} bytes."));
    }
    let new_description = edit.description.as_deref().unwrap_or(description).trim();
    if !new_name.is_empty() && new_name.len() > limits[5] {
        return Some(format!(
            "The room name is {} bytes, and this room's limit is {} (Max Room Name Size).",
            new_name.len(),
            limits[5]
        ));
    }
    if new_description.len() > limits[6] {
        return Some(format!(
            "The description is {} bytes, and this room's limit is {} (Max Room Description Size).",
            new_description.len(),
            limits[6]
        ));
    }
    None
}

/// As derived, except the name and description (a private room's are secret) are not printed.
impl std::fmt::Debug for ConfigEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hidden = |o: &Option<String>| o.as_ref().map(|_| "<hidden>");
        f.debug_struct("ConfigEdit")
            .field("name", &hidden(&self.name))
            .field("description", &hidden(&self.description))
            .field("max_members", &self.max_members)
            .field("max_recent_messages", &self.max_recent_messages)
            .field("max_message_size", &self.max_message_size)
            .field("max_user_bans", &self.max_user_bans)
            .field("max_nickname_size", &self.max_nickname_size)
            .field("max_room_name", &self.max_room_name)
            .field("max_room_description", &self.max_room_description)
            .finish()
    }
}

/// A pasted invite code or link. It is a bearer credential (it carries the invitee's signing
/// key), so its `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct InviteCode(pub String);

impl std::fmt::Debug for InviteCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InviteCode(<hidden>)")
    }
}

impl Action {
    /// Whether this is about the room list rather than one room.
    pub fn is_list_level(&self) -> bool {
        matches!(
            self,
            Action::CreateRoom { .. }
                | Action::Join { .. }
                | Action::RetryWork { .. }
                | Action::DismissWork { .. }
                | Action::SetNotificationMode { .. }
                | Action::LeaveRoom
                | Action::ImportId { .. }
        )
    }
}

/// Why the user can't send in a room. The order of the checks is the web UI's
/// (`RoomData::can_participate`, then `missing_room_secret`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocked {
    NoKey,
    Banned,
    NotMember,
    NoSecret,
}

impl Blocked {
    pub fn reason(self) -> &'static str {
        match self {
            Blocked::NoKey => {
                "This computer doesn't hold your key for this room, so you can't send messages \
                 here. Use River in your browser."
            }
            Blocked::Banned => "You have been banned from sending messages in this room.",
            Blocked::NotMember => {
                "You're not a member of this room. To join, ask a member for an invitation."
            }
            Blocked::NoSecret => {
                "This private room's key hasn't reached this computer yet, so you can't send \
                 messages here until it does."
            }
        }
    }
}

/// Whether the user can send in `room`, as the web UI decides it.
pub fn participation(room: &LiveRoom) -> Result<(), Blocked> {
    let Some(sk) = &room.stored.self_sk else {
        return Err(Blocked::NoKey);
    };
    let vk = sk.verifying_key();
    if self_banned(room, &vk) {
        return Err(Blocked::Banned);
    }
    let state = &room.stored.room_state;
    let member = vk == room.stored.owner_vk
        || state.members.members.iter().any(|m| m.member.member_vk == vk)
        // Pruned for inactivity, and able to re-add themselves.
        || room.stored.self_authorized_member.is_some();
    if !member {
        return Err(Blocked::NotMember);
    }
    if room.is_private() && room.current_secret().is_none() {
        return Err(Blocked::NoSecret);
    }
    Ok(())
}

/// The web UI's `is_self_enforced_banned`: the room's enforced bans, with this member and
/// their invite chain put back if the room already removed them, so a ban on an ancestor
/// still counts.
fn self_banned(room: &LiveRoom, vk: &VerifyingKey) -> bool {
    banned_by_state(
        &room.stored.room_state,
        &room.params(),
        vk,
        room.stored.self_authorized_member.as_ref(),
        &room.stored.invite_chain,
    )
}

/// [`self_banned`] for any state: whether the contract's own rule (`banned_member_ids`) takes
/// `vk` out of `state`, with `admitted` and their `chain` put back when the room already removed
/// them.
pub(super) fn banned_by_state(
    state: &ChatRoomStateV1,
    params: &river_core::room_state::ChatRoomParametersV1,
    vk: &VerifyingKey,
    admitted: Option<&AuthorizedMember>,
    chain: &[AuthorizedMember],
) -> bool {
    let me = MemberId::from(vk);
    let mut members = state.members.clone();
    if !members.members.iter().any(|m| m.member.id() == me) {
        if let Some(admitted) = admitted {
            members.members.push(admitted.clone());
        }
        let mut present: HashSet<MemberId> =
            members.members.iter().map(|m| m.member.id()).collect();
        for ancestor in chain {
            if present.insert(ancestor.member.id()) {
                members.members.push(ancestor.clone());
            }
        }
    }
    members
        .banned_member_ids(&state.ban_evidence, &state.bans, &state.member_info, params)
        .contains(&me)
}

/// The web UI's `build_rejoin_delta`: what re-adds a member the room pruned for inactivity.
/// Nothing for the owner, a current member, or a member with no stored admission.
pub(super) fn rejoin(room: &LiveRoom) -> (Option<MembersDelta>, Option<Vec<AuthorizedMemberInfo>>) {
    let Some(sk) = &room.stored.self_sk else {
        return (None, None);
    };
    let vk = sk.verifying_key();
    let state = &room.stored.room_state;
    if vk == room.stored.owner_vk
        || state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == vk)
    {
        return (None, None);
    }
    let Some(admitted) = &room.stored.self_authorized_member else {
        return (None, None);
    };
    let present: HashSet<MemberId> = state
        .members
        .members
        .iter()
        .map(|m| m.member.id())
        .collect();
    let mut add: Vec<AuthorizedMember> = vec![admitted.clone()];
    for ancestor in &room.stored.invite_chain {
        if !present.contains(&ancestor.member.id()) {
            add.push(ancestor.clone());
        }
    }
    // A stored record is reused, but in a private room only a sealed one: a plain nickname
    // must not be republished there.
    let reusable = room.stored.self_member_info.as_ref().filter(|info| {
        !room.is_private()
            || matches!(
                info.member_info.preferred_nickname,
                SealedBytes::Private { .. }
            )
    });
    let info = match reusable {
        Some(info) => Some(info.clone()),
        None => room.stored.self_nickname.as_ref().and_then(|nickname| {
            let me = MemberId::from(&vk);
            let version = state
                .member_info
                .canonical(me)
                .map_or(0, |i| i.member_info.version);
            let sealed = if room.is_private() {
                room.current_secret()
                    .map(|(secret, v)| seal_bytes(nickname.as_bytes(), &secret, v))
            } else {
                Some(SealedBytes::public(nickname.clone().into_bytes()))
            };
            sealed.map(|preferred_nickname| {
                AuthorizedMemberInfo::new_with_member_key(
                    MemberInfo {
                        member_id: me,
                        version,
                        preferred_nickname,
                        deputies: Vec::new(),
                    },
                    sk,
                )
            })
        }),
    };
    (Some(MembersDelta::new(add)), info.map(|i| vec![i]))
}

/// The update that publishes `messages` in `room`, re-adding the member first if needed.
pub(super) fn delta(room: &LiveRoom, messages: &[AuthorizedMessageV1]) -> ChatRoomStateV1Delta {
    let (members, member_info) = rejoin(room);
    ChatRoomStateV1Delta {
        recent_messages: Some(messages.to_vec()),
        members,
        member_info,
        ..Default::default()
    }
}

/// Why a nickname can't be changed while the room does not list you as a member: removed for
/// inactivity, or not yet shown as joined. Which of those it is cannot be told from here.
pub const PRUNED_MESSAGE: &str = PRUNED;

/// The same for a change to your deputies.
pub(crate) const PRUNED_DEPUTIES: &str = "The room doesn't list you as a member yet. Send a message \
     first, then change your deputies.";

pub(crate) const PRUNED: &str = "The room doesn't list you as a member yet. Send a message \
     first, then change your nickname.";

/// What an entry labels itself as when it is about your nickname.
const NICKNAME_LABEL: &str = "Your nickname";
/// ... about who your deputies are.
const DEPUTY_LABEL: &str = "Your change to a deputy";
/// ... about both, one record carrying the two changes.
const RECORD_LABEL: &str = "Your nickname and deputies";

/// Whether an entry with this label is about your own `member_info` record. A newer change to
/// that record replaces an older one that is not on its way.
fn is_record_label(label: &str) -> bool {
    label == NICKNAME_LABEL || label == DEPUTY_LABEL || label == RECORD_LABEL
}

/// The update that publishes `info`, your own `member_info` record, in `room`. Like the web
/// UI's nickname field it carries the rejoin's `members` delta, but that does not keep a pruned
/// member: the room drops them again in the same apply, and their record with them (pinned by
/// `a_member_the_room_removed_for_inactivity_is_told_to_send_a_message_first`). So
/// [`build_info`] refuses a pruned member rather than build a record that cannot hold. The
/// rejoin's own stored record is not sent: this one replaces it.
pub(super) fn info_delta(room: &LiveRoom, info: &AuthorizedMemberInfo) -> ChatRoomStateV1Delta {
    ChatRoomStateV1Delta {
        members: rejoin(room).0,
        member_info: Some(vec![info.clone()]),
        ..Default::default()
    }
}

/// Why `nickname` can't be set as yours in `room`, if it can't: what the web UI's nickname
/// field refuses (empty, characters that don't show, over the room's `max_nickname_size`).
pub fn nickname_refusal_in(nickname: &str, max_nickname_size: usize) -> Option<String> {
    if nickname.trim().is_empty() {
        return Some("Nickname can't be empty.".into());
    }
    // The web UI's own check and its message, copied verbatim (`chat::visual_fold`).
    if crate::chat::visual_fold::contains_hidden_chars(nickname) {
        return Some(crate::chat::visual_fold::EMOJI_REJECTION_MESSAGE.into());
    }
    let config = river_core::room_state::configuration::Configuration {
        max_nickname_size,
        ..Default::default()
    };
    river_core::room_state::member_info::nickname_refusal(nickname.len(), &config)
}

/// What you changed in your own `member_info` record: only the fields that changed. The record
/// holds a nickname and your deputy grants, and republishing replaces all of it, so a change to
/// one carries the other over from what the room holds (or, while an earlier change is on its
/// way, from that).
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct RecordEdit {
    /// A new nickname, as typed (trimmed). `None` keeps the one the record has.
    nickname: Option<String>,
    /// Members to add to (`true`) or remove from (`false`) your deputies, at most one entry
    /// per member, in the order made.
    deputies: Vec<(MemberId, bool)>,
}

impl RecordEdit {
    fn nickname(nickname: String) -> Self {
        RecordEdit {
            nickname: Some(nickname),
            deputies: Vec::new(),
        }
    }

    fn deputy(target: MemberId, grant: bool) -> Self {
        RecordEdit {
            nickname: None,
            deputies: vec![(target, grant)],
        }
    }

    /// This change made after `older`: what this sets wins, and what it leaves alone is
    /// `older`'s.
    fn over(self, older: &RecordEdit) -> RecordEdit {
        let mut deputies: Vec<(MemberId, bool)> = older
            .deputies
            .iter()
            .filter(|(t, _)| !self.deputies.iter().any(|(n, _)| n == t))
            .copied()
            .collect();
        deputies.extend(self.deputies.iter().copied());
        RecordEdit {
            nickname: self.nickname.or_else(|| older.nickname.clone()),
            deputies,
        }
    }

    fn label(&self) -> &'static str {
        match (&self.nickname, self.deputies.is_empty()) {
            (Some(_), true) => NICKNAME_LABEL,
            (None, _) => DEPUTY_LABEL,
            (Some(_), false) => RECORD_LABEL,
        }
    }

    /// `base` with these changes made, in the order the web UI makes them: a grant is added at
    /// the end, a revoked member is removed, and one already as asked is left alone. The room
    /// takes at most [`MAX_DEPUTIES`].
    fn apply_deputies(&self, mut base: Vec<MemberId>) -> Result<Vec<MemberId>, String> {
        for (target, grant) in &self.deputies {
            let at = base.iter().position(|d| d == target);
            match (grant, at) {
                (true, None) => base.push(*target),
                (false, Some(i)) => {
                    base.remove(i);
                }
                _ => {}
            }
        }
        if base.len() > MAX_DEPUTIES {
            return Err(too_many_deputies());
        }
        Ok(base)
    }

    /// Whether this only ends deputies. A revoke is possible whenever the room takes a write from
    /// you (the web UI's `DeputyButton`: the appointer must be able to take a grant back), even
    /// with no nickname to make again; a member the room removed for inactivity still has to
    /// send a message first, as the room drops their record with them.
    fn only_revokes(&self) -> bool {
        self.nickname.is_none() && !self.deputies.is_empty() && self.deputies.iter().all(|d| !d.1)
    }

    /// What `Entry::superseded` says when another record of yours wins.
    fn superseded(&self) -> &'static str {
        if self.deputies.is_empty() {
            SUPERSEDED
        } else {
            RECORD_SUPERSEDED
        }
    }

    fn no_version_left(&self) -> &'static str {
        if self.deputies.is_empty() {
            NO_VERSION_LEFT
        } else {
            RECORD_NO_VERSION_LEFT
        }
    }
}

/// As derived, except a nickname is not printed.
impl std::fmt::Debug for RecordEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordEdit")
            .field("nickname", &self.nickname.as_ref().map(|_| "<hidden>"))
            .field("deputies", &self.deputies)
            .finish()
    }
}

/// The most deputies one member may list (`river_core`'s `MAX_DEPUTIES`).
pub const MAX_DEPUTIES: usize = river_core::room_state::member_info::MAX_DEPUTIES;

pub fn too_many_deputies() -> String {
    format!("You already have the most deputies a member can have ({MAX_DEPUTIES}).")
}

const NO_RECORD: &str = "This computer has no current record of your deputies for this room, so \
     it can't change them without risking ending them all. Try again once the room has loaded.";

/// What an entry labels itself as when it only says that part of a deputy change was lost.
const DEPUTY_NOTE_LABEL: &str = "Your deputy change";

/// Whether `id` is the room's owner or one of its members now.
fn is_member(room: &LiveRoom, id: &MemberId) -> bool {
    *id == MemberId::from(&room.stored.owner_vk)
        || room
            .stored
            .room_state
            .members
            .members
            .iter()
            .any(|m| m.member.id() == *id)
}

const NO_RECORD_NICKNAME: &str = "Your record is not loaded yet; try again in a moment.";

/// A grant whose member has since left the room (banned, or removed for inactivity).
const GONE_MEMBER: &str = "That member is no longer in the room, so they can't be made a deputy.";

const NO_NICKNAME: &str = "You have no nickname in this room yet. Choose one first, then change \
     your deputies.";

/// Your nickname for a record that does not change it: the sealed one `base` (the record the
/// room holds, or one of yours on its way) carries, as the web UI republishes it. A record
/// sealed the way the room is not sealed now (the room turned private or public since), or under
/// a secret older than the current one that this computer can read, is sealed again, so a
/// private room never gets a plain nickname from here.
fn carried_nickname(
    room: &LiveRoom,
    base: Option<&AuthorizedMemberInfo>,
) -> Result<SealedBytes, String> {
    let held = base.map(|b| &b.member_info.preferred_nickname);
    let current = room.current_secret();
    let same_kind = held.is_some_and(|s| s.is_private() == room.is_private());
    if let Some(s) = held
        && same_kind
        && match (s.secret_version(), current) {
            (Some(v), Some((_, c))) => v >= c,
            _ => true,
        }
    {
        return Ok(s.clone());
    }
    let plain = match held {
        Some(s) => room.unseal(s),
        // No record: the name chosen here, if one was.
        None => room.stored.self_nickname.clone(),
    }
    .filter(|n| !n.trim().is_empty());
    let Some(plain) = plain else {
        // Sealed under an older secret this computer can't read: it stays as it is.
        return match held {
            Some(s) if same_kind => Ok(s.clone()),
            _ => Err(NO_NICKNAME.to_owned()),
        };
    };
    if room.is_private() {
        let (secret, v) = current.ok_or_else(|| Blocked::NoSecret.reason().to_owned())?;
        Ok(seal_bytes(plain.as_bytes(), &secret, v))
    } else {
        Ok(SealedBytes::public(plain.into_bytes()))
    }
}

/// The nickname and deputies of the record `edit` makes on top of `base`: the typed nickname
/// sealed under the room's CURRENT secret in a private room (never plain: a private room with
/// no secret here refuses, freenet/river#299), or the one `base` carries.
fn record_parts(
    room: &LiveRoom,
    edit: &RecordEdit,
    base: Option<&AuthorizedMemberInfo>,
) -> Result<(SealedBytes, Vec<MemberId>), String> {
    // No record of yours to build on, yet a cached one lists deputies: an empty list from here
    // would end them all without anyone asking.
    if base.is_none()
        && room
            .stored
            .self_member_info
            .as_ref()
            .is_some_and(|c| !c.member_info.deputies.is_empty())
    {
        return Err(if edit.deputies.is_empty() {
            NO_RECORD_NICKNAME
        } else {
            NO_RECORD
        }
        .to_owned());
    }
    // A grant is only honoured for a current member (a ban reads only those), so one whose
    // member has since left is dropped, and a record that was only that is refused.
    let mut edit = edit.clone();
    let asked = edit.deputies.len();
    edit.deputies
        .retain(|(t, grant)| !*grant || is_member(room, t));
    if edit.deputies.len() < asked && edit.nickname.is_none() && edit.deputies.is_empty() {
        return Err(GONE_MEMBER.to_owned());
    }
    let edit = &edit;
    let deputies = edit.apply_deputies(
        base.map(|b| b.member_info.deputies.clone())
            .unwrap_or_default(),
    )?;
    let preferred_nickname = match &edit.nickname {
        Some(nickname) => {
            let max = room
                .stored
                .room_state
                .configuration
                .configuration
                .max_nickname_size;
            if let Some(why) = nickname_refusal_in(nickname, max) {
                return Err(why);
            }
            if room.is_private() {
                let (secret, v) = room
                    .current_secret()
                    .ok_or_else(|| Blocked::NoSecret.reason().to_owned())?;
                seal_bytes(nickname.as_bytes(), &secret, v)
            } else {
                SealedBytes::public(nickname.as_bytes().to_vec())
            }
        }
        // A revoke is made again the way a grant is (a name held plain in a room that turned
        // private is sealed, never republished plain), but is not stopped by a name that can't
        // be made again: then the held one is carried as it is.
        None if edit.only_revokes() => match (carried_nickname(room, base), base) {
            (Ok(sealed), _) => sealed,
            (Err(_), Some(b)) => b.member_info.preferred_nickname.clone(),
            (Err(e), None) => return Err(e),
        },
        None => carried_nickname(room, base)?,
    };
    Ok((preferred_nickname, deputies))
}

/// Build and sign your `member_info` record for `edit` at `version`, as the web UI's nickname
/// field and deputy button do: signed with your key, and what `edit` leaves alone (your deputy
/// grants, or your nickname) carried over from `base` (#410), since republishing replaces the
/// whole record.
fn build_record(
    room: &LiveRoom,
    edit: &RecordEdit,
    version: u32,
    base: Option<&AuthorizedMemberInfo>,
) -> Result<(VerifyingKey, AuthorizedMemberInfo), String> {
    participation(room).map_err(|b| b.reason().to_owned())?;
    let sk = room
        .stored
        .self_sk
        .as_ref()
        .ok_or_else(|| Blocked::NoKey.reason().to_owned())?;
    // The room removes a member with no recent message as soon as it applies anything, so a
    // record sent with the re-adding would be dropped with the member. A message re-adds them
    // for good, and a change after it holds.
    if rejoin(room).0.is_some() {
        return Err(if edit.nickname.is_some() {
            PRUNED
        } else {
            PRUNED_DEPUTIES
        }
        .into());
    }
    let (preferred_nickname, deputies) = record_parts(room, edit, base)?;
    let me = MemberId::from(&sk.verifying_key());
    let info = AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: me,
            version,
            preferred_nickname,
            deputies,
        },
        sk,
    );
    Ok((sk.verifying_key(), info))
}

/// Whether `info`, built for `edit` on an earlier view of the room, is not what `edit` makes of
/// the room now (`base`): another device of yours changed the deputies or the nickname it
/// carried over, or the room changed kind. It is signed again, at a fresh version, so a grant
/// revoked elsewhere does not come back and a nickname renamed elsewhere is not undone.
fn record_stale(
    room: &LiveRoom,
    edit: &RecordEdit,
    info: &AuthorizedMemberInfo,
    base: Option<&AuthorizedMemberInfo>,
) -> bool {
    if !privacy_matches(room, info) {
        return true;
    }
    match record_parts(room, edit, base) {
        // What it would be now can't be made: build again, which says why.
        Err(_) => true,
        Ok((nickname, deputies)) => {
            deputies != info.member_info.deputies
                || (edit.nickname.is_none()
                    && room.unseal(&nickname) != room.unseal(&info.member_info.preferred_nickname))
        }
    }
}

/// The version of your `member_info` record the room (or the record cached for rejoining)
/// already holds, or 0.
fn held_info_version(room: &LiveRoom) -> u32 {
    let Some(vk) = room.stored.self_verifying_key() else {
        return 0;
    };
    let me = MemberId::from(&vk);
    let in_state = room
        .stored
        .room_state
        .member_info
        .canonical(me)
        .map_or(0, |i| i.member_info.version);
    let cached = room
        .stored
        .self_member_info
        .as_ref()
        .filter(|i| i.member_info.member_id == me)
        .map_or(0, |i| i.member_info.version);
    in_state.max(cached)
}

/// Where `info` stands in `room`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InfoState {
    /// The room's canonical record for the member is this one.
    Ours,
    /// The canonical record is another: a later version, or the same version that wins on the
    /// signature digest (`member_info_rank`), as when another device renamed at the same time.
    Superseded,
    /// The room holds nothing as new as this (it was not taken, or the member is gone).
    Absent,
}

fn info_state(room: &LiveRoom, info: &AuthorizedMemberInfo) -> InfoState {
    match room
        .stored
        .room_state
        .member_info
        .canonical(info.member_info.member_id)
    {
        Some(c) if c.signature == info.signature => InfoState::Ours,
        Some(c) if c.member_info.version >= info.member_info.version => InfoState::Superseded,
        _ => InfoState::Absent,
    }
}

/// Whether a record's sealing is the room's kind now: sealed in a private room, plain in a
/// public one. A record made while the room was the other kind must be signed again.
fn privacy_matches(room: &LiveRoom, info: &AuthorizedMemberInfo) -> bool {
    matches!(
        (&info.member_info.preferred_nickname, room.is_private()),
        (SealedBytes::Public { .. }, false) | (SealedBytes::Private { .. }, true)
    )
}

/// What a message is stamped with. Tests set it.
pub(super) type Clock = Box<dyn Fn() -> SystemTime + Send>;

/// What an entry labels itself as when it is about the room's details.
const CONFIG_LABEL: &str = "Your change to the room's details";
const BAN_LABEL: &str = "Your ban";
const ROTATE_LABEL: &str = "Your secret rotation";
const DM_LABEL: &str = "Your direct message";
const PURGE_LABEL: &str = "Your deletion of their messages";

pub(super) const CONFIG_SUPERSEDED: &str =
    "A newer change to the room's details, made elsewhere, replaced this one.";
pub(super) const CONFIG_NO_VERSION_LEFT: &str =
    "The room's details have been changed as many times as the room can count.";
const NOT_OWNER: &str = "Only the room's owner can change its details.";

/// The update that publishes `config`, the owner's next configuration.
pub(super) fn config_delta(config: &AuthorizedConfigurationV1) -> ChatRoomStateV1Delta {
    ChatRoomStateV1Delta {
        configuration: Some(config.clone()),
        ..Default::default()
    }
}

/// Build and sign the owner's next configuration for `edit` at `version`, as the web UI's room
/// details dialog does: the configuration the room holds with the changes, the name and
/// description sealed under the room's CURRENT secret in a private room (never plain: a private
/// room with no secret here refuses, freenet/river#299; an empty description is `None`, which
/// has nothing to leak), normalized and checked as `riverctl` does before signing, signed with
/// the owner's key.
fn build_config(
    room: &LiveRoom,
    edit: &ConfigEdit,
    ctx: &BuildCtx,
) -> Result<AuthorizedConfigurationV1, String> {
    let sk = room
        .stored
        .self_sk
        .as_ref()
        .ok_or_else(|| Blocked::NoKey.reason().to_owned())?;
    if sk.verifying_key() != room.stored.owner_vk {
        return Err(NOT_OWNER.into());
    }
    let held = &ctx.base;
    let details = room.details_of(held);
    if let Some(why) = details_refusal(
        &details.limits,
        &details.name_source,
        &details.description_source,
        edit,
    ) {
        return Err(why);
    }
    let secret = if room.is_private() {
        Some(
            room.current_secret()
                .ok_or_else(|| Blocked::NoSecret.reason().to_owned())?,
        )
    } else {
        None
    };
    let seal = |text: &str| match secret {
        Some((secret, v)) => seal_bytes(text.as_bytes(), &secret, v),
        None => SealedBytes::public(text.as_bytes().to_vec()),
    };
    let mut next = held.clone();
    next.configuration_version = ctx
        .version
        .ok_or_else(|| CONFIG_NO_VERSION_LEFT.to_owned())?;
    if let Some(name) = &edit.name {
        next.display.name = seal(name.trim());
    }
    if let Some(description) = &edit.description {
        let description = description.trim();
        next.display.description = (!description.is_empty()).then(|| seal(description));
    }
    edit.apply_limits(&mut next);
    let params = room.params();
    next.normalize_for_signing(held, &params);
    if let Some(why) = next.signing_refusal(&params) {
        return Err(why);
    }
    Ok(AuthorizedConfigurationV1::new(next, sk))
}

/// What a configuration is built on, and at which version.
struct BuildCtx {
    /// The configuration to apply the owner's changes to: the room's, or one of this app's own
    /// that the node may yet take (sent and not refused), so a change made while another is on
    /// its way keeps that one's.
    base: Configuration,
    /// The version to sign at: above the base and above every change still waiting. `None`
    /// when no version is left.
    version: Option<u32>,
    /// What a new `member_info` record of yours is built on: the room's canonical one, or one of
    /// this app's own that the node may yet take (sent and not refused), so a nickname changed
    /// while a deputy change is on its way keeps that deputy, and the other way round. `None`
    /// when there is none.
    info: Option<AuthorizedMemberInfo>,
}

/// Where `config` stands in `room`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigState {
    /// The room's configuration is this one.
    Ours,
    /// The room holds another that outranks it: a later version, or the same version that wins
    /// on the signature digest, as when the owner's other device changed the room at the same
    /// time.
    Superseded,
    /// The room holds nothing as new as this (it was not taken).
    Absent,
}

fn config_state(room: &LiveRoom, config: &AuthorizedConfigurationV1) -> ConfigState {
    let held = &room.stored.room_state.configuration;
    if held.signature == config.signature {
        return ConfigState::Ours;
    }
    let params = room.params();
    if held.rank(&params) > config.rank(&params) {
        ConfigState::Superseded
    } else {
        ConfigState::Absent
    }
}

/// Whether the room holds a configuration at the version of `config` or later that is not
/// `config` itself: another device of the owner got there first (whichever wins the tie on the
/// signature digest), and `config`, built without that device's changes, would replace them.
fn config_outdated(room: &LiveRoom, config: &AuthorizedConfigurationV1) -> bool {
    let held = &room.stored.room_state.configuration;
    held.signature != config.signature
        && held.configuration.configuration_version >= config.configuration.configuration_version
}

/// Whether a configuration's sealing is the room's kind now: its name sealed in a private room,
/// plain in a public one. One made while the room was the other kind must be signed again.
fn config_privacy_matches(room: &LiveRoom, config: &AuthorizedConfigurationV1) -> bool {
    let c = &config.configuration;
    c.privacy_mode
        == room
            .stored
            .room_state
            .configuration
            .configuration
            .privacy_mode
        && c.display.name.is_public() != room.is_private()
}

/// A direct message that was queued: what names it in its thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmQueued {
    pub recipient: MemberId,
    /// The user's own member id, when the room's identity is known.
    pub sender: Option<MemberId>,
    pub token: PurgeToken,
    pub timestamp: u64,
}

/// A direct message of the outbox, as its thread shows it.
#[derive(Clone)]
pub struct DmRow {
    pub recipient: MemberId,
    pub sender: MemberId,
    pub token: PurgeToken,
    pub timestamp: u64,
    /// What was written. Not printed by `Debug`.
    pub text: Option<super::dm_cache::Plaintext>,
    pub pending: Pending,
}

impl std::fmt::Debug for DmRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmRow")
            .field("recipient", &self.recipient)
            .field("timestamp", &self.timestamp)
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

/// A deletion of someone's messages that the room does not hold yet, as the thread shows it.
#[derive(Clone, Debug)]
pub struct PurgeRow {
    pub peer: MemberId,
    /// How many messages it deletes.
    pub count: usize,
    pub pending: Pending,
}

/// What the user wrote or did, kept so it can be built again (on Retry, or sealed again).
#[derive(Clone)]
enum Intent {
    Text {
        text: String,
        reply: Option<ReplyWrap>,
    },
    Actions {
        label: &'static str,
        actions: Vec<ActionContentV1>,
    },
    /// A change to your own `member_info` record: your nickname, your deputies, or both.
    Record(RecordEdit),
    /// A change to the room's details.
    Config(ConfigEdit),
    /// A ban of one member.
    Ban(MemberId),
    /// A rotation of the room's secret.
    Rotate,
    /// A direct message to this member. The signed message is in the entry's `dm`.
    Dm(MemberId),
    /// A deletion of the messages this member sent the user. The signed envelope is in the
    /// entry's `purge`.
    Purge(MemberId),
}

/// As derived, except a nickname is not printed.
impl std::fmt::Debug for Intent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Intent::Text { text, reply } => f
                .debug_struct("Text")
                .field("text", text)
                .field("reply", reply)
                .finish(),
            Intent::Actions { label, actions } => f
                .debug_struct("Actions")
                .field("label", label)
                .field("actions", actions)
                .finish(),
            Intent::Record(edit) => f.debug_tuple("Record").field(edit).finish(),
            Intent::Config(edit) => f.debug_tuple("Config").field(edit).finish(),
            Intent::Ban(target) => f.debug_tuple("Ban").field(target).finish(),
            Intent::Rotate => f.write_str("Rotate"),
            Intent::Dm(peer) => f.debug_tuple("Dm").field(peer).finish(),
            Intent::Purge(peer) => f.debug_tuple("Purge").field(peer).finish(),
        }
    }
}

impl Intent {
    fn label(&self) -> &'static str {
        match self {
            Intent::Text { reply: None, .. } => "Your message",
            Intent::Text { reply: Some(_), .. } => "Your reply",
            Intent::Actions { label, .. } => label,
            Intent::Record(edit) => edit.label(),
            Intent::Config(_) => CONFIG_LABEL,
            Intent::Ban(_) => BAN_LABEL,
            Intent::Rotate => ROTATE_LABEL,
            Intent::Dm(_) => DM_LABEL,
            Intent::Purge(_) => PURGE_LABEL,
        }
    }

    fn plain_bodies(&self) -> Vec<(u32, u32, Vec<u8>)> {
        match self {
            Intent::Text { text, reply: None } => vec![(
                CONTENT_TYPE_TEXT,
                TEXT_CONTENT_VERSION,
                TextContentV1::new(text.clone()).encode(),
            )],
            Intent::Text {
                text,
                reply: Some(r),
            } => vec![(
                CONTENT_TYPE_REPLY,
                REPLY_CONTENT_VERSION,
                ReplyContentV1::new(
                    text.clone(),
                    r.id.clone(),
                    r.author_name.clone(),
                    r.preview.clone(),
                )
                .encode(),
            )],
            Intent::Actions { actions, .. } => actions
                .iter()
                .map(|a| (CONTENT_TYPE_ACTION, ACTION_CONTENT_VERSION, a.encode()))
                .collect(),
            // Not a message: built by `build_record`.
            Intent::Record(_) => Vec::new(),
            // Not a message: built by `build_config`.
            Intent::Config(_) => Vec::new(),
            // Not a message: built by `bans::build`.
            Intent::Ban(_) | Intent::Rotate => Vec::new(),
            // Not a room message: composed once, by `dm_send::prepare`.
            Intent::Dm(_) => Vec::new(),
            // Not a room message: built by `dm_purge::prepare`.
            Intent::Purge(_) => Vec::new(),
        }
    }
}

/// Build and sign `intent` in `room`, stamped `time`, as the web UI does. The author is the
/// key that signed.
fn build(
    room: &LiveRoom,
    intent: &Intent,
    time: SystemTime,
) -> Result<(VerifyingKey, Vec<AuthorizedMessageV1>), String> {
    participation(room).map_err(|b| b.reason().to_owned())?;
    let sk = room
        .stored
        .self_sk
        .as_ref()
        .ok_or_else(|| Blocked::NoKey.reason().to_owned())?;
    let secret = if room.is_private() {
        Some(
            room.current_secret()
                .ok_or_else(|| Blocked::NoSecret.reason().to_owned())?,
        )
    } else {
        None
    };
    let max = room
        .stored
        .room_state
        .configuration
        .configuration
        .max_message_size;
    let mut messages = Vec::new();
    for (content_type, content_version, plain) in intent.plain_bodies() {
        let content = match secret {
            None => RoomMessageBody::public_raw(content_type, content_version, plain),
            Some((secret, version)) => {
                let (ciphertext, nonce) = encrypt_with_symmetric_key(&secret, &plain);
                RoomMessageBody::private(content_type, content_version, ciphertext, nonce, version)
            }
        };
        // The room drops a body over its size limit; refuse it here instead, as the web UI's
        // composer does (it measures the same encoded size).
        if content.content_len() > max {
            return Err(format!(
                "It is too long for this room ({} bytes encoded; the room takes at most {max}).",
                content.content_len()
            ));
        }
        let message = MessageV1 {
            room_owner: MemberId::from(&room.stored.owner_vk),
            author: MemberId::from(&sk.verifying_key()),
            time,
            content,
        };
        messages.push(AuthorizedMessageV1::new(message, sk));
    }
    Ok((sk.verifying_key(), messages))
}

/// `room` with `messages` applied, or why the room would not take them.
fn applied(room: &LiveRoom, messages: &[AuthorizedMessageV1]) -> Result<LiveRoom, String> {
    let mut next = room.clone();
    next.apply_delta(delta(room, messages))
        .map_err(|e| format!("The room refused it: {e}"))?;
    if !held(&next, messages) {
        return Err(why_not_kept(&next, messages));
    }
    Ok(next)
}

/// Why `room`, with `messages` applied, does not hold them.
fn why_not_kept(room: &LiveRoom, messages: &[AuthorizedMessageV1]) -> String {
    let state = &room.stored.room_state;
    let versions: HashSet<u32> = state
        .secrets
        .versions
        .iter()
        .map(|v| v.record.version)
        .collect();
    if room.is_private()
        && messages
            .iter()
            .filter_map(|m| m.message.content.secret_version())
            .any(|v| !versions.contains(&v))
    {
        return KEY_NOT_HERE.into();
    }
    let keeps = state.configuration.configuration.max_recent_messages;
    let held = &state.recent_messages.messages;
    if held.len() >= keeps
        && messages.iter().any(|m| {
            held.first()
                .is_none_or(|o| m.message.time <= o.message.time)
        })
    {
        return CAPPED.into();
    }
    REMOVED.into()
}

pub(super) const KEY_NOT_HERE: &str =
    "The room's copy on this computer doesn't have the key it was sealed with yet.";
pub(super) const CAPPED: &str = "The room keeps only its newest messages, and this was not among \
     them. If this computer's clock is behind, its messages look older than they are.";
pub(super) const NO_VERSION_LEFT: &str =
    "Your nickname has been changed as many times as the room can count.";
pub(super) const SUPERSEDED: &str =
    "A newer change to your nickname, made elsewhere, replaced this one.";
pub(super) const RECORD_NO_VERSION_LEFT: &str =
    "Your nickname and deputies have been changed as many times as the room can count.";
pub(super) const RECORD_SUPERSEDED: &str =
    "A newer change to your nickname or deputies, made elsewhere, replaced this one.";
pub(super) const REMOVED: &str = "The room didn't take it. You may have been removed or banned.";
/// How many times an update the node never answers is sent again on a new connection before
/// it is left unconfirmed: each time costs every room a reconnect.
pub(super) const MAX_FORCED_RECONNECTS: u32 = 3;
pub(super) const NEVER_ANSWERED: &str = "Freenet never answered it, after several tries. It \
     may still have arrived; if it does, this goes away.";

fn held(room: &LiveRoom, messages: &[AuthorizedMessageV1]) -> bool {
    let ids: HashSet<MessageId> = room
        .stored
        .room_state
        .recent_messages
        .messages
        .iter()
        .map(|m| m.id())
        .collect();
    messages.iter().all(|m| ids.contains(&m.id()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    /// Waiting its turn: one update per room is out at a time.
    Queued,
    /// Sent to the node at `at`, waiting for its answer. `late` once unanswered for
    /// [`super::session::Pace::update_wait`]: shown as not confirmed, but it keeps its room's
    /// turn until it is answered, settled by the network, discarded, or the connection drops.
    /// An answer carries no request id, so while it might still come, the next update of the
    /// room cannot go out without its answer being taken for this one's.
    InFlight {
        at: Instant,
        late: bool,
    },
    /// The node answered that it took it, but the room here does not hold it (why is the
    /// text). It is not built or sent again; the network bringing it in settles it, or the
    /// user discards it.
    Unconfirmed(String),
    Failed(String),
}

const LATE: &str =
    "Freenet didn't confirm it in time. It may still arrive; if it does, this goes away.";

#[derive(Clone)]
struct Entry {
    id: u64,
    room: VerifyingKey,
    /// `None` when there was nothing to build (the message acted on is gone).
    intent: Option<Intent>,
    label: &'static str,
    author: Option<VerifyingKey>,
    time: SystemTime,
    messages: Vec<AuthorizedMessageV1>,
    /// For a nickname change: the signed record to publish (and `messages` is empty).
    info: Option<AuthorizedMemberInfo>,
    /// For a change to the room's details: the signed configuration to publish (and `messages`
    /// is empty).
    config: Option<AuthorizedConfigurationV1>,
    /// For a ban or a rotation: the signed ban and/or the rotated secret (and `messages` is
    /// empty).
    ban: Option<super::bans::Moderation>,
    /// For a rotation that stands in for a ban the room already holds (another device, or a
    /// deputy through the web UI, banned them): the name of the banned member, so what is said
    /// of it can mention the ban.
    after_ban: Option<String>,
    /// For a direct message: the signed message to publish (and `messages` is empty). It is
    /// composed once and only ever sent as it is: a new one would be sealed with new randomness,
    /// so the node, which may hold the first, would show both.
    dm: Option<AuthorizedDirectMessage>,
    /// For a direct message: what was written. Sealed to the recipient, so the sender cannot
    /// read their own message back; the thread shows this until the sent text is saved.
    dm_text: Option<super::dm_cache::Plaintext>,
    /// For a deletion of someone's messages ([`super::dm_purge`]): the signed envelope and the
    /// tokens it is for. Built again from the room as it is (`dm_purge::refresh`, which keeps
    /// it when still right) while it has not gone out and when a refused or unconfirmed one is
    /// retried; once out, only ever sent again as it is.
    purge: Option<super::dm_purge::Work>,
    /// For a direct message whose recipient the room no longer lists (the typed
    /// `Refusal::NotMember`, the only refusal that cannot heal): sending it again can only be
    /// refused again, so only Discard is offered. Other refusals keep Retry.
    no_retry: bool,
    status: Status,
    /// These messages went to the node at least once. From then on they are only ever sent
    /// again as they are, never rebuilt: the node may have taken them, and a rebuilt message
    /// has a new id, so it would show twice.
    sent: bool,
    /// How many connections were dropped because this was owed an answer for too long.
    forced: u32,
}

/// `Debug` shows where an entry stands, not what it carries (a nickname, a message).
impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("status", &self.status)
            .field("sent", &self.sent)
            .field("forced", &self.forced)
            .finish_non_exhaustive()
    }
}

impl Entry {
    fn is_text(&self) -> bool {
        matches!(self.intent, Some(Intent::Text { .. }))
    }

    /// Sealed under a secret version newer than `current`: building it again now would seal
    /// it backwards (the web UI's `sealed_at_or_below` refuses that).
    fn sealed_above(&self, current: Option<u32>) -> bool {
        self.secret_versions()
            .any(|v| current.is_none_or(|c| v > c))
    }

    /// The secret versions what it carries is sealed under.
    fn secret_versions(&self) -> impl Iterator<Item = u32> + '_ {
        self.messages
            .iter()
            .filter_map(|m| m.message.content.secret_version())
            .chain(
                self.info
                    .iter()
                    .filter_map(|i| i.member_info.preferred_nickname.secret_version()),
            )
            .chain(self.config.iter().flat_map(|c| {
                let display = &c.configuration.display;
                display.name.secret_version().into_iter().chain(
                    display
                        .description
                        .as_ref()
                        .and_then(|d| d.secret_version()),
                )
            }))
    }

    /// The update that publishes it.
    fn delta(&self, room: &LiveRoom) -> ChatRoomStateV1Delta {
        if let Some(ban) = &self.ban {
            return ban.delta();
        }
        if let Some(dm) = &self.dm {
            return super::dm_send::delta(room, dm);
        }
        if let Some(work) = &self.purge {
            return super::dm_purge::delta(&work.envelope);
        }
        match (&self.info, &self.config) {
            (Some(info), _) => info_delta(room, info),
            (None, Some(config)) => config_delta(config),
            (None, None) => delta(room, &self.messages),
        }
    }

    /// `room` with it applied, or why the room would not take it.
    fn applied(&self, room: &LiveRoom) -> Result<LiveRoom, String> {
        if let Some(dm) = &self.dm {
            return super::dm_send::applied(room, dm).map_err(|r| r.text());
        }
        if let Some(work) = &self.purge {
            return super::dm_purge::applied(room, &work.envelope, &work.tokens)
                .map_err(|r| r.text());
        }
        if let Some(ban) = &self.ban {
            let mut next = room.clone();
            next.apply_delta(ban.delta())
                .map_err(|e| format!("The room refused it: {e}"))?;
            return if ban.held_in(&next) {
                Ok(next)
            } else {
                Err(super::bans::NOT_KEPT.into())
            };
        }
        if let Some(config) = &self.config {
            let mut next = room.clone();
            next.apply_delta(config_delta(config))
                .map_err(|e| format!("The room refused it: {e}"))?;
            return match config_state(&next, config) {
                ConfigState::Ours => Ok(next),
                ConfigState::Superseded => Err(CONFIG_SUPERSEDED.into()),
                ConfigState::Absent => Err(REMOVED.into()),
            };
        }
        let Some(info) = &self.info else {
            return applied(room, &self.messages);
        };
        let mut next = room.clone();
        next.apply_delta(info_delta(room, info))
            .map_err(|e| format!("The room refused it: {e}"))?;
        match info_state(&next, info) {
            InfoState::Ours => Ok(next),
            InfoState::Superseded => Err(self.superseded().into()),
            InfoState::Absent => Err(REMOVED.into()),
        }
    }

    /// Whether `room` holds it.
    fn held_in(&self, room: &LiveRoom) -> bool {
        if let Some(dm) = &self.dm {
            return super::dm_send::landed(room, dm);
        }
        // Deleted is deleted, whoever's envelope lists the tokens.
        if let Some(work) = &self.purge {
            return super::dm_purge::covers(room, work.envelope.recipient_id, &work.tokens);
        }
        // A nickname the room replaced is settled too: there is nothing more to send.
        held(room, &self.messages)
            && self.ban.as_ref().is_none_or(|b| b.held_in(room))
            && self
                .info
                .as_ref()
                .is_none_or(|i| info_state(room, i) != InfoState::Absent)
            && self
                .config
                .as_ref()
                .is_none_or(|c| config_state(room, c) != ConfigState::Absent)
    }

    /// What it says when another record of yours replaced it.
    fn superseded(&self) -> &'static str {
        match &self.intent {
            Some(Intent::Record(edit)) => edit.superseded(),
            _ => SUPERSEDED,
        }
    }

    /// For a direct message the room would not take now, the typed refusal; `None` for anything
    /// else or when the room takes it.
    fn dm_refusal(&self, room: &LiveRoom) -> Option<super::dm_send::Refusal> {
        let dm = self.dm.as_ref()?;
        super::dm_send::applied(room, dm).err()
    }

    /// `why`, with a word that a deletion sent before may still be signed again by River.
    fn with_sent_note(&self, why: &str) -> String {
        if self.sent && self.purge.as_ref().is_some_and(|w| !w.heal) {
            format!("{why} This deletion was sent before, so River may still sign it again.")
        } else {
            why.to_owned()
        }
    }

    /// Whether it carries anything to publish.
    fn has_payload(&self) -> bool {
        !self.messages.is_empty()
            || self.info.is_some()
            || self.config.is_some()
            || self.ban.is_some()
            || self.dm.is_some()
            || self.purge.is_some()
    }

    /// Whether Retry can do anything for it.
    fn retryable(&self) -> bool {
        self.intent.is_some()
            && !self.no_retry
            && (matches!(
                self.status,
                Status::Failed(_) | Status::InFlight { late: true, .. }
            )
                // The node took a deletion and the room does not hold it (another device
                // deleted with a higher version that this copy had not seen, most likely): it is built again from the room.
                || (self.purge.is_some() && matches!(self.status, Status::Unconfirmed(_))))
    }

    /// Sealed under a secret version older than `current` (the web UI only seals forward).
    fn sealed_before(&self, current: Option<u32>) -> bool {
        let Some(current) = current else {
            return false;
        };
        self.secret_versions().any(|v| v < current)
    }

    /// Why it is shown as not sent, if it is.
    fn trouble(&self) -> Option<&str> {
        match &self.status {
            Status::Failed(reason) | Status::Unconfirmed(reason) => Some(reason),
            Status::InFlight { late: true, .. } => Some(LATE),
            _ => None,
        }
    }
}

/// What the user sent that the node has not taken yet. It outlives a connection, and a
/// session: the link keeps it while the session is stopped on something that needs the user.
pub(super) struct Outbox {
    entries: Vec<Entry>,
    next: u64,
    /// Commands that arrived before the rooms were read.
    pub waiting: Vec<Command>,
    /// Rooms being created or joined. Kept here so they outlive a connection and a session
    /// as what was sent does.
    pub work: super::work::Work,
    clock: Clock,
    /// By room, the entries sent on this connection whose answer has not come, oldest first,
    /// with when each was sent. An answer names only the contract, so it is the oldest one's.
    /// Nothing more goes out for a room while it owes an answer, whatever became of the entry
    /// (settled by the network, discarded, retried): otherwise the answer owed would be taken
    /// for the next update's. A new connection owes nothing.
    owed: HashMap<VerifyingKey, VecDeque<(u64, Instant)>>,
    /// The user asked to send again something still owed an answer: that needs a new
    /// connection, since a second copy on this one would owe a second answer to the same one.
    reconnect: bool,
    /// By room contract, how many answers were owed by a room that was left: they may still
    /// come, and are not the answer to anything sent to the room if it is back. A new
    /// connection owes nothing, so this empties with `owed`.
    stale: HashMap<ContractInstanceId, usize>,
    /// The member whose ban the viewer asked what it would do to the room's other bans.
    preview: Option<(VerifyingKey, MemberId)>,
    /// The last answer of [`Outbox::cached_preview`] and the digest of what it was for.
    preview_cache: std::cell::RefCell<Option<(u64, crate::chat::model::BanPreview)>>,
    /// This device's record of the deletions it sent ([`super::purge_record`]). Kept here so it
    /// outlives a session as what was sent does: a deletion is recorded as it first goes out.
    purge_record: super::purge_record::PurgeRecord,
    /// Entries that went out more than once (a connection ended while one was owed an answer):
    /// the node may have taken an earlier copy, so a refusal of a later one is not proof that
    /// the deletion never happened.
    resent: HashSet<u64>,
    /// Take-backs of the record that failed after the node refused a deletion (the storage
    /// failed): entry, room, recipient and tokens. Tried again by [`Outbox::retry_takebacks`];
    /// meanwhile the tokens are kept out of re-signing.
    takeback_owed: Vec<(u64, VerifyingKey, MemberId, Vec<PurgeToken>)>,
}

impl Default for Outbox {
    fn default() -> Self {
        Outbox {
            entries: Vec::new(),
            next: 0,
            waiting: Vec::new(),
            work: Default::default(),
            clock: Box::new(SystemTime::now),
            owed: HashMap::new(),
            reconnect: false,
            stale: HashMap::new(),
            preview: None,
            preview_cache: Default::default(),
            purge_record: Default::default(),
            resent: HashSet::new(),
            takeback_owed: Vec::new(),
        }
    }
}

/// An entry for a direct message that has no message in it. Not reachable from here (a
/// message is composed before its entry is made); said instead of sending nothing.
const DM_LOST: &str = "This direct message could not be prepared again. Write it again.";

/// An entry for a deletion that has no envelope in it. Not reachable from here.
const PURGE_LOST: &str = "This deletion could not be prepared again. Delete again.";

const IDENTITY_CHANGED: &str =
    "This room's identity changed after it was written, so it was not sent.";

impl Outbox {
    #[cfg(test)]
    pub fn with_clock(clock: Clock) -> Self {
        Outbox {
            clock,
            ..Outbox::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The version a new record of yours takes: above what the room holds and above every
    /// nickname still waiting, so ones written in a row are published in order.
    /// `None` when no version is left to sign at.
    fn fresh_info_version(&self, room: &LiveRoom, exclude: u64) -> Option<u32> {
        self.entries
            .iter()
            .filter(|e| e.room == room.stored.owner_vk && e.id != exclude)
            .filter_map(|e| e.info.as_ref())
            .map(|i| i.member_info.version)
            .fold(held_info_version(room), u32::max)
            .checked_add(1)
    }

    /// What the next configuration of `room` is built on ([`BuildCtx`]), leaving out the
    /// entry `exclude`.
    fn config_ctx(&self, room: &LiveRoom, exclude: u64) -> BuildCtx {
        let mut base = room.stored.room_state.configuration.configuration.clone();
        let mut top = base.configuration_version;
        let mut info = room
            .stored
            .self_verifying_key()
            .and_then(|vk| {
                room.stored
                    .room_state
                    .member_info
                    .canonical(MemberId::from(&vk))
            })
            .cloned();
        for e in self
            .entries
            .iter()
            .filter(|e| e.room == room.stored.owner_vk && e.id != exclude)
        {
            if let Some(i) = &e.info
                && e.sent
                && !matches!(e.status, Status::Failed(_))
                && info
                    .as_ref()
                    .is_none_or(|b| i.member_info.version > b.member_info.version)
            {
                info = Some(i.clone());
            }
            let Some(c) = &e.config else {
                continue;
            };
            let version = c.configuration.configuration_version;
            top = top.max(version);
            // Only one that went to the node can still land, and so must be kept.
            if e.sent
                && !matches!(e.status, Status::Failed(_))
                && version > base.configuration_version
            {
                base = c.configuration.clone();
            }
        }
        BuildCtx {
            base,
            version: top.checked_add(1),
            info,
        }
    }

    /// A configuration refused, or replaced, is no longer something a waiting one is built on:
    /// build each waiting configuration of `room` again on what the room holds.
    fn rebuild_waiting_configs(&mut self, room: &LiveRoom) {
        let ids: Vec<u64> = self
            .entries
            .iter()
            .filter(|e| {
                e.room == room.stored.owner_vk && e.status == Status::Queued && e.config.is_some()
            })
            .map(|e| e.id)
            .collect();
        for id in ids {
            let ctx = self.config_ctx(room, id);
            if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                e.config = None;
                rebuild(e, room, None, &ctx);
            }
        }
    }

    /// Say, as a notice to discard, something about a change the node took.
    fn note_taken(&mut self, room: &LiveRoom, reason: String) {
        self.next += 1;
        let entry = Entry {
            id: self.next,
            room: room.stored.owner_vk,
            intent: None,
            label: BAN_LABEL,
            author: room.stored.self_verifying_key(),
            time: (self.clock)(),
            messages: Vec::new(),
            info: None,
            config: None,
            ban: None,
            after_ban: None,
            dm: None,
            dm_text: None,
            purge: None,
            no_retry: false,
            status: Status::Unconfirmed(reason),
            sent: false,
            forced: 0,
        };
        self.entries.push(entry);
    }

    /// Say, as a trouble to discard, that part of a deputy change was lost.
    fn note(&mut self, room: &LiveRoom, reason: String) {
        self.next += 1;
        let entry = Entry {
            id: self.next,
            room: room.stored.owner_vk,
            intent: None,
            label: DEPUTY_NOTE_LABEL,
            author: room.stored.self_verifying_key(),
            time: (self.clock)(),
            messages: Vec::new(),
            info: None,
            config: None,
            ban: None,
            after_ban: None,
            dm: None,
            dm_text: None,
            purge: None,
            no_retry: false,
            status: Status::Failed(reason),
            sent: false,
            forced: 0,
        };
        self.entries.push(entry);
    }

    /// Before a waiting ban goes out: one whose member is already gone and banned by the room's
    /// own decision (another device of the owner, or a deputy through the web UI) has nothing
    /// left to ban. In a public room it is done. In a private room the banned member may still
    /// hold the current secret, and no rotation seen here can be told to have come after they
    /// left (one that landed while they were still a member gave them its secret), so the owner's
    /// entry goes out as a rotation alone, built when it is sent so it leaves them out. One
    /// rotation covers every such member, so a second is not queued behind it. Only the owner
    /// can rotate: for anyone else the ban is as done as it can be from here, and the notice
    /// says the owner must rotate.
    fn settle_bans(&mut self, room: &LiveRoom, changed: &mut bool) {
        let vk = room.stored.owner_vk;
        let me = room.stored.self_verifying_key();
        let me_id = me.map(|k| MemberId::from(&k));
        let ids: Vec<(u64, MemberId)> = self
            .entries
            .iter()
            .filter(|e| e.room == vk && e.status == Status::Queued && !e.sent)
            .filter_map(|e| match &e.intent {
                Some(Intent::Ban(t)) if super::bans::banned_and_gone(room, *t) => Some((e.id, *t)),
                _ => None,
            })
            .collect();
        for (id, target) in ids {
            *changed = true;
            let name = room.nickname(target, me_id);
            if !room.is_private() {
                self.entries.retain(|e| e.id != id);
                continue;
            }
            if me != Some(vk) {
                self.entries.retain(|e| e.id != id);
                self.note_taken(
                    room,
                    format!(
                        "{name} is banned. This room is now private, so only the owner can \
                         change its secret; ask the owner to rotate it."
                    ),
                );
                continue;
            }
            let covered_by_a_waiting_rotation = self.entries.iter().any(|e| {
                e.id != id
                    && e.room == vk
                    && e.status == Status::Queued
                    && !e.sent
                    && matches!(e.intent, Some(Intent::Rotate))
            });
            if covered_by_a_waiting_rotation {
                self.entries.retain(|e| e.id != id);
                continue;
            }
            let ctx = self.config_ctx(room, id);
            if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                e.intent = Some(Intent::Rotate);
                e.label = ROTATE_LABEL;
                e.ban = None;
                e.after_ban = Some(name);
                rebuild(e, room, None, &ctx);
            }
        }
    }

    /// Before a waiting record goes out: a grant whose member has left the room is not sent
    /// (and said), and a change already as asked (a revoke of one who is no deputy any more)
    /// is not published as a new version that changes nothing. A record left with no change is
    /// dropped as done.
    fn settle_deputy_changes(&mut self, room: &LiveRoom, changed: &mut bool) {
        let vk = room.stored.owner_vk;
        let me = room.stored.self_verifying_key().map(|k| MemberId::from(&k));
        let ids: Vec<u64> = self
            .entries
            .iter()
            .filter(|e| e.room == vk && e.status == Status::Queued && !e.sent)
            .filter(|e| matches!(&e.intent, Some(Intent::Record(r)) if !r.deputies.is_empty()))
            .map(|e| e.id)
            .collect();
        for id in ids {
            let ctx = self.config_ctx(room, id);
            let base: Vec<MemberId> = ctx
                .info
                .as_ref()
                .map(|i| i.member_info.deputies.clone())
                .unwrap_or_default();
            let Some(Intent::Record(edit)) = self
                .entries
                .iter()
                .find(|e| e.id == id)
                .and_then(|e| e.intent.clone())
            else {
                continue;
            };
            let mut gone = Vec::new();
            let mut kept = Vec::new();
            for (target, grant) in &edit.deputies {
                if *grant && !is_member(room, target) {
                    gone.push(*target);
                } else if base.contains(target) != *grant {
                    kept.push((*target, *grant));
                }
            }
            if kept.len() == edit.deputies.len() {
                continue;
            }
            *changed = true;
            for target in gone {
                let name = room.nickname(target, me);
                self.note(
                    room,
                    format!("The grant for {name} was not sent: they are no longer a member."),
                );
            }
            if edit.nickname.is_none() && kept.is_empty() {
                self.entries.retain(|e| e.id != id);
                continue;
            }
            let fresh = self.fresh_info_version(room, id);
            if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                let mut edit = edit;
                edit.deputies = kept;
                e.label = edit.label();
                e.intent = Some(Intent::Record(edit));
                e.info = None;
                rebuild(e, room, fresh, &ctx);
            }
        }
    }

    /// A record of yours refused, or replaced, is no longer something a waiting one is built on:
    /// build each waiting record of `room` again on what the room holds.
    fn rebuild_waiting_records(&mut self, room: &LiveRoom) {
        let ids: Vec<u64> = self
            .entries
            .iter()
            .filter(|e| {
                e.room == room.stored.owner_vk && e.status == Status::Queued && e.info.is_some()
            })
            .map(|e| e.id)
            .collect();
        for id in ids {
            let ctx = self.config_ctx(room, id);
            let fresh = self.fresh_info_version(room, id);
            if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                e.info = None;
                rebuild(e, room, fresh, &ctx);
            }
        }
    }

    /// Say something plainly in the room, as a failed entry with nothing to send: for what this
    /// app wanted to do for the user and could not.
    pub fn notice(&mut self, room: &LiveRoom, label: &'static str, reason: String) {
        let id = self.add(room, label, None);
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.status = Status::Failed(reason);
        }
    }

    fn add(&mut self, room: &LiveRoom, label: &'static str, mut intent: Option<Intent>) -> u64 {
        self.next += 1;
        let id = self.next;
        let fresh = self.fresh_info_version(room, id);
        if let Some(Intent::Config(edit)) = &mut intent {
            // A newer change to the details replaces every older one that is not on its way
            // (waiting, refused, or taken but replaced), keeping what the older ones changed
            // that this does not: the dialog starts from the room as shown, which counts them.
            // Keeping one would let its Retry bring the old values back at a higher version.
            // One that is out keeps its turn: the node's answer for it is still owed.
            let mut merged = std::mem::take(edit);
            let (older, keep): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut self.entries)
                .into_iter()
                .partition(|e| {
                    e.room == room.stored.owner_vk
                        && matches!(e.intent, Some(Intent::Config(_)))
                        && !matches!(e.status, Status::InFlight { .. })
                });
            self.entries = keep;
            for e in older {
                // Only what the dialog showed is kept: one that failed, or that the room
                // replaced, was not shown, and its values would come back over a newer change
                // (of this owner's other device, or the node's refusal of them).
                let shown = !matches!(e.status, Status::Failed(_)) && e.applied(room).is_ok();
                if shown && let Some(Intent::Config(old)) = &e.intent {
                    merged = merged.over(old);
                }
            }
            *edit = merged;
        }
        if let Some(Intent::Ban(target)) = &intent {
            // A newer ban of the same member replaces every older one that is not on its way
            // (waiting, refused, or taken but not shown): keeping one would let its Retry sign
            // the same ban again at a later time.
            let target = *target;
            self.entries.retain(|e| {
                !(e.room == room.stored.owner_vk
                    && matches!(&e.intent, Some(Intent::Ban(t)) if *t == target)
                    && !matches!(e.status, Status::InFlight { .. }))
            });
        }
        if matches!(intent, Some(Intent::Rotate)) {
            // A newer rotation replaces every older one that is not on its way: each is built
            // again at the version above the room's when it goes out, so two would only move
            // the secret twice.
            self.entries.retain(|e| {
                !(e.room == room.stored.owner_vk
                    && matches!(e.intent, Some(Intent::Rotate))
                    && !matches!(e.status, Status::InFlight { .. }))
            });
        }
        let mut label = label;
        if let Some(Intent::Record(edit)) = &mut intent {
            // A newer change to the record replaces every older one that is not on its way
            // (waiting, refused, or taken but replaced), keeping what the older ones changed
            // that this does not: a nickname typed, and a deputy made, one after the other
            // while the first still waits, are one record. Keeping one would let its Retry
            // bring the old values back at a higher version than this. One that is out keeps
            // its turn: the node's answer for it is still owed.
            let mut merged = std::mem::take(edit);
            let (older, keep): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut self.entries)
                .into_iter()
                .partition(|e| {
                    e.room == room.stored.owner_vk
                        && is_record_label(e.label)
                        && !matches!(e.status, Status::InFlight { .. })
                });
            self.entries = keep;
            let mut lost = Vec::new();
            for e in older {
                // Only what the room showed is kept: one that failed, or that the room
                // replaced, was not shown, and its values would come back over a newer change
                // (of this member's other device, or the node's refusal of them).
                let shown = !matches!(e.status, Status::Failed(_)) && e.applied(room).is_ok();
                if shown && let Some(Intent::Record(old)) = &e.intent {
                    merged = merged.over(old);
                } else if let (Status::Failed(why), Some(Intent::Record(old))) =
                    (&e.status, &e.intent)
                    && !old.deputies.is_empty()
                {
                    // A refused change to who is a deputy is not lost without a word.
                    lost.push(why.clone());
                }
            }
            for why in lost {
                self.note(room, why);
            }
            label = merged.label();
            *edit = merged;
        } else if intent.is_none() && is_record_label(label) {
            // One refused before it was built replaces the older refusals, but not a change
            // still waiting, which is not affected by it.
            self.entries.retain(|e| {
                !(e.room == room.stored.owner_vk
                    && is_record_label(e.label)
                    && matches!(e.status, Status::Failed(_)))
            });
        }
        let ctx = self.config_ctx(room, id);
        let mut entry = Entry {
            id,
            room: room.stored.owner_vk,
            intent,
            label,
            // Written as the room's identity now, whether or not it can be built yet.
            author: room.stored.self_verifying_key(),
            time: (self.clock)(),
            messages: Vec::new(),
            info: None,
            config: None,
            ban: None,
            after_ban: None,
            dm: None,
            dm_text: None,
            purge: None,
            no_retry: false,
            status: Status::Queued,
            sent: false,
            forced: 0,
        };
        rebuild(&mut entry, room, fresh, &ctx);
        self.entries.push(entry);
        id
    }

    /// Check, compose, sign and queue a direct message of `text` to `peer` (River's full member
    /// id) in `room` ([`super::dm_send`]). Nothing is queued unless the room would
    /// keep the message, so a refusal leaves nothing behind and the caller keeps the text.
    ///
    /// The text is kept with the entry for the thread to show (the sender cannot open their own
    /// message), and the entry rides the room's one-update-at-a-time queue like any other.
    /// Returns what the thread needs to name the message.
    pub fn send_dm(
        &mut self,
        room: &LiveRoom,
        peer: MemberId,
        text: &str,
    ) -> Result<DmQueued, String> {
        let now = (self.clock)()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let message = super::dm_send::prepare(room, peer, text, now).map_err(|r| r.text())?;
        let author = room.stored.self_verifying_key();
        let queued = DmQueued {
            recipient: peer,
            sender: author.map(|vk| MemberId::from(&vk)),
            token: message.purge_token(),
            timestamp: message.message.timestamp,
        };
        self.next += 1;
        self.entries.push(Entry {
            id: self.next,
            room: room.stored.owner_vk,
            intent: Some(Intent::Dm(peer)),
            label: DM_LABEL,
            author,
            time: (self.clock)(),
            messages: Vec::new(),
            info: None,
            config: None,
            ban: None,
            after_ban: None,
            dm: Some(message),
            dm_text: Some(super::dm_cache::Plaintext::new(text.to_owned())),
            purge: None,
            no_retry: false,
            status: Status::Queued,
            sent: false,
            forced: 0,
        });
        Ok(queued)
    }

    /// Make, compose, sign and queue an invitation to `target` as a direct message to `peer` in
    /// `room` ([`super::dm_invite`]), with `note`. Queued exactly like a text message
    /// ([`Outbox::send_dm`]); the text kept for its row is only the summary the web UI keeps
    /// ([`super::dm_invite::summary`]), which is also returned for the sent-message cache. The
    /// invitation is in the signed message only, sealed to `peer`.
    pub fn send_invite_dm(
        &mut self,
        room: &LiveRoom,
        target: &LiveRoom,
        peer: MemberId,
        note: Option<&str>,
    ) -> Result<(DmQueued, String), String> {
        let now = (self.clock)()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let (message, summary) =
            super::dm_invite::prepare(room, target, peer, note, now).map_err(|r| r.text())?;
        let author = room.stored.self_verifying_key();
        let queued = DmQueued {
            recipient: peer,
            sender: author.map(|vk| MemberId::from(&vk)),
            token: message.purge_token(),
            timestamp: message.message.timestamp,
        };
        self.next += 1;
        self.entries.push(Entry {
            id: self.next,
            room: room.stored.owner_vk,
            intent: Some(Intent::Dm(peer)),
            label: DM_LABEL,
            author,
            time: (self.clock)(),
            messages: Vec::new(),
            info: None,
            config: None,
            ban: None,
            after_ban: None,
            dm: Some(message),
            dm_text: Some(super::dm_cache::Plaintext::new(summary.clone())),
            purge: None,
            no_retry: false,
            status: Status::Queued,
            sent: false,
            forced: 0,
        });
        Ok((queued, summary))
    }

    /// The direct messages of `room` still in the outbox, oldest first: what its threads show
    /// for a message the room does not hold yet or that was not sent.
    pub fn dm_rows(&self, room: &VerifyingKey) -> Vec<DmRow> {
        self.entries
            .iter()
            .filter(|e| e.room == *room)
            .filter_map(|e| {
                let (Some(dm), Some(Intent::Dm(recipient))) = (&e.dm, &e.intent) else {
                    return None;
                };
                Some(DmRow {
                    recipient: *recipient,
                    sender: dm.message.sender,
                    token: dm.purge_token(),
                    timestamp: dm.message.timestamp,
                    text: e.dm_text.clone(),
                    pending: Pending {
                        entry: e.id,
                        failed: e.trouble().map(str::to_owned),
                        retry: e.retryable(),
                    },
                })
            })
            .collect()
    }

    /// The recipient and purge token of the direct message in entry `id` of `room`, if it is one.
    pub fn dm_identity(&self, room: &VerifyingKey, id: u64) -> Option<(MemberId, PurgeToken)> {
        self.entries
            .iter()
            .find(|e| e.id == id && e.room == *room)
            .and_then(|e| match (&e.dm, &e.intent) {
                (Some(dm), Some(Intent::Dm(r))) => Some((*r, dm.purge_token())),
                _ => None,
            })
    }

    /// Check, build, sign and queue a deletion of the messages `peer` sent the user in `room`
    /// whose purge tokens are `tokens` ([`super::dm_purge`]). Nothing is queued unless the room
    /// would keep it, so a refusal leaves nothing behind.
    pub fn purge_dm(
        &mut self,
        room: &LiveRoom,
        peer: MemberId,
        tokens: &[PurgeToken],
    ) -> Result<(), String> {
        let work = super::dm_purge::prepare(room, peer, tokens).map_err(|r| r.text())?;
        // The same deletion asked for again (after the window gave up waiting for the answer)
        // while it is still queued or out is that deletion: not a second entry.
        if self.entries.iter().any(|e| {
            e.room == room.stored.owner_vk
                && matches!(e.status, Status::Queued | Status::InFlight { .. })
                && e.purge.as_ref().is_some_and(|w| {
                    w.peer == peer && work.tokens.iter().all(|t| w.tokens.contains(t))
                })
        }) {
            return Ok(());
        }
        self.push_purge(room, work);
        Ok(())
    }

    /// Keep this device's record of deletions in `dir` (the app's config directory), unless it
    /// already is. Without a directory it stays in memory.
    pub fn keep_purge_record_in(&mut self, dir: Option<&std::path::Path>) {
        if let Some(dir) = dir
            && !self.purge_record.is_in(dir)
        {
            self.purge_record = super::purge_record::PurgeRecord::open(dir);
        }
    }

    /// This device's record of the deletions it sent.
    pub fn purge_record(&self) -> &super::purge_record::PurgeRecord {
        &self.purge_record
    }

    /// The same, to change it (pruning, forgetting a room left).
    pub fn purge_record_mut(&mut self) -> &mut super::purge_record::PurgeRecord {
        &mut self.purge_record
    }

    /// Whether a deletion the user asked for in `room` is queued or out: a re-signed envelope
    /// waits for it, since its own tokens are recorded already and it may be the one that puts
    /// them in the room. A failed one does not hold anything up: a refusal took its tokens out
    /// of the record, and one that was sent and then failed (requeued, then stopped by the
    /// identity check or River moving on) stays recorded and is signed again like any other.
    /// Re-signed envelopes themselves never count ([`Outbox::heal_in_progress`]).
    pub fn purge_pending(&self, room: &VerifyingKey) -> bool {
        self.entries.iter().any(|e| {
            e.room == *room
                && e.purge.as_ref().is_some_and(|w| !w.heal)
                && matches!(e.status, Status::Queued | Status::InFlight { .. })
        })
    }

    /// Whether a re-signed envelope for `room` is queued or out.
    pub fn heal_in_progress(&self, room: &VerifyingKey) -> bool {
        self.entries.iter().any(|e| {
            e.room == *room
                && e.purge.as_ref().is_some_and(|w| w.heal)
                && matches!(e.status, Status::Queued | Status::InFlight { .. })
        })
    }

    /// Drop the re-signed envelopes that failed or that the room did not keep. They are
    /// background work, never shown: the next winning envelope that omits a recorded token
    /// derives a new one. Returns how many went.
    pub fn drop_failed_heals(&mut self) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| {
            !(e.purge.as_ref().is_some_and(|w| w.heal)
                && matches!(e.status, Status::Failed(_) | Status::Unconfirmed(_)))
        });
        before - self.entries.len()
    }

    /// `own` without the tokens of a refused deletion whose record could not be taken back yet.
    pub fn without_abandoned(
        &self,
        room: &VerifyingKey,
        own: &mut river_core::own_purges::OwnPurges,
    ) {
        for (_, vk, me, tokens) in &self.takeback_owed {
            if vk == room {
                own.purged.retain(|(r, t)| !(r == me && tokens.contains(t)));
            }
        }
    }

    /// Queue the re-signed envelope `work` ([`super::dm_purge::heal`]) for `room`.
    pub fn heal_purge(&mut self, room: &LiveRoom, work: super::dm_purge::Work) {
        debug_assert!(work.heal);
        self.push_purge(room, work);
    }

    fn push_purge(&mut self, room: &LiveRoom, work: super::dm_purge::Work) {
        self.next += 1;
        self.entries.push(Entry {
            id: self.next,
            room: room.stored.owner_vk,
            intent: Some(Intent::Purge(work.peer)),
            label: PURGE_LABEL,
            author: room.stored.self_verifying_key(),
            time: (self.clock)(),
            messages: Vec::new(),
            info: None,
            config: None,
            ban: None,
            after_ban: None,
            dm: None,
            dm_text: None,
            purge: Some(work),
            no_retry: false,
            status: Status::Queued,
            sent: false,
            forced: 0,
        });
    }

    /// The deletions of `room` still in the outbox, oldest first: what the thread of the peer
    /// shows for them.
    pub fn purge_rows(&self, room: &VerifyingKey) -> Vec<PurgeRow> {
        self.entries
            .iter()
            .filter(|e| e.room == *room)
            .filter_map(|e| {
                let work = e.purge.as_ref().filter(|w| !w.heal)?;
                Some(PurgeRow {
                    peer: work.peer,
                    count: work.tokens.len(),
                    pending: Pending {
                        entry: e.id,
                        failed: e.trouble().map(str::to_owned),
                        retry: e.retryable(),
                    },
                })
            })
            .collect()
    }

    /// Take a command for `room`. `false` for a command that changed nothing.
    pub fn command(&mut self, room: &LiveRoom, action: Action) -> bool {
        let intent = match action {
            Action::Retry { entry } => return self.retry(entry, room),
            Action::PreviewBan { target } => {
                let found = room
                    .stored
                    .room_state
                    .members
                    .members
                    .iter()
                    .map(|m| m.member.id())
                    .find(|id| id.to_string() == target);
                let next = found.map(|id| (room.stored.owner_vk, id));
                let changed = self.preview != next;
                self.preview = next;
                return changed;
            }
            Action::Discard { entry } => {
                // Only what is shown as not sent, and only in the room it was written in.
                // The record of deletions is not touched: a dismissed entry may have been
                // taken by the node ([`super::purge_record`]).
                let before = self.entries.len();
                self.entries.retain(|e| {
                    e.id != entry || e.room != room.stored.owner_vk || e.trouble().is_none()
                });
                return self.entries.len() != before;
            }
            action => self.intent(room, action),
        };
        match intent {
            Ok(intent) => {
                self.add(room, intent.label(), Some(intent));
            }
            Err((label, reason)) => {
                let id = self.add(room, label, None);
                if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                    e.status = Status::Failed(reason);
                }
            }
        }
        true
    }

    /// What an action builds, against the room as shown (with what is still on its way).
    fn intent(&self, room: &LiveRoom, action: Action) -> Result<Intent, (&'static str, String)> {
        let me = room
            .stored
            .self_verifying_key()
            .map(|vk| MemberId::from(&vk));
        let own = |target: &str, label: &'static str| {
            let m = room
                .message_by_key(target)
                .ok_or((label, "That message is gone.".to_owned()))?;
            if Some(m.message.author) != me {
                return Err((label, "You can only change your own messages.".to_owned()));
            }
            Ok(m.id())
        };
        if let Action::SetRoomDetails { edit } = &action {
            // Refused before it is queued, so a bad value shows as a failed entry with its
            // reason rather than a silent no-op.
            let details = room.details();
            if edit.is_empty() {
                return Err((CONFIG_LABEL, "There is nothing to change.".into()));
            }
            return match details_refusal(
                &details.limits,
                &details.name_source,
                &details.description_source,
                edit,
            ) {
                Some(why) => Err((CONFIG_LABEL, why)),
                None => Ok(Intent::Config(edit.clone())),
            };
        }
        if let Action::RotateSecret = &action {
            let label = ROTATE_LABEL;
            let sk = room
                .stored
                .self_sk
                .as_ref()
                .ok_or((label, Blocked::NoKey.reason().to_owned()))?;
            if !room.is_private() {
                return Err((label, super::bans::NOT_PRIVATE.to_owned()));
            }
            if sk.verifying_key() != room.stored.owner_vk {
                return Err((label, super::bans::NOT_OWNER.to_owned()));
            }
            return Ok(Intent::Rotate);
        }
        if let Action::Ban { target } = &action {
            let label = BAN_LABEL;
            let target = room
                .stored
                .room_state
                .members
                .members
                .iter()
                .map(|m| m.member.id())
                .find(|id| id.to_string() == *target)
                .ok_or((label, super::bans::NOT_A_MEMBER.to_owned()))?;
            // Counting bans still on their way: a member already being banned is not banned twice.
            let shown = self.overlay(room).0;
            if !shown
                .stored
                .room_state
                .members
                .members
                .iter()
                .any(|m| m.member.id() == target)
            {
                return Err((label, "They are already being banned.".to_owned()));
            }
            let me = me.ok_or((label, Blocked::NoKey.reason().to_owned()))?;
            return match super::bans::gate(room, me, target) {
                Some(crate::chat::model::BanOffer::Allowed) => Ok(Intent::Ban(target)),
                Some(crate::chat::model::BanOffer::Withheld(why)) => Err((label, why)),
                None => Err((label, "You can't ban this member.".to_owned())),
            };
        }
        if let Action::SetNickname { nickname } = &action {
            let nickname = nickname.trim().to_owned();
            let max = room
                .stored
                .room_state
                .configuration
                .configuration
                .max_nickname_size;
            return match nickname_refusal_in(&nickname, max) {
                Some(why) => Err((NICKNAME_LABEL, why)),
                None => Ok(Intent::Record(RecordEdit::nickname(nickname))),
            };
        }
        if let Action::SetDeputy { target, grant } = &action {
            let label = DEPUTY_LABEL;
            let me = me.ok_or((label, Blocked::NoKey.reason().to_owned()))?;
            let owner = MemberId::from(&room.stored.owner_vk);
            // Yours as shown, counting changes still on their way.
            let shown = self.overlay(room).0;
            let held: Vec<MemberId> = shown
                .stored
                .room_state
                .member_info
                .canonical(me)
                .map(|i| i.member_info.deputies.clone())
                .unwrap_or_default();
            let target = std::iter::once(owner)
                .chain(
                    room.stored
                        .room_state
                        .members
                        .members
                        .iter()
                        .map(|m| m.member.id()),
                )
                .chain(held.iter().copied())
                .find(|id| id.to_string() == *target)
                .ok_or((label, "That member is gone.".to_owned()))?;
            if target == me {
                return Err((label, "You can't make yourself a deputy.".to_owned()));
            }
            if target == owner {
                return Err((
                    label,
                    "The room's owner already has full authority, so can't be made a deputy."
                        .to_owned(),
                ));
            }
            let is_deputy = held.contains(&target);
            if *grant && is_deputy {
                return Err((label, "They are already your deputy.".to_owned()));
            }
            if !*grant && !is_deputy {
                return Err((label, "They are not your deputy.".to_owned()));
            }
            if *grant && held.len() >= MAX_DEPUTIES {
                return Err((label, too_many_deputies()));
            }
            return Ok(Intent::Record(RecordEdit::deputy(target, *grant)));
        }
        let empty = match &action {
            Action::Send { text, .. } => text.trim().is_empty().then_some("Your message"),
            Action::Edit { text, .. } => text.trim().is_empty().then_some("Your edit"),
            _ => None,
        };
        if let Some(label) = empty {
            return Err((label, "It is empty.".to_owned()));
        }
        match action {
            Action::Send {
                text,
                reply_to: None,
            } => Ok(Intent::Text { text, reply: None }),
            Action::Send {
                text,
                reply_to: Some(target),
            } => {
                let m = room.message_by_key(&target).ok_or((
                    "Your reply",
                    "The message you replied to is gone.".to_owned(),
                ))?;
                Ok(Intent::Text {
                    text,
                    reply: Some(room.reply_wrap(m)),
                })
            }
            Action::Edit { target, text } => Ok(Intent::Actions {
                label: "Your edit",
                actions: vec![ActionContentV1::edit(own(&target, "Your edit")?, text)],
            }),
            Action::Delete { target } => Ok(Intent::Actions {
                label: "Your deletion",
                actions: vec![ActionContentV1::delete(own(&target, "Your deletion")?)],
            }),
            Action::React { target, emoji } => {
                let label = "Your reaction";
                let id = room
                    .message_by_key(&target)
                    .ok_or((label, "That message is gone.".to_owned()))?
                    .id();
                // Yours as shown, counting reactions still on their way.
                let shown = self.overlay(room).0;
                let current = me.and_then(|me| {
                    shown
                        .stored
                        .room_state
                        .recent_messages
                        .reactions(&id)?
                        .iter()
                        .find(|(_, by)| by.contains(&me))
                        .map(|(e, _)| e.clone())
                });
                let mut actions = Vec::new();
                if let Some(old) = &current {
                    actions.push(ActionContentV1::remove_reaction(id.clone(), old.clone()));
                }
                if current.as_deref() != Some(emoji.as_str()) {
                    actions.push(ActionContentV1::reaction(id, emoji));
                }
                Ok(Intent::Actions { label, actions })
            }
            // Retry and Discard are taken in `command`; the rest are not about a message, and
            // the session takes them before they get here.
            Action::Retry { .. }
            | Action::Discard { .. }
            | Action::SetNickname { .. }
            | Action::SetDeputy { .. }
            | Action::Ban { .. }
            | Action::RotateSecret
            | Action::PreviewBan { .. }
            | Action::SetRoomDetails { .. }
            | Action::Invite
            | Action::CreateRoom { .. }
            | Action::Join { .. }
            | Action::RetryWork { .. }
            | Action::DismissWork { .. }
            | Action::SetNotificationMode { .. }
            | Action::LeaveRoom
            | Action::ExportId
            | Action::ImportId { .. }
            | Action::SendDm { .. }
            | Action::PurgeDm { .. }
            | Action::SendInviteDm { .. }
            | Action::AcceptInviteDm { .. }
            | Action::ArchiveDm { .. }
            | Action::UnarchiveDm { .. }
            | Action::Viewing
            | Action::Resync => Err(("Your message", "Nothing to do.".to_owned())),
        }
    }

    fn retry(&mut self, id: u64, room: &LiveRoom) -> bool {
        let fresh = self.fresh_info_version(room, id);
        let ctx = self.config_ctx(room, id);
        let current = room.current_secret().map(|(_, v)| v);
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.id == id && e.room == room.stored.owner_vk)
        else {
            return false;
        };
        if !entry.retryable() {
            return false;
        }
        // Tried again: an owed take-back of its record is no longer owed, and one the node took
        // (shown as not kept) may be taken already, so a refusal of this try proves nothing.
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.takeback_owed)
            .into_iter()
            .partition(|o| o.0 == id);
        self.takeback_owed = rest;
        if let Some(work) = entry.purge.as_mut() {
            // Its tokens are this deletion's again: a refusal of this try takes them back.
            for (_, _, _, tokens) in mine {
                work.added = tokens;
            }
        }
        if matches!(entry.status, Status::Unconfirmed(_)) {
            self.resent.insert(id);
        }
        match &entry.status {
            // Unanswered: the same messages again, which the node may already hold, and only
            // on a new connection. On this one its answer is still owed, and a second copy
            // would owe a second answer that would be taken for the next update's.
            Status::InFlight { late: true, .. } => self.reconnect = true,
            // Sealed under a key newer than the room's copy here has (after a restart, before
            // the network caught up): building it again now would seal it backwards.
            Status::Failed(_) if entry.sealed_above(current) => {
                entry.status = Status::Failed(
                    "The room's newer key hasn't reached this computer yet, so it can't be \
                     sealed again. Try again once the room has caught up."
                        .into(),
                );
            }
            // Refused, or never sendable: a new send, stamped now, as the web UI's
            // `Outbox::retry`. Still as the identity it was written as (`rebuild` checks).
            _ => {
                entry.time = (self.clock)();
                entry.status = Status::Queued;
                entry.sent = false;
                // A refused nickname is signed again at a fresh version, never one it already
                // had: the node may have kept the refused record's version for another.
                let above_old = entry
                    .info
                    .as_ref()
                    .and_then(|i| i.member_info.version.checked_add(1));
                let had = entry.info.is_some();
                entry.info = None;
                let fresh = match (fresh, above_old) {
                    (Some(f), Some(o)) => Some(f.max(o)),
                    (_, None) if had => None,
                    (f, _) => f,
                };
                // Likewise a refused configuration: a fresh version, never one it had.
                // A refused ban is signed again, stamped now, against the room as it is.
                entry.ban = None;
                let had_config = entry.config.is_some();
                let above_old_config = entry
                    .config
                    .take()
                    .and_then(|c| c.configuration.configuration_version.checked_add(1));
                let ctx = BuildCtx {
                    version: match (ctx.version, above_old_config) {
                        (Some(f), Some(o)) => Some(f.max(o)),
                        (_, None) if had_config => None,
                        (f, _) => f,
                    },
                    ..ctx
                };
                rebuild(entry, room, fresh, &ctx);
            }
        }
        true
    }

    /// The update to send next for `room`, if the room owes no answer on this connection. The
    /// entry it carries is then in flight. `changed` is set when an entry failed here.
    pub fn next_update(
        &mut self,
        room: &LiveRoom,
        now: Instant,
        changed: &mut bool,
    ) -> Option<ChatRoomStateV1Delta> {
        let vk = room.stored.owner_vk;
        if self.owed.get(&vk).is_some_and(|o| !o.is_empty()) {
            return None;
        }
        self.settle_deputy_changes(room, changed);
        self.settle_bans(room, changed);
        let current = room.current_secret().map(|(_, v)| v);
        let me = room.stored.self_verifying_key();
        let fresh = self.fresh_info_version(room, 0);
        let ctxs: Vec<BuildCtx> = self
            .entries
            .iter()
            .map(|e| self.config_ctx(room, e.id))
            .collect();
        for (entry, ctx) in self.entries.iter_mut().zip(&ctxs) {
            if entry.room != vk || entry.status != Status::Queued {
                continue;
            }
            if entry.author.is_some() && entry.author != me {
                entry.status = Status::Failed(entry.with_sent_note(IDENTITY_CHANGED));
                *changed = true;
                continue;
            }
            // A record signed for what the room was is signed again for what it is: a nickname
            // made while the room was public must never go out plain into a room that has
            // since turned private, and the deputy grants and nickname it carries over must be
            // the ones the room holds now (a grant revoked elsewhere would come back, a rename
            // be undone). It is a new record, at a fresh version above what the room holds.
            if let (Some(info), Some(Intent::Record(edit))) = (&entry.info, &entry.intent) {
                let stale = !privacy_matches(room, info)
                    || (!entry.sent && record_stale(room, edit, info, ctx.info.as_ref()));
                if stale {
                    entry.info = None;
                    entry.sent = false;
                    rebuild(entry, room, fresh, ctx);
                    *changed = true;
                    if entry.status != Status::Queued {
                        continue;
                    }
                }
            }
            // A ban built for what the room was is built again for what it is, at the moment it
            // goes out: who remains to get the new secret, the secret version to move to and
            // whether the room is private are read from the room now, and the ban may no longer
            // be one this member can make.
            if !entry.sent && entry.ban.is_some() {
                entry.ban = None;
                rebuild(entry, room, fresh, ctx);
                *changed = true;
                if entry.status != Status::Queued {
                    continue;
                }
            }
            // A configuration signed for what the room was is signed again for what it is: its
            // name must never go out plain into a room that has since turned private, and the
            // room may have moved to a version it no longer outranks (another device of the
            // owner). It is a new one, built on what the room holds now.
            if !entry.sent
                && let Some(config) = &entry.config
                && (!config_privacy_matches(room, config) || config_outdated(room, config))
            {
                entry.config = None;
                rebuild(entry, room, fresh, ctx);
                *changed = true;
                if entry.status != Status::Queued {
                    continue;
                }
            }
            if !entry.sent {
                if let Err(blocked) = participation(room) {
                    entry.status = Status::Failed(blocked.reason().into());
                    *changed = true;
                    continue;
                }
                if let Some(work) = &entry.purge
                    && super::dm_purge::covers(room, work.envelope.recipient_id, &work.tokens)
                {
                    // The room already lists these tokens (this envelope reached it from the
                    // network): done, not to be sent or failed. `reconcile` settles it.
                    continue;
                }
                if entry.purge.is_some() {
                    // The room's envelope may have changed since this was built (a deletion
                    // from another device): one that would lift a tombstone is built again from
                    // the room as it is now, and one that is still right goes out as it is.
                    // Checks the room would keep it, as the other entries are checked below.
                    rebuild(entry, room, None, ctx);
                    if entry.status != Status::Queued {
                        *changed = true;
                        continue;
                    }
                } else {
                    if entry.sealed_before(current) {
                        // Sealed again under the room's current secret, keeping its time.
                        rebuild(entry, room, None, ctx);
                        *changed = true;
                        if entry.status != Status::Queued {
                            continue;
                        }
                    }
                    if let Err(e) = entry.applied(room) {
                        entry.status = Status::Failed(e);
                        entry.no_retry = entry
                            .dm_refusal(room)
                            .is_some_and(|r| super::dm_send::is_permanent(&r));
                        *changed = true;
                        continue;
                    }
                }
            }
            // A deletion is recorded before it first goes out (and again when one whose record
            // was taken back is tried again), so a crash after it is sent cannot lose the record
            // that lets a lost race be signed again. A record that cannot be kept stops the
            // deletion: sent unrecorded, it could come back unrepaired.
            if let Some(work) = &mut entry.purge
                && !work.heal
                && !work.recorded
            {
                // A token whose take-back is owed (an earlier try of it was refused and the
                // storage failed) is asked for again by this deletion: that take-back must not
                // undo it, and the token is this deletion's now.
                let me = work.envelope.recipient_id;
                let mut taken_over = Vec::new();
                for owed in &mut self.takeback_owed {
                    if owed.1 == vk && owed.2 == me {
                        owed.3.retain(|t| {
                            let ours = work.tokens.contains(t);
                            if ours {
                                taken_over.push(*t);
                            }
                            !ours
                        });
                    }
                }
                self.takeback_owed.retain(|o| !o.3.is_empty());
                match self.purge_record.record(&vk, me, &work.tokens) {
                    Ok(mut added) => {
                        added.extend(taken_over);
                        added.sort();
                        added.dedup();
                        work.recorded = true;
                        work.added = added;
                    }
                    Err(e) => {
                        crate::diag!(
                            "River: a deletion was not sent, its record was not saved: {}",
                            crate::diag::scrub(&e)
                        );
                        entry.status = Status::Failed(format!(
                            "The deletion was not sent: this computer could not save the record \
                             that lets River sign it again if another device of yours deletes at \
                             the same moment. {e}"
                        ));
                        *changed = true;
                        continue;
                    }
                }
            }
            if entry.sent {
                self.resent.insert(entry.id);
            }
            entry.status = Status::InFlight {
                at: now,
                late: false,
            };
            entry.sent = true;
            self.owed.entry(vk).or_default().push_back((entry.id, now));
            return Some(entry.delta(room));
        }
        None
    }

    /// Fail everything waiting to be sent, with `why`: River has moved on and this build
    /// writes nothing more ([`super::generation`]). What is out keeps its answer. True when
    /// anything failed.
    pub fn fail_queued(&mut self, why: &str) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            if entry.status == Status::Queued {
                entry.status = Status::Failed(entry.with_sent_note(why));
                changed = true;
            }
        }
        changed
    }

    /// The node answered an update for `room`: the oldest one it still owes an answer. If that
    /// entry is gone (settled by the network, or discarded) the answer is only consumed. Taken:
    /// its messages join the room at once, and the entry is done if the room kept them;
    /// otherwise it stays unconfirmed with why. Refused: it fails with the node's reason. True
    /// when that changed what shows.
    pub fn answered(&mut self, room: &mut LiveRoom, result: Result<(), String>) -> bool {
        let Some((id, _)) = self
            .owed
            .get_mut(&room.stored.owner_vk)
            .and_then(VecDeque::pop_front)
        else {
            return false;
        };
        let Some(i) = self
            .entries
            .iter()
            .position(|e| e.id == id && matches!(e.status, Status::InFlight { .. }))
        else {
            return false;
        };
        match result {
            Ok(()) => {
                let messages = self.entries[i].messages.clone();
                let entry = self.entries[i].clone();
                let applied = room.apply_delta(entry.delta(room));
                let state = entry.info.as_ref().map(|info| info_state(room, info));
                let config_state_now = entry.config.as_ref().map(|c| config_state(room, c));
                match applied {
                    // Taken, but another configuration outranks it (the owner's other device).
                    Ok(()) if config_state_now == Some(ConfigState::Superseded) => {
                        self.entries[i].status = Status::Unconfirmed(CONFIG_SUPERSEDED.into());
                    }
                    Ok(()) if state == Some(InfoState::Superseded) => {
                        // Taken, but another record of yours won: never the cache.
                        self.entries[i].status = Status::Unconfirmed(entry.superseded().into());
                    }
                    Ok(()) if entry.held_in(room) => {
                        // Keep what is cached for re-adding us in step with the edit, as the
                        // web UI's `record_self_nickname_edit` does, but only when this record
                        // is the room's.
                        if let (Some(info), Some(Intent::Record(edit))) =
                            (&entry.info, &entry.intent)
                        {
                            room.stored.self_member_info = Some(info.clone());
                            if let Some(nickname) = &edit.nickname {
                                room.stored.self_nickname = Some(nickname.clone());
                            }
                        }
                        let removed = self.entries.remove(i);
                        // A ban that made the room drop others' bans (it keeps a fixed number)
                        // says so, as a notice to discard.
                        if let Some(b) = &removed.ban
                            && !b.displaced.is_empty()
                        {
                            let names = b.displaced.clone();
                            let max = room
                                .stored
                                .room_state
                                .configuration
                                .configuration
                                .max_user_bans;
                            self.note_taken(room, super::bans::lifted_notice(&names, max));
                        }
                        if let Some(name) = &removed.after_ban {
                            self.note_taken(
                                room,
                                format!(
                                    "{name} was already banned. The room's secret is rotated \
                                     now, so they cannot read new messages."
                                ),
                            );
                        }
                        // A newer change of yours is the room's now, so an older one still
                        // waiting for its answer is explained by it.
                        self.entries.retain(|e| {
                            !(e.room == entry.room
                                && e.id < entry.id
                                && ((entry.info.is_some() && is_record_label(e.label))
                                    || (entry.config.is_some() && e.label == CONFIG_LABEL)))
                        });
                    }
                    // Taken by the node, so never built or sent again from here.
                    Ok(()) => {
                        self.entries[i].status = Status::Unconfirmed(format!(
                            "Freenet took it, but the room here doesn't show it. {}",
                            if entry.ban.is_some() {
                                super::bans::NOT_KEPT.to_owned()
                            } else if entry.dm.is_some() {
                                super::dm_send::NOT_KEPT.to_owned()
                            } else if entry.purge.is_some() {
                                super::dm_purge::NOT_KEPT.to_owned()
                            } else if entry.info.is_some() || entry.config.is_some() {
                                REMOVED.to_owned()
                            } else {
                                why_not_kept(room, &messages)
                            }
                        ));
                    }
                    Err(e) => {
                        crate::diag!(
                            "River: a message Freenet took was not applied here: {}",
                            crate::diag::scrub(&e)
                        );
                        self.entries[i].status = Status::Unconfirmed(format!(
                            "Freenet took it, but the room here couldn't apply it: {e}"
                        ));
                    }
                }
            }
            Err(reason) => {
                // A nickname (or change to the details) refused while a newer one of yours
                // exists is simply replaced: a Retry would sign it above the newer and put the
                // old values back.
                let (vk, id) = (self.entries[i].room, self.entries[i].id);
                let label = self.entries[i].label;
                let replaced = (is_record_label(label) || label == CONFIG_LABEL)
                    && self.entries.iter().any(|e| {
                        e.room == vk
                            && e.id > id
                            && if is_record_label(label) {
                                is_record_label(e.label)
                            } else {
                                e.label == label
                            }
                    });
                if replaced {
                    let lost = matches!(&self.entries[i].intent,
                        Some(Intent::Record(r)) if !r.deputies.is_empty());
                    self.entries.remove(i);
                    if lost {
                        self.note(room, format!("Freenet refused it: {reason}"));
                    }
                } else {
                    self.entries[i].status =
                        Status::Failed(format!("Freenet refused it: {reason}"));
                    self.take_back_refused_purge(i);
                }
                if label == CONFIG_LABEL {
                    self.rebuild_waiting_configs(room);
                }
                if is_record_label(label) {
                    self.rebuild_waiting_records(room);
                }
            }
        }
        true
    }

    /// The node refused entry `i`, a deletion: when it went out only once, the node never took
    /// it, so the tokens its recording newly added are taken back out of the record (it is no
    /// longer one to sign again). This is the only place a record is taken back. When that
    /// fails, the take-back is owed ([`Outbox::retry_takebacks`]) and the failure is shown.
    fn take_back_refused_purge(&mut self, i: usize) {
        let entry = &mut self.entries[i];
        let Some(work) = entry.purge.as_mut().filter(|w| !w.heal && w.recorded) else {
            return;
        };
        if self.resent.contains(&entry.id) {
            return;
        }
        let (id, vk, me) = (entry.id, entry.room, work.envelope.recipient_id);
        let added = std::mem::take(&mut work.added);
        match self.purge_record.unrecord(&vk, me, &added) {
            Ok(()) => work.recorded = false,
            Err(e) => {
                crate::diag!(
                    "River: a refused deletion stays recorded for now: {}",
                    crate::diag::scrub(&e)
                );
                self.takeback_owed.push((id, vk, me, added));
                if let Status::Failed(why) = &mut entry.status {
                    why.push_str(&format!(
                        " This computer could not update its record of your deletions yet: {e}"
                    ));
                }
            }
        }
    }

    /// Try again the take-backs that failed. True when any is still owed.
    pub fn retry_takebacks(&mut self) -> bool {
        for (id, vk, me, added) in std::mem::take(&mut self.takeback_owed) {
            match self.purge_record.unrecord(&vk, me, &added) {
                Ok(()) => {
                    if let Some(work) = self
                        .entries
                        .iter_mut()
                        .find(|e| e.id == id)
                        .and_then(|e| e.purge.as_mut())
                    {
                        work.recorded = false;
                    }
                }
                Err(e) => {
                    crate::diag!(
                        "River: a refused deletion stays recorded for now: {}",
                        crate::diag::scrub(&e)
                    );
                    self.takeback_owed.push((id, vk, me, added));
                }
            }
        }
        !self.takeback_owed.is_empty()
    }

    /// Updates unanswered for `wait` by `now` are shown as not confirmed (their room still
    /// owes the answer). True when any newly were.
    pub fn expire(&mut self, wait: Duration, now: Instant) -> bool {
        let mut any = false;
        for e in &mut self.entries {
            if let Status::InFlight { at, late } = &mut e.status
                && !*late
                && now.saturating_duration_since(*at) >= wait
            {
                *late = true;
                any = true;
            }
        }
        any
    }

    /// Whether this connection should be dropped: the user asked to send again something
    /// still owed an answer, or an answer has been owed for `bound`, which would otherwise
    /// hold its room for good. An entry that has cost [`MAX_FORCED_RECONNECTS`] connections
    /// this way is left unconfirmed, and not sent again.
    pub fn wants_reconnect(&mut self, bound: Duration, now: Instant) -> bool {
        let overdue: Vec<u64> = self
            .owed
            .values()
            .filter_map(VecDeque::front)
            .filter(|(_, at)| now.saturating_duration_since(*at) >= bound)
            .map(|(id, _)| *id)
            .collect();
        for e in self.entries.iter_mut().filter(|e| overdue.contains(&e.id)) {
            e.forced += 1;
            if e.forced >= MAX_FORCED_RECONNECTS {
                e.status = Status::Unconfirmed(NEVER_ANSWERED.into());
            }
        }
        self.reconnect || !overdue.is_empty()
    }

    /// Whether `room` owes an answer on this connection.
    pub fn owes(&self, room: &VerifyingKey) -> bool {
        self.owed.get(room).is_some_and(|o| !o.is_empty())
    }

    /// Whether `room` owes an answer that was asked for less than `cap` ago. An answer owed for
    /// longer is taken to be one that will not come, which is not a reason to hold anything back
    /// (the connection is dropped for it at [`Pace::owed_limit`](super::session::Pace)).
    pub fn owes_within(&self, room: &VerifyingKey, cap: Duration, now: Instant) -> bool {
        self.owed
            .get(room)
            .and_then(VecDeque::front)
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) < cap)
    }

    /// The room's answers owed are for a contract it no longer uses (re-keyed): they will not
    /// come under its key.
    /// What was in flight for it goes again, unchanged, under the new contract.
    pub fn forget_owed(&mut self, room: &VerifyingKey) {
        self.owed.remove(room);
        for e in self.entries.iter_mut().filter(|e| e.room == *room) {
            if matches!(e.status, Status::InFlight { .. }) {
                e.status = Status::Queued;
            }
        }
    }

    /// The connection is gone: nothing is owed on the next one, and what was in flight goes
    /// again on it, as the same signed messages, so one the node did take is not added twice.
    pub fn requeue(&mut self) {
        self.work.connection_lost();
        self.owed.clear();
        self.stale.clear();
        self.reconnect = false;
        for e in &mut self.entries {
            if matches!(e.status, Status::InFlight { .. }) {
                e.status = Status::Queued;
            }
        }
    }

    /// Entries whose messages `room` now holds (the network brought them in) are done,
    /// whatever their status. True when any were.
    ///
    /// A nickname the room holds as the canonical record is also written to the cache that
    /// re-adds a pruned member (as when the node answers), and one another record of yours
    /// replaced is not dropped silently: it stays as a notice saying so.
    pub fn reconcile(&mut self, room: &mut LiveRoom) -> bool {
        let mut changed = false;
        let mut cache = None;
        let vk = room.stored.owner_vk;
        let mut keep = Vec::with_capacity(self.entries.len());
        let entries = std::mem::take(&mut self.entries);
        // The newest change of yours here: an older one is replaced by it, not by "elsewhere".
        let newest_own = entries
            .iter()
            .filter(|e| e.room == vk && e.info.is_some())
            .map(|e| e.id)
            .max();
        // Likewise for the details.
        let newest_config = entries
            .iter()
            .filter(|e| e.room == vk && e.config.is_some())
            .map(|e| e.id)
            .max();
        for mut e in entries {
            if e.room != vk || !e.has_payload() {
                keep.push(e);
                continue;
            }
            match e.config.as_ref().map(|c| config_state(room, c)) {
                Some(ConfigState::Superseded) if newest_config.is_some_and(|n| n > e.id) => {
                    changed = true;
                    continue;
                }
                Some(ConfigState::Superseded) => {
                    let already = matches!(&e.status, Status::Failed(w) | Status::Unconfirmed(w) if w == CONFIG_SUPERSEDED);
                    // Waiting, it is built again on top of what the room holds when it goes
                    // out; only one that was sent is replaced.
                    if e.sent && !already {
                        e.status = Status::Unconfirmed(CONFIG_SUPERSEDED.into());
                        changed = true;
                    }
                    keep.push(e);
                    continue;
                }
                Some(ConfigState::Ours) => {
                    changed = true;
                    continue;
                }
                _ => {}
            }
            match e.info.as_ref().map(|i| info_state(room, i)) {
                Some(InfoState::Superseded) if newest_own.is_some_and(|n| n > e.id) => {
                    changed = true;
                }
                Some(InfoState::Superseded) => {
                    let text = e.superseded();
                    let already = matches!(&e.status, Status::Failed(w) | Status::Unconfirmed(w) if w == text);
                    if !already {
                        e.status = if e.sent {
                            Status::Unconfirmed(text.into())
                        } else {
                            Status::Failed(text.into())
                        };
                        changed = true;
                    }
                    keep.push(e);
                }
                Some(InfoState::Ours) if e.held_in(room) => {
                    if let (Some(info), Some(Intent::Record(edit))) = (&e.info, &e.intent) {
                        cache = Some((info.clone(), edit.nickname.clone()));
                    }
                    changed = true;
                }
                _ if e.held_in(room) => changed = true,
                _ => keep.push(e),
            }
        }
        self.entries = keep;
        if let Some((info, nickname)) = cache {
            room.stored.self_member_info = Some(info);
            if nickname.is_some() {
                room.stored.self_nickname = nickname;
            }
        }
        changed
    }

    /// Forget everything waiting for `room`: its unsent entries and the answers owed. For a
    /// room just left, as the web UI's `clear_outbox_for_room_now` does.
    /// The answers it owed (the room's contract is `id`) are remembered, to be discarded when
    /// they come.
    pub fn purge_room(&mut self, room: &VerifyingKey, id: ContractInstanceId) {
        self.preview = self.preview.take().filter(|(vk, _)| vk != room);
        self.entries.retain(|e| e.room != *room);
        if let Some(owed) = self.owed.remove(room)
            && !owed.is_empty()
        {
            *self.stale.entry(id).or_default() += owed.len();
        }
    }

    /// Whether `answer` for the contract `id` is one owed by a room that was left: it is taken
    /// and forgotten.
    pub fn take_stale(&mut self, id: &ContractInstanceId) -> bool {
        match self.stale.get_mut(id) {
            Some(n) if *n > 1 => {
                *n -= 1;
                true
            }
            Some(_) => {
                self.stale.remove(id);
                true
            }
            None => false,
        }
    }

    /// Whether answers owed by rooms left may still come, so an error that names nothing
    /// cannot be blamed on a room that is shown.
    pub fn has_stale(&self) -> bool {
        !self.stale.is_empty()
    }

    /// Forget the rooms no longer shown: what was waiting there goes with them.
    pub fn keep_rooms(&mut self, shown: &HashSet<VerifyingKey>) {
        self.preview = self.preview.take().filter(|(vk, _)| shown.contains(vk));
        self.entries.retain(|e| shown.contains(&e.room));
        self.owed.retain(|room, _| shown.contains(room));
    }

    /// `room` with what is on its way applied, which entries show as rows (by message key),
    /// and the troubles with no row to show them on.
    fn overlay(&self, room: &LiveRoom) -> Overlay {
        let mut shown = room.clone();
        let mut rows = HashMap::new();
        let mut notices = Vec::new();
        let mut own_nickname = None;
        let mut config_pending = None;
        let mut rotation_pending = None;
        let mut deputy_changes = Vec::new();
        // A direct message is not in the room's conversation: it is shown by its thread
        // ([`Outbox::dm_rows`]).
        for e in self
            .entries
            .iter()
            .filter(|e| e.room == room.stored.owner_vk && e.dm.is_none() && e.purge.is_none())
        {
            let trouble = e.trouble().map(str::to_owned);
            // A failed edit, deletion or reaction has no effect to show. One that is only
            // unconfirmed may yet land, so its effect shows (and the next reaction counts it).
            let show = e.is_text() || !matches!(e.status, Status::Failed(_));
            let on_screen = show
                && e.has_payload()
                && match e.applied(&shown) {
                    Ok(next) => {
                        shown = next;
                        true
                    }
                    Err(_) => false,
                };
            // Said as the dialog shows it: each outcome in its own words.
            let dialog_pending = || Pending {
                entry: e.id,
                failed: match &e.status {
                    Status::Failed(why) => Some(format!("Not saved. {why}")),
                    Status::Unconfirmed(why) => Some(why.clone()),
                    Status::InFlight { late: true, .. } => Some(LATE.to_owned()),
                    _ => None,
                },
                retry: e.retryable(),
            };
            if let Some(Intent::Record(edit)) = &e.intent {
                if edit.nickname.is_some() {
                    // The newest nickname change is the one the dialog shows.
                    own_nickname = Some(dialog_pending());
                }
                // Likewise, for each member, the newest change to whether they are a deputy.
                for (target, grant) in &edit.deputies {
                    // A grant for one who has left is not sent, so it is not shown as on its way.
                    if *grant && !is_member(room, target) {
                        continue;
                    }
                    let uid = target.to_string();
                    deputy_changes.retain(|c: &DeputyChange| c.uid != uid);
                    deputy_changes.push(DeputyChange {
                        uid,
                        grant: *grant,
                        pending: dialog_pending(),
                    });
                }
            }
            if matches!(e.intent, Some(Intent::Config(_))) {
                config_pending = Some(dialog_pending());
            }
            if matches!(e.intent, Some(Intent::Rotate)) {
                rotation_pending = Some(dialog_pending());
            }
            if on_screen && e.is_text() {
                for m in &e.messages {
                    rows.insert(
                        message_key(&m.id()),
                        Pending {
                            entry: e.id,
                            failed: trouble.clone(),
                            retry: e.retryable(),
                        },
                    );
                }
            } else if let Some(reason) = trouble {
                // A ban says whom: two can be waiting. The name is the room's own (a ban not
                // taken is not applied to `room`, so the member is still listed there).
                let what = match (&e.intent, &e.after_ban) {
                    (Some(Intent::Ban(target)), _) => {
                        format!("{} of {}", e.label, room.nickname(*target, None))
                    }
                    (_, Some(name)) => format!("{} after the ban of {name}", e.label),
                    _ => e.label.to_owned(),
                };
                // Said so that discarding a failed rotation does not hide the danger.
                let reason = match &e.after_ban {
                    Some(name) => format!(
                        "{reason} {name} is banned but can still read new messages until the \
                         secret is rotated."
                    ),
                    None => reason,
                };
                notices.push(Notice {
                    entry: e.id,
                    text: match e.status {
                        // Taken by the node, so not "couldn't be sent".
                        Status::Unconfirmed(_) => format!("{what}: {reason}"),
                        _ => format!("{what} couldn't be sent. {reason}"),
                    },
                    retry: e.retryable(),
                });
            }
        }
        (
            shown,
            rows,
            notices,
            own_nickname,
            config_pending,
            deputy_changes,
            rotation_pending,
        )
    }

    /// [`super::bans::preview`] of `target` on `basis`, worked out again only when the bans, the
    /// members or the limits it depends on are not what the last answer was for.
    fn cached_preview(&self, basis: &LiveRoom, target: MemberId) -> crate::chat::model::BanPreview {
        use std::hash::{Hash, Hasher};
        let state = &basis.stored.room_state;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        target.hash(&mut hasher);
        state
            .configuration
            .configuration
            .max_user_bans
            .hash(&mut hasher);
        state
            .configuration
            .configuration
            .max_members
            .hash(&mut hasher);
        let mut bytes = Vec::new();
        let _ = ciborium::ser::into_writer(&state.bans, &mut bytes);
        let _ = ciborium::ser::into_writer(&state.members, &mut bytes);
        let _ = ciborium::ser::into_writer(&state.member_info, &mut bytes);
        let _ = ciborium::ser::into_writer(&state.ban_evidence, &mut bytes);
        bytes.hash(&mut hasher);
        let key = hasher.finish();
        if let Some((k, p)) = &*self.preview_cache.borrow()
            && *k == key
        {
            return p.clone();
        }
        let p = super::bans::preview(basis, target);
        *self.preview_cache.borrow_mut() = Some((key, p.clone()));
        p
    }

    /// The chat screen's view of `room`, with what is on its way shown as if sent and marked.
    pub fn view(&self, room: &LiveRoom, minutes: &dyn Fn(SystemTime) -> u32) -> Room {
        // The member the viewer asked what a ban would do to the other bans, in this room.
        let asked = self
            .preview
            .as_ref()
            .filter(|(vk, _)| *vk == room.stored.owner_vk)
            .map(|(_, t)| *t);
        let mut basis: Option<LiveRoom> = None;
        let mut view = if self.entries.iter().any(|e| e.room == room.stored.owner_vk) {
            let (shown, rows, notices, own_nickname, config_pending, deputy_changes, rotation) =
                self.overlay(room);
            if asked.is_some() {
                basis = Some(shown.clone());
            }
            let mut view = shown.view(minutes);
            for m in &mut view.messages {
                m.pending = rows.get(&m.key).cloned();
            }
            view.notices = notices;
            view.own_nickname = own_nickname;
            view.details.pending = config_pending;
            view.details.rotation = rotation;
            view.deputy_changes = deputy_changes;
            view
        } else {
            room.view(minutes)
        };
        // Ban is offered by the room as it is, not as it will be: a member already being banned
        // is no longer listed.
        view.ban_offers = super::bans::offers(room);
        // What banning the member the viewer asked about does to the other bans, as it will be
        // when sent: with the bans still on their way applied. For that member only, and kept
        // until what it depends on changes, since working it out applies the room's ban cap.
        view.ban_preview.clear();
        if let Some(target) = asked
            && view.ban_offers.contains_key(&target.to_string())
        {
            let basis = basis.as_ref().unwrap_or(room);
            view.ban_preview
                .insert(target.to_string(), self.cached_preview(basis, target));
        }
        let listed: HashSet<&str> = view.members.iter().map(|m| m.uid.as_str()).collect();
        view.ban_offers
            .retain(|uid, _| listed.contains(uid.as_str()));
        view.ban_preview
            .retain(|uid, _| listed.contains(uid.as_str()));
        view.send_block = participation(room).err().map(|b| b.reason().to_owned());
        view.not_member = matches!(
            participation(room),
            Err(Blocked::Banned | Blocked::NotMember)
        );
        view.nickname_block = rejoin(room).0.is_some().then(|| PRUNED.to_owned());
        // A change to your deputies is a change to your record: the room must take it, and it
        // must have a nickname to carry over.
        view.deputy_block = view.send_block.clone().or_else(|| {
            view.nickname_block
                .is_some()
                .then(|| PRUNED_DEPUTIES.to_owned())
        });
        // A grant also needs a nickname to carry over and a record to build on; a revoke does
        // not (it is possible whenever the room takes a write from you, except that a member the
        // room removed for inactivity must send a message first).
        view.deputy_grant_block = (|| {
            let me = room
                .stored
                .self_verifying_key()
                .map(|vk| MemberId::from(&vk))?;
            let base = room.stored.room_state.member_info.canonical(me);
            record_parts(room, &RecordEdit::deputy(me, true), base)
                .err()
                .filter(|why| why != GONE_MEMBER)
        })();
        view
    }
}

/// `room` with what is on its way applied, the pending message rows, the notices, the pending
/// nickname and details changes, and the pending changes to who your deputies are.
type Overlay = (
    LiveRoom,
    HashMap<String, Pending>,
    Vec<Notice>,
    Option<Pending>,
    Option<Pending>,
    Vec<DeputyChange>,
    Option<Pending>,
);

/// Build `entry` again against `room`: at its time, under the room's current secret. Fails
/// it when it cannot be built, or when the room's identity is no longer the one it was
/// written as.
fn rebuild(entry: &mut Entry, room: &LiveRoom, fresh_version: Option<u32>, ctx: &BuildCtx) {
    let Some(intent) = &entry.intent else {
        return;
    };
    if let Intent::Dm(_) = intent {
        // Never composed again: the signed message is the one that goes out, however many
        // times (see `Entry::dm`). Only whether it still can is checked.
        if entry.dm.is_none() {
            entry.status = Status::Failed(DM_LOST.into());
        } else if entry.author.is_some() && entry.author != room.stored.self_verifying_key() {
            entry.status = Status::Failed(IDENTITY_CHANGED.into());
        } else if let Err(e) = entry.applied(room) {
            entry.status = Status::Failed(e);
            entry.no_retry = entry
                .dm_refusal(room)
                .is_some_and(|r| super::dm_send::is_permanent(&r));
        }
        return;
    }
    if let Intent::Purge(_) = intent {
        // The one rule: called while it has not gone out, and when a refused or unconfirmed one
        // is retried; never for one that is out or unanswered ([`dm_purge::refresh`] keeps the
        // envelope as it is when it is still right).
        let Some(work) = entry.purge.clone() else {
            entry.status = Status::Failed(PURGE_LOST.into());
            return;
        };
        if entry.author.is_some() && entry.author != room.stored.self_verifying_key() {
            entry.status = Status::Failed(entry.with_sent_note(IDENTITY_CHANGED));
            return;
        }
        match super::dm_purge::refresh(room, &work) {
            Ok(None) => {}
            Ok(Some(next)) => entry.purge = Some(next),
            Err(refusal) => {
                entry.status = Status::Failed(refusal.text());
                // Nothing left to delete cannot be sent again.
                entry.no_retry = refusal == super::dm_purge::Refusal::Nothing;
            }
        }
        return;
    }
    if let Intent::Config(edit) = intent {
        match build_config(room, edit, ctx) {
            Ok(config) => {
                // Written as the owner it is signed as.
                let author = room.stored.self_verifying_key();
                if entry.author.is_some() && entry.author != author {
                    entry.status = Status::Failed(IDENTITY_CHANGED.into());
                    return;
                }
                entry.author = author;
                entry.config = Some(config);
                if let Err(e) = entry.applied(room) {
                    entry.status = Status::Failed(e);
                }
            }
            Err(e) => entry.status = Status::Failed(e),
        }
        return;
    }
    if matches!(intent, Intent::Ban(_) | Intent::Rotate) {
        let built = match intent {
            Intent::Ban(target) => super::bans::build(room, *target, entry.time),
            _ => super::bans::build_rotation(room, entry.time),
        };
        match built {
            Ok(built) => {
                let author = room.stored.self_verifying_key();
                if entry.author.is_some() && entry.author != author {
                    entry.status = Status::Failed(IDENTITY_CHANGED.into());
                    return;
                }
                entry.author = author;
                entry.ban = Some(built);
                if let Err(e) = entry.applied(room) {
                    entry.status = Status::Failed(e);
                }
            }
            Err(e) => entry.status = Status::Failed(e),
        }
        return;
    }
    if let Intent::Record(edit) = intent {
        // Sealed again keeps the version it had; a new one takes `fresh_version`.
        let Some(version) = entry
            .info
            .as_ref()
            .map(|i| i.member_info.version)
            .or(fresh_version)
        else {
            entry.status = Status::Failed(edit.no_version_left().into());
            return;
        };
        match build_record(room, edit, version, ctx.info.as_ref()) {
            Ok((author, info)) => {
                if entry.author.is_some_and(|a| a != author) {
                    entry.status = Status::Failed(IDENTITY_CHANGED.into());
                    return;
                }
                entry.author = Some(author);
                entry.info = Some(info);
                if let Err(e) = entry.applied(room) {
                    entry.status = Status::Failed(e);
                }
            }
            Err(e) => entry.status = Status::Failed(e),
        }
        return;
    }
    match build(room, intent, entry.time) {
        Ok((author, messages)) => {
            if entry.author.is_some_and(|a| a != author) {
                entry.status = Status::Failed(IDENTITY_CHANGED.into());
                return;
            }
            entry.author = Some(author);
            entry.messages = messages;
            if let Err(e) = entry.applied(room) {
                entry.status = Status::Failed(e);
            }
        }
        Err(e) => entry.status = Status::Failed(e),
    }
}

#[cfg(test)]
#[path = "outbox_tests.rs"]
mod tests;
