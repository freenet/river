//! Archiving a direct-message thread: the rail's per-row action, the thread header's action, the
//! Undo line, and the "Archived (N)" fold-out with Un-archive.
//!
//! The rules (what the cutoff is, when a thread comes back, what a send does) are
//! `rooms::dm_archive`, ported from the web UI; the saving is in the background work
//! (`rooms::dm_store`, `rooms::session`). This module decides what the screen offers and says.
//!
//! - **Shared with the web UI.** The list is `hidden_threads` of the chat delegate's
//!   `outbound_dms` value, so a thread archived there is archived here and the other way round.
//!   The window is sent the list on every read of that value ([`DmSnapshot::archived`]), so an
//!   entry another device wrote shows and one it removed does not. The web UI's blind save of
//!   its whole copy can put an older list back, which unarchives what was archived here since
//!   that copy loaded; it is not fought (it would also fight a deliberate Un-archive there). The
//!   thread simply shows on the rail again, and archiving it again fixes it.
//! - **Shown at once.** A click changes the rail before the background work has answered: the
//!   change is kept as a pending overlay over the snapshot until a snapshot holds it
//!   ([`DmArchiveUi::reconcile`]), so a snapshot built a moment earlier cannot flash the row
//!   back. Nothing waits on the delegate: the write is retried by the background work like the
//!   saved text is, and if it never works the change holds for the session.
//! - **The cutoff** is the newest time of a message the PEER sent, 0 when they sent none (see
//!   `rooms::dm_archive` for why never the user's own message and never the clock, and for why
//!   it is the time as signed, [`DmThread::last_inbound_signed`], and not the clamped time the
//!   thread shows). The background work takes the larger of the
//!   thread as shown and the room as it is, as the web UI does.
//! - **Hidden until the peer writes.** The thread leaves the rail while the peer's newest message
//!   is not past the cutoff; a newer one brings it back with its unread count. Sending into it
//!   brings it back at once (the background work does that on the send, not this module).
//! - **Undo** is a line at the top of the rail for [`UNDO_FOR`]. The clock is passed in
//!   ([`Chat::dm_archive_at`], [`Chat::dm_tick`]); nothing here reads it.
//! - **Off, with the reason,** while River has moved on and this build writes nothing.
//! - **Privacy.** Nothing here logs, and the peer's name appears only in the window's own rail
//!   and header, never in a title, notification, status or diagnostic.
//!
//! The action on a row and in the header is a real button: reachable by Tab, pressed by Enter or
//! Space.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rune::{El, col, icon, row};
use rune_kit::{Button, ButtonVariant, Kit};

use super::conversation::text_button;
use super::dm_model::{DmSnapshot, DmThread};
use super::dm_view::DmUi;
use super::icons;
use super::style::{self as st, Palette};
use super::view::label;
use super::{Chat, Source};
use crate::RiverApp;
use crate::rooms::dm::peer_from_key;
use crate::rooms::dm_archive::is_hidden;
use crate::rooms::generation::UPDATE_REQUIRED_ARCHIVE;
use crate::rooms::outbox::Action;

/// How long the Undo line stays after an archive.
pub const UNDO_FOR: Duration = Duration::from_secs(8);

/// The most changes kept over the snapshot while it catches up.
const MAX_PENDING: usize = 100;

/// The line after an archive.
pub const ARCHIVED_LINE: &str = "Archived conversation with";

/// What an archive or un-archive the window made says, until a snapshot holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pending {
    room: String,
    peer: String,
    /// `Some(cutoff)`: archived at that cutoff. `None`: taken out of the archive.
    cutoff: Option<u64>,
}

/// The Undo line.
#[derive(Clone, PartialEq, Eq)]
struct Undo {
    room: String,
    peer: String,
    /// The peer's name as the rail showed it, for the line.
    name: String,
    until: Instant,
}

/// The archive state of the direct-message view.
#[derive(Default)]
pub struct DmArchiveUi {
    /// The "Archived (N)" fold-out is open.
    pub(super) open: bool,
    undo: Option<Undo>,
    pending: Vec<Pending>,
    /// The list as shown: the snapshot's, with what the window just did laid over it, by
    /// `(room, peer)`.
    cutoffs: HashMap<(String, String), u64>,
    /// The archived threads that are off the rail now, by `(room, peer)`: those whose peer has
    /// not written past the cutoff.
    hidden: Vec<(String, String)>,
}

