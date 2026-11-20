//! Direct messages on screen: the rail's "Direct messages" section and the thread view in the
//! main pane. Read only: nothing here sends, deletes or saves anything.
//!
//! The threads come from the background work as a [`DmSnapshot`] (built by `rooms::dm`, which
//! verifies and opens the messages). This module never sees a key or a ciphertext.
//!
//! What it takes care over:
//!
//! - **Privacy.** Nothing here logs, and no title, notification or status text is made from a
//!   thread. Everything a peer controls (nickname, an invitation's note and room label) is drawn
//!   as plain text. A message's text is drawn as the room's markdown, as the web UI does. The
//!   rail's preview is a bounded plain line that holds no invitation payload (the view model
//!   has none).
//! - **Honest state.** The user's own sent messages have their text only once the sent-text
//!   cache has been read; until then they say so, rather than saying something about the
//!   message. A thread that hit this app's bounds says older messages may be missing.
//! - **Read state**, as the web UI keeps it (`DM_LAST_SEEN`) but by message and not by time: in
//!   memory only, per room and peer, seeded per room when the room is first met (so a restart
//!   does not make every old message unread), and advanced only while that thread is open, the
//!   window is in front and the view is at its newest message. A message's time is whatever its
//!   sender signed, so no time decides what is unread, and every time shown is clamped once, in
//!   `rooms::dm` (`MAX_SHOWN_SECS`).
//! - **Retention.** The decrypted threads of every room are held once (shared by `Arc` between
//!   the background work and this screen), and only the rail's bounded previews are copied. A
//!   thread's newest message decides its rail position by its signed time, so a member who signs
//!   a far-future time pins their thread to the top of the rail (shown as 2100 at most); and the
//!   bounds in `rooms::dm` keep the newest messages by that same time, so a hostile member can
//!   crowd other threads out of a room's list. Both are accepted: they need the contract to
//!   bound a signed time.
//!   Sending full messages only for the open thread would hold less, and needs a request from
//!   the window to the background work: a follow-up.

use std::collections::{HashMap, HashSet};

use rune::tok::TextStyle;
use rune::{El, ScrollSpec, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Kit};

use super::Chat;
use super::conversation::{day_divider, message_body};
use super::dm_model::{DmBody, DmMessage, DmSnapshot, DmThread, Placeholder};
use super::icons;
use super::model::{self, MINUTES_PER_DAY, Message};
use super::style::{self as st, Palette};
use super::view::{HEADER_H, hline, label};
use crate::RiverApp;
use crate::rooms::room::local_minutes_of_secs;

/// The most rows the rail lists. A user with more threads than this sees the newest and unread
/// ones, and is told how many are not shown.
const RAIL_MAX: usize = 50;
/// The most characters of a message the rail previews.
const PREVIEW_CHARS: usize = 100;
/// A message body's widest line, as the room's conversation sets it.
const BODY_MAX_W: f32 = 604.0;
const AVATAR: f32 = 36.0;
const GUTTER: f32 = 60.0;
const COLUMN_MAX_W: f32 = GUTTER + BODY_MAX_W + 24.0;

/// The words under the thread when there is nothing to type into. PR 3 replaces this with a
/// composer.
pub const NO_SEND: &str = "Sending is not available yet in this build.";
/// What a thread says when this app's bounds left older messages out.
pub const TRUNCATED: &str = "Older messages may be missing. River shows only the newest \
     messages of a conversation.";
/// What a thread says while the user's own sent messages have no text to show yet.
pub const LOADING_SENT: &str = "Your sent messages are still loading from River's storage, so \
     their text is not shown yet.";

/// One rail row: a thread, with what the rail shows of it.
struct RailRow {
    room: String,
    peer_uid: String,
    peer_name: String,
    room_name: String,
    preview: String,
    unread: usize,
    last_ts: u64,
}

/// A thread's rail preview, kept while the thread is the very value it was made from (the
/// background work hands the same `Arc` back for a room that did not change, and a new one when
/// its messages, a name, an invitation's room label or the sent-text cache did), so a change
/// elsewhere does not strip every thread's markdown again.
struct Preview {
    thread: std::sync::Arc<DmThread>,
    hydrated: bool,
    text: String,
}

