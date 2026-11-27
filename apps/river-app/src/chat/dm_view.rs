//! Direct messages on screen: the rail's "Direct messages" section and the thread view in the
//! main pane. Sending is in [`super::dm_compose`] and deleting the peer's messages in
//! [`super::dm_purge`]; nothing here saves anything.
//!
//! The threads come from the background work as a [`DmSnapshot`] (built by `rooms::dm`, which
//! verifies and opens the messages). This module never sees a key or a ciphertext.
//!
//! What it takes care over:
//!
//! - **Privacy.** Nothing here logs, and no title or status text is made from a thread. A
//!   notification for a new message is built by `notify::dm_arrival`, which leaves out the sender
//!   and the text unless the previews setting is on. Everything a peer controls (nickname, an invitation's note and room label) is drawn
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
use rune_kit::{Button, ButtonVariant, Kit, ScrollBar, ScrollBarStyle};

use super::Chat;
use super::conversation::{day_divider, message_body};
use super::dm_model::{DmArrival, DmBody, DmMessage, DmSnapshot, DmThread, Placeholder};
use super::icons;
use super::model::{self, MINUTES_PER_DAY, Message};
use super::style::{self as st, Palette};
use super::view::{HEADER_H, hline, label};
use crate::RiverApp;
use crate::rooms::modes::NotificationMode;
use crate::rooms::room::local_minutes_of_secs;

/// The most deletions whose messages are waited for to leave the read positions.
const MAX_DELETING: usize = 20;

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

/// What a thread says when this app's bounds left older messages out.
pub const TRUNCATED: &str = "Older messages may be missing. River shows only the newest \
     messages of a conversation.";
/// What a thread says while the user's own sent messages have no text to show yet.
pub const LOADING_SENT: &str = "Your sent messages are still loading from River's storage, so \
     their text is not shown yet.";

/// One rail row: a thread, with what the rail shows of it.
struct RailRow {
    room: String,
    peer_key: String,
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
/// Opening a thread covers the room's own screen but leaves its state in place: the composer,
/// `mode` (a reply or an edit in progress) and the room's draft belong to the room and are not
/// touched. The thread has a composer of its own ([`DmUi::compose`]), so a reply started in the
/// room cannot be sent as a direct message and is still there when the thread closes.
#[derive(Default)]
pub struct DmUi {
    /// The thread's own composer: text, drafts and send state.
    pub(super) compose: super::dm_compose::DmCompose,
    snapshot: DmSnapshot,
    /// What the rail lists, in order. Rebuilt when the snapshot or the read state changes, never
    /// per frame.
    rows: Vec<RailRow>,
    previews: HashMap<(String, String), Preview>,
    /// Per room (key) and peer (`peer_key`): the messages (by purge token) the user has seen. Never
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
    /// The box had the keyboard when a send made it read-only: it takes it back when the answer
    /// comes.
    refocus_after_send: bool,
    /// Deleting the peer's messages ([`super::dm_purge`]).
    pub(super) purge: super::dm_purge::DmPurgeUi,
    /// Invitations: Accept on a card and the "Share an invite" dialog ([`super::dm_invite`]).
    pub(super) invite: super::dm_invite::DmInviteUi,
    /// Archiving threads: the fold-out, the Undo line, and what shows before a snapshot holds it
    /// ([`super::dm_archive`]).
    pub(super) archive: super::dm_archive::DmArchiveUi,
    /// Deletions the background work took, whose messages the room still held at the last
    /// snapshot: `(room key, peer, message keys)`. The read position forgets a key once the
    /// thread no longer has it ([`DmUi::forget_deleted`]), so the badge does not count messages
    /// that are on their way out as unread. Bounded by [`MAX_DELETING`].
    deleting: Vec<(String, String, Vec<String>)>,
    /// Every inbound message (by purge token) seen in a snapshot of a met room, oldest first,
    /// so a message is news once: not when it is delivered again after a reconnect, not when a
    /// thread leaves a snapshot and comes back, not when a deletion that lagged is undone.
    known: KnownTokens,
    /// New messages not yet handed to the notifications, one entry per thread
    /// ([`DmUi::take_arrivals`]).
    arrivals: Vec<DmArrival>,
    /// How many times the rail rows were rebuilt, for tests.
    rebuilds: usize,
}

/// How old (by the time its sender signed) a direct message may be and still raise a
/// notification: one hour. Older ones count as unread but stay quiet.
pub const MAX_NOTIFY_AGE_SECS: u64 = 3600;

// Read-side unix seconds for notification age; nothing is signed.
#[allow(clippy::disallowed_methods)]
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The least number of tokens remembered as known. The set also never falls below twice what the
/// latest snapshot holds, so a message the snapshot still has is never forgotten.
const KNOWN_MIN: usize = 8192;

/// A bounded set of purge tokens that forgets the oldest first.
#[derive(Default)]
struct KnownTokens {
    set: HashSet<String>,
    order: std::collections::VecDeque<String>,
}

impl KnownTokens {
    /// Remember `token`. True if it was not known.
    fn insert(&mut self, token: &str) -> bool {
        if self.set.contains(token) {
            return false;
        }
        self.set.insert(token.to_owned());
        self.order.push_back(token.to_owned());
        true
    }

