//! The composer of a direct-message thread: its own text, drafts, checks and send.
//!
//! It is not the room's composer. The room's `composer`, `mode` (a reply or an edit in progress),
//! `drafts` and mention state stay exactly as they were while a thread is open, so a reply
//! started in the room is never sent as a direct message and is still there when the thread is
//! closed. A thread has its own draft, kept in memory for the run, by room and peer.
//!
//! What it takes care over:
//!
//! - **The checks the screen can make** come from the room as shown ([`Chat::dm_block`]): the
//!   room's `send_block` (which includes the newer-River hold, so a read-only build cannot send),
//!   a peer who is not a member, the user themself, and the byte cap ([`BODY_CAP`]). They are
//!   drawn live as one line under the box and the send button is off, so nothing is stored for
//!   them. The background work checks again with the room's own state
//!   ([`crate::rooms::dm_send::preflight`]) and its refusal is shown the same way, kept until
//!   the next send.
//! - **The text is cleared only when the message was queued.** A send waits for the background
//!   work's answer ([`Chat::dm_sent`]); the box keeps its text until the answer is a success, and
//!   keeps it for a refusal. After that the message is a row of the thread, which carries its own
//!   failure and Retry (the same signed message is sent again, never composed again).
//! - **Nothing is logged or printed.** The text sent is held only until the answer, wiped when
//!   dropped, and its `Debug` shows a length.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rune::{El, col, icon, row};
use rune_kit::Kit;
use zeroize::Zeroize;

use super::conversation::text_button;
use super::dm_view::DmUi;
use super::icons;
use super::model::Pending;
use super::quick_reply::{MAX_DRAFT_CHARS, NO_ROOM, ReplyRefusal, appended, clean_reply};
use super::style::{self as st, Palette};
use super::view::label;
use super::{Chat, Source};
use crate::RiverApp;
use crate::rooms::dm_send::{self, BODY_CAP};
use crate::rooms::outbox::Action;

/// The most lines the composer grows to before it scrolls, as the room's does.
const MAX_LINES: usize = 8;
/// A message body's column, as the thread draws it.
const COLUMN_MAX_W: f32 = 60.0 + 604.0 + 24.0;

/// How long a send may wait for the background work's answer before the thread says it has not
/// heard back. The work takes commands every few hundred milliseconds, so this is long.
pub const ANSWER_WAIT: Duration = Duration::from_secs(20);

/// What a thread's composer says when a send has not been answered for [`ANSWER_WAIT`].
pub const NO_ANSWER: &str = "River has not answered yet. The message may still go out: look for it \
     above before you send it again.";

/// A send asked for and not answered yet, in one thread.
struct Sending {
    ticket: u64,
    room: String,
    peer: String,
    /// The box's whole text when Send was pressed, as it was sent. The answer takes exactly this
    /// out of the box and leaves what was typed since.
    sent: String,
    at: Instant,
    /// Not answered for [`ANSWER_WAIT`]: the thread says so and lets the user send again. The
    /// answer, if it ever comes, still takes `sent` out of the box.
    silent: bool,
    /// Sent from a notification's reply: the box is not the user's to change. The answer leaves
    /// the box alone, and a refusal keeps the text in [`DmCompose::stash`].
    notification: bool,
}

impl Drop for Sending {
    fn drop(&mut self) {
        self.sent.zeroize();
    }
}

/// The composer state of the thread view.
#[derive(Default)]
pub struct DmCompose {
    /// A draft per thread, by `(room key, peer key)`. Emptied ones are dropped as threads open
    /// and close.
    drafts: HashMap<(String, String), rune::TextEdit>,
    /// The box while no thread is open (never drawn; keeps an edit handler total).
    unused: rune::TextEdit,
    /// Sends waiting for their answer, any thread: what one thread waits for never holds
    /// another's Send.
    sends: Vec<Sending>,
    /// Why the background work refused a send: the thread, the send's ticket and the words.
    refused: Option<((String, String), u64, String)>,
    /// The newest ticket of each thread that was queued, so a late refusal of an older send does
    /// not show over a newer one that went through.
    queued_up_to: HashMap<(String, String), u64>,
    next_ticket: u64,
    /// Text of replies typed into notifications that were refused, for threads that are OPEN:
    /// their box is live and is never written to behind the user's back. Merged into the
    /// thread's draft when it is closed or left ([`Chat::merge_stash`]); for a thread that is not
    /// open the text goes to the draft at once.
    pub(super) stash: HashMap<(String, String), String>,
}