/// The direct-message state of the chat screen.
///
/// **For the composer (PR 3).** Opening a thread covers the room's own screen but leaves its
/// state in place: the composer, `mode` (a reply or an edit in progress) and the room's draft
/// belong to the room. `open_dm` clears what would show over the thread, and must stash and
/// restore `mode` and the draft when the DM composer takes the keyboard, or a reply started in
/// the room would be sent as a direct message.
#[derive(Default)]
pub struct DmUi {
    snapshot: DmSnapshot,
    /// What the rail lists, in order. Rebuilt when the snapshot or the read state changes, never
    /// per frame.
    rows: Vec<RailRow>,
    previews: HashMap<(String, String), Preview>,
    /// Per room (key) and peer (`peer_uid`): the messages (by purge token) the user has seen. Never
    /// shrinks (see [`DmUi::apply`]).
    /// By message and not by time, because a time is whatever its sender signed: a forged
    /// future time must not hide later messages, and an old one must still count. In memory only.
    pub(super) seen: HashMap<(String, String), HashSet<String>>,
    /// Rooms already met: their messages at that moment were seeded as seen. Never shrinks.
    known_rooms: HashSet<String>,
    /// The open thread: its room's key and the peer's uid.
    pub(super) open: Option<(String, String)>,
    /// Whether the open thread shows its newest message. Reported by its scroll area.
    pub at_bottom: bool,
    /// The composer had the keyboard when the thread opened: Back gives it back.
    restore_focus: bool,
    /// How many times the rail rows were rebuilt, for tests.
    rebuilds: usize,
}

impl std::fmt::Debug for DmUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmUi")
            .field("snapshot", &self.snapshot)
            .field("open", &self.open.is_some())
            .finish_non_exhaustive()
    }
}

impl DmUi {
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// The open thread's room and peer, as `(room key, peer uid)`.
    pub fn open_key(&self) -> Option<(&str, &str)> {
        self.open.as_ref().map(|(r, p)| (r.as_str(), p.as_str()))
    }

    pub fn snapshot(&self) -> &DmSnapshot {
        &self.snapshot
    }

    fn thread(&self, room: &str, uid: &str) -> Option<&DmThread> {
        self.snapshot
            .threads
            .iter()
            .find(|t| t.room == room && t.peer_uid == uid)
            .map(|t| &**t)
    }

    /// How many messages of `t` the user has not seen: those the peer sent that are not in the
    /// seen set.
    pub fn unread_of(&self, t: &DmThread) -> usize {
        // A room whose state has not loaded has no read position yet: not news.
        if !self.known_rooms.contains(&t.room) {
            return 0;
        }
        let seen = self.seen.get(&(t.room.clone(), t.peer_uid.clone()));
        t.messages
            .iter()
            .filter(|m| !m.outgoing && !seen.is_some_and(|s| s.contains(&m.key)))
            .count()
    }

    /// Unread messages across all threads: what the rail's badges add up to.
    pub fn unread_total(&self) -> usize {
        self.snapshot
            .threads
            .iter()
            .map(|t| self.unread_of(t))
            .sum()
    }

    /// Take a new snapshot.
    ///
    /// **Nothing is forgotten because a list was empty, partial or missing a thread**, and what
    /// is seen is bounded: for each thread that is present and not empty, the seen set is cut to
    /// the messages it still has, so it never holds more than the thread does (a thread's
    /// bounds are `rooms::dm`'s; a token is about 130 bytes). The sets of threads that are
    /// absent stay as they were.
    ///
    /// A room is met on the first snapshot that lists it in `room_keys`, which holds only rooms
    /// whose state has been loaded from the network (a room just joined or imported is listed
    /// before that, with a stub). Whatever messages it has at that moment, possibly none, are
    /// seeded as seen, as the web UI seeds its read positions once at start (so a restart does
    /// not make every old message unread), but per room. After that nothing is seeded, so every
    /// new message is unread, including the first one in a room that had none. Until a room is
    /// met its threads count as read.
    ///
    /// Consequences, accepted for now: a room left and joined again keeps its read positions;
    /// and messages that arrived while the app was closed are seeded as seen (no unread, and no
    /// notification; notifications come with PR 8).
    pub fn apply(&mut self, snapshot: DmSnapshot) {
        for room in &snapshot.room_keys {
            if !self.known_rooms.insert(room.clone()) {
                continue;
            }
            for t in snapshot.threads.iter().filter(|t| &t.room == room) {
                self.seen
                    .entry((t.room.clone(), t.peer_uid.clone()))
                    .or_default()
                    .extend(
                        t.messages
                            .iter()
                            .filter(|m| !m.outgoing)
                            .map(|m| m.key.clone()),
                    );
            }
        }
        // Cut to what is still there, for the threads that are (and only those).
        for t in snapshot
            .threads
            .iter()
            .filter(|t| !t.messages.is_empty() && self.known_rooms.contains(&t.room))
        {
            if let Some(seen) = self.seen.get_mut(&(t.room.clone(), t.peer_uid.clone())) {
                let present: HashSet<&str> = t.messages.iter().map(|m| m.key.as_str()).collect();
                seen.retain(|k| present.contains(k.as_str()));
            }
        }
        self.snapshot = snapshot;
        self.rebuild();
    }