impl std::fmt::Debug for DmArchiveUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmArchiveUi")
            .field("open", &self.open)
            .field("hidden", &self.hidden.len())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl DmArchiveUi {
    /// Drop the changes a snapshot holds: an archive it lists at the cutoff asked for or past
    /// it, an un-archive it no longer lists. Run for every snapshot taken, before it is used.
    pub(super) fn reconcile(&mut self, snapshot: &DmSnapshot) {
        self.pending.retain(|p| {
            let held = snapshot
                .archived
                .iter()
                .find(|a| a.room == p.room && a.peer_key == p.peer)
                .map(|a| a.hidden_at_ts);
            match p.cutoff {
                Some(ts) => !held.is_some_and(|h| h >= ts),
                None => held.is_some(),
            }
        });
    }

    /// Work out [`DmArchiveUi::is_hidden`] for a snapshot.
    pub(super) fn refresh(&mut self, snapshot: &DmSnapshot) {
        let mut cutoffs: HashMap<(String, String), u64> = snapshot
            .archived
            .iter()
            .map(|a| ((a.room.clone(), a.peer_key.clone()), a.hidden_at_ts))
            .collect();
        for p in &self.pending {
            let key = (p.room.clone(), p.peer.clone());
            match p.cutoff {
                Some(ts) => {
                    let held = cutoffs.get(&key).copied().unwrap_or(0);
                    cutoffs.insert(key, held.max(ts));
                }
                None => {
                    cutoffs.remove(&key);
                }
            }
        }
        let inbound = |room: &str, peer: &str| {
            snapshot
                .threads
                .iter()
                .find(|t| t.room == room && t.peer_key == peer)
                .map_or(0, |t| t.last_inbound_signed)
        };
        let mut hidden: Vec<(String, String)> = cutoffs
            .iter()
            .filter(|((room, peer), cutoff)| is_hidden(Some(**cutoff), inbound(room, peer)))
            .map(|(k, _)| k.clone())
            .collect();
        hidden.sort();
        self.cutoffs = cutoffs;
        self.hidden = hidden;
    }

    /// Whether the thread is archived and its peer has not written past the cutoff: off the rail.
    pub fn is_hidden(&self, t: &DmThread) -> bool {
        self.cutoffs
            .get(&(t.room.clone(), t.peer_key.clone()))
            .is_some_and(|c| is_hidden(Some(*c), t.last_inbound_signed))
    }

    /// Whether the thread `(room, peer)` is archived and off the rail, thread or not.
    pub fn is_archived(&self, room: &str, peer: &str) -> bool {
        self.hidden.iter().any(|(r, p)| r == room && p == peer)
    }

    /// How many archived threads are off the rail: what "Archived (N)" says.
    pub fn count(&self) -> usize {
        self.hidden.len()
    }

    /// Drop what waits for a room that is no longer listed.
    pub(super) fn forget_rooms(&mut self, listed: impl Fn(&str) -> bool) {
        self.pending.retain(|p| listed(&p.room));
        if self.undo.as_ref().is_some_and(|u| !listed(&u.room)) {
            self.undo = None;
        }
    }

    fn push(&mut self, pending: Pending) {
        self.pending
            .retain(|p| !(p.room == pending.room && p.peer == pending.peer));
        self.pending.push(pending);
        if self.pending.len() > MAX_PENDING {
            self.pending.remove(0);
        }
    }

    /// The user sent a message into the thread `(room, peer)`: if it is archived it is brought
    /// back, as the background work does for the saved list. True when that changed anything.
    pub(super) fn note_sent(&mut self, room: &str, peer: &str) -> bool {
        if !self
            .cutoffs
            .contains_key(&(room.to_owned(), peer.to_owned()))
        {
            return false;
        }
        self.push(Pending {
            room: room.to_owned(),
            peer: peer.to_owned(),
            cutoff: None,
        });
        true
    }

    /// Whether the Undo line is showing.
    pub fn undo_showing(&self) -> bool {
        self.undo.is_some()
    }
}

