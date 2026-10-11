//! The room history's reader position: the state shared by the render and
//! the raw scroll and resize callbacks (`ReaderPosition`), the range
//! resolution that records it, and the wasm-only DOM side that measures and
//! moves the view. Policy: `.claude/rules/history-scrolling.md`.

#[cfg(target_arch = "wasm32")]
use super::history_window::trim_would_rearm_backfill;
use super::history_window::{
    relocate_anchor, relocate_tail, relocate_window, HistoryWindow, RangeHold, WindowAnchor,
    INITIAL_WINDOW_ITEMS, WINDOW_ANCHOR_KEYS, WINDOW_GROWTH_ITEMS,
};
use super::{display_item_key, display_item_newest, DisplayItem, NewestKey};
#[cfg(target_arch = "wasm32")]
use crate::components::app::CURRENT_ROOM;
#[cfg(target_arch = "wasm32")]
use crate::components::foreground::has_layout;
#[cfg(target_arch = "wasm32")]
use crate::components::scroll_to_latest::{
    is_arrival, scroll_to_end, HistoryDom, NEWEST_IN_VIEW_SLACK_PX,
};
#[cfg(target_arch = "wasm32")]
use dioxus::prelude::*;
use river_core::room_state::message::MessageId;
#[cfg(target_arch = "wasm32")]
use std::rc::Rc;

/// A one-shot request to take the view to the end of `room`'s history
/// (`.claude/rules/history-scrolling.md`). The render drops one made for
/// another room, and the reading-position corrections stand down while it is
/// pending.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct ScrollRequest {
    pub(super) room: Option<ed25519_dalek::VerifyingKey>,
}

/// The last reading position measured while the history was visible. A hidden
/// panel measures 0 everywhere, so the reveal restores from this instead.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct ReadingAnchor {
    /// `(data-anchor-key, offset from the visible top)`, reading row first, then
    /// its neighbours nearest first, the one above before the one below. A
    /// neighbour stands in if the reading row is deleted. Keyed by message id
    /// because a group re-keys when its first message is pruned.
    rows: Vec<(String, i32)>,
    /// Display-item index at capture time; a relocation hint.
    item_hint: usize,
}

/// Rows a [`ReadingAnchor`] remembers, the reading row included.
#[cfg(target_arch = "wasm32")]
const READING_ANCHOR_ROWS: usize = 7;

/// Does the end hold survive the step from the last render's range to this
/// one, given each range's newest message (`None` for an empty range)? Only
/// when both have one and it is unchanged.
fn end_hold_survives(before: Option<&MessageId>, now: Option<&MessageId>) -> bool {
    before.is_some() && before == now
}

/// State shared by the render, its effects and the raw scroll and resize
/// callbacks. Plain cells, so reads never subscribe; re-renders come from
/// `window_items`. The render writes `has_newer` before the catch-up button
/// reads it.
#[derive(Default)]
pub(super) struct ReaderPosition {
    /// The IDENTITY of the item the last render's window started at, so
    /// arrivals GROW the window instead of sliding it (#501), and so index
    /// shifts from at-cap message pruning cannot move the head in content
    /// space (#505). See [`WindowAnchor`].
    pub(super) window_anchor: std::cell::RefCell<Option<WindowAnchor>>,
    /// Identity of a held range's last items (newest first), set only while
    /// `HistoryWindow::has_newer`. Relocated like `window_anchor`.
    pub(super) window_tail: std::cell::RefCell<Option<WindowAnchor>>,
    /// How many display items the last render actually put on screen; the
    /// backfill growth step grows from it (see `grown_window`).
    pub(super) window_rendered: std::cell::Cell<usize>,
    /// Whether the rendered window holds more than a fresh room-open would
    /// render, i.e. whether the bottom-settle trim has anything to do.
    pub(super) window_overgrown: std::cell::Cell<bool>,
    /// See [`ReadingAnchor`].
    pub(super) anchor: std::cell::RefCell<Option<ReadingAnchor>>,
    /// Whether the ResizeObserver last saw the container without layout
    /// (behind a mobile panel). The next resize with layout restores.
    #[cfg(target_arch = "wasm32")]
    hidden: std::cell::Cell<bool>,
    /// The last render's `HistoryWindow::has_newer`.
    pub(super) has_newer: std::cell::Cell<bool>,
    /// The pending explicit scroll, if any. While newer items are held back
    /// the render first selects the latest range for it.
    pub(super) request: std::cell::Cell<Option<ScrollRequest>>,
    /// The next render extends a held range by one page of newer items.
    pub(super) extend_newer: std::cell::Cell<bool>,
    /// Bumped on every room change, so a deferred trim can detect one.
    pub(super) room_epoch: std::cell::Cell<u64>,
    /// The last value `note_newest_seen` published, so a repeat check (every
    /// settle, every render) schedules nothing.
    #[cfg(target_arch = "wasm32")]
    seen_noted: std::cell::RefCell<Option<(ed25519_dalek::VerifyingKey, MessageId)>>,
    /// The end hold: `Some(scrollTop)` while the view sits at the end an
    /// explicit request put it at, or a scroll settled at. Rows changing
    /// height take it back there (the ResizeObserver). The reader's first
    /// scroll away, a change of newest message (the render), a room change and
    /// the panel hiding end it; an arrival that is followed lands a new
    /// request, which holds again.
    end_hold: std::cell::Cell<Option<i32>>,
    /// The room and newest message of the last render's range (`None` for an
    /// empty range). The next render tells a change of newest message from
    /// rows changing height by it. While the range reaches the room's latest
    /// message (`!has_newer`), it is what `follow_arrival_at_end` compares an
    /// arrival against and what `note_newest_seen` publishes.
    range_newest: std::cell::RefCell<Option<(ed25519_dalek::VerifyingKey, NewestKey)>>,
    /// The reading rows measured just before a patch that adds or removes
    /// rows above them (the backfill, a head swap), for
    /// `correct_view_after_patch` to put back. Offsets are from the view's
    /// top, so the clamp the browser applies when a patch shortens the
    /// content, before the effect runs, cancels out (#505).
    restore_after_patch: std::cell::RefCell<Option<ReadingAnchor>>,
}

