//! Each room remembers where the reader was in it, as the web UI does
//! (`ui/src/components/conversation/history_scroll.rs`, `remember_reading_position`).
//!
//! The place is an anchor, not a raw offset: a message and how far below the history's top its
//! top was (negative when the message starts above the top, as a tall one does), plus the raw
//! scroll offset as the fallback for a message that has since gone.
//!
//! Scrolling does not run the view, and a handler cannot read layout, so the place cannot be
//! recorded as the reader scrolls. It is taken when the room is LEFT: the leaving code
//! ([`Chat::leave_room_history`]) queues a [`Capture`] with the latest scroll offset the history
//! reported, and the next view run turns it into a [`Place`] from the rows' content-space rects
//! (`Cx::layout` does not move with scrolling), which rune still holds for the rows of the
//! history just removed because the previous run read them. Only while the history is scrolled
//! away from its end does the view read those rects, and only for every [`SAMPLE`]th message;
//! the nearest sampled row is the anchor, so it costs nothing at the end.
//!
//! Opening a room, or coming back to it from a direct-message thread, stages a [`Restore`]
//! ([`Chat::stage_restore`]) that the per-frame hook applies ([`Chat::apply_restore`], called by
//! `apply_reveal`): it waits for the history to report (it is drawn), scrolls, and waits for the
//! next report (the scroll landed). No count of frames decides anything; a cap only ends a wait
//! for a report that a scroll to the same offset never makes.
//!
//! Reading: while a restore is pending the room is not "at its end", and the history's own
//! at-end reports are set aside, so the node is not told that messages never on screen were
//! read; what the history then really shows decides. The unread count and local read mark follow
//! the same rule as the node's report, as in the web UI ("an arrival below the fold stays unread
//! until the reader reaches it", `document_title.rs`): they move when the end is reached.
//!
//! Precedence on opening: an explicit jump (a reveal: the overview's first unread mention, a
//! search hit, a quoted reply) beats the remembered place, which beats the newest message.
//! The memory lasts for the session and is not written to disk.

use rune::{Cx, Tx};

use super::Chat;
use super::model::Room;
use crate::RiverApp;

/// Every this-many messages is read for its rect; the nearest one is the anchor.
const SAMPLE: usize = 8;
/// Frames that a wait for a report that may never come (a scroll to where it already is) lasts.
const SETTLE_CAP: u8 = 30;
/// Frames the per-frame hook keeps asking for more while the history has not drawn.
const DRAW_WAKES: u16 = 60;

/// Where in a room the reader was.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Place {
    /// The message nearest the top of the history (its key) and its top's offset from the
    /// history's top; `None` when no rect was known.
    anchor: Option<(String, f32)>,
    /// The history's scroll offset, for when there is no anchor or its message is gone.
    pub(super) offset: f32,
}

/// A place to take once the history just left is still held by rune.
#[derive(Clone, Debug)]
pub(super) struct Capture {
    room: String,
    offset: f32,
    /// The keys of the messages whose rects were read.
    keys: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    /// Waiting for the history to report that it is drawn (a report after `seen`).
    Waiting { seen: u64, waited: u16 },
    /// Waiting for the report that follows the scroll (after `seen`).
    Settling { seen: u64, frames: u8 },
}

/// A remembered place waiting to be scrolled to.
#[derive(Clone, Debug)]
pub(super) struct Restore {
    room: String,
    place: Place,
    phase: Phase,
}

fn scroll_key(room: &str) -> String {
    format!("history-scroll-{room}")
}

/// The messages whose rects are read for `room`: confirmed ones (a pending message is renamed
/// when it is confirmed), every [`SAMPLE`]th.
fn sampled(room: &Room) -> Vec<String> {
    room.messages
        .iter()
        .enumerate()
        .filter(|(i, m)| i % SAMPLE == 0 && m.pending.is_none())
        .map(|(_, m)| m.key.clone())
        .collect()
}

impl Chat {
    /// The history of the open room is about to leave the screen (another room, a thread, a
    /// list without the room): keep where it was scrolled to, or forget the place when the
    /// reader was at its end.
    pub(super) fn leave_room_history(&mut self) {
        // A pending restore means the history has not been the reader's yet: the old place stands.
        if self.dm.is_open() || self.restore.is_some() {
            return;
        }
        let Some(room) = self.room() else {
            return;
        };
        if self.at_bottom {
            self.scroll_memory.borrow_mut().remove(&room.key);
            return;
        }
        let offset = {
            let pos = self.history_pos.borrow();
            if pos.key != scroll_key(&room.key) {
                return;
            }
            pos.offset
        };
        let capture = Capture {
            room: room.key.clone(),
            offset,
            keys: sampled(room),
        };
        self.captures.borrow_mut().push(capture);
    }