    /// Record that the open thread's inbound messages have been seen, when they have: the
    /// window is in front and the thread is at its newest message. True when that moved the read
    /// position.
    pub fn mark_read(&mut self, foreground: bool) -> bool {
        if !foreground || !self.at_bottom {
            return false;
        }
        let Some((room, uid)) = self.open.clone() else {
            return false;
        };
        let Some(keys) = self.thread(&room, &uid).map(|t| {
            t.messages
                .iter()
                .filter(|m| !m.outgoing)
                .map(|m| m.key.clone())
                .collect::<Vec<_>>()
        }) else {
            return false;
        };
        let seen = self.seen.entry((room, uid)).or_default();
        let before = seen.len();
        seen.extend(keys);
        if seen.len() == before {
            return false;
        }
        self.rebuild();
        true
    }

    fn rebuild(&mut self) {
        self.rebuilds += 1;
        let hydrated = self.snapshot.hydrated;
        let mut previews = std::mem::take(&mut self.previews);
        let mut kept = HashMap::new();
        let mut rows = Vec::with_capacity(self.snapshot.threads.len());
        for t in &self.snapshot.threads {
            let id = (t.room.clone(), t.peer_uid.clone());
            let p = match previews.remove(&id) {
                Some(p) if std::sync::Arc::ptr_eq(&p.thread, t) && p.hydrated == hydrated => p,
                _ => Preview {
                    text: preview(t, hydrated),
                    thread: t.clone(),
                    hydrated,
                },
            };
            rows.push(RailRow {
                room: t.room.clone(),
                peer_uid: t.peer_uid.clone(),
                peer_name: name(&t.peer_name),
                room_name: name(&t.room_name),
                preview: p.text.clone(),
                unread: self.unread_of(t),
                last_ts: t.last_ts,
            });
            kept.insert(id, p);
        }
        self.previews = kept;
        rows.sort_by(|a, b| {
            (b.unread > 0)
                .cmp(&(a.unread > 0))
                .then_with(|| b.last_ts.cmp(&a.last_ts))
                .then_with(|| a.room.cmp(&b.room))
                .then_with(|| a.peer_uid.cmp(&b.peer_uid))
        });
        self.rows = rows;
    }

    /// The rail's rows, newest and unread first: `(room key, peer uid)` of each.
    pub fn rail_order(&self) -> Vec<(&str, &str)> {
        self.rows
            .iter()
            .map(|r| (r.room.as_str(), r.peer_uid.as_str()))
            .collect()
    }

    /// The first rail row's preview, for tests.
    #[cfg(test)]
    pub(super) fn first_preview(&self) -> &str {
        &self.rows[0].preview
    }

    pub fn rebuilds(&self) -> usize {
        self.rebuilds
    }
}

/// A name a peer or a room chose, as one line with nothing hidden or reordering in it.
fn name(s: &str) -> String {
    crate::rooms::room::clean(s)
}

/// The rail's one-line preview of a thread's newest message: plain text, bounded, with no
/// markup and no invitation payload.
fn preview(t: &DmThread, hydrated: bool) -> String {
    let Some(m) = t.messages.last() else {
        return String::new();
    };
    let you = |s: String| if m.outgoing { format!("You: {s}") } else { s };
    let line = match &m.body {
        DmBody::Text(text) => you(model::preview(text, PREVIEW_CHARS)),
        DmBody::Invite { room_label, .. } => {
            format!("Invitation to {}", bounded(room_label, PREVIEW_CHARS))
        }
        DmBody::SentInvite { .. } => "You sent an invitation".to_owned(),
        DmBody::Placeholder(p) => {
            if m.outgoing {
                format!("You: {}", placeholder_short(*p, hydrated))
            } else {
                "A message that could not be shown".to_owned()
            }
        }
    };
    // Whatever the text held, the rail shows one printable line.
    crate::rooms::room::clean(&line)
}