impl std::fmt::Debug for DmCompose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmCompose")
            .field("drafts", &self.drafts.len())
            .field("sends", &self.sends.len())
            .finish_non_exhaustive()
    }
}

impl DmCompose {
    /// The box of the thread `open`.
    fn edit_of(&mut self, open: Option<&(String, String)>) -> &mut rune::TextEdit {
        match open {
            Some(key) => self.drafts.entry(key.clone()).or_default(),
            None => &mut self.unused,
        }
    }

    /// Forget the drafts that hold nothing.
    pub(super) fn tidy(&mut self, keep: Option<&(String, String)>) {
        self.drafts
            .retain(|k, d| !d.text.is_empty() || Some(k) == keep);
    }

    /// The draft of a thread, for tests.
    #[cfg(test)]
    pub(super) fn draft(&self, room: &str, peer: &str) -> Option<&str> {
        self.drafts
            .get(&(room.to_owned(), peer.to_owned()))
            .map(|d| d.text.as_str())
    }

    #[cfg(test)]
    pub(super) fn refusal_for(&self, room: &str, peer: &str) -> Option<&str> {
        self.refused
            .as_ref()
            .filter(|(k, _, _)| k.0 == room && k.1 == peer)
            .map(|(_, _, why)| why.as_str())
    }

    pub(super) fn drafts_mut(&mut self) -> &mut HashMap<(String, String), rune::TextEdit> {
        &mut self.drafts
    }

    /// Whether the thread has a send still within its wait for an answer.
    pub(super) fn is_sending_in(&self, key: &(String, String)) -> bool {
        self.sends
            .iter()
            .any(|s| !s.silent && s.room == key.0 && s.peer == key.1)
    }

    /// Whether the thread has a send that was not answered in time and whose text is exactly what
    /// the box holds now. Sending it again composes a second, different message (fresh
    /// encryption, a new id), so if the first was only delayed the recipient gets both: the
    /// screen asks first.
    pub(super) fn needs_confirm(&self, key: &(String, String)) -> bool {
        let text = self.drafts.get(key).map_or("", |d| d.text.as_str());
        self.sends
            .iter()
            .any(|s| s.silent && s.room == key.0 && s.peer == key.1 && s.sent == text)
    }

    /// Whether the thread has a send, from anywhere, that was not answered in time and whose
    /// text is exactly `text`: sending it again may deliver it twice.
    pub(super) fn has_unanswered(&self, key: &(String, String), text: &str) -> bool {
        self.sends
            .iter()
            .any(|s| s.silent && s.room == key.0 && s.peer == key.1 && s.sent == text)
    }

    /// Whether the thread has a send that was not answered in time.
    pub(super) fn is_silent_in(&self, key: &(String, String)) -> bool {
        self.sends
            .iter()
            .any(|s| s.silent && s.room == key.0 && s.peer == key.1)
    }

    /// Drop what is waiting for a room that is no longer listed: its answer will not come.
    pub(super) fn forget_rooms(&mut self, listed: impl Fn(&str) -> bool) {
        self.sends.retain(|s| listed(&s.room));
        if self.refused.as_ref().is_some_and(|(k, _, _)| !listed(&k.0)) {
            self.refused = None;
        }
        self.queued_up_to.retain(|k, _| listed(&k.0));
        self.stash.retain(|k, _| listed(&k.0));
    }
}

/// Empty `edit` if it still holds exactly what was sent. A box the user has edited since (typed
/// into, or cleared and written again) is never touched: an answer, especially a late one, does
/// not know what the text in it now means. What this leaves is the sent text with new text after
/// it, which is shown for the user to deal with, not guessed at.
fn take_sent(edit: &mut rune::TextEdit, sent: &str) {
    if edit.text == sent {
        edit.clear();
    }
}

impl DmUi {
    #[cfg(test)]
    pub(super) fn compose_edit_focused(&mut self) -> bool {
        self.compose_edit().focused
    }

    /// The text box of the open thread. The edit handler of the field.
    pub(super) fn compose_edit(&mut self) -> &mut rune::TextEdit {
        let open = self.open.clone();
        self.compose.edit_of(open.as_ref())
    }
}