impl ReaderPosition {
    #[cfg(target_arch = "wasm32")]
    fn hidden(&self) -> bool {
        self.hidden.get()
    }

    /// Ask for one move to the end of `room`'s history (see [`ScrollRequest`]).
    pub(super) fn request_end(&self, room: Option<ed25519_dalek::VerifyingKey>) {
        self.request.set(Some(ScrollRequest { room }));
        self.drop_pending_corrections();
    }

    /// Forget a backfill restore or head reposition still waiting for its
    /// effect. It was measured against the view a request is replacing, and an
    /// effect that ran after the request completed would undo it.
    fn drop_pending_corrections(&self) {
        *self.restore_after_patch.borrow_mut() = None;
    }

    /// Measure the reading rows before a patch moves rows above them (see
    /// `restore_after_patch`). Nothing to measure without layout.
    #[cfg(target_arch = "wasm32")]
    pub(super) fn capture_before_patch(&self) {
        // The index hint only serves `keep`, which never reads this capture.
        if let Some(anchor) = capture_reading_anchor(0) {
            *self.restore_after_patch.borrow_mut() = Some(anchor);
        }
    }

    /// No range on screen, or a new room's: end the hold and forget the last
    /// range, so the next render's range is never compared against it.
    pub(super) fn forget_range(&self) {
        *self.range_newest.borrow_mut() = None;
        self.end_hold.set(None);
    }
}

/// Does `item` render the row a [`ReadingAnchor`] key names?
fn item_holds_anchor_key(item: &DisplayItem, key: &str) -> bool {
    match item {
        DisplayItem::Messages(group) => group.messages.iter().any(|m| m.id == key),
        DisplayItem::Event(summary) => summary.id == key,
    }
}

/// Index of the first remembered row still in the room.
fn locate_reading_item(items: &[DisplayItem], anchor: &ReadingAnchor) -> Option<usize> {
    anchor.rows.iter().find_map(|(key, _)| {
        relocate_anchor(items.len(), anchor.item_hint, |i| {
            item_holds_anchor_key(&items[i], key)
        })
    })
}

/// The scroll shift that puts the first surviving remembered row back at its
/// offset. `None` when none is rendered.
#[cfg(any(target_arch = "wasm32", test))]
fn reading_anchor_shift(
    anchor: &ReadingAnchor,
    top_now: impl Fn(&str) -> Option<i32>,
) -> Option<i32> {
    anchor
        .rows
        .iter()
        .find_map(|(key, saved)| top_now(key).map(|now| now - saved))
}

/// Does `key` identify `item`, without allocating a key `String`?
///
/// The relocation walk compares keys against up to every item on a miss;
/// building a `String` per probe made that O(total) allocations per render at
/// the at-cap steady state (#505).
fn display_item_key_matches(item: &DisplayItem, key: &str) -> bool {
    match item {
        DisplayItem::Messages(group) => group.messages[0].id == key,
        DisplayItem::Event(summary) => summary.id == key,
    }
}

/// Resolve the range this render puts on screen, and record it in `reader` for
/// the next render, the settle handler and the read rule. Writes only
/// `reader`'s cells, which nothing renders from: re-resolving with the values
/// just written is a fixed point. `room` is the open room.
///
/// Also returns whether rendered content at or above the new head was removed
/// in this very patch: the old head is gone (pruned, deleted, or re-keyed by a
/// drained first message) or now sits BEFORE the range (newer paging). The
/// reader's offset then needs the head reposition's compensation.
pub(super) fn resolve_rendered_range(
    groups: &[DisplayItem],
    requested_window: usize,
    room: Option<ed25519_dalek::VerifyingKey>,
    reader: &ReaderPosition,
) -> (HistoryWindow, bool) {
    // Re-locate the anchored window by IDENTITY: at-cap pruning shifts every
    // index, so the stored index is only a hint, and the head key alone can
    // vanish while its neighbors survive (#505; see `WindowAnchor`
    // and `relocate_window`). A pending request (Latest, an own send) swaps a
    // held range for the latest one, so it forgets all three.
    let select_latest = reader.request.get().is_some() && reader.has_newer.get();
    let (prev_anchor, prev_tail, keep) = if select_latest {
        (None, None, None)
    } else {
        (
            reader.window_anchor.borrow().clone(),
            reader.window_tail.borrow().clone(),
            // Keep the reading row, plus one item for a group the view top
            // cuts through.
            reader
                .anchor
                .borrow()
                .as_ref()
                .and_then(|a| locate_reading_item(groups, a))
                .map(|i| i.saturating_sub(1)),
        )
    };
    let key_at = |i: usize, key: &str| display_item_key_matches(&groups[i], key);
    let relocated = prev_anchor
        .as_ref()
        .map(|a| relocate_window(groups.len(), a, key_at));
    let extend_newer = reader.extend_newer.replace(false);
    let held_end = prev_tail.as_ref().map(|t| {
        let end = relocate_tail(groups.len(), t, key_at)
            // Every held item gone: keep the count.
            .unwrap_or_else(|| relocated.map_or(0, |r| r.start) + reader.window_rendered.get());
        if extend_newer {
            end + WINDOW_GROWTH_ITEMS
        } else {
            end
        }
    });
    let mut history_window = HistoryWindow::resolve_held(
        groups.len(),
        requested_window,
        relocated.as_ref().map(|r| r.start),
        RangeHold {
            end: held_end,
            keep,
        },
    );
    // A pending request lands at the latest message. If this render's own
    // arrivals hold the range's end (the ceiling), as a followed burst or a
    // send at the ceiling does, it takes the latest range instead.
    if reader.request.get().is_some() && history_window.has_newer && !select_latest {
        history_window =
            HistoryWindow::resolve_held(groups.len(), requested_window, None, RangeHold::default());
    }
    let head_removed =
        relocated.is_some_and(|r| !r.head_survived || history_window.start > r.start);
    let shown = &groups[history_window.start..history_window.end];

    // Where this render started, so the NEXT one grows instead of sliding
    // (#501).
    *reader.window_anchor.borrow_mut() = Some(WindowAnchor {
        keys: groups[history_window.start..]
            .iter()
            .take(WINDOW_ANCHOR_KEYS)
            .map(display_item_key)
            .collect(),
        index: history_window.start,
    });
    // See `grown_window`.
    reader
        .window_rendered
        .set(history_window.end - history_window.start);
    // Only a held range remembers its end.
    *reader.window_tail.borrow_mut() = history_window.has_newer.then(|| WindowAnchor {
        keys: shown
            .iter()
            .rev()
            .take(WINDOW_ANCHOR_KEYS)
            .map(display_item_key)
            .collect(),
        index: history_window.end - 1,
    });
    reader.has_newer.set(history_window.has_newer);
    // A change of newest message ends the end hold; anything else keeps it.
    // Against the last render, so a request's own render (an own
    // send's message) comes before its hold.
    let shown_newest = room.zip(shown.last().and_then(display_item_newest));
    let before = reader.range_newest.replace(shown_newest.clone());
    let id = |newest: &Option<(_, NewestKey)>| newest.as_ref().map(|(_, key)| key.id.clone());
    if !end_hold_survives(id(&before).as_ref(), id(&shown_newest).as_ref()) {
        reader.end_hold.set(None);
    }
    // Tell the settle handler whether a bottom-settle trim would shrink
    // anything.
    reader.window_overgrown.set(
        requested_window > INITIAL_WINDOW_ITEMS
            || history_window.end - history_window.start > INITIAL_WINDOW_ITEMS,
    );
    (history_window, head_removed)
}