fn bounded(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

fn placeholder_short(p: Placeholder, hydrated: bool) -> &'static str {
    match p {
        Placeholder::CiphertextOnly if !hydrated => "sent, text not available yet",
        Placeholder::CiphertextOnly => "sent, text not available",
        _ => "a message that could not be shown",
    }
}

/// The words of a row that cannot show its body. A message the user sent has no text until the
/// sent-text cache has been read, and that is said as a state of the cache, not of the message.
pub fn placeholder_text(p: Placeholder, hydrated: bool) -> &'static str {
    match p {
        Placeholder::CiphertextOnly if !hydrated => {
            "Sent. The text is not available yet: River is still reading your sent messages."
        }
        other => other.text(),
    }
}

// What the window does with it ---------------------------------------------------------------

impl Chat {
    /// Take the direct-message threads of every room.
    pub fn apply_dms(&mut self, snapshot: DmSnapshot) {
        self.dm.apply(snapshot);
        self.dm_settle();
    }

    /// Close the open thread when there is nothing left to show it as: its room is gone, or its
    /// peer has no messages and is not a member of the room.
    pub(super) fn dm_settle(&mut self) {
        let Some((room, uid)) = self.dm.open.clone() else {
            return;
        };
        let known = self.rooms().iter().any(|r| {
            r.key == room
                && (self.dm.thread(&room, &uid).is_some()
                    || r.member_by_uid(&uid).is_some_and(|m| m.id != r.me))
        });
        if !known {
            self.dm.open = None;
        }
    }

    /// Open the conversation with the member `uid` of the room keyed `room`, in the main pane.
    /// A member with no messages yet opens an empty thread. False when there is no such
    /// room or member.
    ///
    /// This does not select the room: the room that was open stays the selected one, so Back
    /// and Ctrl+F act on it. Whether the composer had the keyboard (directly, or through a
    /// search that would give it back) is kept, and [`Chat::close_dm`] gives it back.
    pub fn open_dm(&mut self, room: &str, uid: &str) -> bool {
        let known = self.rooms().iter().any(|r| {
            r.key == room
                && (self.dm.thread(room, uid).is_some()
                    || r.member_by_uid(uid).is_some_and(|m| m.id != r.me))
        });
        if !known {
            return false;
        }
        // What belonged to the room's own screen gives way: it is no longer in front.
        self.member_modal = None;
        self.clear_ban_confirm();
        // Both searches close and hand the keyboard to no one: the thread covers the room, its
        // composer and its members panel, so neither box may keep a focus flag nor give the
        // keyboard to a composer nobody can see.
        let from_search = self.search.as_ref().is_some_and(|s| s.restore_composer);
        self.drop_search();
        let from_members = self.dismiss_member_search();
        let had = self.composer.focused || from_search || from_members;
        // Switching from one thread to another keeps what the first one kept.
        self.dm.restore_focus = had || (self.dm.is_open() && self.dm.restore_focus);
        self.emoji_open = false;
        self.picker = None;
        self.members_overlay = false;
        self.link_menu = None;
        self.notify_dialog = None;
        self.details = None;
        self.deputy_confirm = None;
        self.mention_off = None;
        self.composer.focused = false;
        self.dm.open = Some((room.to_owned(), uid.to_owned()));
        self.dm.at_bottom = true;
        true
    }

    /// Back to the room that was open.
    pub fn close_dm(&mut self) {
        if self.dm.open.take().is_none() {
            return;
        }
        // The room is in front again: what arrived in it meanwhile is being read now, and the
        // composer gets the keyboard back if it had it, unless a dialog or the switcher has it
        // now. (A thread that another room or a dialog closes gives nothing back: they took
        // the keyboard on purpose.)
        let restore = std::mem::take(&mut self.dm.restore_focus);
        if let Some(key) = self.room().map(|r| r.key.clone()) {
            self.mark_read(&key);
        }
        if restore
            && self.actionable().is_some()
            && !self.modal_dialog_open()
            && self.switcher.is_none()
        {
            self.focus_composer();
        }
    }

