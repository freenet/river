//! "Delete their messages" in a direct-message thread: the header action, its confirmation, the
//! answer, and what the thread says while the room has not taken the deletion.
//!
//! It follows the web UI's action of the same name and differs where noted. The signing and the
//! update are in `rooms::dm_purge`; this module only decides what the screen offers and says.
//!
//! - **Only what the user saw.** Opening the confirmation takes the purge tokens of the thread's
//!   messages from the peer as the thread shows them, and the dialog says how many. Pressing
//!   Delete deletes those and no others: a message that arrived while the dialog was open is
//!   not deleted (the web UI reads the room again on the click and deletes it too). The
//!   background work deletes only what it still finds in the room from that peer to the user,
//!   whatever the tokens name.
//! - **Only their messages.** Messages the user sent are never counted, offered or named: a
//!   recipient deletes, a sender cannot.
//! - **Off, with the reason in words,** when there is nothing to delete, the room's own state
//!   forbids writing (no key here, banned, not a member, a private room whose key has not
//!   arrived), River has moved on and this build writes nothing, the room has not been read
//!   from the network yet, or a deletion in this thread is still under way or not taken.
//! - **The answer.** The background work answers each ask by ticket: queued, or why not (the
//!   cap, a room that would not keep it). A refusal is shown above the messages until the next
//!   ask. A queued deletion is a line of the thread while the room does not hold it: "Deleting
//!   their messages", or why it was not taken, with Retry and Discard. Retry sends the same signed envelope again unless the room's list of
//!   deleted messages changed meanwhile, in which case it is signed again from the room as it is.
//!   Once the room holds it the messages are gone from the thread, and the read position of
//!   the thread forgets them.
//! - **Nothing is logged**, and nothing here has a `Debug` that prints a token or a message.

use std::time::{Duration, Instant};

use rune::{El, col, icon, row};
use rune_kit::{Button, ButtonVariant, Dialog, Kit};

use super::conversation::text_button;
use super::dm_model::DmPurge;
use super::icons;
use super::style::{self as st, Palette};
use super::view::label;
use super::{Chat, Source};
use crate::RiverApp;
use crate::rooms::dm::token_from_hex;
use crate::rooms::generation::UPDATE_REQUIRED_PURGE;
use crate::rooms::outbox::Action;

/// How long a deletion may wait for the background work's answer before the thread says it has
/// not heard back (and lets the user ask again: a second ask builds from the room as it is, and
/// deleting the same messages twice changes nothing).
pub const ANSWER_WAIT: Duration = Duration::from_secs(20);

/// What the thread says when an ask has not been answered for [`ANSWER_WAIT`].
pub const NO_ANSWER: &str = "River has not answered yet. The deletion may still go ahead: look \
     for the messages above before you ask again.";

/// Why the open thread's deletion is off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// The room is gone.
    RoomGone,
    /// River has moved on and this build writes nothing.
    UpdateRequired,
    /// The room's own state forbids writing, in the words the composer uses.
    Room(String),
    /// The room has not been read from the network yet.
    NotLoaded,
    /// A deletion in this thread is on its way.
    Under,
    /// A deletion in this thread was not taken: Retry or Discard it first.
    NotTaken,
    /// The thread holds no message from them.
    Nothing,
}

impl Block {
    /// The words, for `name` (the peer, as the thread shows them).
    pub fn text(&self, name: &str) -> String {
        match self {
            Block::RoomGone => "This room is no longer here.".to_owned(),
            Block::UpdateRequired => UPDATE_REQUIRED_PURGE.to_owned(),
            Block::Room(why) => why.clone(),
            Block::NotLoaded => "Waiting for this room to load from Freenet.".to_owned(),
            Block::Under => "Deleting their messages\u{2026}".to_owned(),
            Block::NotTaken => {
                "A deletion in this conversation was not taken. Retry or discard it first."
                    .to_owned()
            }
            Block::Nothing => format!("There are no messages from {name} to delete."),
        }
    }
}

/// The confirmation: the thread and exactly the messages it counted.
struct Confirm {
    room: String,
    peer: String,
    /// The messages' purge tokens in hex, as the thread keys them.
    keys: Vec<String>,
}

