//! Each room remembers where the reader was in it, as the web UI does
//! (`ui/src/components/conversation/history_scroll.rs`, `remember_reading_position`).
//!
//! The place is an anchor, not a raw offset: the first message whose top is inside the history
//! and how far below the history's top that is. A deletion or edit above the reader while away
//! would shift a raw offset, and the anchor still finds its message. The raw offset stays as
//! the fallback for an anchor whose message has gone.
//!
//! The view records the anchor of the open room as it is scrolled ([`Chat::note_reading_place`]),
//! forgetting it when the reader is at the bottom; opening a room (or coming back from a
//! direct-message thread over it) stages a [`Restore`] that the per-frame hook applies once the
//! history is laid out ([`Chat::apply_restore`], called by `apply_reveal`). The memory lasts for
//! the session and is not written to disk.
//!
//! Precedence on opening: an explicit jump (a reveal: the overview's first unread mention, a
//! search hit, a quoted reply) beats the remembered place, which beats the newest message.

use rune::{Cx, Tx};

use super::Chat;
use super::model::Room;
use crate::RiverApp;

/// Where in a room the reader was.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Place {
    /// The key of the first message whose top was in view.
    message: String,
    /// How far below the top of the history that message's top was.
    at: f32,
    /// The history's scroll offset, for when the message is no longer in the room.
    offset: f32,
}

/// A remembered place waiting to be scrolled to.
#[derive(Clone, Debug)]
pub(super) struct Restore {
    room: String,
    place: Place,
    /// Frames to wait first: the room's history has to be laid out before it can be scrolled.
    wait: u8,
    /// Once the scroll went out, frames left before the history's own reports are believed.
    hold: Option<u8>,
}

const HOLD_FRAMES: u8 = 3;

impl Chat {
    /// Record where the open room's history is scrolled to, or forget the place when it is at
    /// its end. Run by the view, which can read the laid-out rows; it re-runs on every scroll
    /// because each offset report is a state change.
    pub(super) fn note_reading_place(&self, room: &Room, cx: &Cx<RiverApp>) {
        let key = format!("history-scroll-{}", room.key);
        // While a place waits to be restored the history still reports the end it opened at,
        // which must not replace the place about to be restored.
        if self.restore.is_some() || room.messages.is_empty() {
            return;
        }
        let (offset, reported) = {
            let pos = self.history_pos.borrow();
            (pos.offset, pos.key == key)
        };
        // The history that reported last is another one: nothing is known of this one yet.
        if !reported {
            return;
        }
        // Every row is read, in every run (also at the end, so the rects are there when the
        // reader scrolls up), not a binary search's few: a rect is only recorded for a key a
        // view read in an earlier run, so a changing set of keys would never settle. The view
        // builds every message each run anyway, so this adds nothing of another order.
        let tops: Vec<Option<f32>> = room
            .messages
            .iter()
            .map(|m| cx.layout(&format!("row-{}", m.key)).map(|r| r[1]))
            .collect();
        let view = cx.layout(&key);
        if self.at_bottom {
            self.scroll_memory.borrow_mut().remove(&room.key);
            return;
        }
        let Some(view) = view else {
            return;
        };
        let top = view[1] + offset;
        // The first message whose top is in view. A row not laid out yet leaves the old place.
        let mut first = None;
        for (i, t) in tops.iter().enumerate() {
            let Some(t) = t else {
                return;
            };
            if first.is_none() && *t >= top - 0.5 {
                first = Some((i, *t));
            }
        }
        let Some((i, t)) = first else {
            return;
        };
        self.scroll_memory.borrow_mut().insert(
            room.key.clone(),
            Place {
                message: room.messages[i].key.clone(),
                at: (t - top).max(0.0),
                offset,
            },
        );
    }

    /// The open room's history is about to be drawn afresh: scroll it back to where the reader
    /// left it, once laid out, if it was left away from the end. Not at the end meanwhile, so
    /// that what the room's end shows is not reported read before it has been on screen.
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
        self.restore = Some(Restore {
            room,
            place,
            wait: 1,
            hold: None,
        });
    }

    /// Whether a restored place is still settling, so the history's reports are not the
    /// reader's.
    pub(super) fn restoring(&self) -> bool {
        self.restore.is_some()
    }

    /// Scroll a staged [`Restore`] out once its room's history is laid out. Called every frame;
    /// true while it still needs frames.
    pub(super) fn apply_restore(&mut self, tx: &mut Tx) -> bool {
        let Some(r) = &mut self.restore else {
            return false;
        };
        let gone = self.selected != r.room || self.dm.is_open();
        // A jump to a message in this room is what the reader asked for just now.
        let jumping = self.reveal.as_ref().is_some_and(|j| j.room == r.room);
        if gone || jumping {
            self.restore = None;
            if jumping {
                self.resync_at_bottom();
            }
            return false;
        }
        if r.wait > 0 {
            r.wait -= 1;
            return true;
        }
        if let Some(hold) = &mut r.hold {
            if *hold > 0 {
                *hold -= 1;
                return true;
            }
            self.restore = None;
            self.resync_at_bottom();
            return false;
        }
        let there = self
            .rooms
            .iter()
            .find(|room| room.key == r.room)
            .is_some_and(|room| room.message_by_key(&r.place.message).is_some());
        if there {
            tx.scroll_key_to(format!("row-{}", r.place.message), r.place.at);
        } else {
            tx.scroll_to_y(format!("history-scroll-{}", r.room), r.place.offset);
        }
        r.hold = Some(HOLD_FRAMES);
        true
    }

    /// Take the history's own word on whether it is at its end, after reports were set aside.
    fn resync_at_bottom(&mut self) {
        let key = format!("history-scroll-{}", self.selected);
        let at = {
            let pos = self.history_pos.borrow();
            pos.key == key && pos.offset >= pos.max - 1.0
        };
        self.set_at_bottom(at);
    }
}