    /// Mark the open thread's messages seen when they are on screen. `foreground`: the window is
    /// in front. Run every frame.
    pub fn dm_sync(&mut self, foreground: bool) {
        self.dm.mark_read(foreground);
    }

    /// The room whose conversation is on screen: none while a direct-message thread covers it.
    pub fn visible_room(&self) -> Option<&super::Room> {
        if self.dm.is_open() { None } else { self.room() }
    }
}

// The rail ------------------------------------------------------------------------------------

/// The rail's "Direct messages" section: nothing when there are no threads.
pub(super) fn rail(chat: &Chat, p: &Palette) -> Vec<El> {
    let dm = &chat.dm;
    if dm.rows.is_empty() {
        return Vec::new();
    }
    let mut out = vec![
        row()
            .key("dm-heading")
            .pl(8.0)
            .pr(8.0)
            .pt(14.0)
            .pb(4.0)
            .center_y()
            .child(label("Direct messages", st::SECTION, p.text_muted))
            .child(rune::spacer()),
    ];
    for r in dm.rows.iter().take(RAIL_MAX) {
        let selected = dm.open_key() == Some((r.room.as_str(), r.peer_uid.as_str()));
        out.push(rail_row(r, selected, p));
    }
    if dm.rows.len() > RAIL_MAX {
        out.push(
            label(
                format!("and {} more", dm.rows.len() - RAIL_MAX),
                st::SMALL,
                p.text_muted,
            )
            .key("dm-more")
            .px(10.0)
            .py(4.0),
        );
    }
    out
}

fn rail_row(r: &RailRow, selected: bool, p: &Palette) -> El {
    // The badge is the count `DmUi::unread_total` adds up, selected or not.
    let unread = r.unread > 0;
    let strong = unread || selected;
    let (room, uid) = (r.room.clone(), r.peer_uid.clone());
    let name = row()
        .gap(6.0)
        .center_y()
        .min_w(0.0)
        .child(
            label(
                r.peer_name.clone(),
                if strong { st::ROOM_STRONG } else { st::ROOM },
                if strong { p.text } else { p.text_secondary },
            )
            .truncate()
            .shrink(),
        )
        .child(
            label(r.room_name.clone(), st::SMALL, p.text_muted)
                .truncate()
                .shrink(),
        );
    let mut out = row()
        .key(format!("dm-{}-{}", r.room, r.peer_uid))
        .role("button")
        .label(format!(
            "Direct messages with {} in {}",
            r.peer_name, r.room_name
        ))
        .w_full()
        .h(50.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .radius(8.0)
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut RiverApp| {
            s.chat.open_dm(&room, &uid);
        })
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(3.0)
                .child(name)
                .child(label(r.preview.clone(), st::SMALL, p.text_muted).truncate()),
        );
    if unread {
        out = out.child(
            el().key("dm-unread")
                .h(18.0)
                .min_w(20.0)
                .px(6.0)
                .radius(9.0)
                .center()
                .fill(p.accent_soft)
                .child(label(r.unread.to_string(), st::BADGE, p.accent)),
        );
    }
    if selected {
        out.fill(p.surface)
    } else {
        out.hover(p.row_hover)
    }
}

// The thread ----------------------------------------------------------------------------------

/// What the thread view shows: an existing thread, or the empty one with a member who has no
/// messages yet.
struct Shown<'a> {
    room: &'a str,
    uid: &'a str,
    peer_name: String,
    room_name: String,
    messages: &'a [DmMessage],
}