impl Chat {
    /// Why the open thread's composer cannot send now, whatever it holds: from the room as
    /// shown. `None` when it can.
    pub fn dm_block(&self) -> Option<String> {
        let (room_key, peer) = self.dm.open_key()?;
        let Some(room) = self.rooms().iter().find(|r| r.key == room_key) else {
            return Some("This room is no longer here.".to_owned());
        };
        if let Some(why) = &room.send_block {
            return Some(why.clone());
        }
        // A thread with a person who has left is still readable, and says so here. The member
        // is found by their full id, never by the label.
        match room.member_by_peer_key(peer) {
            None => Some(dm_send::Refusal::NotMember.text()),
            Some(m) if m.id == room.me => Some(dm_send::Refusal::SelfMessage.text()),
            Some(_) => None,
        }
    }

    /// Put `text` in the open thread's box, in place of what it held. For scenes and tests.
    pub fn dm_write(&mut self, text: &str) {
        self.dm.compose_edit().replace_text(text);
    }

    /// The bytes of what the open thread's box would send (as written), and the most it may be.
    pub fn dm_size(&self) -> Option<(usize, usize)> {
        let (room, peer) = self.dm.open_key()?;
        let edit = self
            .dm
            .compose
            .drafts
            .get(&(room.to_owned(), peer.to_owned()))?;
        Some((edit.text.len(), BODY_CAP))
    }

    /// Whether the open thread's box holds more than a message may.
    pub fn dm_over_limit(&self) -> bool {
        self.dm_size().is_some_and(|(n, cap)| n > cap)
    }

    /// [`Chat::dm_send_at`] at the time it is.
    pub fn dm_send(&mut self) -> bool {
        self.dm_send_at(Instant::now())
    }

    /// Whether the open thread's Send is waiting for the user to confirm: a send of this very
    /// text was not answered in time, so sending again may deliver it twice.
    pub fn dm_needs_confirm(&self) -> bool {
        self.dm
            .open
            .as_ref()
            .is_some_and(|k| self.dm.compose.needs_confirm(k))
    }

    /// [`Chat::dm_send_again_at`] at the time it is.
    pub fn dm_send_again(&mut self) -> bool {
        self.dm_send_again_at(Instant::now())
    }

    /// Send what the box holds although a send of the same text was not answered: the user
    /// pressed "Send again anyway".
    pub fn dm_send_again_at(&mut self, now: Instant) -> bool {
        self.dm_send_checked(now, true)
    }

    /// Send what the open thread's box holds, as written. Blank text sends nothing. The text
    /// stays in the box until the background work says the message was queued
    /// ([`Chat::dm_sent`]). `now` times the wait for that answer ([`Chat::dm_tick`]). False when
    /// nothing was sent.
    pub fn dm_send_at(&mut self, now: Instant) -> bool {
        self.dm_send_checked(now, false)
    }

    /// Enter and the plain Send button do not send again a text whose earlier send was not
    /// answered ([`Chat::dm_needs_confirm`]); `confirmed` is the user saying to.
    fn dm_send_checked(&mut self, now: Instant, confirmed: bool) -> bool {
        let Some((room, peer)) = self.dm.open.clone() else {
            return false;
        };
        let key = (room.clone(), peer.clone());
        let text = self
            .dm
            .compose
            .drafts
            .get(&key)
            .map(|d| d.text.clone())
            .unwrap_or_default();
        if text.trim().is_empty()
            || text.len() > BODY_CAP
            || self.dm.compose.is_sending_in(&key)
            || (!confirmed && self.dm.compose.needs_confirm(&key))
            || self.dm_block().is_some()
        {
            return false;
        }
        let Some(to) = crate::rooms::dm::peer_from_key(&peer) else {
            return false;
        };
        self.dm.compose.refused = None;
        if matches!(self.source, Source::Example) {
            // The example sends nothing: it only shows the box emptying.
            if let Some(d) = self.dm.compose.drafts.get_mut(&key) {
                d.clear();
            }
            return true;
        }
        self.dm.compose.next_ticket += 1;
        let ticket = self.dm.compose.next_ticket;
        self.dm.compose.sends.push(Sending {
            ticket,
            room: room.clone(),
            peer,
            sent: text.clone(),
            at: now,
            silent: false,
            notification: false,
        });
        self.emit(
            room,
            Action::SendDm {
                peer: to,
                text,
                ticket,
            },
        );
        true
    }

