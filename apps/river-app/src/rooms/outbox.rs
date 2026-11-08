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
//! - **A rotated secret.** A message sealed under a secret the room has moved on from is sealed
//!   again under the current one before it goes out, keeping its time (`take_committable`).
//!   A message written as an identity the room no longer uses is never sent under the new one.
//!
//! Where this differs: the web UI marks a room to sync and never hears whether the node took
//! the update. Here one update per room is out at a time, and its answer decides: an
//! `UpdateResponse` means sent, an update error means failed, and no answer within
//! [`super::session::Pace::update_wait`] means failed. A failed send stays on screen marked
//! "Not sent" with Retry and Discard; a failed edit, deletion or reaction is said above the
//! composer. Whatever the network later brings in with these messages' ids counts as sent,
//! whatever the answer was. Nothing here writes the chat delegate's storage, so the web UI's
//! after-send bookkeeping (its read position, its stored copy of the room) is not done: the
//! web UI picks the message up from the network.
//!
//! One deviation in rejoining: when the user has no published nickname record and never chose
//! a nickname, the web UI publishes a generated one; this app re-adds the member with no
//! nickname record, as the web UI itself does for a private room whose secret is missing, and
//! the web UI's own repair publishes one later.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};

use ed25519_dalek::VerifyingKey;
use river_core::ecies::{encrypt_with_symmetric_key, seal_bytes};
use river_core::room_state::ChatRoomStateV1Delta;
use river_core::room_state::content::{
    ACTION_CONTENT_VERSION, ActionContentV1, CONTENT_TYPE_ACTION, CONTENT_TYPE_REPLY,
    CONTENT_TYPE_TEXT, REPLY_CONTENT_VERSION, ReplyContentV1, TEXT_CONTENT_VERSION, TextContentV1,
};
use river_core::room_state::member::{AuthorizedMember, MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageId, MessageV1, RoomMessageBody,
};
use river_core::room_state::privacy::SealedBytes;

use super::room::{LiveRoom, message_key};
use crate::chat::model::{Notice, Pending, Room};

/// How many characters of the replied-to message's shown text a reply carries, as the web
/// UI's `reply_preview` takes 100 (of its markdown-stripped text).
const REPLY_PREVIEW_CHARS: usize = 100;

/// Something the user did in a room, sent from the chat screen to the background work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The room's key (`Room::key`).
    pub room: String,
    pub action: Action,
}

/// Messages are named by their key (`Message::key`).
#[derive(Clone, Debug, PartialEq, Eq)]
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
    let state = &room.stored.room_state;
    let me = MemberId::from(vk);
    let mut members = state.members.clone();
    if !members.members.iter().any(|m| m.member.id() == me) {
        if let Some(admitted) = &room.stored.self_authorized_member {
            members.members.push(admitted.clone());
        }
        let mut present: HashSet<MemberId> = members.members.iter().map(|m| m.member.id()).collect();
        for ancestor in &room.stored.invite_chain {
            if present.insert(ancestor.member.id()) {
                members.members.push(ancestor.clone());
            }
        }
    }
    members
        .banned_member_ids(
            &state.ban_evidence,
            &state.bans,
            &state.member_info,
            &room.params(),
        )
        .contains(&me)
}

/// The web UI's `build_rejoin_delta`: what re-adds a member the room pruned for inactivity.
/// Nothing for the owner, a current member, or a member with no stored admission.
fn rejoin(
    room: &LiveRoom,
) -> (Option<MembersDelta>, Option<Vec<AuthorizedMemberInfo>>) {
    let Some(sk) = &room.stored.self_sk else {
        return (None, None);
    };
    let vk = sk.verifying_key();
    let state = &room.stored.room_state;
    if vk == room.stored.owner_vk || state.members.members.iter().any(|m| m.member.member_vk == vk)
    {
        return (None, None);
    }
    let Some(admitted) = &room.stored.self_authorized_member else {
        return (None, None);
    };
    let present: HashSet<MemberId> = state.members.members.iter().map(|m| m.member.id()).collect();
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
            || matches!(info.member_info.preferred_nickname, SealedBytes::Private { .. })
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

/// What the user wrote or did, kept so it can be built again (on Retry, or sealed again).
#[derive(Clone, Debug)]
enum Intent {
    Text {
        text: String,
        reply: Option<ReplyTarget>,
    },
    Actions {
        label: &'static str,
        actions: Vec<ActionContentV1>,
    },
}

#[derive(Clone, Debug)]
struct ReplyTarget {
    id: MessageId,
    author_name: String,
    preview: String,
}

impl Intent {
    fn label(&self) -> &'static str {
        match self {
            Intent::Text { reply: None, .. } => "Your message",
            Intent::Text { reply: Some(_), .. } => "Your reply",
            Intent::Actions { label, .. } => label,
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
        return Err("The room didn't take it. You may have been removed or banned.".into());
    }
    Ok(next)
}

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
    /// Sent to the node at this time; waiting for its answer.
    InFlight(Instant),
    Failed(String),
}