/// The open thread's pane, in place of the room's conversation.
pub(super) fn thread_pane(chat: &Chat, narrow: bool, p: &Palette, k: &Kit) -> El {
    let Some((room_key, uid)) = chat.dm.open_key() else {
        return el().key("dm-thread");
    };
    let thread = chat.dm.thread(room_key, uid);
    let room = chat.rooms().iter().find(|r| r.key == room_key);
    let shown = Shown {
        room: room_key,
        uid,
        peer_name: name(&thread.map_or_else(
            || {
                room.and_then(|r| r.member_by_uid(uid))
                    .map_or_else(String::new, |m| m.nickname.clone())
            },
            |t| t.peer_name.clone(),
        )),
        room_name: name(&thread.map_or_else(
            || room.map_or_else(String::new, |r| r.name.clone()),
            |t| t.room_name.clone(),
        )),
        messages: thread.map_or(&[][..], |t| t.messages.as_slice()),
    };
    let hydrated = chat.dm.snapshot.hydrated;
    let mut c = col()
        .key("dm-thread")
        // Escape goes back to the room, wherever focus is in the thread. Dialogs and menus live
        // in the overlay layer and take their own Escape first.
        .on_key(|s: &mut RiverApp, ev, _| {
            ev.key == rune::Key::Escape && s.chat.dm.is_open() && {
                s.chat.close_dm();
                true
            }
        })
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .child(header(&shown, narrow, p, k))
        .child(hline(p));
    // Said once, above the messages, whatever the pane's width.
    let mut notes = col().key("dm-notes").w_full().px(20.0).gap(4.0);
    let mut any = false;
    if chat.dm.snapshot.is_truncated(room_key) {
        notes = notes.child(
            label(TRUNCATED, st::SMALL, p.text_muted)
                .key("dm-truncated")
                .wrap(),
        );
        any = true;
    }
    if !hydrated && shown.messages.iter().any(|m| m.outgoing) {
        notes = notes.child(
            label(LOADING_SENT, st::SMALL, p.text_muted)
                .key("dm-loading-sent")
                .wrap(),
        );
        any = true;
    }
    if any {
        c = c.child(notes.pt(8.0));
    }
    c.child(history(chat, &shown, hydrated, p, k))
        .child(hline(p))
        .child(
            row().key("dm-footer").w_full().px(20.0).py(14.0).child(
                label(NO_SEND, st::BODY, p.text_muted)
                    .key("dm-no-send")
                    .wrap(),
            ),
        )
}

fn header(shown: &Shown<'_>, narrow: bool, p: &Palette, k: &Kit) -> El {
    let back = Button::new("Back")
        .variant(ButtonVariant::Ghost)
        .on_press(|s: &mut RiverApp| s.chat.close_dm())
        .build(k)
        .key("dm-back");
    row()
        .key("dm-header")
        .w_full()
        .min_h(HEADER_H)
        .pl(if narrow { 8.0 } else { 12.0 })
        .pr(12.0)
        .gap(if narrow { 8.0 } else { 12.0 })
        .center_y()
        .child(back)
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(2.0)
                .child(
                    label(shown.peer_name.clone(), st::HEADER, p.text)
                        .key("dm-title")
                        .truncate()
                        .shrink(),
                )
                .child(
                    label(format!("in {}", shown.room_name), st::SMALL, p.text_muted)
                        .key("dm-subtitle")
                        .truncate()
                        .shrink(),
                ),
        )
}

/// A message time (unix seconds) as the conversation's clock counts it: local minutes since the
/// start of its day 0.
fn minutes(chat: &Chat, ts: u64) -> u32 {
    let offset = u32::try_from(chat.day_zero.max(0))
        .unwrap_or(u32::MAX)
        .saturating_mul(MINUTES_PER_DAY);
    local_minutes_of_secs(ts).saturating_sub(offset)
}

/// The messages as the conversation's grouping reads them: author 0 is the user, 1 the peer.
/// The text is set only for a body drawn as markdown.
fn as_messages(chat: &Chat, messages: &[DmMessage]) -> Vec<Message> {
    messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let text = match &m.body {
                DmBody::Text(t) => t.clone(),
                _ => String::new(),
            };
            Message {
                id: i as u32,
                key: m.key.clone(),
                author: u32::from(!m.outgoing),
                time: minutes(chat, m.timestamp),
                source: text.clone(),
                md: text.clone(),
                text,
                reply_to: None,
                reactions: Vec::new(),
                edited: false,
                pending: None,
            }
        })
        .collect()
}