/// Trailing delay used to spot a scroll settling on browsers with no
/// `scrollend` event (Safari before 17.4).
#[cfg(target_arch = "wasm32")]
const SCROLL_SETTLE_DEBOUNCE_MS: i32 = 120;

/// Refresh the saved reading position, keeping the old one when nothing can be
/// measured. Called at every settle, before a newer page, once an explicit
/// request or a reveal has placed the view, and when the Rooms or Members
/// button hides the chat panel (afterwards there is no geometry to read). A
/// breakpoint hide relies on the last settle.
#[cfg(target_arch = "wasm32")]
pub(super) fn remember_reading_position(reader: &ReaderPosition) {
    let range_start = reader
        .window_anchor
        .borrow()
        .as_ref()
        .map_or(0, |a| a.index);
    if let Some(anchor) = capture_reading_anchor(range_start) {
        *reader.anchor.borrow_mut() = Some(anchor);
    }
}

/// Natively there is no layout to measure.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn remember_reading_position(_reader: &ReaderPosition) {}

/// The room history's scroll container and newest-message sentinel.
#[cfg(target_arch = "wasm32")]
pub(super) const ROOM_HISTORY: HistoryDom = HistoryDom {
    container_id: "chat-scroll-container",
    sentinel_id: "bottom-sentinel",
    under_dm_thread: true,
};