#[derive(Clone, Debug)]
struct Entry {
    id: u64,
    room: VerifyingKey,
    /// `None` when there was nothing to build (the message acted on is gone).
    intent: Option<Intent>,
    label: &'static str,
    author: Option<VerifyingKey>,
    time: SystemTime,
    messages: Vec<AuthorizedMessageV1>,
    status: Status,
}

impl Entry {
    fn is_text(&self) -> bool {
        matches!(self.intent, Some(Intent::Text { .. }))
    }

    /// Sealed under a secret version other than `current`.
    fn stale_sealed(&self, current: Option<u32>) -> bool {
        self.messages
            .iter()
            .filter_map(|m| m.message.content.secret_version())
            .any(|v| Some(v) != current)
    }
}

/// What the user sent that the node has not taken yet, across reconnects.
#[derive(Default)]
pub(super) struct Outbox {
    entries: Vec<Entry>,
    next: u64,
}

impl Outbox {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn add(&mut self, room: &LiveRoom, label: &'static str, intent: Option<Intent>) -> u64 {
        self.next += 1;
        let id = self.next;
        let time = SystemTime::now();
        let mut entry = Entry {
            id,
            room: room.stored.owner_vk,
            intent,
            label,
            author: None,
            time,
            messages: Vec::new(),
            status: Status::Queued,
        };
        rebuild(&mut entry, room);
        self.entries.push(entry);
        id
    }