fn history(chat: &Chat, shown: &Shown<'_>, hydrated: bool, p: &Palette, k: &Kit) -> El {
    if shown.messages.is_empty() {
        return col()
            .key("dm-empty")
            .grow(1.0)
            .min_h(0.0)
            .w_full()
            .center()
            .gap(10.0)
            .child(
                el().size(56.0, 56.0)
                    .radius(16.0)
                    .center()
                    .fill(p.surface)
                    .border(1.0, p.line)
                    .child(icon(&icons::CHAT, 26.0).color(p.text_muted)),
            )
            .child(label("No messages yet", st::TITLE, p.text).mt(6.0))
            .child(
                label(
                    format!(
                        "Messages you exchange with {} in {} appear here.",
                        shown.peer_name, shown.room_name
                    ),
                    st::BODY,
                    p.text_secondary,
                )
                .wrap()
                .max_w(420.0),
            );
    }
    let msgs = as_messages(chat, shown.messages);
    let mut items = col()
        .key("dm-history")
        .w_full()
        .max_w(COLUMN_MAX_W)
        .self_center()
        .pt(16.0)
        .pb(26.0)
        .gap(14.0);
    let mut day = None;
    let mut at = 0;
    for group in model::groups(&msgs) {
        let d = group.messages[0].time / MINUTES_PER_DAY;
        if day != Some(d) {
            day = Some(d);
            let text = model::day_label(
                chat.day_zero + i64::from(d),
                chat.day_zero + i64::from(chat.today),
            );
            items = items.child(day_divider(d, text, p));
        }
        let n = group.messages.len();
        items = items.child(group_el(
            chat,
            shown,
            &group,
            &shown.messages[at..at + n],
            hydrated,
            p,
            k,
        ));
        at += n;
    }
    let scroll_key = format!("dm-scroll-{}-{}", shown.room, shown.uid);
    let mut frame = col().key("dm-frame").grow(1.0).min_h(0.0).w_full().child(
        col()
            .key(scroll_key.clone())
            .grow(1.0)
            .min_h(0.0)
            .w_full()
            .child(items.selection_group(true))
            .scroll(ScrollSpec { stick_bottom: true })
            .scroll_edge_fade(40.0)
            .on_at_bottom(|s: &mut RiverApp, at| s.chat.dm.at_bottom = at),
    );
    if !chat.dm.at_bottom {
        frame = frame.child(
            row()
                .key("dm-latest")
                .role("button")
                .label("Scroll to latest message")
                .help("Latest")
                .size(40.0, 40.0)
                .radius(20.0)
                .center()
                .fill(p.surface)
                .border(1.0, p.line)
                .shadow(8.0, 24.0, p.shadow)
                .hover(p.surface_hover)
                .cursor(rune::Cursor::Pointer)
                .pin(None, None, Some(16.0), Some(16.0))
                .on_press_tx(move |_: &mut RiverApp, tx: &mut rune::Tx| {
                    tx.scroll_to_bottom(scroll_key.clone())
                })
                .child(icon(&icons::CHEVRON_DOWN, 18.0).color(p.text_muted)),
        );
    }
    frame
}

/// A face: the initials of `name` on a colour taken from `seed`.
fn face(name: &str, seed: &str, ring: Option<rune::Color>) -> El {
    let id = seed
        .bytes()
        .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    let (bg, fg) = st::avatar_colors(id);
    let disc = el()
        .size(AVATAR, AVATAR)
        .radius(AVATAR / 2.0)
        .center()
        .fill(bg)
        .child(
            text(model::initials(name))
                .font(TextStyle::new((AVATAR * 0.38).round(), 700.0))
                .color(fg),
        );
    match ring {
        Some(c) => {
            let outer = AVATAR + 2.0 * 3.5;
            el().size(outer, outer)
                .radius(outer / 2.0)
                .center()
                .border(1.5, c)
                .child(disc)
        }
        None => disc,
    }
}