/// Measure the first message or event row that starts inside the view (else
/// the one the view's top cuts through, see [`reading_row_index`]), with its
/// nearest neighbours. A binary search over item rows keeps this to a
/// handful of layout reads, cheap enough for every settle.
#[cfg(target_arch = "wasm32")]
fn capture_reading_anchor(range_start: usize) -> Option<ReadingAnchor> {
    use wasm_bindgen::JsCast;
    let container = ROOM_HISTORY.container()?;
    if !has_layout(&container) {
        return None;
    }
    let view = container.get_bounding_client_rect();
    let view_top = view.top();
    let rows = container.query_selector_all("[data-item-key]").ok()?;
    let row = |i: u32| {
        rows.item(i)
            .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
    };
    let n = rows.length();
    // The first item row whose bottom is below the visible top.
    let (mut lo, mut hi) = (0u32, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let bottom = row(mid).map_or(f64::MAX, |r| r.get_bounding_client_rect().bottom());
        if bottom <= view_top {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == n {
        return None;
    }
    // The anchor-keyed rows in that item and its neighbours, in page order.
    let mut found: Vec<(String, f64, f64)> = Vec::new();
    for i in lo.saturating_sub(1)..(lo + 3).min(n) {
        let Some(item) = row(i) else { continue };
        let mut push = |el: &web_sys::Element| {
            if let Some(key) = el.get_attribute("data-anchor-key") {
                let rect = el.get_bounding_client_rect();
                found.push((key, rect.top(), rect.bottom()));
            }
        };
        if item.has_attribute("data-anchor-key") {
            push(&item);
        } else if let Ok(inner) = item.query_selector_all("[data-anchor-key]") {
            for j in 0..inner.length() {
                if let Some(el) = inner
                    .item(j)
                    .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
                {
                    push(&el);
                }
            }
        }
    }
    let edges: Vec<(f64, f64)> = found
        .iter()
        .map(|(_, top, bottom)| (*top, *bottom))
        .collect();
    let reading = reading_row_index(&edges, view_top, view.bottom())?;
    let mut rows = Vec::with_capacity(READING_ANCHOR_ROWS);
    let entry = |i: usize| (found[i].0.clone(), (found[i].1 - view_top).round() as i32);
    rows.push(entry(reading));
    // The row above first: if the reading row is deleted, the row above stays
    // still, as it does when nothing corrects the removal.
    for d in 1..found.len() {
        if rows.len() >= READING_ANCHOR_ROWS {
            break;
        }
        if d <= reading {
            rows.push(entry(reading - d));
        }
        if reading + d < found.len() && rows.len() < READING_ANCHOR_ROWS {
            rows.push(entry(reading + d));
        }
    }
    Some(ReadingAnchor {
        rows,
        item_hint: range_start + lo as usize,
    })
}

/// Index of the row being read among `rows` (`(top, bottom)`, page order): the
/// first whose top lies in the view, else the one the view's top cuts through.
#[cfg(any(target_arch = "wasm32", test))]
fn reading_row_index(rows: &[(f64, f64)], view_top: f64, view_bottom: f64) -> Option<usize> {
    rows.iter()
        .position(|&(top, _)| top >= view_top - 0.5 && top < view_bottom)
        .or_else(|| {
            rows.iter()
                .position(|&(top, bottom)| top < view_top && bottom > view_top)
        })
}

/// Scroll so `anchor`'s first surviving row is back at its saved offset.
#[cfg(target_arch = "wasm32")]
fn restore_reading_anchor(anchor: &ReadingAnchor) {
    use wasm_bindgen::JsCast;
    let Some(container) = ROOM_HISTORY.container() else {
        return;
    };
    let view_top = container.get_bounding_client_rect().top();
    let Ok(els) = container.query_selector_all("[data-anchor-key]") else {
        return;
    };
    // One pass, measuring only the remembered keys, and stopping once they are
    // all found.
    let mut tops: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    for i in 0..els.length() {
        if tops.len() == anchor.rows.len() {
            break;
        }
        let Some(el) = els
            .item(i)
            .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        if let Some(key) = el.get_attribute("data-anchor-key") {
            if anchor.rows.iter().any(|(k, _)| *k == key) {
                let top = (el.get_bounding_client_rect().top() - view_top).round() as i32;
                tops.entry(key).or_insert(top);
            }
        }
    }
    let Some(shift) = reading_anchor_shift(anchor, |key| tops.get(key).copied()) else {
        return;
    };
    if shift != 0 {
        container.set_scroll_top(container.scroll_top() + shift);
    }
}

/// Height a trim to the newest `retained` items would leave. `None` when no
/// more than that many are rendered.
#[cfg(target_arch = "wasm32")]
fn retained_tail_height(container: &web_sys::Element, retained: usize) -> Option<i32> {
    let rows = container.query_selector_all("[data-item-key]").ok()?;
    let n = rows.length() as usize;
    if n <= retained {
        return None;
    }
    use wasm_bindgen::JsCast;
    let first = rows
        .item((n - retained) as u32)?
        .dyn_into::<web_sys::Element>()
        .ok()?;
    // Content coordinates, from the same rects as the container's own.
    let top_in_content = container.scroll_top() as f64 + first.get_bounding_client_rect().top()
        - container.get_bounding_client_rect().top();
    Some(container.scroll_height() - top_in_content.round() as i32)
}

/// Slack for deciding whether a scroll offset is at the end of the history.
///
/// `scrollTop` is fractional in every engine while `Element::scroll_top`
/// rounds, so an exact comparison would report a 1px difference on a view that
/// never moved. `conversation-autoscroll.spec.ts` is what catches this being
/// too tight.
#[cfg(target_arch = "wasm32")]
const SCROLL_TOP_SLACK_PX: i32 = 2;

/// Is the view within the end hold's release margin of the scroller's
/// absolute end? Inside it a settle arms the hold; past it an upward scroll
/// releases it.
#[cfg(target_arch = "wasm32")]
fn at_end_within_release_margin(container: &web_sys::Element) -> bool {
    let from_end = container.scroll_height() - container.scroll_top() - container.client_height();
    from_end as f64 <= NEWEST_IN_VIEW_SLACK_PX
}

/// Take the view to the end and hold it there.
#[cfg(target_arch = "wasm32")]
fn hold_end(reader: &ReaderPosition, container: &web_sys::Element) {
    scroll_to_end(container);
    reader.end_hold.set(Some(container.scroll_top()));
    // Now, not at the settle: the row read before the request would bound the
    // next render's range (`keep`) and pull a swapped-out range back in.
    remember_reading_position(reader);
    note_newest_seen(reader);
}

/// Land an explicit request (opening, own send, Latest) at the end of the
/// history, and hold it there while rows change height.
#[cfg(target_arch = "wasm32")]
pub(super) fn land_at_end(reader: &ReaderPosition, container: &web_sys::Element) {
    reader.drop_pending_corrections();
    hold_end(reader, container);
}

/// Keep the reader's view still across the patch that just landed, from the
/// reading rows captured before it (`ReaderPosition::restore_after_patch`),
/// consumed here, applied or not.
///
/// - The backfill restore: the revealed items land ABOVE the current offset,
///   so without it the history jumps by their full height. Restoring the
///   offset is also what stops the backfill cascading: the sentinel ends up
///   above the viewport again, so it stops intersecting until the reader
///   scrolls back up to it.
/// - The head reposition, when the render swapped the window head for a
///   later item: the head was pruned out from under the anchor (an at-cap
///   room drains its oldest message on every arrival), or newer paging slid
///   the start past it (#505).
///
/// Browser scroll anchoring is off (see the container's `style`), so nothing
/// else compensates. A mid-window removal or late-loading media above the
/// viewport still shifts the reader: #507, in
/// `.claude/rules/history-scrolling.md`.
///
/// Synchronous, not deferred: Dioxus has already patched the DOM when effects
/// run, and reading layout here forces the reflow the correction needs. A
/// macrotask hop let the browser paint one frame of the moved rows at the
/// wrong offset before the correction landed — a flicker native scroll
/// anchoring used to mask (#505).
#[cfg(target_arch = "wasm32")]
pub(super) fn correct_view_after_patch(reader: &ReaderPosition) {
    let Some(anchor) = reader.restore_after_patch.borrow_mut().take() else {
        return;
    };
    // An explicit request owns the view until it lands (#501).
    if reader.request.get().is_some() {
        return;
    }
    // Hidden rows measure 0; the reveal restores the reader instead.
    if ROOM_HISTORY.container().is_some_and(|c| has_layout(&c)) {
        restore_reading_anchor(&anchor);
    }
}

/// Follow an arrival for a reader at the end, by the rule in
/// `.claude/rules/history-scrolling.md`: ask for one move to the end when this
/// render brings a newer newest message and `HistoryDom::arrival_follows`.
/// Runs in render, so the DOM still shows the previous render; measuring after
/// the patch would count the arrival's own height against the band.
#[cfg(target_arch = "wasm32")]
pub(super) fn follow_arrival_at_end(
    groups: &[DisplayItem],
    room: Option<ed25519_dalek::VerifyingKey>,
    reader: &ReaderPosition,
) {
    // A pending request already owns the view; a held range is not at the end.
    if reader.request.get().is_some() || reader.has_newer.get() || reader.hidden() {
        return;
    }
    let Some(room) = room else {
        return;
    };
    let previous = reader
        .range_newest
        .borrow()
        .as_ref()
        .filter(|(rendered_room, _)| *rendered_room == room)
        .map(|(_, key)| key.clone());
    let now = groups.last().and_then(display_item_newest);
    if !is_arrival(previous.as_ref(), now.as_ref(), false) {
        return;
    }
    if ROOM_HISTORY.arrival_follows() {
        reader.request_end(Some(room));
    }
}

/// Finish the pending [`ScrollRequest`] once the history has rows and layout:
/// take the view to the end, then let the paging strips mount. A hidden panel
/// (including one revealed but not yet restored) or an empty room keeps it
/// pending. Returns whether it finished one.
#[cfg(target_arch = "wasm32")]
pub(super) fn complete_scroll_request(
    reader: &ReaderPosition,
    mut opening_snap_done: Signal<bool>,
) -> bool {
    let Some(request) = reader.request.get() else {
        return false;
    };
    let Some(container) = ROOM_HISTORY.container() else {
        return false;
    };
    if !has_layout(&container)
        || reader.hidden()
        || !container
            .query_selector("[data-item-key]")
            .is_ok_and(|row| row.is_some())
    {
        return false;
    }
    reader.request.set(None);
    land_at_end(reader, &container);
    // The strips wait for the view to reach the end once (#501). Deferred:
    // this can run from a raw observer callback, and the signal is rendered.
    crate::util::defer(move || {
        if CURRENT_ROOM.peek().owner_key == request.room && !*opening_snap_done.peek() {
            opening_snap_done.set(true);
        }
    });
    true
}

/// Publish the newest message as seen if the reader can see it now (the read
/// rule in `.claude/rules/history-scrolling.md`). Marking a room read never
/// goes past what this publishes; see `document_title::NEWEST_SEEN`. Touches
/// no signal itself, so raw JS callbacks may call it; the write is deferred.
#[cfg(target_arch = "wasm32")]
pub(super) fn note_newest_seen(reader: &ReaderPosition) {
    // Only a range that reaches the latest message has a candidate.
    if reader.has_newer.get() {
        return;
    }
    let Some(newest) = reader
        .range_newest
        .borrow()
        .as_ref()
        .map(|(room, key)| (*room, key.id.clone()))
    else {
        return;
    };
    if reader.seen_noted.borrow().as_ref() == Some(&newest) {
        return;
    }
    if reader.hidden() || !ROOM_HISTORY.end_seen() {
        return;
    }
    *reader.seen_noted.borrow_mut() = Some(newest.clone());
    crate::util::defer(move || {
        *crate::components::app::document_title::NEWEST_SEEN.write() = Some(newest);
    });
}

/// May a bottom-settle trim run now? Requires layout, the exact bottom of a
/// range that reaches the latest message, an overgrown window, nothing
/// pending, and a measured tail that clears the backfill strip (see
/// `trim_would_rearm_backfill`).
///
/// A settle at the bottom is the one moment a trim is invisible: removing rows
/// above the viewport shrinks `scrollHeight`, the browser clamps `scrollTop`
/// with it, and the same tail stays on the bottom edge. Anywhere else, with
/// scroll anchoring off, a trim shifts content under the reader, so growth
/// there is bounded by `WINDOW_ITEMS_CEILING` instead. Gated at
/// `SCROLL_TOP_SLACK_PX`: a trim from even a little above the bottom would
/// clamp the reader to the exact end, a visible yank.
#[cfg(target_arch = "wasm32")]
fn trim_is_due(container: &web_sys::Element, overgrown: bool, reader: &ReaderPosition) -> bool {
    if !overgrown
        || !has_layout(container)
        || reader.has_newer.get()
        || reader.hidden()
        || reader.request.get().is_some()
    {
        return false;
    }
    let distance = container.scroll_height() - container.scroll_top() - container.client_height();
    if distance > SCROLL_TOP_SLACK_PX {
        return false;
    }
    retained_tail_height(container, INITIAL_WINDOW_ITEMS)
        .is_some_and(|tail| !trim_would_rearm_backfill(tail, container.client_height()))
}

/// Listen for the history's scroll settling. Each settle remembers the reading
/// position, and a settle at the exact bottom trims a grown window.
///
/// Returns whether the listener was installed; `false` means the container was
/// not in the DOM yet and the caller should try again.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub(super) fn install_scroll_settle_listener(
    window_items: Signal<usize>,
    reader: Rc<ReaderPosition>,
) -> bool {
    use wasm_bindgen::prelude::*;

    let Some(container) = ROOM_HISTORY.container() else {
        return false;
    };

    // Passive throughout: none of these handlers call `preventDefault`, and the
    // jank this replaces (#151) came from doing work on the scroll path.
    let passive = web_sys::AddEventListenerOptions::new();
    passive.set_passive(true);

    // One DOM measurement per settle, not per scroll event.
    let settle = {
        let reader = reader.clone();
        Closure::wrap(Box::new(move || {
            let Some(container) = ROOM_HISTORY.container() else {
                return;
            };
            // Hidden, everything measures 0 and would read as "at the bottom".
            if !has_layout(&container) {
                return;
            }
            // Revealed but not restored yet: not the reader's position.
            if reader.hidden() {
                return;
            }
            // See `trim_is_due`. Any settle counts, ours included, so a touch
            // reader who only returns to the bottom through the
            // scroll-to-latest button still trims (#505).
            //
            // Deferred: this runs from a raw JS callback with no Dioxus scope,
            // and `window_items` is a signal the render subscribes to. See
            // .claude/rules/dioxus-signal-safety.md.
            if trim_is_due(&container, reader.window_overgrown.get(), &reader) {
                reader.window_overgrown.set(false);
                let reader = reader.clone();
                let epoch = reader.room_epoch.get();
                let mut window_items = window_items;
                crate::util::defer(move || {
                    // Re-check: the reader may have moved, switched rooms or hidden
                    // the panel since the settle.
                    if reader.room_epoch.get() != epoch {
                        return;
                    }
                    if !ROOM_HISTORY
                        .container()
                        .is_some_and(|c| trim_is_due(&c, true, &reader))
                    {
                        // Still overgrown; a later settle at the bottom may trim.
                        reader.window_overgrown.set(true);
                        return;
                    }
                    *reader.window_anchor.borrow_mut() = None;
                    window_items.set(INITIAL_WINDOW_ITEMS);
                });
            }
            // A reader who scrolled back to the end by hand is held there, as a
            // landed request would be, so the composer growing or the phone
            // keyboard opening keeps the newest message in view. Arrivals still
            // follow only by the follow rule: the hold ends on a newer newest.
            if reader.end_hold.get().is_none()
                && reader.request.get().is_none()
                && !reader.has_newer.get()
                && at_end_within_release_margin(&container)
            {
                hold_end(&reader, &container);
            } else {
                remember_reading_position(&reader);
                note_newest_seen(&reader);
            }
        }) as Box<dyn FnMut()>)
    };
    let settle_fn: js_sys::Function = settle.as_ref().unchecked_ref::<js_sys::Function>().clone();
    // Leaked deliberately, on the same reasoning as the IntersectionObserver
    // below: `Conversation` mounts once for the app's lifetime (rooms are
    // swapped by CSS, not by unmount) and `use_effect` has no cleanup hook, so
    // there is nothing to disconnect these from.
    settle.forget();

    // `scrollend` fires once, when the position has settled. Where it is
    // missing, a trailing debounce on `scroll` stands in; that listener still
    // measures nothing per event, it only resets a timer.
    let has_scrollend =
        js_sys::Reflect::has(&container, &JsValue::from_str("onscrollend")).unwrap_or(false);
    if has_scrollend {
        let cb = Closure::wrap(Box::new(move |_: web_sys::Event| {
            let _ = settle_fn.call0(&JsValue::NULL);
        }) as Box<dyn FnMut(web_sys::Event)>);
        let _ = container.add_event_listener_with_callback_and_add_event_listener_options(
            "scrollend",
            cb.as_ref().unchecked_ref(),
            &passive,
        );
        cb.forget();
    } else {
        let pending: Rc<std::cell::Cell<Option<i32>>> = Rc::new(std::cell::Cell::new(None));
        let cb = Closure::wrap(Box::new(move |_: web_sys::Event| {
            let Some(window) = web_sys::window() else {
                return;
            };
            if let Some(handle) = pending.take() {
                window.clear_timeout_with_handle(handle);
            }
            if let Ok(handle) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                &settle_fn,
                SCROLL_SETTLE_DEBOUNCE_MS,
            ) {
                pending.set(Some(handle));
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        let _ = container.add_event_listener_with_callback_and_add_event_listener_options(
            "scroll",
            cb.as_ref().unchecked_ref(),
            &passive,
        );
        cb.forget();
    }

    // Release the hold on an upward scroll more than 4px from the scroller's
    // absolute end. Accepted for now: the padding below the newest message
    // lets this release while that message is visible and Latest stays hidden.
    // Returns at once unless a hold is on, keeping the scroll path light (#151).
    let release_hold = Closure::wrap(Box::new(move |_: web_sys::Event| {
        let Some(held_top) = reader.end_hold.get() else {
            return;
        };
        let Some(container) = ROOM_HISTORY.container() else {
            return;
        };
        let top = container.scroll_top();
        // Only a move UP counts. A row growing leaves `scrollTop` alone (the
        // ResizeObserver then takes the view back to the end), and a clamp
        // when the history shrinks lands on the end.
        if top < held_top && !at_end_within_release_margin(&container) {
            reader.end_hold.set(None);
        } else {
            reader.end_hold.set(Some(top));
        }
    }) as Box<dyn FnMut(web_sys::Event)>);
    let _ = container.add_event_listener_with_callback_and_add_event_listener_options(
        "scroll",
        release_hold.as_ref().unchecked_ref(),
        &passive,
    );
    release_hold.forget();

    true
}

/// A size change of the history. The container's size is the history's only
/// notice that the chat panel was hidden (it measures 0 behind the mobile
/// Rooms or Members panel) or shown again. While the end hold is on, any size
/// change, of the container (the composer, the phone keyboard) or of
/// `#chat-content` (a row growing), takes the view back to the end. Otherwise
/// the browser keeps the top edge.
#[cfg(target_arch = "wasm32")]
fn on_history_resize(reader: &ReaderPosition, opening_snap_done: Signal<bool>) {
    let Some(container) = ROOM_HISTORY.container() else {
        return;
    };
    if !has_layout(&container) {
        reader.hidden.set(true);
        // The view the hold kept is gone.
        reader.end_hold.set(None);
        return;
    }
    // Revealed: finish a request made while hidden (a room opened behind the
    // panel), else put the reader back on their row.
    if reader.hidden.replace(false) {
        if !complete_scroll_request(reader, opening_snap_done) {
            let anchor = reader.anchor.borrow().clone();
            if let Some(anchor) = anchor {
                restore_reading_anchor(&anchor);
            }
            // The revealed height need not be the one captured.
            remember_reading_position(reader);
            note_newest_seen(reader);
        }
        return;
    }
    if reader.end_hold.get().is_some() {
        hold_end(reader, &container);
    }
}

/// Observe the history container and `#chat-content` with
/// [`on_history_resize`]. Returns whether the observer was installed; `false`
/// means the container was not in the DOM yet and the caller should try again.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub(super) fn observe_history_resize(
    reader: Rc<ReaderPosition>,
    opening_snap_done: Signal<bool>,
) -> bool {
    use wasm_bindgen::prelude::*;

    let Some(container) = ROOM_HISTORY.container() else {
        return false;
    };
    let cb = Closure::wrap(Box::new(move |_: js_sys::Array| {
        on_history_resize(&reader, opening_snap_done);
    }) as Box<dyn FnMut(js_sys::Array)>);
    let Ok(observer) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) else {
        return false;
    };
    observer.observe(&container);
    if let Some(content) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("chat-content"))
    {
        observer.observe(&content);
    }
    // Leaked for the same reason as the settle listener's closures.
    cb.forget();
    true
}

#[cfg(test)]
mod tests {
    use super::super::history_window::WINDOW_ITEMS_CEILING;
    use super::super::EventSummary;
    use super::*;
    use chrono::DateTime;

    fn events(count: usize) -> Vec<DisplayItem> {
        (0..count)
            .map(|i| {
                DisplayItem::Event(EventSummary {
                    names: vec![format!("member {i}")],
                    id: format!("e{i}"),
                    last_time: DateTime::from_timestamp(i as i64, 0).unwrap(),
                    last_sent_ms: i as i64 * 1_000,
                    last_message_id: MessageId(freenet_scaffold::util::fast_hash(
                        format!("e{i}").as_bytes(),
                    )),
                })
            })
            .collect()
    }

    /// A burst past the ceiling holds the range's end, unless a request is
    /// pending (a followed arrival, an own send): that lands on the latest
    /// range.
    #[test]
    fn a_pending_request_takes_the_latest_range_past_the_ceiling() {
        let all = events(INITIAL_WINDOW_ITEMS + WINDOW_ITEMS_CEILING);
        for (pending, expect_held) in [(false, true), (true, false)] {
            let reader = ReaderPosition::default();
            resolve_rendered_range(
                &all[..INITIAL_WINDOW_ITEMS],
                INITIAL_WINDOW_ITEMS,
                None,
                &reader,
            );
            if pending {
                reader.request_end(None);
            }
            let (window, _) = resolve_rendered_range(&all, INITIAL_WINDOW_ITEMS, None, &reader);
            assert_eq!(window.has_newer, expect_held, "pending request: {pending}");
            if pending {
                assert_eq!(
                    window.end,
                    all.len(),
                    "the latest range ends at the newest item"
                );
            }
        }
    }

    /// The newest message id of a render of `items`, as the resolver
    /// extracts it.
    fn newest(items: &[DisplayItem]) -> Option<MessageId> {
        items.last().and_then(display_item_newest).map(|key| key.id)
    }

    /// `items` with `extra` inserted above the last one.
    fn above_last(items: &[DisplayItem], extra: &DisplayItem) -> Vec<DisplayItem> {
        let (last, rest) = items.split_last().expect("a non-empty range");
        rest.iter().chain([extra, last]).cloned().collect()
    }

    /// Only a change of newest message ends the hold (a followed arrival lands
    /// a new request, which holds again). Rows that only change height
    /// (decryption, an image, an edit) leave the newest as it was and keep it.
    /// Deleting the newest ends it, though it is no arrival.
    #[test]
    fn the_end_hold_survives_only_an_unchanged_newest_message() {
        let all = events(20);
        let extra = &events(21)[20];
        let before = newest(&all[5..15]);
        for (now, survives, what) in [
            (all[5..16].to_vec(), false, "an append"),
            (
                all[6..16].to_vec(),
                false,
                "an append with an at-cap head drain",
            ),
            (all[0..15].to_vec(), true, "a backfill"),
            (
                above_last(&all[5..15], extra),
                true,
                "an insert above the newest",
            ),
            (
                above_last(&all[6..15], extra),
                true,
                "an insert above with a head drain in the same patch",
            ),
            (all[5..15].to_vec(), true, "an unchanged render"),
            (all[8..15].to_vec(), true, "a trim above"),
            ([&all[5..8], &all[9..15]].concat(), true, "a deletion above"),
            (all[5..14].to_vec(), false, "a deletion of the newest"),
        ] {
            assert_eq!(
                end_hold_survives(before.as_ref(), newest(&now).as_ref()),
                survives,
                "{what}"
            );
        }
    }

    /// A backfill restore or head reposition measured before a request must
    /// not run after it: it would put the reader back where the request took
    /// them from.
    #[test]
    fn requesting_the_end_drops_pending_corrections() {
        let reader = ReaderPosition::default();
        *reader.restore_after_patch.borrow_mut() = Some(ReadingAnchor {
            rows: vec![("a".to_string(), 10)],
            item_hint: 0,
        });

        reader.request_end(None);

        assert_eq!(reader.request.get(), Some(ScrollRequest { room: None }));
        assert!(
            reader.restore_after_patch.borrow().is_none(),
            "a backfill restore or head reposition survived the request"
        );
    }

    /// A deleted reading row falls back to its nearest surviving neighbour,
    /// the one above first (the order `capture_reading_anchor` records).
    #[test]
    fn the_reading_anchor_restores_through_its_neighbours() {
        let anchor = ReadingAnchor {
            rows: vec![
                ("read".into(), 40),
                ("above".into(), -30),
                ("below".into(), 120),
            ],
            item_hint: 0,
        };
        // Everything survives: the reading row goes back to 40px.
        let dom = |rows: &'static [(&'static str, i32)]| {
            move |k: &str| rows.iter().find(|(rk, _)| *rk == k).map(|(_, t)| *t)
        };
        assert_eq!(
            reading_anchor_shift(&anchor, dom(&[("read", -600), ("below", -520)])),
            Some(-640)
        );
        // The reading row was deleted: the row above is restored to ITS
        // offset, even though the row below survived too.
        assert_eq!(
            reading_anchor_shift(&anchor, dom(&[("below", 700), ("above", 10)])),
            Some(40)
        );
        // Nothing survives: no move at all.
        assert_eq!(reading_anchor_shift(&anchor, dom(&[("other", 0)])), None);
    }

    /// The reading row is the first starting inside the view, else the one
    /// the view's top cuts through: never a row starting below the view.
    #[test]
    fn the_reading_row_starts_inside_the_view_or_spans_its_top() {
        let (top, bottom) = (0.0, 600.0);
        // A message spanning the whole view, the next one below the fold.
        assert_eq!(
            reading_row_index(&[(-100.0, 900.0), (900.0, 1000.0)], top, bottom),
            Some(0)
        );
        // The first row starting inside the view, not the one cut above it.
        assert_eq!(
            reading_row_index(&[(-50.0, 40.0), (40.0, 120.0)], top, bottom),
            Some(1)
        );
        // Subpixel above the top still counts as starting inside.
        assert_eq!(
            reading_row_index(&[(-0.4, 80.0), (80.0, 160.0)], top, bottom),
            Some(0)
        );
        // Every row above the view: none is being read.
        assert_eq!(
            reading_row_index(&[(-200.0, -100.0), (-100.0, 0.0)], top, bottom),
            None
        );
    }
}