    fn trim(&mut self, keep: usize) {
        while self.order.len() > keep.max(KNOWN_MIN) {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
    }
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
            .find(|t| t.room == room && t.peer_key == uid)
            .map(|t| &**t)
    }

    /// How many messages of `t` the user has not seen: those the peer sent that are not in the
    /// seen set.
    pub fn unread_of(&self, t: &DmThread) -> usize {
        // A room whose state has not loaded has no read position yet: not news.
        if !self.known_rooms.contains(&t.room) {
            return 0;
        }
        let seen = self.seen.get(&(t.room.clone(), t.peer_key.clone()));
        t.messages
            .iter()
            .filter(|m| !m.outgoing && !seen.is_some_and(|s| s.contains(&m.key)))
            .count()
    }

    /// Unread messages across the threads on the rail: what its badges add up to. An archived
    /// thread is off the rail and does not count until its peer writes again.
    pub fn unread_total(&self) -> usize {
        self.snapshot
            .threads
            .iter()
            .filter(|t| !self.archive.is_hidden(t))
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
    /// notification).
    ///
    /// **What is news** ([`DmUi::take_arrivals`]): an inbound message whose token no snapshot of
    /// a met room has held before, in a thread whose peer is a current member, that is not
    /// archived (a message that makes the peer write past the archive cutoff revives the thread,
    /// so it counts), and that no deletion of that peer's messages is waiting on. What a room
    /// held when it was met is known, not news, and a row that could not be opened is never news.
    pub fn apply(&mut self, snapshot: DmSnapshot) {
        self.apply_at(snapshot, now_secs());
    }

    /// [`DmUi::apply`] at the time `now` (unix seconds), which decides how old a message may be
    /// and still notify ([`MAX_NOTIFY_AGE_SECS`]).
    pub fn apply_at(&mut self, snapshot: DmSnapshot, now: u64) {
        let mut met_now: HashSet<String> = HashSet::new();
        for room in &snapshot.room_keys {
            if !self.known_rooms.insert(room.clone()) {
                continue;
            }
            met_now.insert(room.clone());
            for t in snapshot.threads.iter().filter(|t| &t.room == room) {
                self.seen
                    .entry((t.room.clone(), t.peer_key.clone()))
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
            if let Some(seen) = self.seen.get_mut(&(t.room.clone(), t.peer_key.clone())) {
                let present: HashSet<&str> = t.messages.iter().map(|m| m.key.as_str()).collect();
                seen.retain(|k| present.contains(k.as_str()));
            }
        }
        self.archive.reconcile(&snapshot);
        self.snapshot = snapshot;
        self.forget_deleted();
        self.rebuild();
        self.find_arrivals(&met_now, now);
    }

    /// Note which inbound messages of the snapshot just taken are news, and remember them all.
    fn find_arrivals(&mut self, met_now: &HashSet<String>, now: u64) {
        let mut inbound = 0;
        for t in self.snapshot.threads.iter() {
            // A room not met yet has no read position: what it holds is seeded when it is.
            if !self.known_rooms.contains(&t.room) {
                continue;
            }
            let quiet = met_now.contains(&t.room)
                || !t.peer_is_member
                || self.archive.is_hidden(t)
                || self
                    .snapshot
                    .purges
                    .iter()
                    .any(|p| p.room == t.room && p.peer_key == t.peer_key)
                || self
                    .deleting
                    .iter()
                    .any(|(room, peer, _)| *room == t.room && *peer == t.peer_key);
            let seen = self.seen.get(&(t.room.clone(), t.peer_key.clone()));
            let mut count = 0;
            let mut newest: Option<&DmMessage> = None;
            for m in t.messages.iter().filter(|m| !m.outgoing) {
                inbound += 1;
                // A row that cannot be opened is not remembered, so it can notify once it can
                // be (when the room's key arrives), if it is still recent.
                if matches!(m.body, DmBody::Placeholder(_)) {
                    continue;
                }
                let new = self.known.insert(&m.key);
                // The age is of the time the sender signed. A message hours old (a stale first
                // state, a reconnect after a long outage) is still unread but not announced; a
                // sender who backdates only silences their own notification, and a time in the
                // future is "recent".
                let recent = m.timestamp.saturating_add(MAX_NOTIFY_AGE_SECS) >= now;
                if new && recent && !quiet && !seen.is_some_and(|s| s.contains(&m.key)) {
                    count += 1;
                    newest = Some(m);
                }
            }
            let Some(newest) = newest else { continue };
            let preview = match &newest.body {
                DmBody::Text(text) => model::preview(text, PREVIEW_CHARS),
                DmBody::Invite { room_label, .. } => {
                    format!(
                        "Invitation to {}",
                        bounded(&name(room_label), PREVIEW_CHARS)
                    )
                }
                _ => String::new(),
            };
            let (room, peer_key) = (t.room.clone(), t.peer_key.clone());
            let arrival = DmArrival {
                room,
                peer_key,
                peer_name: name(&t.peer_name),
                count,
                key: newest.key.clone(),
                preview: name(&preview),
            };
            match self
                .arrivals
                .iter_mut()
                .find(|a| a.room == arrival.room && a.peer_key == arrival.peer_key)
            {
                Some(old) => {
                    old.count += arrival.count;
                    old.key = arrival.key;
                    old.preview = arrival.preview;
                    old.peer_name = arrival.peer_name;
                }
                None => self.arrivals.push(arrival),
            }
        }
        self.known.trim(inbound * 2);
    }

    /// How many messages the thread `(room, peer)` has not seen; 0 if it is not there.
    pub fn unread_in(&self, room: &str, peer: &str) -> usize {
        self.thread(room, peer).map_or(0, |t| self.unread_of(t))
    }

    /// The unread messages of the threads on the rail, less those of rooms that `muted` names:
    /// what the window title and tray count add for direct messages. A direct message is
    /// addressed to the user, so a room's "mentions and replies" mode counts it.
    pub fn counted_unread(&self, muted: impl Fn(&str) -> bool) -> usize {
        // The rows hold each thread's unread count, worked out when the snapshot or a read
        // position changed ([`DmUi::rebuild`]), and leave out archived threads.
        self.rows
            .iter()
            .filter(|r| !muted(&r.room))
            .map(|r| r.unread)
            .sum()
    }

    /// The thread to open to see what [`DmUi::counted_unread`] counts: the first on the rail with
    /// unread messages in a room `muted` does not name, as `(room, peer)`.
    pub fn first_unread_thread(&self, muted: impl Fn(&str) -> bool) -> Option<(String, String)> {
        self.rows
            .iter()
            .find(|r| r.unread > 0 && !muted(&r.room))
            .map(|r| (r.room.clone(), r.peer_key.clone()))
    }

    /// Mark every thread [`DmUi::counted_unread`] counts as seen, without opening any. True if
    /// that moved a read position. Direct-message read positions live in memory only, as a click
    /// on a thread leaves them.
    pub fn mark_all_seen(&mut self, muted: impl Fn(&str) -> bool) -> bool {
        let threads: Vec<(String, String)> = self
            .rows
            .iter()
            .filter(|r| r.unread > 0 && !muted(&r.room))
            .map(|r| (r.room.clone(), r.peer_key.clone()))
            .collect();
        let mut moved = false;
        for (room, peer) in threads {
            let Some(keys) = self.thread(&room, &peer).map(|t| {
                t.messages
                    .iter()
                    .filter(|m| !m.outgoing)
                    .map(|m| m.key.clone())
                    .collect::<Vec<_>>()
            }) else {
                continue;
            };
            let seen = self.seen.entry((room, peer)).or_default();
            let before = seen.len();
            seen.extend(keys);
            moved |= seen.len() != before;
        }
        if moved {
            self.rebuild();
        }
        moved
    }

    /// Mark the thread `(room, peer)` read up to the message with purge token `token`, without
    /// opening it: what a notification showed was seen and what is newer was not. Nothing for a
    /// message the thread no longer holds. True if the read position moved.
    pub fn mark_read_through(&mut self, room: &str, peer: &str, token: &str) -> bool {
        if !self.known_rooms.contains(room) {
            return false;
        }
        let Some(keys) = self.thread(room, peer).and_then(|t| {
            let at = t.messages.iter().position(|m| m.key == token)?;
            Some(
                t.messages[..=at]
                    .iter()
                    .filter(|m| !m.outgoing)
                    .map(|m| m.key.clone())
                    .collect::<Vec<_>>(),
            )
        }) else {
            return false;
        };
        let seen = self
            .seen
            .entry((room.to_owned(), peer.to_owned()))
            .or_default();
        let before = seen.len();
        seen.extend(keys);
        if seen.len() == before {
            return false;
        }
        self.rebuild();
        true
    }

    /// Whether `token` is remembered as known, for tests.
    #[cfg(test)]
    pub(super) fn knows(&self, token: &str) -> bool {
        self.known.set.contains(token)
    }

    /// The new messages found since the last call, one entry per thread.
    pub fn take_arrivals(&mut self) -> Vec<DmArrival> {
        std::mem::take(&mut self.arrivals)
    }

    /// Note that the messages `keys` of the thread `(room, peer)` are being deleted: once the
    /// thread no longer holds one, the read position forgets it.
    pub(super) fn note_deleting(&mut self, room: &str, peer: &str, keys: Vec<String>) {
        self.deleting.push((room.to_owned(), peer.to_owned(), keys));
        if self.deleting.len() > MAX_DELETING {
            self.deleting.remove(0);
        }
        self.forget_deleted();
    }

    /// Forget, from the read positions, the keys of messages being deleted that the last
    /// snapshot of a loaded room no longer holds. A key the thread still has waits.
    fn forget_deleted(&mut self) {
        let snapshot = &self.snapshot;
        let seen = &mut self.seen;
        self.deleting.retain_mut(|(room, peer, keys)| {
            if !snapshot.room_keys.iter().any(|k| k == room) {
                return true;
            }
            let present: HashSet<&str> = snapshot
                .threads
                .iter()
                .filter(|t| &t.room == room && &t.peer_key == peer)
                .flat_map(|t| t.messages.iter().map(|m| m.key.as_str()))
                .collect();
            if let Some(set) = seen.get_mut(&(room.clone(), peer.clone())) {
                set.retain(|k| !keys.contains(k) || present.contains(k.as_str()));
            }
            keys.retain(|k| present.contains(k.as_str()));
            !keys.is_empty()
        });
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

    pub(super) fn rebuild(&mut self) {
        self.rebuilds += 1;
        let hydrated = self.snapshot.hydrated;
        self.archive.refresh(&self.snapshot);
        let mut previews = std::mem::take(&mut self.previews);
        let mut kept = HashMap::new();
        let mut rows = Vec::with_capacity(self.snapshot.threads.len());
        for t in &self.snapshot.threads {
            // An archived thread is off the rail until its peer writes again.
            if self.archive.is_hidden(t) {
                continue;
            }
            let id = (t.room.clone(), t.peer_key.clone());
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
                peer_key: t.peer_key.clone(),
                peer_name: name(&t.peer_name),
                room_name: name(&t.room_name),
                preview: p.text.clone(),
                unread: self.unread_of(t),
                last_ts: t.last_ts,
            });
            kept.insert(id, p);
        }
        self.previews = kept;
        // Two rows in one room with one name (two members can have the same nickname, or the
        // same short ID) are told apart by a slice of their full id, which differs where the
        // short ID, the id's low bits, does not.
        let mut count: HashMap<(String, String), usize> = HashMap::new();
        for r in &rows {
            *count
                .entry((r.room.clone(), r.peer_name.clone()))
                .or_default() += 1;
        }
        for r in &mut rows {
            if count[&(r.room.clone(), r.peer_name.clone())] > 1 {
                r.peer_name = format!("{} ({})", r.peer_name, id_slice(&r.peer_key));
            }
        }
        rows.sort_by(|a, b| {
            (b.unread > 0)
                .cmp(&(a.unread > 0))
                .then_with(|| b.last_ts.cmp(&a.last_ts))
                .then_with(|| a.room.cmp(&b.room))
                .then_with(|| a.peer_key.cmp(&b.peer_key))
        });
        self.rows = rows;
    }

    /// The rail's rows, newest and unread first: `(room key, peer uid)` of each.
    pub fn rail_order(&self) -> Vec<(&str, &str)> {
        self.rows
            .iter()
            .map(|r| (r.room.as_str(), r.peer_key.as_str()))
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

/// The key of a thread's scroll area: what an own send scrolls to the end of.
pub fn scroll_key(room: &str, peer: &str) -> String {
    format!("dm-scroll-{room}-{peer}")
}

/// Six hex digits of a peer's full id taken from its high bits (the low bits are what the short
/// ID shows), for telling two rows with one name apart.
fn id_slice(peer_key: &str) -> String {
    let n = peer_key.parse::<i64>().unwrap_or(0) as u64;
    format!("{:06x}", n >> 40)
}

/// A name a peer or a room chose, as one line with nothing hidden or reordering in it.
fn name(s: &str) -> String {
    crate::rooms::room::clean(s)
}

/// [`name`], for the deletion's confirmation.
pub(super) fn clean_name(s: &str) -> String {
    name(s)
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

    /// [`Chat::apply_dms`] at the time `now` (unix seconds), for tests.
    #[cfg(test)]
    pub(super) fn apply_dms_at(&mut self, snapshot: DmSnapshot, now: u64) {
        self.dm.apply_at(snapshot, now);
        self.dm_settle();
    }

    /// The thread "Direct messages" in the tray opens: the first with unread messages that count.
    pub fn first_unread_dm(&self) -> Option<(String, String)> {
        self.dm
            .first_unread_thread(|room| self.mode_of(room) == NotificationMode::Muted)
    }

    /// Mark every counted thread seen, as the tray's "Mark all as read" does.
    pub fn mark_all_dms_seen(&mut self) -> bool {
        let muted: std::collections::HashSet<String> = self
            .rooms
            .iter()
            .filter(|r| self.mode_of(&r.key) == NotificationMode::Muted)
            .map(|r| r.key.clone())
            .collect();
        self.dm.mark_all_seen(|room| muted.contains(room))
    }

    /// The unread direct messages counted by the title and the tray: not those of a muted room.
    pub fn dm_unread_counted(&self) -> usize {
        self.dm
            .counted_unread(|room| self.mode_of(room) == NotificationMode::Muted)
    }

    /// The new direct messages since the last call, for the notifications.
    pub fn take_dm_arrivals(&mut self) -> Vec<DmArrival> {
        self.dm.take_arrivals()
    }

    /// Close the open thread when there is nothing left to show it as: its room is gone, or its
    /// peer has no messages and is not a member of the room.
    pub(super) fn dm_settle(&mut self) {
        // Before what waits for a room that is gone is dropped, so what a reply left for the
        // open thread reaches its draft.
        if let Some((room, uid)) = self.dm.open.clone() {
            let known = self.rooms().iter().any(|r| {
                r.key == room
                    && (self.dm.thread(&room, &uid).is_some()
                        || r.member_by_peer_key(&uid).is_some_and(|m| m.id != r.me))
            });
            if !known {
                self.leave_dm();
            }
        }
        self.dm_forget_gone();
        // A confirmation belongs to the thread that was open when it was asked.
        let open = self.dm.open.clone();
        self.dm.purge.keep_confirm_for(open.as_ref());
    }

    /// Stop showing the open thread, whatever the reason (Back, another room chosen, the room or
    /// the peer gone): the one place `dm.open` is cleared, so what a notification reply left in
    /// the thread's stash always reaches its draft ([`Chat::merge_stash`]). Returns the thread.
    pub(super) fn leave_dm(&mut self) -> Option<(String, String)> {
        let closed = self.dm.open.take()?;
        // The room's history is drawn afresh, back where it was left.
        self.stage_restore();
        self.merge_stash(&closed);
        self.dm.purge.keep_confirm_for(None);
        Some(closed)
    }

    /// What waits for an answer for a room that is gone will not get one.
    fn dm_forget_gone(&mut self) {
        let keys: Vec<String> = self.rooms().iter().map(|r| r.key.clone()).collect();
        self.dm
            .compose
            .forget_rooms(|r| keys.iter().any(|k| k == r));
        self.dm.purge.forget_rooms(|r| keys.iter().any(|k| k == r));
        self.dm.invite.forget_rooms(|r| keys.iter().any(|k| k == r));
        self.dm
            .archive
            .forget_rooms(|r| keys.iter().any(|k| k == r));
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
                    || r.member_by_peer_key(uid).is_some_and(|m| m.id != r.me))
        });
        if !known {
            return false;
        }
        self.remember_scroll();
        // A thread left for another gets what a notification reply left for it.
        if let Some(prev) = self.dm.open.clone()
            && (prev.0 != room || prev.1 != uid)
        {
            self.merge_stash(&prev);
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
        // The thread's own box takes the keyboard, with the draft it kept, if it can take text.
        // A box that cannot (the peer left the room) has no edit handler, so rune could never
        // clear the flag again: a click in the thread would leave it focused (#1022).
        let open = self.dm.open.clone();
        self.dm.compose.tidy(open.as_ref());
        let can_type = self.dm_block().is_none();
        self.dm.compose_edit().focused = can_type;
        true
    }

    /// Back to the room that was open.
    pub fn close_dm(&mut self) {
        let Some(closed) = self.leave_dm() else {
            return;
        };
        // What was typed stays as the thread's draft; the box gives up the keyboard.
        if let Some(d) = self.dm.compose.drafts_mut().get_mut(&closed) {
            d.focused = false;
        }
        self.dm.compose.tidy(None);
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
        self.sync_box_focus();
    }

    /// A box that cannot take text does not count as focused. It has no edit handler, so rune
    /// could not clear the flag when a click took the keyboard elsewhere (#1022). A box made
    /// read-only by a send takes the keyboard back once it can type again (a click in the thread meanwhile is not
    /// seen here, so it is taken back too).
    fn sync_box_focus(&mut self) {
        let Some(key) = self.dm.open.clone() else {
            self.dm.refocus_after_send = false;
            return;
        };
        let sending = self.dm.compose.is_sending_in(&key);
        if sending || self.dm_block().is_some() {
            if std::mem::take(&mut self.dm.compose_edit().focused) && sending {
                self.dm.refocus_after_send = true;
            }
        } else if std::mem::take(&mut self.dm.refocus_after_send) {
            self.dm.compose_edit().focused = true;
        }
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
    if !super::dm_archive::section_shown(dm, dm.rows.len()) {
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
    out.extend(super::dm_archive::undo_line(chat, p));
    for r in dm.rows.iter().take(RAIL_MAX) {
        let selected = dm.open_key() == Some((r.room.as_str(), r.peer_key.as_str()));
        out.push(rail_row(r, selected, chat.dm_archive_block(), p));
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
    out.extend(super::dm_archive::fold_out(chat, p));
    out
}

fn rail_row(r: &RailRow, selected: bool, blocked: Option<&'static str>, p: &Palette) -> El {
    // The badge is the count `DmUi::unread_total` adds up, selected or not.
    let unread = r.unread > 0;
    let strong = unread || selected;
    let (room, uid) = (r.room.clone(), r.peer_key.clone());
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
        .key(format!("dm-{}-{}", r.room, r.peer_key))
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
    out = out.child(super::dm_archive::row_button(
        &r.room,
        &r.peer_key,
        &r.peer_name,
        blocked,
        p,
    ));
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
                room.and_then(|r| r.member_by_peer_key(uid))
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
        .child(header(chat, &shown, narrow, p, k))
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
    // About deleting their messages: why it is off when that needs saying, what is under way or
    // was refused.
    for line in super::dm_purge::notes(chat, &shown.peer_name, p) {
        notes = notes.child(line);
        any = true;
    }
    if any {
        c = c.child(notes.pt(8.0));
    }
    c.child(history(chat, &shown, hydrated, p, k))
        .child(hline(p))
        .child(super::dm_compose::composer(chat, p, k))
}

fn header(chat: &Chat, shown: &Shown<'_>, narrow: bool, p: &Palette, k: &Kit) -> El {
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
        .child(super::dm_archive::header_action(chat, narrow, p, k))
        .child(super::dm_purge::header_action(
            chat,
            narrow,
            &shown.peer_name,
            p,
            k,
        ))
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
                reply_gone: false,
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
    let scroll_key = scroll_key(shown.room, shown.uid);
    let mut frame = col().key("dm-frame").grow(1.0).min_h(0.0).w_full().child(
        col()
            .key(scroll_key.clone())
            .grow(1.0)
            .min_h(0.0)
            .w_full()
            .child(items.selection_group(true))
            .scroll(ScrollSpec { stick_bottom: true })
            // Focusable so a click focuses it and Up and Down scroll it (see `conversation::history`).
            .focusable(true)
            .label("Conversation history")
            .scroll_edge_fade(40.0)
            .on_at_bottom(|s: &mut RiverApp, at| s.chat.dm.at_bottom = at)
            .on_scroll_offset(chat.history_probe(scroll_key.clone()))
            // Room for the bar's track, so nothing at the right edge sits under it.
            .pr(ScrollBarStyle::from_kit(k).strip())
            .scroll_bar(k),
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
        let body = body_el(chat, shown, m, dm, hydrated, p, k);
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
pub(super) fn plain(s: impl Into<String>, color: rune::Color) -> El {
    rune::rich(s.into(), Vec::new())
        .font(st::MESSAGE)
        .color(color)
        .w_full()
        .max_w(BODY_MAX_W)
        .selectable(true)
}

fn body_el(
    chat: &Chat,
    shown: &Shown<'_>,
    m: &Message,
    dm: &DmMessage,
    hydrated: bool,
    p: &Palette,
    k: &Kit,
) -> El {
    let body = match &dm.body {
        // As the room does: the text's markdown.
        DmBody::Text(_) => message_body(m, p, k),
        DmBody::Invite {
            room,
            room_label,
            note,
        } => super::dm_invite::card(
            chat,
            (shown.room, shown.uid),
            &m.key,
            Some((room_label.as_str(), room.as_str())),
            Some(room_label.as_str()),
            note.as_deref(),
            &shown.peer_name,
            p,
            k,
        ),
        DmBody::SentInvite { note } => super::dm_invite::card(
            chat,
            (shown.room, shown.uid),
            &m.key,
            None,
            None,
            note.as_deref(),
            &shown.peer_name,
            p,
            k,
        ),
        DmBody::Placeholder(ph) => plain(placeholder_text(*ph, hydrated), p.text_muted),
    };
    let mut c = col().key(format!("dm-msg-{}", m.key)).w_full().child(body);
    if let Some(pending) = &dm.pending {
        c = c.gap(4.0).child(super::dm_compose::delivery(pending, p));
    }
    c
}