    /// The ticket of the newest send still waiting in the thread `(room, peer)`, for tests.
    #[cfg(test)]
    pub(super) fn dm_waiting_ticket_of(&self, room: &str, peer: &str) -> Option<u64> {
        self.dm
            .compose
            .sends
            .iter()
            .rev()
            .find(|s| s.room == room && s.peer == peer)
            .map(|s| s.ticket)
    }

    /// The ticket of the newest send still waiting for its answer in the open thread, for scenes
    /// and tests.
    pub fn dm_waiting_ticket(&self) -> Option<u64> {
        let (room, peer) = self.dm.open_key()?;
        self.dm
            .compose
            .sends
            .iter()
            .rev()
            .find(|s| s.room == room && s.peer == peer)
            .map(|s| s.ticket)
    }

    /// Mark the sends not answered within [`ANSWER_WAIT`] of `now`. True when that changed what
    /// shows.
    pub fn dm_tick(&mut self, now: Instant) -> bool {
        let mut changed = self.dm_purge_tick(now) | self.dm_invite_tick(now);
        changed |= self.dm_archive_tick(now);
        for s in self.dm.compose.sends.iter_mut().filter(|s| !s.silent) {
            if now.saturating_duration_since(s.at) >= ANSWER_WAIT {
                s.silent = true;
                changed = true;
            }
        }
        changed
    }

    /// What became of the send with `ticket`: queued (the box lets go of exactly what was sent,
    /// and keeps what was typed since), or why not (the box keeps its text and the reason
    /// shows). A late answer, after the thread said it had not heard back, counts the same.
    ///
    /// Returns the thread to scroll to its end (an own send goes to the end) when the send was
    /// queued and that thread is the one open: the thread the user switched away from is not
    /// moved, and the one they switched to is not either.
    pub fn dm_sent(
        &mut self,
        room: &str,
        peer: &str,
        ticket: u64,
        result: Result<(), String>,
    ) -> Option<(String, String)> {
        let at = self
            .dm
            .compose
            .sends
            .iter()
            .position(|s| s.ticket == ticket)?;
        let sending = self.dm.compose.sends.remove(at);
        debug_assert!(sending.room == room && sending.peer == peer);
        let key = (sending.room.clone(), sending.peer.clone());
        match result {
            Ok(()) => {
                let newest = self.dm.compose.queued_up_to.entry(key.clone()).or_default();
                *newest = (*newest).max(ticket);
                // A refusal of this send or an older one is over; a newer one's stands.
                if self
                    .dm
                    .compose
                    .refused
                    .as_ref()
                    .is_some_and(|(k, t, _)| *k == key && *t <= ticket)
                {
                    self.dm.compose.refused = None;
                }
                // A reply typed into a notification never owned the box.
                if !sending.notification
                    && let Some(d) = self.dm.compose.drafts.get_mut(&key)
                {
                    take_sent(d, &sending.sent);
                }
                // Sending into an archived thread brings it back (the background work does that
                // for the saved list): shown at once.
                if self.dm.archive.note_sent(&key.0, &key.1) {
                    self.dm.refresh_archive();
                }
                (self.dm.open.as_ref() == Some(&key)).then_some(key)
            }
            Err(why) => {
                // A refusal that comes after a newer send of this thread went through is
                // history: it must not show over what the user did since.
                let newer_ok = self
                    .dm
                    .compose
                    .queued_up_to
                    .get(&key)
                    .is_some_and(|t| *t > ticket);
                let newer_refusal = self
                    .dm
                    .compose
                    .refused
                    .as_ref()
                    .is_some_and(|(k, t, _)| *k == key && *t > ticket);
                if !newer_ok && !newer_refusal {
                    self.dm.compose.refused = Some((key.clone(), ticket, why));
                }
                if sending.notification {
                    self.keep_notification_reply(&key, &sending.sent);
                }
                None
            }
        }
    }