/// Source-grep pins for the scroll wiring in [`Conversation`].
///
/// The behaviour these guard is only observable in a browser, and some of it
/// in no browser CI runs (a `scrollend`-less engine) or only on a race the
/// suite cannot force, so these make a silent revert in a refactor fail at
/// `cargo test` rather than in the field. Everything else is measured by
/// `ui/tests/conversation-autoscroll.spec.ts` and
/// `conversation-history-position.spec.ts`.
#[cfg(test)]
mod autoscroll_wiring_pins {
    /// The production halves of `conversation.rs` (the render) and this file
    /// (the scroll machinery), each cut at its first test module.
    ///
    /// Cut by a needle that cannot match itself — it contains an escaped
    /// newline in the source, not a literal one. `rfind` would land on
    /// whichever test module was appended most recently and quietly put these
    /// needles inside the scanned text, making every assertion below
    /// self-satisfying (freenet/river#471).
    fn production_source() -> String {
        [
            ("conversation.rs", include_str!("../conversation.rs")),
            ("history_scroll.rs", include_str!("history_scroll.rs")),
        ]
        .into_iter()
        .map(|(file, source)| {
            &source[..source
                .find("#[cfg(test)]\nmod tests {")
                .unwrap_or_else(|| panic!("{file} should have a `#[cfg(test)] mod tests` block"))]
        })
        .collect()
    }