    /// Take a command for `room`. `false` for a command that names something gone.
    pub fn command(&mut self, room: &LiveRoom, action: Action) -> bool {
        let intent = match action {
            Action::Retry { entry } => return self.retry(entry, room),
            Action::Discard { entry } => {
                let before = self.entries.len();
                self.entries.retain(|e| e.id != entry);
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
    fn intent(
        &self,
        room: &LiveRoom,
        action: Action,
    ) -> Result<Intent, (&'static str, String)> {
        let me = room.stored.self_verifying_key().map(|vk| MemberId::from(&vk));
        let own = |target: &str, label: &'static str| {
            let m = room
                .message_by_key(target)
                .ok_or((label, "That message is gone.".to_owned()))?;
            if Some(m.message.author) != me {
                return Err((label, "You can only change your own messages.".to_owned()));
            }
            Ok(m.id())
        };
        let empty = match &action {
            Action::Send { text, .. } => text.trim().is_empty().then_some("Your message"),
            Action::Edit { text, .. } => text.trim().is_empty().then_some("Your edit"),
            _ => None,
        };
        if let Some(label) = empty {
            return Err((label, "It is empty.".to_owned()));
        }
        match action {
            Action::Send { text, reply_to: None } => Ok(Intent::Text { text, reply: None }),
            Action::Send {
                text,
                reply_to: Some(target),
            } => {
                let m = room
                    .message_by_key(&target)
                    .ok_or(("Your reply", "The message you replied to is gone.".to_owned()))?;
                Ok(Intent::Text {
                    text,
                    reply: Some(ReplyTarget {
                        id: m.id(),
                        author_name: room.member_name(m.message.author),
                        preview: room
                            .shown_text(m)
                            .chars()
                            .take(REPLY_PREVIEW_CHARS)
                            .collect(),
                    }),
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
            Action::Retry { .. } | Action::Discard { .. } => {
                Err(("Your message", "Nothing to do.".to_owned()))
            }
        }
    }

    fn retry(&mut self, id: u64, room: &LiveRoom) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) else {
            return false;
        };
        if !matches!(entry.status, Status::Failed(_)) || entry.intent.is_none() {
            return false;
        }
        // A retry is a new send, stamped now, as the web UI's `Outbox::retry`.
        entry.time = SystemTime::now();
        entry.author = None;
        entry.status = Status::Queued;
        rebuild(entry, room);
        true
    }

    /// The update to send next for `room`, if none of its updates is waiting for an answer.
    /// The entry it carries is then in flight. `changed` is set when an entry failed here.
    pub fn next_update(
        &mut self,
        room: &LiveRoom,
        now: Instant,
        changed: &mut bool,
    ) -> Option<ChatRoomStateV1Delta> {
        let vk = room.stored.owner_vk;
        if self
            .entries
            .iter()
            .any(|e| e.room == vk && matches!(e.status, Status::InFlight(_)))
        {
            return None;
        }
        let current = room.current_secret().map(|(_, v)| v);
        let me = room.stored.self_verifying_key();
        for entry in self.entries.iter_mut() {
            if entry.room != vk || entry.status != Status::Queued {
                continue;
            }
            if entry.author.is_some() && entry.author != me {
                entry.status = Status::Failed(
                    "This room's identity changed after it was written, so it was not sent."
                        .into(),
                );
                *changed = true;
                continue;
            }
            if entry.stale_sealed(current) {
                // Sealed again under the room's current secret, keeping its time.
                rebuild(entry, room);
                *changed = true;
                if entry.status != Status::Queued {
                    continue;
                }
            }
            if let Err(e) = applied(room, &entry.messages) {
                entry.status = Status::Failed(e);
                *changed = true;
                continue;
            }
            entry.status = Status::InFlight(now);
            return Some(delta(room, &entry.messages));
        }
        None
    }

    /// The node answered `room`'s update in flight. Taken: the entry, for the caller to apply
    /// to the room. Refused: the entry fails with the node's reason.
    pub fn answered(&mut self, room: &VerifyingKey, result: Result<(), String>) -> Option<Vec<AuthorizedMessageV1>> {
        let i = self
            .entries
            .iter()
            .position(|e| e.room == *room && matches!(e.status, Status::InFlight(_)))?;
        match result {
            Ok(()) => Some(self.entries.remove(i).messages),
            Err(reason) => {
                self.entries[i].status = Status::Failed(format!("Freenet refused it: {reason}"));
                None
            }
        }
    }

    /// Updates unanswered for `wait` fail. True when any did.
    pub fn expire(&mut self, wait: Duration) -> bool {
        let mut any = false;
        for e in &mut self.entries {
            if let Status::InFlight(at) = e.status
                && at.elapsed() >= wait
            {
                e.status = Status::Failed(
                    "Freenet didn't confirm it in time. It may still arrive; if it does, this \
                     goes away."
                        .into(),
                );
                any = true;
            }
        }
        any
    }

    /// The connection is gone: what was in flight goes again on the next one. The same signed
    /// messages are sent, so one the node did take is not added twice.
    pub fn requeue(&mut self) {
        for e in &mut self.entries {
            if matches!(e.status, Status::InFlight(_)) {
                e.status = Status::Queued;
            }
        }
    }

    /// Entries whose messages `room` now holds (the network brought them in) are done,
    /// whatever their status. True when any were.
    pub fn reconcile(&mut self, room: &LiveRoom) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| {
            e.room != room.stored.owner_vk || e.messages.is_empty() || !held(room, &e.messages)
        });
        self.entries.len() != before
    }

    /// Forget the rooms no longer shown: what was waiting there goes with them.
    pub fn keep_rooms(&mut self, shown: &HashSet<VerifyingKey>) {
        self.entries.retain(|e| shown.contains(&e.room));
    }

    /// `room` with what is on its way applied, which entries show as rows (by message key),
    /// and the failures with no row to show them on.
    fn overlay(&self, room: &LiveRoom) -> (LiveRoom, HashMap<String, Pending>, Vec<Notice>) {
        let mut shown = room.clone();
        let mut rows = HashMap::new();
        let mut notices = Vec::new();
        for e in self.entries.iter().filter(|e| e.room == room.stored.owner_vk) {
            let failed = match &e.status {
                Status::Failed(reason) => Some(reason.clone()),
                _ => None,
            };
            // A failed edit, deletion or reaction has no effect to show.
            let show = e.is_text() || failed.is_none();
            let on_screen = show
                && !e.messages.is_empty()
                && match applied(&shown, &e.messages) {
                    Ok(next) => {
                        shown = next;
                        true
                    }
                    Err(_) => false,
                };
            if on_screen && e.is_text() {
                for m in &e.messages {
                    rows.insert(
                        message_key(&m.id()),
                        Pending {
                            entry: e.id,
                            failed: failed.clone(),
                        },
                    );
                }
            } else if let Some(reason) = failed {
                notices.push(Notice {
                    entry: e.id,
                    text: format!("{} couldn't be sent. {reason}", e.label),
                    retry: e.intent.is_some(),
                });
            }
        }
        (shown, rows, notices)
    }

    /// The chat screen's view of `room`, with what is on its way shown as if sent and marked.
    pub fn view(&self, room: &LiveRoom, minutes: &dyn Fn(SystemTime) -> u32) -> Room {
        let mut view = if self.entries.iter().any(|e| e.room == room.stored.owner_vk) {
            let (shown, rows, notices) = self.overlay(room);
            let mut view = shown.view(minutes);
            for m in &mut view.messages {
                m.pending = rows.get(&m.key).cloned();
            }
            view.notices = notices;
            view
        } else {
            room.view(minutes)
        };
        view.send_block = participation(room).err().map(|b| b.reason().to_owned());
        view
    }
}

/// Build `entry` again against `room`: at its time, under the room's current secret. Fails
/// it when it cannot be built, or when the room's identity is no longer the one it was
/// written as.
fn rebuild(entry: &mut Entry, room: &LiveRoom) {
    let Some(intent) = &entry.intent else {
        return;
    };
    match build(room, intent, entry.time) {
        Ok((author, messages)) => {
            if entry.author.is_some_and(|a| a != author) {
                entry.status = Status::Failed(
                    "This room's identity changed after it was written, so it was not sent."
                        .into(),
                );
                return;
            }
            entry.author = Some(author);
            entry.messages = messages;
            if let Err(e) = applied(room, &entry.messages) {
                entry.status = Status::Failed(e);
            }
        }
        Err(e) => entry.status = Status::Failed(e),
    }
}

#[cfg(test)]
#[path = "outbox_tests.rs"]
mod tests;