    /// The text typed into a direct-message notification's reply field, sent to the peer
    /// `peer` (their full member id) in the room keyed `room`, after the checks the thread's
    /// composer makes: the room is here and takes messages, the peer is a current member and
    /// not the user, the text fits, and no send of this thread is waiting. The window, the open
    /// thread and every other box are left alone.
    ///
    /// Not sent: the text is appended to that thread's draft (on a line of its own, up to
    /// [`MAX_DRAFT_CHARS`]) and the thread says why. A refusal that comes later from the
    /// background work keeps it the same way ([`Chat::dm_sent`]).
    pub fn send_dm_reply(
        &mut self,
        room: &str,
        peer: &str,
        text: &str,
        now: Instant,
    ) -> Result<(), ReplyRefusal> {
        let (text, cut) = clean_reply(text);
        if text.is_empty() {
            return Err(ReplyRefusal::Empty);
        }
        let Some(r) = self.rooms().iter().find(|r| r.key == room) else {
            self.reply_notices.insert(
                NO_ROOM.to_owned(),
                "A reply typed into a notification was not sent, and could not be kept: that room is no longer here."
                    .to_owned(),
            );
            return Err(ReplyRefusal::NoRoom);
        };
        let to = crate::rooms::dm::peer_from_key(peer);
        if !matches!(self.source, Source::Node(_)) || to.is_none() {
            return Err(ReplyRefusal::NoRoom);
        }
        let key = (room.to_owned(), peer.to_owned());
        let blocked = match (&r.send_block, r.member_by_peer_key(peer)) {
            (Some(why), _) => Some(why.clone()),
            (None, None) => Some(dm_send::Refusal::NotMember.text()),
            (None, Some(m)) if m.id == r.me => Some(dm_send::Refusal::SelfMessage.text()),
            _ => None,
        };
        let why = if let Some(why) = blocked {
            Some((ReplyRefusal::Blocked, why))
        } else if cut || text.len() > BODY_CAP {
            Some((ReplyRefusal::TooLong, "the message is too long".to_owned()))
        } else if self.dm.compose.is_sending_in(&key) {
            Some((
                ReplyRefusal::Blocked,
                "a message in this conversation is still waiting to be sent".to_owned(),
            ))
        } else if self.dm.compose.has_unanswered(&key, &text) {
            Some((
                ReplyRefusal::Blocked,
                "River has not answered for an identical message, which may still go out. \
                 Send it from the conversation if you still want it sent"
                    .to_owned(),
            ))
        } else {
            None
        };
        let (Some(to), None) = (to, &why) else {
            let (refusal, reason) = why.unwrap_or((ReplyRefusal::NoRoom, String::new()));
            let tail = self.keep_notification_reply(&key, &text);
            self.dm.compose.next_ticket += 1;
            let ticket = self.dm.compose.next_ticket;
            self.dm.compose.refused = Some((
                key,
                ticket,
                format!("Your reply was not sent because {reason}. {tail}"),
            ));
            return Err(refusal);
        };
        self.dm.compose.next_ticket += 1;
        let ticket = self.dm.compose.next_ticket;
        self.dm.compose.sends.push(Sending {
            ticket,
            room: room.to_owned(),
            peer: peer.to_owned(),
            sent: text.clone(),
            at: now,
            silent: false,
            notification: true,
        });
        self.emit(
            room.to_owned(),
            Action::SendDm {
                peer: to,
                text,
                ticket,
            },
        );
        Ok(())
    }