    fn dense_production_source() -> String {
        production_source()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    /// The settle listener remembers the reading position and trims at the
    /// bottom. Safari before 17.4 has no `scrollend`, so the settle is spotted
    /// by a trailing debounce on `scroll` instead. Every engine Playwright
    /// drives exposes `scrollend`, so that branch NEVER executes in CI and
    /// could be deleted or broken without a single test going red.
    #[test]
    fn the_settle_listener_has_a_debounced_fallback() {
        let prod = production_source();
        assert!(
            prod.contains("listening.set(install_scroll_settle_listener("),
            "`install_scroll_settle_listener` must actually be called from `Conversation`"
        );
        assert!(
            prod.contains("\"scrollend\""),
            "the settle is spotted once per scroll, via `scrollend`"
        );
        assert!(
            prod.contains("SCROLL_SETTLE_DEBOUNCE_MS") && prod.contains("\"scroll\""),
            "browsers without `scrollend` need the debounced `scroll` fallback; \
             nothing in the browser suite covers it, so it is pinned here"
        );
    }

    /// The scroll container opts out of browser scroll anchoring (see the
    /// comment at its `style`). With anchoring on, CI's WebKit, which anchors,
    /// could not see what iOS 26 and older do.
    #[test]
    fn the_scroll_container_disables_scroll_anchoring() {
        assert!(
            dense_production_source().contains(concat!("style:\"overflow-", "anchor:none;\"")),
            "#chat-scroll-container must set `overflow-anchor: none` — Safari \
             before 27 has no scroll anchoring, so the app's own corrections \
             must be the only ones"
        );
    }

    /// The #501/#505 windowing races the browser suite catches only when they
    /// fire, so these pins are their deterministic guard: the deferred trim
    /// re-checks the room, the backfill restore stands down while an explicit
    /// request is pending, and the backfill sentinel waits for the opening and
    /// for the room-switch reset to catch up.
    #[test]
    fn the_window_trims_at_the_bottom_and_backfill_waits_for_the_opening() {
        let dense = dense_production_source();
        assert!(
            dense.contains("ifreader.room_epoch.get()!=epoch{return;}"),
            "the deferred trim must re-check the room: the settle that \
             scheduled it is a task old, and the reader may have switched \
             rooms since"
        );
        // The capture is taken, and the guard returns, before the restore.
        let corrections = dense
            .split("fncorrect_view_after_patch(")
            .nth(1)
            .and_then(|body| body.split("restore_reading_anchor(&anchor);").next())
            .expect("correct_view_after_patch is missing");
        assert!(
            corrections.contains("letSome(anchor)=reader.restore_after_patch.borrow_mut().take()")
                && corrections.contains("ifreader.request.get().is_some(){return;}"),
            "the backfill restore (#501 H3) and the head reposition must stand \
             down while an explicit request is pending"
        );
        assert!(
            dense.contains(
                "ifhistory_window.has_older&&opening_snap_done()&&!room_changed_this_render"
            ),
            "the backfill sentinel must not mount until the room-open snap has \
             landed (#501 H2), including the one render where the previous \
             room's `opening_snap_done` is still true"
        );
    }

    /// Both #505 compensations capture the reading rows before the patch: the
    /// backfill sentinel before it grows the window, and the render when it
    /// swaps the head. The restore measures from the view's top, so neither
    /// the browser's clamp of `scrollTop` nor rows appended below the view in
    /// the same patch move it.
    #[test]
    fn head_swaps_reposition_the_reader() {
        let dense = dense_production_source();
        for guard in [
            "ifhead_removed&&reader_position.request.get().is_none(){",
            "ifevt.data().is_intersecting().unwrap_or(false){",
        ] {
            let body = dense
                .split(guard)
                .nth(1)
                .and_then(|after| after.split('}').next())
                .unwrap_or_else(|| panic!("`{guard}` is missing"));
            assert!(
                body.contains("reader_position.capture_before_patch();"),
                "the head swap and the backfill must each capture before the patch"
            );
        }
        assert_eq!(
            dense
                .matches("reader_position.capture_before_patch();")
                .count(),
            2,
            "one capture for the head swap and one for the backfill"
        );
        assert!(
            dense.contains("letview_top=container.get_bounding_client_rect().top();"),
            "the reading rows are measured from the view's top"
        );
    }
}