impl Chat {
    /// Why archiving is off, `None` when it is on.
    pub fn dm_archive_block(&self) -> Option<&'static str> {
        self.issues
            .update_required
            .then_some(UPDATE_REQUIRED_ARCHIVE)
    }

    /// Whether the thread `(room, peer)` is archived and off the rail.
    pub fn dm_is_archived(&self, room: &str, peer: &str) -> bool {
        self.dm.archive.is_archived(room, peer)
    }

    /// How many archived threads the fold-out lists.
    pub fn dm_archived_count(&self) -> usize {
        self.dm.archive.count()
    }

    /// Whether the fold-out is open.
    pub fn dm_archived_open(&self) -> bool {
        self.dm.archive.open
    }

    /// Open or close the fold-out.
    pub fn dm_archived_toggle(&mut self) {
        self.dm.archive.open = !self.dm.archive.open;
    }

    /// [`Chat::dm_archive_at`] at the time it is.
    pub fn dm_archive(&mut self, room: &str, peer: &str) -> bool {
        self.dm_archive_at(room, peer, Instant::now())
    }

    /// Archive the thread with the member `peer` (their `peer_key`) in the room keyed `room`:
    /// off the rail at once, saved by the background work, with the Undo line until
    /// `now + UNDO_FOR`. The open thread, if it is this one, closes. False when archiving is off,
    /// the thread is not known, or it is archived already.
    pub fn dm_archive_at(&mut self, room: &str, peer: &str, now: Instant) -> bool {
        if self.dm_archive_block().is_some() || self.dm_is_archived(room, peer) {
            return false;
        }
        let Some(peer_id) = peer_from_key(peer) else {
            return false;
        };
        let thread = self
            .dm
            .snapshot()
            .threads
            .iter()
            .find(|t| t.room == room && t.peer_key == peer);
        let (inbound, name) = match thread {
            Some(t) => (t.last_inbound_signed, t.peer_name.clone()),
            None => {
                // A member with no messages yet, opened from the member list.
                let Some(member) = self
                    .rooms()
                    .iter()
                    .find(|r| r.key == room)
                    .and_then(|r| r.member_by_peer_key(peer))
                else {
                    return false;
                };
                (0, member.nickname.clone())
            }
        };
        let name = crate::rooms::room::clean(&name);
        self.dm.archive.push(Pending {
            room: room.to_owned(),
            peer: peer.to_owned(),
            cutoff: Some(inbound),
        });
        self.dm.archive.undo = Some(Undo {
            room: room.to_owned(),
            peer: peer.to_owned(),
            name,
            until: now + UNDO_FOR,
        });
        if matches!(self.source, Source::Node(_)) {
            self.emit(
                room.to_owned(),
                Action::ArchiveDm {
                    peer: peer_id,
                    row_inbound_ts: inbound,
                },
            );
        }
        if self.dm.open_key() == Some((room, peer)) {
            self.close_dm();
        }
        self.dm.refresh_archive();
        true
    }

    /// Take the thread out of the archive, whatever its cutoff: Un-archive, and Undo. The thread
    /// is back on the rail at once. False when archiving is off or it is not archived.
    pub fn dm_unarchive(&mut self, room: &str, peer: &str) -> bool {
        if self.dm_archive_block().is_some() || !self.dm_is_archived(room, peer) {
            return false;
        }
        let Some(peer_id) = peer_from_key(peer) else {
            return false;
        };
        self.dm.archive.push(Pending {
            room: room.to_owned(),
            peer: peer.to_owned(),
            cutoff: None,
        });
        if self
            .dm
            .archive
            .undo
            .as_ref()
            .is_some_and(|u| u.room == room && u.peer == peer)
        {
            self.dm.archive.undo = None;
        }
        if matches!(self.source, Source::Node(_)) {
            self.emit(room.to_owned(), Action::UnarchiveDm { peer: peer_id });
        }
        self.dm.refresh_archive();
        true
    }

    /// Press Undo: take back the archive the line names.
    pub fn dm_undo(&mut self) -> bool {
        let Some(u) = self.dm.archive.undo.clone() else {
            return false;
        };
        self.dm.archive.undo = None;
        self.dm_unarchive(&u.room, &u.peer)
    }

    /// Take the Undo line away when its time is up at `now`. True when that changed what shows.
    pub(super) fn dm_archive_tick(&mut self, now: Instant) -> bool {
        if self
            .dm
            .archive
            .undo
            .as_ref()
            .is_some_and(|u| now >= u.until)
        {
            self.dm.archive.undo = None;
            return true;
        }
        false
    }
}