/// An ask the background work has not answered.
struct Asked {
    ticket: u64,
    room: String,
    peer: String,
    keys: Vec<String>,
    at: Instant,
    silent: bool,
}

/// The deletion state of the direct-message view.
#[derive(Default)]
pub struct DmPurgeUi {
    confirm: Option<Confirm>,
    asked: Vec<Asked>,
    /// Why the background work refused an ask: the thread, the ask's ticket and the words.
    refused: Option<((String, String), u64, String)>,
    next_ticket: u64,
}

impl std::fmt::Debug for DmPurgeUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmPurgeUi")
            .field("confirming", &self.confirm.as_ref().map(|c| c.keys.len()))
            .field("asked", &self.asked.len())
            .finish_non_exhaustive()
    }
}

impl DmPurgeUi {
    /// Whether the confirmation is showing.
    pub fn confirming(&self) -> bool {
        self.confirm.is_some()
    }

    /// Drop what waits for a room that is no longer listed.
    pub(super) fn forget_rooms(&mut self, listed: impl Fn(&str) -> bool) {
        self.asked.retain(|a| listed(&a.room));
        if self.refused.as_ref().is_some_and(|(k, _, _)| !listed(&k.0)) {
            self.refused = None;
        }
        if self.confirm.as_ref().is_some_and(|c| !listed(&c.room)) {
            self.confirm = None;
        }
    }

    /// Close the confirmation unless it is for the thread `open`.
    pub(super) fn keep_confirm_for(&mut self, open: Option<&(String, String)>) {
        if self
            .confirm
            .as_ref()
            .is_some_and(|c| open != Some(&(c.room.clone(), c.peer.clone())))
        {
            self.confirm = None;
        }
    }

    fn asking(&self, room: &str, peer: &str) -> bool {
        self.asked
            .iter()
            .any(|a| !a.silent && a.room == room && a.peer == peer)
    }

    fn silent_in(&self, room: &str, peer: &str) -> bool {
        self.asked
            .iter()
            .any(|a| a.silent && a.room == room && a.peer == peer)
    }
}