    /// Keep the text of a reply from a notification that was not sent, and say where it is.
    /// The open thread's box is live and is not touched: the text waits in the stash and joins
    /// the draft when the thread is left. For a thread that is not open it is the draft's own
    /// new last line, which the box shows when the thread opens.
    fn keep_notification_reply(&mut self, key: &(String, String), text: &str) -> &'static str {
        let open = self.dm.open.as_ref() == Some(key);
        let held = if open {
            self.dm.compose.stash.get(key).map(String::as_str)
        } else {
            self.dm.compose.drafts.get(key).map(|d| d.text.as_str())
        }
        .unwrap_or("");
        let joined = appended(held, text);
        // The stash is added to the live draft when the thread is left, so both count.
        let live = if open {
            self.dm
                .compose
                .drafts
                .get(key)
                .map_or(0, |d| d.text.chars().count())
        } else {
            0
        };
        if joined.chars().count() + live + usize::from(live > 0) > MAX_DRAFT_CHARS {
            return "It could not be kept: the draft is full.";
        }
        if open {
            self.dm.compose.stash.insert(key.clone(), joined);
            "It is saved, and will be in the draft when you leave this conversation."
        } else {
            self.dm
                .compose
                .drafts
                .entry(key.clone())
                .or_default()
                .replace_text(joined);
            "It is kept in this conversation's draft."
        }
    }

    /// Put what a refused notification reply left in the stash of the thread `key` into its
    /// draft, after what is there. Called when the thread is closed or another is opened.
    pub(super) fn merge_stash(&mut self, key: &(String, String)) {
        let Some(text) = self.dm.compose.stash.remove(key) else {
            return;
        };
        let draft = self.dm.compose.drafts.entry(key.clone()).or_default();
        let joined = appended(&draft.text, &text);
        if joined.chars().count() <= MAX_DRAFT_CHARS {
            draft.replace_text(joined);
        } else {
            // No room in the draft: the text stays aside, and is tried again when the thread is
            // next left. It is never dropped.
            self.dm.compose.stash.insert(key.clone(), text);
        }
    }

    /// Ask for the message of entry `entry` (a failed or unanswered send of the open thread) to
    /// be sent again, unchanged.
    pub fn dm_retry(&mut self, entry: u64) {
        if let Some((room, _)) = self.dm.open.clone() {
            self.emit_dm(room, Action::Retry { entry });
        }
    }

    /// Drop the failed message of entry `entry`.
    pub fn dm_discard(&mut self, entry: u64) {
        if let Some((room, _)) = self.dm.open.clone() {
            self.emit_dm(room, Action::Discard { entry });
        }
    }

    fn emit_dm(&mut self, room: String, action: Action) {
        if matches!(self.source, Source::Node(_)) {
            self.emit(room, action);
        }
    }

    /// A command taken from the queue of commands not yet taken by the background work, for
    /// tests.
    #[cfg(test)]
    pub(super) fn queued_commands(&self) -> &[crate::rooms::outbox::Command] {
        &self.outgoing
    }
}

// The view ------------------------------------------------------------------------------------