impl DmUi {
    /// Work out the archive state again for the snapshot held, and the rail with it.
    pub(super) fn refresh_archive(&mut self) {
        self.rebuild();
    }
}

// The view ------------------------------------------------------------------------------------

/// Whether the rail's section has anything to show: threads, an archived one to bring back, or
/// the Undo line.
pub(super) fn section_shown(dm: &DmUi, rows: usize) -> bool {
    rows > 0 || dm.archive.count() > 0 || dm.archive.undo_showing()
}

/// A row's archive button: always there, so it can be reached with the keyboard and found by
/// touch.
pub(super) fn row_button(
    room: &str,
    peer: &str,
    name: &str,
    blocked: Option<&'static str>,
    p: &Palette,
) -> El {
    let (room, peer) = (room.to_owned(), peer.to_owned());
    let (r2, p2) = (room.clone(), peer.clone());
    let button = row()
        .key(format!("dm-archive-{room}-{peer}"))
        .role("button")
        .label(format!("Archive conversation with {name}"))
        .help("Archive")
        .focusable(true)
        .size(28.0, 28.0)
        .radius(6.0)
        .center()
        .hover(p.surface_hover)
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut RiverApp| {
            s.chat.dm_archive(&room, &peer);
        })
        .on_key(move |s: &mut RiverApp, ev, _| {
            let enter = ev.key == rune::Key::Enter;
            if enter {
                s.chat.dm_archive(&r2, &p2);
            }
            enter
        })
        .child(icon(&icons::ARCHIVE, 15.0).color(p.text_muted));
    match blocked {
        Some(why) => button.help(why).disabled(true).opacity(0.45),
        None => button,
    }
}

/// The Undo line, when it is showing.
pub(super) fn undo_line(chat: &Chat, p: &Palette) -> Option<El> {
    let u = chat.dm.archive.undo.as_ref()?;
    Some(
        row()
            .key("dm-undo-line")
            .w_full()
            .px(10.0)
            .py(4.0)
            .gap(8.0)
            .center_y()
            .child(
                label(
                    format!("{ARCHIVED_LINE} {}", u.name),
                    st::SMALL,
                    p.text_muted,
                )
                .wrap()
                .shrink(),
            )
            .child(rune::spacer())
            .child(
                text_button(
                    "dm-undo".to_owned(),
                    "Undo",
                    p.accent,
                    |s: &mut RiverApp| {
                        s.chat.dm_undo();
                    },
                )
                .focusable(true)
                .on_key(|s: &mut RiverApp, ev, _| {
                    let enter = ev.key == rune::Key::Enter;
                    if enter {
                        s.chat.dm_undo();
                    }
                    enter
                }),
            ),
    )
}

/// What the fold-out lists for one archived thread.
struct Listed {
    room: String,
    peer: String,
    name: String,
    room_name: String,
}

fn listed(chat: &Chat) -> Vec<Listed> {
    let mut out: Vec<Listed> = chat
        .dm
        .archive
        .hidden
        .iter()
        .map(|(room, peer)| {
            let thread = chat
                .dm
                .snapshot()
                .threads
                .iter()
                .find(|t| &t.room == room && &t.peer_key == peer);
            let r = chat.rooms().iter().find(|r| &r.key == room);
            let name = thread
                .map(|t| t.peer_name.clone())
                .or_else(|| {
                    r.and_then(|r| r.member_by_peer_key(peer))
                        .map(|m| m.nickname.clone())
                })
                .unwrap_or_else(|| "A member who has left".to_owned());
            let room_name = thread
                .map(|t| t.room_name.clone())
                .or_else(|| r.map(|r| r.name.clone()))
                .unwrap_or_default();
            Listed {
                room: room.clone(),
                peer: peer.clone(),
                name: crate::rooms::room::clean(&name),
                room_name: crate::rooms::room::clean(&room_name),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        a.room_name
            .cmp(&b.room_name)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| (&a.room, &a.peer).cmp(&(&b.room, &b.peer)))
    });
    out
}