    /// What the view does about reading places: turn queued captures into places, and, while the
    /// open history is scrolled away from its end, read the sampled rows' rects so that they are
    /// held when it is left.
    pub(super) fn read_reading_places(&self, cx: &Cx<RiverApp>) {
        for c in self.captures.borrow_mut().drain(..) {
            let key = scroll_key(&c.room);
            let top = cx.layout(&key).map(|v| v[1] + c.offset);
            let anchor = top.and_then(|top| {
                c.keys
                    .iter()
                    .filter_map(|k| cx.layout(&format!("row-{k}")).map(|r| (k, r[1] - top)))
                    .min_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                    .map(|(k, at)| (k.clone(), at))
            });
            self.scroll_memory.borrow_mut().insert(
                c.room,
                Place {
                    anchor,
                    offset: c.offset,
                },
            );
        }
        if self.dm.is_open() || self.at_bottom || self.restore.is_some() {
            return;
        }
        if let Some(room) = self.room() {
            cx.layout(&scroll_key(&room.key));
            for k in sampled(room) {
                cx.layout(&format!("row-{k}"));
            }
        }
    }

    /// The open room's history is about to be drawn afresh: scroll it back to where the reader
    /// left it, once drawn, if it was left away from the end. Not at the end meanwhile, so that
    /// what the room's end shows is not reported read before it has been on screen.
    pub(super) fn stage_restore(&mut self) {
        self.restore = None;
        let Some(room) = self.room().map(|r| r.key.clone()) else {
            return;
        };
        let place = self.scroll_memory.borrow().get(&room).cloned();
        let Some(place) = place else {
            return;
        };
        self.at_bottom = false;
        let seen = self.history_pos.borrow().reports;
        self.restore = Some(Restore {
            room,
            place,
            phase: Phase::Waiting { seen, waited: 0 },
        });
    }

    /// Whether a restored place is still settling, so the history's at-end reports are not the
    /// reader's.
    pub(super) fn restoring(&self) -> bool {
        self.restore.is_some()
    }

    /// Drop the places of rooms that are not in the list.
    pub(super) fn forget_places_of_gone_rooms(&mut self) {
        let rooms = &self.rooms;
        self.scroll_memory
            .borrow_mut()
            .retain(|k, _| rooms.iter().any(|r| &r.key == k));
        self.captures
            .borrow_mut()
            .retain(|c| rooms.iter().any(|r| r.key == c.room));
    }

    /// Advance a staged [`Restore`]. Called every frame; true while it wants more frames.
    pub(super) fn apply_restore(&mut self, tx: &mut Tx) -> bool {
        let Some(r) = &mut self.restore else {
            return false;
        };
        if self.selected != r.room || self.dm.is_open() {
            // Left again before it was drawn (a thread coming back stages it afresh).
            self.restore = None;
            return false;
        }
        let key = scroll_key(&r.room);
        let (drawn_reports, drawn) = {
            let pos = self.history_pos.borrow();
            (pos.reports, pos.key == key)
        };
        match r.phase {
            Phase::Waiting { seen, waited } => {
                // A jump to a message in this room is what the reader asked for just now; its
                // scroll goes out in this same call.
                let jumping = self.reveal.as_ref().is_some_and(|j| j.room == r.room);
                if jumping {
                    r.phase = Phase::Settling {
                        seen: drawn_reports,
                        frames: 0,
                    };
                    return true;
                }
                if !(drawn && drawn_reports > seen) {
                    r.phase = Phase::Waiting {
                        seen,
                        waited: waited.saturating_add(1),
                    };
                    // Not drawn (a narrow window showing the rooms, a hidden window): nothing
                    // to do until it is, and the frame that draws it runs this again.
                    return waited < DRAW_WAKES;
                }
                let there = self
                    .rooms
                    .iter()
                    .find(|room| room.key == r.room)
                    .is_some_and(|room| {
                        r.place.anchor.as_ref().is_some_and(|(k, _)| {
                            room.message_by_key(k).is_some_and(|m| m.pending.is_none())
                        })
                    });
                match (&r.place.anchor, there) {
                    (Some((k, at)), true) => tx.scroll_key_to(format!("row-{k}"), *at),
                    _ => tx.scroll_to_y(key, r.place.offset),
                }
                r.phase = Phase::Settling {
                    seen: drawn_reports,
                    frames: 0,
                };
                true
            }
            Phase::Settling { seen, frames } => {
                if (drawn && drawn_reports > seen) || frames >= SETTLE_CAP {
                    self.restore = None;
                    self.resync_at_bottom();
                    return false;
                }
                r.phase = Phase::Settling {
                    seen,
                    frames: frames + 1,
                };
                true
            }
        }
    }

    /// Take the history's own word on whether it is at its end, now that its reports are
    /// believed again; a history that has not reported leaves the room "not at its end".
    fn resync_at_bottom(&mut self) {
        let key = scroll_key(&self.selected);
        let at = {
            let pos = self.history_pos.borrow();
            pos.key == key && pos.offset >= pos.max - 1.0
        };
        if at {
            self.set_at_bottom(true);
        }
    }
}