/// One author's run of messages, as the room's conversation draws it: the first row has the
/// face and the name line, the others their time in the gutter while the pointer is over them.
fn group_el(
    chat: &Chat,
    shown: &Shown<'_>,
    group: &model::Group<'_>,
    dms: &[DmMessage],
    hydrated: bool,
    p: &Palette,
    k: &Kit,
) -> El {
    let first = &group.messages[0];
    let mine = dms[0].outgoing;
    let name = if mine {
        "You"
    } else {
        shown.peer_name.as_str()
    };
    let seed = if mine { "you" } else { shown.uid };
    let face = face(name, seed, mine.then_some(p.accent));
    let name_row = row()
        .gap(8.0)
        .center_y()
        .child(label(
            name.to_owned(),
            st::NAME,
            if mine { p.accent } else { p.text },
        ))
        .child(
            label(model::clock(first.time), st::SMALL, p.text_muted)
                .key(format!("dm-time-{}", first.key))
                .help(model::full_time(chat.day_zero, first.time)),
        );
    let (mut face, mut name_row) = (Some(face), Some(name_row));
    let mut rows = col()
        .key(format!("dm-group-{}", first.key))
        .w_full()
        .radius(8.0);
    if mine {
        rows = rows.fill(p.own_row);
    }
    for (i, (m, dm)) in group.messages.iter().zip(dms).enumerate() {
        let body = body_el(m, dm, hydrated, p, k);
        let (gutter, content) = if i == 0 {
            (
                row()
                    .w(GUTTER)
                    .justify_center()
                    .pt(2.0)
                    .child(face.take().unwrap_or_else(el)),
                col()
                    .grow(1.0)
                    .min_w(0.0)
                    .gap(4.0)
                    .child(name_row.take().unwrap_or_else(row))
                    .child(body),
            )
        } else {
            (
                row().w(GUTTER).justify_center().pt(4.0).child(
                    label(model::clock(m.time), st::GUTTER_TIME, p.text_muted)
                        .key(format!("dm-time-{}", m.key))
                        .help(model::full_time(chat.day_zero, m.time))
                        .group_opacity(0.0, 1.0),
                ),
                col().grow(1.0).min_w(0.0).child(body),
            )
        };
        rows = rows.child(
            row()
                .key(format!("dm-row-{}", m.key))
                .w_full()
                .pt(if i == 0 { 6.0 } else { 3.0 })
                .pb(3.0)
                .pr(24.0)
                .radius(8.0)
                .items_start()
                .hover_group()
                .hover(if mine { p.own_row_hover } else { p.row_hover })
                .child(gutter)
                .child(content),
        );
    }
    rows
}

/// Plain text a peer or the app wrote: no markdown, no links.
fn plain(s: impl Into<String>, color: rune::Color) -> El {
    rune::rich(s.into(), Vec::new())
        .font(st::MESSAGE)
        .color(color)
        .w_full()
        .max_w(BODY_MAX_W)
        .selectable(true)
}

fn body_el(m: &Message, dm: &DmMessage, hydrated: bool, p: &Palette, k: &Kit) -> El {
    let body = match &dm.body {
        // As the room does: the text's markdown.
        DmBody::Text(_) => message_body(m, p, k),
        DmBody::Invite { room_label, note } => invite_card(
            &m.key,
            "Invitation",
            Some(room_label.as_str()),
            note.as_deref(),
            Some("Accepting an invitation is not available yet in this build."),
            p,
        ),
        DmBody::SentInvite { note } => invite_card(
            &m.key,
            "You sent an invitation",
            None,
            note.as_deref(),
            None,
            p,
        ),
        DmBody::Placeholder(ph) => plain(placeholder_text(*ph, hydrated), p.text_muted),
    };
    col().key(format!("dm-msg-{}", m.key)).w_full().child(body)
}

/// A read-only invitation card. `room` and `note` are a peer's words: plain text.
fn invite_card(
    key: &str,
    title: &str,
    room: Option<&str>,
    note: Option<&str>,
    footer: Option<&str>,
    p: &Palette,
) -> El {
    let mut c = col()
        .key(format!("dm-invite-{key}"))
        .w_full()
        .max_w(420.0)
        .px(12.0)
        .py(10.0)
        .gap(6.0)
        .radius(10.0)
        .fill(p.surface)
        .border(1.0, p.line_strong)
        .child(
            row()
                .gap(8.0)
                .center_y()
                .child(icon(&icons::INVITE, 15.0).color(p.text_secondary))
                .child(label(title.to_owned(), st::NAME, p.text)),
        );
    if let Some(room) = room {
        c = c.child(
            row()
                .gap(6.0)
                .child(label("Room:", st::SMALL, p.text_muted))
                .child(
                    label(room.to_owned(), st::SMALL_STRONG, p.text)
                        .key(format!("dm-invite-room-{key}"))
                        .wrap()
                        .shrink(),
                ),
        );
    }
    if let Some(note) = note {
        c = c.child(
            col()
                .key(format!("dm-invite-note-{key}"))
                .w_full()
                .child(plain(note.to_owned(), p.text)),
        );
    }
    if let Some(footer) = footer {
        c = c.child(label(footer.to_owned(), st::SMALL, p.text_muted).wrap());
    }
    c
}