/// "Archived (N)" and, open, the threads in it with Un-archive. Nothing when none is archived.
pub(super) fn fold_out(chat: &Chat, p: &Palette) -> Vec<El> {
    let n = chat.dm.archive.count();
    if n == 0 {
        return Vec::new();
    }
    let open = chat.dm.archive.open;
    let toggle = row()
        .key("dm-archived-toggle")
        .role("button")
        .label(format!(
            "Archived conversations ({n}), {}",
            if open { "shown" } else { "hidden" }
        ))
        .focusable(true)
        .w_full()
        .h(32.0)
        .px(10.0)
        .gap(6.0)
        .center_y()
        .radius(8.0)
        .hover(p.row_hover)
        .cursor(rune::Cursor::Pointer)
        .on_press(|s: &mut RiverApp| s.chat.dm_archived_toggle())
        .on_key(|s: &mut RiverApp, ev, _| {
            let enter = ev.key == rune::Key::Enter;
            if enter {
                s.chat.dm_archived_toggle();
            }
            enter
        })
        .child(label(format!("Archived ({n})"), st::SMALL, p.text_muted))
        .child(
            icon(&icons::CHEVRON_DOWN, 12.0)
                .color(p.text_muted)
                .rotate(if open { std::f32::consts::PI } else { 0.0 }),
        );
    let mut out = vec![toggle];
    if open {
        for l in listed(chat) {
            let (room, peer) = (l.room.clone(), l.peer.clone());
            let (r2, p2) = (l.room.clone(), l.peer.clone());
            out.push(
                row()
                    .key(format!("dm-archived-{}-{}", l.room, l.peer))
                    .w_full()
                    .px(10.0)
                    .py(4.0)
                    .gap(8.0)
                    .center_y()
                    .child(
                        col()
                            .grow(1.0)
                            .min_w(0.0)
                            .gap(1.0)
                            .child(label(l.name.clone(), st::SMALL, p.text_secondary).truncate())
                            .child(
                                label(format!("in {}", l.room_name), st::SMALL, p.text_muted)
                                    .truncate(),
                            ),
                    )
                    .child(
                        text_button(
                            format!("dm-unarchive-{}-{}", l.room, l.peer),
                            "Un-archive",
                            p.accent,
                            move |s: &mut RiverApp| {
                                s.chat.dm_unarchive(&room, &peer);
                            },
                        )
                        .focusable(true)
                        .label(format!("Un-archive conversation with {}", l.name))
                        .on_key(move |s: &mut RiverApp, ev, _| {
                            let enter = ev.key == rune::Key::Enter;
                            if enter {
                                s.chat.dm_unarchive(&r2, &p2);
                            }
                            enter
                        }),
                    ),
            );
        }
    }
    out
}