/// Under a message of yours the room has not taken: "Sending\u{2026}", or why it was not sent with
/// Retry and Discard. Retry sends the same signed message again.
pub(super) fn delivery(pending: &Pending, p: &Palette) -> El {
    let Some(reason) = &pending.failed else {
        return row().key("dm-sending-row").child(label(
            "Sending\u{2026}",
            st::SMALL,
            p.text_muted,
        ));
    };
    let entry = pending.entry;
    let mut choices = row().gap(12.0);
    if pending.retry {
        choices = choices.child(text_button(
            format!("dm-retry-{entry}"),
            "Retry",
            p.accent,
            move |s| s.chat.dm_retry(entry),
        ));
    }
    choices = choices.child(text_button(
        format!("dm-discard-{entry}"),
        "Discard",
        p.text_secondary,
        move |s| s.chat.dm_discard(entry),
    ));
    col()
        .key("dm-not-sent")
        .max_w(604.0)
        .gap(4.0)
        .child(
            label(
                if pending.retry {
                    format!("Not sent. {reason}")
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

/// The composer under a thread.
pub(super) fn composer(chat: &Chat, p: &Palette, k: &Kit) -> El {
    let block = chat.dm_block();
    let open = chat.dm.open.clone();
    let sending = open
        .as_ref()
        .is_some_and(|k| chat.dm.compose.is_sending_in(k));
    let silent = open
        .as_ref()
        .is_some_and(|k| chat.dm.compose.is_silent_in(k));
    let text: &str = open
        .as_ref()
        .and_then(|key| chat.dm.compose.drafts.get(key))
        .map_or("", |d| d.text.as_str());
    let over = text.len() > BODY_CAP;
    let can_type = block.is_none();
    // While a send waits for its answer the box is read-only: nothing typed then could mix with
    // the text that was sent. It is editable again when the answer comes or the wait times out.
    let editable = can_type && !sending;
    let has_text = can_type && !over && !sending && !text.trim().is_empty();
    let placeholder = match &block {
        None => "Write a direct message".to_owned(),
        Some(reason) => reason.clone(),
    };
    let empty = rune::TextEdit::new("");
    let edit = open
        .as_ref()
        .and_then(|key| chat.dm.compose.drafts.get(key))
        .unwrap_or(&empty);
    let mut input = rune::text_input(edit, st::MESSAGE.lh(24.0))
        .key("dm-composer-field")
        .w_full()
        .color(p.text)
        .placeholder(placeholder, p.text_muted)
        .caret(p.text, k.selection())
        .label("Direct message")
        .disabled(!editable);
    if editable {
        input = input
            .on_input_key(|s: &mut RiverApp, ev, _| {
                // Escape leaves the thread, as it does from anywhere else in it; the draft stays.
                let escape = ev.key == rune::Key::Escape;
                if escape {
                    s.chat.close_dm();
                }
                escape
            })
            .on_edit(|s: &mut RiverApp| s.chat.dm.compose_edit())
            .on_submit(|s: &mut RiverApp, _| {
                s.chat.dm_send();
            });
    }
    let mut field = row()
        .key("dm-composer-input")
        .role("textbox")
        .grow(1.0)
        .min_w(0.0)
        .px(8.0)
        .py(6.0)
        .child(rune::auto_grow(input, 1, MAX_LINES));
    if !editable {
        field = field.opacity(0.5);
    }
    let confirm = can_type && !sending && chat.dm_needs_confirm();
    let mut send = row()
        .key("dm-send")
        .role("button")
        .label("Send direct message")
        .help("Send")
        .size(36.0, 36.0)
        .radius(8.0)
        .center()
        .hover(p.surface_hover)
        .cursor(rune::Cursor::Pointer)
        .on_press(|s: &mut RiverApp| {
            s.chat.dm_send();
        })
        .child(icon(&icons::SEND, 20.0).color(if has_text { p.accent } else { p.text_muted }));
    if !can_type || over || sending {
        send = send.disabled(true).opacity(0.45);
    }
    if confirm {
        // Said in words, and a second, explicit press: Enter does not send it again.
        send = row()
            .key("dm-send")
            .role("button")
            .label("Send again anyway")
            .help("Send again anyway")
            .h(36.0)
            .px(12.0)
            .radius(8.0)
            .center()
            .fill(p.surface_hover)
            .cursor(rune::Cursor::Pointer)
            .on_press(|s: &mut RiverApp| {
                s.chat.dm_send_again();
            })
            .child(label("Send again anyway", st::SMALL_STRONG, p.text));
    }
    let mut column = col()
        .key("dm-composer")
        .w_full()
        .max_w(COLUMN_MAX_W)
        .self_center()
        .px(20.0)
        .pt(8.0)
        .pb(18.0)
        .gap(6.0);
    if let Some(reason) = &block {
        // Said in full, not only as the placeholder, which a long reason would not fit.
        column = column.child(
            label(reason.clone(), st::SMALL, p.text_secondary)
                .key("dm-block")
                .wrap()
                .w_full()
                .px(4.0),
        );
    }
    if let (Some(open), Some((key, _, why))) = (&open, &chat.dm.compose.refused)
        && open == key
    {
        column = column.child(
            label(why.clone(), st::SMALL, p.danger)
                .key("dm-error")
                .wrap()
                .w_full()
                .px(4.0),
        );
    }
    column = column.child(
        row()
            .key("dm-composer-box")
            .w_full()
            .min_h(50.0)
            .px(6.0)
            .gap(4.0)
            .items_end()
            .py(7.0)
            .radius(12.0)
            .fill(p.surface)
            .border(1.0, if edit.focused { p.text_muted } else { p.line })
            .shadow(8.0, 24.0, p.shadow)
            .child(field)
            .child(send),
    );
    let bytes = text.len();
    if can_type && bytes > BODY_CAP / 5 * 4 {
        column = column.child(if over {
            label(dm_send::too_long(bytes), st::SMALL, p.danger)
                .key("dm-byte-counter")
                .self_end()
                .pr(4.0)
        } else {
            label(format!("{bytes}/{BODY_CAP}"), st::SMALL, p.text_muted)
                .key("dm-byte-counter")
                .self_end()
                .pr(4.0)
        });
    }
    if silent {
        column = column.child(
            label(NO_ANSWER, st::SMALL, p.danger)
                .key("dm-no-answer")
                .wrap()
                .w_full()
                .px(4.0),
        );
    }
    if sending {
        column = column.child(
            label("Sending\u{2026}", st::SMALL, p.text_muted)
                .key("dm-sending")
                .self_end()
                .pr(4.0),
        );
    }
    column
}