impl Chat {
    /// The open thread's messages from the peer, by purge token in hex: the ones a deletion
    /// would name. A message of the user's own is never here.
    fn dm_purge_keys(&self) -> Vec<String> {
        let Some((room, peer)) = self.dm.open_key() else {
            return Vec::new();
        };
        self.dm
            .snapshot()
            .threads
            .iter()
            .find(|t| t.room == room && t.peer_key == peer)
            .map(|t| {
                t.messages
                    .iter()
                    .filter(|m| !m.outgoing)
                    .map(|m| m.key.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The deletion of the open thread that the room does not hold yet, if there is one.
    fn dm_purge_row(&self) -> Option<&DmPurge> {
        let (room, peer) = self.dm.open_key()?;
        self.dm
            .snapshot()
            .purges
            .iter()
            .find(|p| p.room == room && p.peer_key == peer)
    }

    /// Why "Delete their messages" is off in the open thread, `None` when it is on.
    pub fn dm_purge_block(&self) -> Option<Block> {
        let (room_key, peer) = self.dm.open_key()?;
        let Some(room) = self.rooms().iter().find(|r| r.key == room_key) else {
            return Some(Block::RoomGone);
        };
        if self.issues.update_required {
            return Some(Block::UpdateRequired);
        }
        if let Some(why) = &room.send_block {
            return Some(Block::Room(why.clone()));
        }
        // A room met only as a stub, or read only from the store, may lack a deletion made
        // elsewhere: the background work refuses it too.
        if matches!(self.source, Source::Node(_))
            && !self.dm.snapshot().room_keys.iter().any(|k| k == room_key)
        {
            return Some(Block::NotLoaded);
        }
        match self.dm_purge_row() {
            Some(p) if p.pending.failed.is_none() => return Some(Block::Under),
            Some(_) => return Some(Block::NotTaken),
            None => {}
        }
        if self.dm.purge.asking(room_key, peer) {
            return Some(Block::Under);
        }
        if self.dm_purge_keys().is_empty() {
            return Some(Block::Nothing);
        }
        None
    }

    /// Open the confirmation for the open thread, counting the messages it shows from the peer
    /// now. False when the action is off.
    pub fn dm_purge_ask(&mut self) -> bool {
        if self.dm_purge_block().is_some() {
            return false;
        }
        let Some((room, peer)) = self
            .dm
            .open_key()
            .map(|(r, p)| (r.to_owned(), p.to_owned()))
        else {
            return false;
        };
        let keys = self.dm_purge_keys();
        self.dm.purge.confirm = Some(Confirm { room, peer, keys });
        true
    }

    /// Close the confirmation without deleting.
    pub fn dm_purge_cancel(&mut self) {
        self.dm.purge.confirm = None;
    }

    /// Whether the confirmation is showing.
    pub fn dm_purge_confirming(&self) -> bool {
        self.dm.purge.confirming()
    }

    /// How many messages the confirmation counted.
    pub fn dm_purge_count(&self) -> Option<usize> {
        self.dm.purge.confirm.as_ref().map(|c| c.keys.len())
    }

    /// [`Chat::dm_purge_confirm_at`] at the time it is.
    pub fn dm_purge_confirm(&mut self) -> bool {
        self.dm_purge_confirm_at(Instant::now())
    }

    /// Delete what the confirmation counted. The dialog closes whatever happens; the answer comes
    /// later ([`Chat::dm_purged`]). `now` times the wait for it ([`Chat::dm_tick`]). False when
    /// nothing was asked.
    pub fn dm_purge_confirm_at(&mut self, now: Instant) -> bool {
        let Some(confirm) = self.dm.purge.confirm.take() else {
            return false;
        };
        let key = (confirm.room.clone(), confirm.peer.clone());
        // What the thread says now may have changed since the dialog opened: the room, River's
        // version, a deletion already under way.
        if self.dm.open.as_ref() != Some(&key) || self.dm_purge_block().is_some() {
            return false;
        }
        if matches!(self.source, Source::Example) {
            // The example sends nothing.
            return true;
        }
        let Some(peer) = crate::rooms::dm::peer_from_key(&confirm.peer) else {
            return false;
        };
        let tokens: Vec<_> = confirm
            .keys
            .iter()
            .filter_map(|k| token_from_hex(k))
            .collect();
        if tokens.is_empty() {
            return false;
        }
        self.dm.purge.refused = None;
        self.dm.purge.next_ticket += 1;
        let ticket = self.dm.purge.next_ticket;
        self.dm.purge.asked.push(Asked {
            ticket,
            room: confirm.room.clone(),
            peer: confirm.peer,
            keys: confirm.keys,
            at: now,
            silent: false,
        });
        self.emit(
            confirm.room,
            Action::PurgeDm {
                peer,
                tokens,
                ticket,
            },
        );
        true
    }

    /// The ticket of the newest ask still waiting for its answer in the open thread, for scenes
    /// and tests.
    pub fn dm_purge_waiting_ticket(&self) -> Option<u64> {
        let (room, peer) = self.dm.open_key()?;
        self.dm
            .purge
            .asked
            .iter()
            .rev()
            .find(|a| a.room == room && a.peer == peer)
            .map(|a| a.ticket)
    }

    /// Mark the asks not answered within [`ANSWER_WAIT`] of `now`. True when that changed what
    /// shows.
    pub(super) fn dm_purge_tick(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for a in self.dm.purge.asked.iter_mut().filter(|a| !a.silent) {
            if now.saturating_duration_since(a.at) >= ANSWER_WAIT {
                a.silent = true;
                changed = true;
            }
        }
        changed
    }

    /// What became of the ask with `ticket`: queued (the thread forgets that it had seen those
    /// messages, which are on their way out), or why not (said above the messages). A late answer
    /// counts the same.
    pub fn dm_purged(&mut self, room: &str, peer: &str, ticket: u64, result: Result<(), String>) {
        let Some(at) = self.dm.purge.asked.iter().position(|a| a.ticket == ticket) else {
            return;
        };
        let asked = self.dm.purge.asked.remove(at);
        debug_assert!(asked.room == room && asked.peer == peer);
        let key = (asked.room.clone(), asked.peer.clone());
        match result {
            Ok(()) => {
                if self
                    .dm
                    .purge
                    .refused
                    .as_ref()
                    .is_some_and(|(k, t, _)| *k == key && *t <= ticket)
                {
                    self.dm.purge.refused = None;
                }
                self.dm.note_deleting(&asked.room, &asked.peer, asked.keys);
            }
            Err(why) => {
                let newer = self
                    .dm
                    .purge
                    .refused
                    .as_ref()
                    .is_some_and(|(k, t, _)| *k == key && *t > ticket);
                if !newer {
                    self.dm.purge.refused = Some((key, ticket, why));
                }
            }
        }
    }
}

// The view ------------------------------------------------------------------------------------

/// The action in the thread's header: a button, off with its reason as the tooltip. On a narrow
/// window it is the trash icon alone.
pub(super) fn header_action(chat: &Chat, narrow: bool, name: &str, p: &Palette, k: &Kit) -> El {
    let block = chat.dm_purge_block();
    let reason = block.as_ref().map(|b| b.text(name));
    let enabled = block.is_none();
    let help = reason
        .clone()
        .unwrap_or_else(|| "Delete their messages".to_owned());
    let mut action = if narrow {
        row()
            .key("dm-purge")
            .role("button")
            .label("Delete their messages")
            .size(36.0, 36.0)
            .radius(8.0)
            .center()
            .hover(p.surface_hover)
            .cursor(rune::Cursor::Pointer)
            .on_press(|s: &mut RiverApp| {
                s.chat.dm_purge_ask();
            })
            .child(icon(&icons::TRASH, 18.0).color(p.text_secondary))
    } else {
        Button::new("Delete their messages")
            .variant(ButtonVariant::Ghost)
            .on_press(|s: &mut RiverApp| {
                s.chat.dm_purge_ask();
            })
            .build(k)
            .key("dm-purge")
    };
    action = action.help(help);
    if !enabled {
        action = action.disabled(true).opacity(0.45);
    }
    action
}

/// The thread's lines about a deletion, above the messages: why it is off when that is worth
/// saying, a refusal, the deletion under way or not taken (with Retry and Discard), and an ask
/// that went unanswered. Empty when there is nothing to say.
pub(super) fn notes(chat: &Chat, name: &str, p: &Palette) -> Vec<El> {
    let mut out = Vec::new();
    let Some((room, peer)) = chat.dm.open_key() else {
        return out;
    };
    // "Nothing to delete" and "under way" have their own lines or need no words: the others are
    // said, because the button alone would leave the user guessing.
    if let Some(b @ (Block::UpdateRequired | Block::Room(_) | Block::NotLoaded | Block::RoomGone)) =
        chat.dm_purge_block()
    {
        out.push(
            label(
                format!("Delete their messages is off. {}", b.text(name)),
                st::SMALL,
                p.text_muted,
            )
            .key("dm-purge-off")
            .wrap()
            .w_full(),
        );
    }
    if let Some(purge) = chat.dm_purge_row() {
        out.push(purge_line(purge, p));
    }
    if let Some((key, _, why)) = &chat.dm.purge.refused
        && key.0 == room
        && key.1 == peer
    {
        out.push(
            label(format!("Not deleted. {why}"), st::SMALL, p.danger)
                .key("dm-purge-error")
                .wrap()
                .w_full(),
        );
    }
    if chat.dm.purge.silent_in(room, peer) {
        out.push(
            label(NO_ANSWER, st::SMALL, p.danger)
                .key("dm-purge-no-answer")
                .wrap()
                .w_full(),
        );
    }
    out
}

/// A queued deletion: under way, or why it was not taken with Retry and Discard.
fn purge_line(purge: &DmPurge, p: &Palette) -> El {
    let many = |n: usize| {
        if n == 1 {
            "1 message".to_owned()
        } else {
            format!("{n} messages")
        }
    };
    let Some(reason) = &purge.pending.failed else {
        return label(
            format!("Deleting {}\u{2026}", many(purge.count)),
            st::SMALL,
            p.text_muted,
        )
        .key("dm-purge-under")
        .wrap()
        .w_full();
    };
    let entry = purge.pending.entry;
    let mut choices = row().gap(12.0);
    if purge.pending.retry {
        choices = choices.child(text_button(
            format!("dm-purge-retry-{entry}"),
            "Retry",
            p.accent,
            move |s| s.chat.dm_retry(entry),
        ));
    }
    choices = choices.child(text_button(
        format!("dm-purge-discard-{entry}"),
        "Discard",
        p.text_secondary,
        move |s| s.chat.dm_discard(entry),
    ));
    col()
        .key("dm-purge-not-taken")
        .w_full()
        .gap(4.0)
        .child(
            label(
                if purge.pending.retry {
                    format!("Not deleted. {reason}")
                } else {
                    reason.clone()
                },
                st::SMALL,
                p.danger,
            )
            .wrap()
            .w_full(),
        )
        .child(choices)
}

/// The confirmation: what it deletes and for whom, in words.
pub(super) fn confirm_dialog(chat: &Chat, k: &Kit, layer: &mut Vec<El>) {
    let Some(c) = &chat.dm.purge.confirm else {
        return;
    };
    let p = st::palette();
    let thread = chat
        .dm
        .snapshot()
        .threads
        .iter()
        .find(|t| t.room == c.room && t.peer_key == c.peer);
    let name = thread.map_or_else(String::new, |t| super::dm_view::clean_name(&t.peer_name));
    let name = if name.is_empty() {
        "this person".to_owned()
    } else {
        name
    };
    let room_name = thread.map_or_else(String::new, |t| super::dm_view::clean_name(&t.room_name));
    let n = c.keys.len();
    let count = if n == 1 {
        "the message".to_owned()
    } else {
        format!("the {n} messages")
    };
    let body = col()
        .key("dm-purge-body")
        .w_full()
        .px(16.0)
        .pb(16.0)
        .gap(12.0)
        .child(
            label(
                format!(
                    "This deletes {count} {name} sent you in this room. They are removed from \
                     the room itself, so {name} no longer sees them in the conversation either, \
                     and this cannot be undone."
                ),
                st::BODY,
                p.text,
            )
            .key("dm-purge-what")
            .wrap()
            .w_full(),
        )
        .child(
            label(
                format!(
                    "Messages you sent {name} are not deleted, and neither are any {name} \
                     sends later."
                ),
                st::SMALL,
                p.text_secondary,
            )
            .key("dm-purge-kept")
            .wrap()
            .w_full(),
        )
        .child(
            label(
                "Each deleted message is recorded in the room so that it stays deleted. A \
                 room records at most 1000 deleted messages for you. If another device signed \
                 in as you has deleted messages recently, some messages can come back.",
                st::SMALL,
                p.text_muted,
            )
            .key("dm-purge-record")
            .wrap()
            .w_full(),
        )
        .child(
            row()
                .w_full()
                .gap(8.0)
                .justify_end()
                .child(
                    Button::new("Cancel")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut RiverApp| s.chat.dm_purge_cancel())
                        .build(k)
                        .key("dm-purge-cancel"),
                )
                .child(
                    Button::new(if n == 1 {
                        "Delete 1 message".to_owned()
                    } else {
                        format!("Delete {n} messages")
                    })
                    .variant(ButtonVariant::Danger)
                    .on_press(|s: &mut RiverApp| {
                        s.chat.dm_purge_confirm();
                    })
                    .build(k)
                    .key("dm-purge-confirm"),
                ),
        );
    Dialog::new("dm-purge", "Delete their messages?")
        .description(if room_name.is_empty() {
            name
        } else {
            format!("{name}, in {room_name}")
        })
        .width(460.0)
        .fit()
        .open(true)
        .on_close(|s: &mut RiverApp| s.chat.dm_purge_cancel())
        .child(body)
        .build(k, layer);
}