/// The thread header's archive action: Archive, or Un-archive for a thread that is archived (one
/// opened from the member list). Off, with the reason as the tooltip, while River has moved on.
pub(super) fn header_action(chat: &Chat, narrow: bool, p: &Palette, k: &Kit) -> El {
    let Some((room, peer)) = chat
        .dm
        .open_key()
        .map(|(r, p)| (r.to_owned(), p.to_owned()))
    else {
        return row().key("dm-archive");
    };
    let archived = chat.dm_is_archived(&room, &peer);
    let name = if archived { "Un-archive" } else { "Archive" };
    let block = chat.dm_archive_block();
    let press = move |s: &mut RiverApp| {
        if archived {
            s.chat.dm_unarchive(&room, &peer);
        } else {
            s.chat.dm_archive(&room, &peer);
        }
    };
    let mut action = if narrow {
        row()
            .key("dm-archive")
            .role("button")
            .label(name)
            .size(36.0, 36.0)
            .radius(8.0)
            .center()
            .hover(p.surface_hover)
            .cursor(rune::Cursor::Pointer)
            .on_press(press)
            .child(icon(&icons::ARCHIVE, 18.0).color(p.text_secondary))
    } else {
        Button::new(name)
            .variant(ButtonVariant::Ghost)
            .on_press(press)
            .build(k)
            .key("dm-archive")
    };
    action = action.help(block.unwrap_or(name));
    if block.is_some() {
        action = action.disabled(true).opacity(0.45);
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::dm_model::DmArchived;

    fn archived(room: &str, peer: &str, ts: u64) -> DmArchived {
        DmArchived {
            room: room.into(),
            peer_key: peer.into(),
            hidden_at_ts: ts,
        }
    }

    fn snapshot(list: Vec<DmArchived>) -> DmSnapshot {
        DmSnapshot {
            archived: list,
            ..Default::default()
        }
    }

    fn pending(room: &str, peer: &str, cutoff: Option<u64>) -> Pending {
        Pending {
            room: room.into(),
            peer: peer.into(),
            cutoff,
        }
    }

    #[test]
    fn a_pending_archive_settles_only_when_a_snapshot_holds_its_cutoff_or_more() {
        let mut ui = DmArchiveUi::default();
        ui.push(pending("r", "p", Some(100)));
        // Not listed, or listed lower: not settled.
        ui.reconcile(&snapshot(vec![]));
        assert_eq!(ui.pending.len(), 1);
        ui.reconcile(&snapshot(vec![archived("r", "p", 99)]));
        assert_eq!(ui.pending.len(), 1);
        // At the cutoff asked for, or past it (another device's higher one): settled.
        ui.reconcile(&snapshot(vec![archived("r", "p", 100)]));
        assert!(ui.pending.is_empty());
        ui.push(pending("r", "p", Some(100)));
        ui.reconcile(&snapshot(vec![archived("r", "p", 500)]));
        assert!(ui.pending.is_empty());
        // Another thread's entry says nothing about this one.
        ui.push(pending("r", "p", Some(100)));
        ui.reconcile(&snapshot(vec![
            archived("r", "q", 500),
            archived("s", "p", 500),
        ]));
        assert_eq!(ui.pending.len(), 1);
    }

    #[test]
    fn a_pending_unarchive_settles_when_the_snapshot_no_longer_lists_the_thread() {
        let mut ui = DmArchiveUi::default();
        ui.push(pending("r", "p", None));
        ui.reconcile(&snapshot(vec![archived("r", "p", 1)]));
        assert_eq!(ui.pending.len(), 1, "a snapshot from before the change");
        ui.reconcile(&snapshot(vec![]));
        assert!(ui.pending.is_empty());
    }

    #[test]
    fn the_overlay_lays_a_pending_change_over_the_snapshot_by_thread() {
        let mut ui = DmArchiveUi::default();
        ui.push(pending("r", "new", Some(50)));
        ui.push(pending("r", "gone", None));
        let snap = snapshot(vec![archived("r", "gone", 10), archived("r", "kept", 20)]);
        ui.refresh(&snap);
        let mut cutoffs: Vec<_> = ui
            .cutoffs
            .iter()
            .map(|((_, p), c)| (p.as_str(), *c))
            .collect();
        cutoffs.sort();
        assert_eq!(cutoffs, vec![("kept", 20), ("new", 50)]);
        // With no thread built for them, each is hidden (the peer has not written: 0 <= cutoff).
        assert_eq!(ui.count(), 2);
    }

    #[test]
    fn a_later_change_to_a_thread_replaces_the_earlier_and_the_list_is_bounded() {
        let mut ui = DmArchiveUi::default();
        ui.push(pending("r", "p", Some(1)));
        ui.push(pending("r", "p", None));
        assert_eq!(ui.pending, vec![pending("r", "p", None)]);
        for n in 0..(MAX_PENDING + 20) {
            ui.push(pending("r", &n.to_string(), Some(1)));
        }
        assert_eq!(ui.pending.len(), MAX_PENDING);
        // The oldest went, the newest stayed.
        assert!(
            ui.pending
                .iter()
                .any(|p| p.peer == (MAX_PENDING + 19).to_string())
        );
        assert!(!ui.pending.iter().any(|p| p.peer == "p"));
    }

    #[test]
    fn what_waits_for_a_room_that_is_gone_goes_with_it() {
        let mut ui = DmArchiveUi::default();
        ui.push(pending("gone", "p", Some(1)));
        ui.push(pending("here", "p", Some(1)));
        ui.undo = Some(Undo {
            room: "gone".into(),
            peer: "p".into(),
            name: "N".into(),
            until: Instant::now(),
        });
        ui.forget_rooms(|r| r == "here");
        assert_eq!(ui.pending, vec![pending("here", "p", Some(1))]);
        assert!(!ui.undo_showing());
    }

    #[test]
    fn a_send_brings_an_archived_thread_back_and_does_nothing_for_one_that_is_not() {
        let mut ui = DmArchiveUi::default();
        ui.refresh(&snapshot(vec![archived("r", "p", 5)]));
        assert!(!ui.note_sent("r", "other"));
        assert!(ui.note_sent("r", "p"));
        ui.refresh(&snapshot(vec![archived("r", "p", 5)]));
        assert_eq!(ui.count(), 0);
    }
}
