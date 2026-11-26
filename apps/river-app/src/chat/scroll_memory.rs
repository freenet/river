//! Each room remembers how far its history was scrolled when it was left, for this session.
//!
//! What is kept is the RAW scroll offset the history last reported, not a message. Content
//! inserted above that place while the room is away (rare in the native app, where history is
//! not paged in at the top) shifts what the offset points at. Nothing is written to disk.
//!
//! A room left at its end is forgotten, so it opens at its newest message as it always did.
//! Opening a room still marks it read, exactly as before: this only moves the scroll.
//!
//! On reopening, the scroll is applied once the history has reported that it is drawn. An
//! explicit jump (the overview's first unread mention, a search hit, a quoted reply) wins over
//! the remembered offset.

use std::collections::HashMap;

use rune::Tx;

use super::Chat;
use super::model::Room;

/// Frames the per-frame hook keeps asking for while the history has not drawn.
const DRAW_WAKES: u16 = 60;

fn scroll_key(room: &str) -> String {
    format!("history-scroll-{room}")
}

/// A remembered offset waiting for its room's history to be drawn.
#[derive(Debug)]
struct Restore {
    room: String,
    offset: f32,
    /// The number of history reports when it was staged; a later one means a fresh draw.
    seen: u64,
    waited: u16,
}

#[derive(Default)]
pub(super) struct ScrollMemory {
    pub(super) offsets: HashMap<String, f32>,
    restore: Option<Restore>,
}

impl ScrollMemory {
    pub(super) fn retain_rooms(&mut self, rooms: &[Room]) {
        self.offsets
            .retain(|k, _| rooms.iter().any(|r| &r.key == k));
    }
}

impl Chat {
    /// The open room's history is about to leave the screen: keep its offset, or forget the
    /// room when it was at its end.
    pub(super) fn remember_scroll(&mut self) {
        // A thread covering the room recorded it when it opened.
        if self.dm.is_open() {
            return;
        }
        let Some(room) = self.room().map(|r| r.key.clone()) else {
            return;
        };
        // Its remembered offset has not been put back yet: where the history is now was never
        // the reader's place, so the memory stays as it was.
        if self
            .scroll_memory
            .restore
            .as_ref()
            .is_some_and(|r| r.room == room)
        {
            return;
        }
        let (offset, at_end) = {
            let pos = self.history_pos.borrow();
            if pos.key != scroll_key(&room) {
                return;
            }
            (pos.offset, pos.offset >= pos.max - 1.0)
        };
        if at_end {
            self.scroll_memory.offsets.remove(&room);
        } else {
            self.scroll_memory.offsets.insert(room, offset);
        }
    }

    /// The open room's history is about to be drawn afresh: scroll it back once it is drawn.
    pub(super) fn stage_restore(&mut self) {
        self.scroll_memory.restore = None;
        let Some(room) = self.room().map(|r| r.key.clone()) else {
            return;
        };
        if let Some(&offset) = self.scroll_memory.offsets.get(&room) {
            let seen = self.history_pos.borrow().reports;
            self.scroll_memory.restore = Some(Restore {
                room,
                offset,
                seen,
                waited: 0,
            });
        }
    }

    /// Apply a staged restore. Called every frame; true while it wants more frames.
    pub(super) fn apply_restore(&mut self, tx: &mut Tx) -> bool {
        let Some(r) = &mut self.scroll_memory.restore else {
            return false;
        };
        let jumping = self.reveal.as_ref().is_some_and(|j| j.room == r.room);
        if self.selected != r.room || self.dm.is_open() || jumping {
            self.scroll_memory.restore = None;
            return false;
        }
        let key = scroll_key(&r.room);
        let drawn = {
            let pos = self.history_pos.borrow();
            pos.key == key && pos.reports > r.seen
        };
        if !drawn {
            r.waited = r.waited.saturating_add(1);
            return r.waited < DRAW_WAKES;
        }
        tx.scroll_to_y(key, r.offset);
        self.scroll_memory.restore = None;
        false
    }
}
