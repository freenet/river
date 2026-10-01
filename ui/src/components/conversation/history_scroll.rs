//! Where the history's view goes: the reader's position is a message, not an offset.
//!
//! We remember the newest row that is visible (`data-anchor-row`) plus up to
//! `ANCHOR_FALLBACK_ROWS` above it, each with its `gap` (the container's bottom
//! edge minus the row's top edge). Layout changes put that row back at its gap;
//! they never re-measure what the reader meant.
//!
//! * **Pinned**: within `BOTTOM_THRESHOLD_PX` of the end, measured only in `capture`.
//! * **Capture** runs only from the `scroll` listener, and only for a reader's
//!   scroll. It sets the anchor, the pin, and the recorded layout signature and
//!   `scrollTop`. It also runs the window trim, which is why a trim needs no
//!   flag of its own: the rows it removes sit above the view.
//! * **Restore** runs on every layout or content change (the ResizeObserver, the
//!   content-change effect, and a `scroll` classified as layout). Forced or
//!   pinned, it goes to the bottom; otherwise it scrolls the first surviving
//!   anchor row back to its gap. It never changes the anchor or clears the pin.
//! * **The one heuristic** (`classify_scroll`): a `scroll` event is layout's (a
//!   browser clamp) if the recorded `scrollTop` is out of reach of the live
//!   scroll range and the view now sits at its end, whether or not anything was
//!   resized (removing a positioned overhang clamps with nothing resized); or if
//!   the layout signature changed since it was recorded AND `scrollTop` moved no
//!   more than `LAYOUT_SHIFT_ALLOWANCE_PX`, for a clamp taken during a shorter
//!   intermediate layout that ends short of the end. Geometry, not provenance.
//!   Residuals: a reader who moves less than the allowance in the very frame a
//!   layout change lands loses that frame's movement (the pin can't latch on it,
//!   since the next scroll event captures); an intermediate clamp larger than
//!   the allowance is taken as the reader; and a reader who scrolls to the end in
//!   the frame of a final-end clamp is indistinguishable from it.
//!   The signature records the layout's shape (content and container sizes); the
//!   scroll range is always read live. It can still be stale when an event is
//!   classified: ResizeObserver delivery is asynchronous, a hidden history
//!   records nothing, and the sizes read are client sizes, not exactly the boxes
//!   the observer watches.
//! * **Late scroll events**: a `scroll` event arrives a frame after the scroll,
//!   and a content change can land first. So `restore`, and the render before a
//!   patch, first read a pending reader scroll (`take_in_undelivered_scroll`,
//!   through `on_scroll`), or a stale pin would drag the reader back down, or an
//!   anchor would be measured after the patch had moved the rows.
//! * **Why this can't latch as #486 did**: the pin comes only from the reader's
//!   own positions. Growing content, a growing composer or a rewrap restore
//!   instead of measuring, so none of them can clear it.
//!
//! * **Hidden**: the mobile layout hides the history (`display:none`), and every
//!   read is then 0. It stays observed, but nothing measures, records or
//!   restores it until it has height again: the ResizeObserver's restore on
//!   reveal picks up where it was.
//!
//! Known limit: the scroll-to-latest button scrolls smoothly, so captures during
//! its animation read as the reader's, and a message arriving mid-animation
//! lands one row short until the next change.
//!
//! State is `Cell`/`RefCell`, never signals: raw JS callbacks write it.

// Only the wasm build drives the DOM half; natively the pure half is exercised by
// the unit tests.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use super::{WindowAnchor, BOTTOM_THRESHOLD_PX, SCROLL_TOP_SLACK_PX};
use dioxus::prelude::Signal;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[cfg(target_arch = "wasm32")]
use super::{trim_would_rearm_backfill, INITIAL_WINDOW_ITEMS};
#[cfg(target_arch = "wasm32")]
use dioxus::prelude::WritableExt;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{prelude::*, JsCast};

/// Trailing delay that stands in for `scrollend` where the browser has none
/// (Safari before 17.4), as on `main`.
#[cfg(target_arch = "wasm32")]
const SCROLL_SETTLE_DEBOUNCE_MS: i32 = 120;

/// How many rows above the newest visible one are remembered as fallbacks, for
/// when the anchor row itself is deleted or windowed out before the restore.
const ANCHOR_FALLBACK_ROWS: usize = 4;

/// The most a `scroll` event may move `scrollTop`, after the layout signature
/// changed, and still be read as the browser's clamp rather than the reader.
///
/// Only needed for a clamp that does NOT end at the final end: one taken during
/// an intermediate, shorter layout before the history came out taller. A clamp
/// to the final end is recognized at any size, by the final-end clause of
/// `classify_scroll`. The 8px (Linux CI) and 56px clamps quoted for this before
/// came from other trees with CSS size containers, and the 111px one from a
/// synthetic test, so they are context rather than measurements of this code.
/// What this costs either way (a reader's small move in the same frame taken as
/// layout, a larger intermediate clamp taken as the reader) is in the module doc.
const LAYOUT_SHIFT_ALLOWANCE_PX: i32 = 200;

/// The shape the history is laid out in: the content's height, the container's
/// size, and the width the content wraps at.
///
/// Deliberately not the scroll range (`scrollHeight`), which is read live: an
/// absolutely positioned popover changes the range without resizing any box the
/// ResizeObserver watches, so a recorded range goes stale with nothing to
/// refresh it.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
struct LayoutSig {
    content_height: i32,
    client_height: i32,
    client_width: i32,
    content_width: i32,
}

/// Who a `scroll` event belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScrollCause {
    /// Nothing moved or resized since the record: no unaccounted movement. Most
    /// often our own write's event, but that is not something it can prove.
    Echo,
    /// The reader moved the view: it becomes the new anchor.
    Reader,
    /// A layout change moved it (a browser clamp): put the anchor back.
    Layout,
}

/// The view's two edges in the content: `scrollTop`, and `scrollTop` plus the
/// container's height.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ViewEdges {
    top: i32,
    bottom: i32,
}

/// Whether the view is up past rounding from `from`, where the gesture started.
/// Measured against the start, never the previous frame, so slow 1-2px frames
/// add up. BOTH edges, as `main`'s `reader_moved_up_since` (#722): a composer
/// collapsing grows the container and clamps `scrollTop` up while the bottom
/// edge stays where it was, and that is not the reader looking back.
fn moved_up(from: ViewEdges, now: ViewEdges) -> bool {
    now.top < from.top - SCROLL_TOP_SLACK_PX && now.bottom < from.bottom - SCROLL_TOP_SLACK_PX
}

/// Whether `top` is within rounding of the live end `max`.
fn at_end(top: i32, max: i32) -> bool {
    max - top <= SCROLL_TOP_SLACK_PX
}

/// What a reader-classified frame means while the scroll-to-latest animation
/// is running.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SeekStep {
    /// Still on its way down (or stalled within rounding).
    Travelling,
    /// At the live end.
    Arrived,
    /// Up past rounding from the previous frame. The animation only moves
    /// down, so that frame is the reader's.
    TakenOver,
}

fn seek_step(prev_top: i32, top: i32, max: i32) -> SeekStep {
    if at_end(top, max) {
        SeekStep::Arrived
    } else if top < prev_top - SCROLL_TOP_SLACK_PX {
        SeekStep::TakenOver
    } else {
        SeekStep::Travelling
    }
}

/// Indices of up to `n` rows, newest first, starting at the newest row that
/// intersects the viewport `[view_top, view_bottom]`. The rows above it are
/// fallbacks and need not be visible. Empty if no row is visible.
///
/// There are `len` rows in document order and `rect(i)` gives row `i`'s
/// `(top, bottom)`, so their tops are monotonic and the newest row starting above
/// the viewport's bottom edge is a binary search away. `rect` is a closure, not a
/// slice, because each call is a layout read in production: this makes O(log n)
/// of them plus one for the newest row's bottom edge. Touching an edge is not
/// intersecting it.
fn newest_visible_rows(
    len: usize,
    rect: impl Fn(usize) -> (i32, i32),
    view_top: i32,
    view_bottom: i32,
    n: usize,
) -> Vec<usize> {
    // The first row whose top is not above the viewport's bottom edge.
    let (mut lo, mut hi) = (0, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if rect(mid).0 < view_bottom {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let Some(newest) = lo.checked_sub(1) else {
        return Vec::new();
    };
    if rect(newest).1 <= view_top {
        return Vec::new();
    }
    (0..=newest).rev().take(n).collect()
}

/// Read a `scroll` event as an echo, a layout change's doing or the reader's.
/// `max` is the live scroll range (`max_scroll_top`), never a recorded one.
///
/// Echo first: the signature and `scrollTop` are both as recorded.
///
/// Otherwise layout if either:
///
/// * **Final-end clamp**: the recorded `scrollTop` is now out of reach (past
///   `max` by more than rounding) and the view sits at `max` (within rounding,
///   either side). Only the browser's clamp puts it there, whether or not the
///   signature changed: removing overflow clamps without resizing anything.
/// * **Changed layout, small move**: the signature changed since it was
///   recorded AND `scrollTop` moved no more than `LAYOUT_SHIFT_ALLOWANCE_PX`.
///   This is what catches an intermediate clamp that ends short of the end.
///
/// A geometry heuristic, not provenance: see the module doc for what each
/// clause gets wrong.
fn classify_scroll(
    recorded: LayoutSig,
    now: LayoutSig,
    recorded_top: i32,
    now_top: i32,
    max: i32,
) -> ScrollCause {
    if recorded == now && recorded_top == now_top {
        return ScrollCause::Echo;
    }
    let clamped_to_end =
        recorded_top > max + SCROLL_TOP_SLACK_PX && (now_top - max).abs() <= SCROLL_TOP_SLACK_PX;
    let small_layout_move =
        recorded != now && (now_top - recorded_top).abs() <= LAYOUT_SHIFT_ALLOWANCE_PX;
    if clamped_to_end || small_layout_move {
        ScrollCause::Layout
    } else {
        ScrollCause::Reader
    }
}

/// Whether a reader `distance_from_bottom` px above the end is still following.
fn is_pinned(distance_from_bottom: f64) -> bool {
    distance_from_bottom <= BOTTOM_THRESHOLD_PX
}

/// What the history needs from the component: the trim's window state (the
/// window reset at the bottom), and who to tell when a restore snapped.
#[derive(Clone)]
pub(super) struct HistoryHooks {
    pub window_items: Signal<usize>,
    pub window_anchor: Rc<RefCell<Option<WindowAnchor>>>,
    pub window_overgrown: Rc<Cell<bool>>,
    pub window_rendered: Rc<Cell<usize>>,
    /// Called from raw JS callbacks too, so it may only defer signal work.
    pub snapped_to_bottom: Rc<dyn Fn()>,
}

/// What drives the view between reader scrolls.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Follow {
    /// The pin and the anchor drive restores.
    #[default]
    Free,
    /// The scroll-to-latest animation is travelling to the end.
    Seeking,
    /// A reader gesture moved up from where it started: restores keep the
    /// anchor, never snap, until it settles.
    Held,
}

/// The history's scroll state. See the module doc.
pub(super) struct HistoryScroll {
    /// Newest visible row first, each with its gap. Empty until a reader scrolls.
    anchor: RefCell<Vec<(String, i32)>>,
    pinned: Cell<bool>,
    /// Set by a room switch or the reader's own send: the next restore goes to the
    /// bottom whatever the pin says.
    force: Cell<bool>,
    /// The layout and `scrollTop` as last accounted for, to classify a `scroll`.
    sig: Cell<LayoutSig>,
    top: Cell<i32>,
    follow: Cell<Follow>,
    /// The view's edges before the current reader gesture's first move, shifted
    /// by every layout correction since; `None` outside a gesture.
    gesture_from: Cell<Option<ViewEdges>>,
    /// A gesture ended while the history was hidden; finish it on the first
    /// laid-out restore.
    settle_pending: Cell<bool>,
    /// The debounce standing in for `scrollend` where the browser lacks it.
    settle_timer: Cell<Option<i32>>,
    /// Set by `install` once the listeners exist, so `is_some` also means
    /// "installed".
    hooks: RefCell<Option<HistoryHooks>>,
}

impl Default for HistoryScroll {
    fn default() -> Self {
        Self {
            anchor: RefCell::new(Vec::new()),
            pinned: Cell::new(true),
            force: Cell::new(false),
            sig: Cell::new(LayoutSig::default()),
            top: Cell::new(0),
            follow: Cell::new(Follow::Free),
            gesture_from: Cell::new(None),
            settle_pending: Cell::new(false),
            settle_timer: Cell::new(None),
            hooks: RefCell::new(None),
        }
    }
}

impl HistoryScroll {
    /// Make the next restore go to the bottom.
    pub(super) fn force_next(&self) {
        self.force.set(true);
    }

    /// A new room opens at its newest message: forget the old room's position.
    pub(super) fn reset_for_room(&self) {
        self.anchor.borrow_mut().clear();
        self.pinned.set(true);
        self.force.set(true);
        self.sig.set(LayoutSig::default());
        self.top.set(0);
        self.end_interaction();
    }

    /// Forget any seek or gesture in flight, and any settle still to come for
    /// it: a room switch, a forced snap or a new seek supersedes them.
    fn end_interaction(&self) {
        self.follow.set(Follow::Free);
        self.gesture_from.set(None);
        self.settle_pending.set(false);
        #[cfg(target_arch = "wasm32")]
        if let (Some(handle), Some(window)) = (self.settle_timer.take(), web_sys::window()) {
            window.clear_timeout_with_handle(handle);
        }
    }
}

/// The history's scroll container, if it has a layout box to measure. Hidden,
/// every read is 0, which must not be taken for where the reader is.
#[cfg(target_arch = "wasm32")]
fn laid_out_container() -> Option<web_sys::Element> {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("chat-scroll-container"))
        .filter(|c| c.client_height() > 0)
}

/// Read the wrapper the history's rows are laid out in.
#[cfg(target_arch = "wasm32")]
fn chat_content_wrapper() -> Option<web_sys::Element> {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("chat-content"))
}

#[cfg(target_arch = "wasm32")]
const ANCHOR_ROWS: &str = "#chat-content [data-anchor-row]";
#[cfg(target_arch = "wasm32")]
const ANCHOR_ATTR: &str = "data-anchor-row";

#[cfg(target_arch = "wasm32")]
fn read_sig(container: &web_sys::Element) -> LayoutSig {
    let (content_height, content_width) = chat_content_wrapper().map_or((0, 0), |content| {
        (content.client_height(), content.client_width())
    });
    LayoutSig {
        content_height,
        client_height: container.client_height(),
        client_width: container.client_width(),
        content_width,
    }
}

/// The furthest `scrollTop` can go right now, read live. Never negative.
#[cfg(target_arch = "wasm32")]
fn max_scroll_top(container: &web_sys::Element) -> i32 {
    (container.scroll_height() - container.client_height()).max(0)
}

/// How far above the container's bottom edge `row`'s top edge sits.
#[cfg(target_arch = "wasm32")]
fn gap(view: &web_sys::DomRect, row: &web_sys::Element) -> i32 {
    (view.bottom() - row.get_bounding_client_rect().top()).round() as i32
}

/// `CSS.escape(value)`, through `Reflect` because web-sys's `Css` feature is not
/// enabled. `None` if the browser has no `CSS.escape`.
#[cfg(target_arch = "wasm32")]
fn css_escape(value: &str) -> Option<String> {
    let css = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("CSS")).ok()?;
    let escape: js_sys::Function = js_sys::Reflect::get(&css, &JsValue::from_str("escape"))
        .ok()?
        .dyn_into()
        .ok()?;
    escape
        .call1(&css, &JsValue::from_str(value))
        .ok()?
        .as_string()
}

#[cfg(target_arch = "wasm32")]
impl HistoryScroll {
    /// The view's edges as last recorded.
    fn recorded_edges(&self) -> ViewEdges {
        let top = self.top.get();
        ViewEdges {
            top,
            bottom: top + self.sig.get().client_height,
        }
    }

    /// Record the layout and `scrollTop` as they are now. Read back after any write,
    /// since the browser clamps.
    fn record(&self, container: &web_sys::Element) {
        self.sig.set(read_sig(container));
        self.top.set(container.scroll_top());
    }

    /// Take the reader's position as the new truth: for a scroll the reader made,
    /// and for a touch that stops the scroll-to-latest animation.
    fn capture(&self) {
        let Some(container) = laid_out_container() else {
            return;
        };
        let sig = read_sig(&container);
        let top = container.scroll_top();
        let distance = (max_scroll_top(&container) - top) as f64;
        self.pinned.set(is_pinned(distance));
        if let Ok(list) = container.query_selector_all(ANCHOR_ROWS) {
            // Relative to the container, the frame `newest_visible_rows` works in.
            // Each call is a layout read, and the search makes few of them.
            let view = container.get_bounding_client_rect();
            let view_bottom = (view.bottom() - view.top()).round() as i32;
            let item = |i: usize| {
                list.item(i as u32)
                    .unwrap_throw()
                    .unchecked_into::<web_sys::Element>()
            };
            let rect = |i: usize| {
                let r = item(i).get_bounding_client_rect();
                (
                    (r.top() - view.top()).round() as i32,
                    (r.bottom() - view.top()).round() as i32,
                )
            };
            let picked = newest_visible_rows(
                list.length() as usize,
                rect,
                0,
                view_bottom,
                ANCHOR_FALLBACK_ROWS + 1,
            );
            *self.anchor.borrow_mut() = picked
                .into_iter()
                .filter_map(|i| {
                    let row = item(i);
                    Some((row.get_attribute(ANCHOR_ATTR)?, gap(&view, &row)))
                })
                .collect();
        }
        self.sig.set(sig);
        self.top.set(top);
        self.trim_at_bottom(&container);
    }

    /// A view landing AT the bottom is the ONE moment a window trim is provably
    /// invisible: the rows it removes are above the view, so the browser's clamp
    /// keeps the same tail glued to the bottom edge (and the ResizeObserver's
    /// restore keeps a pinned reader there). Gated at SCROLL_TOP_SLACK_PX, not
    /// BOTTOM_THRESHOLD_PX: a reader parked 100px up still counts as pinned, and
    /// a trim from there would yank them to the exact bottom. Skipped when the
    /// trimmed tail would leave the backfill sentinel in range of the bottom, or
    /// the two oscillate at render speed (#505; see `trim_would_rearm_backfill`).
    ///
    /// Runs from a reader's capture, from an echo (a follower's snap), and from
    /// the scroll-to-latest animation arriving.
    fn trim_at_bottom(&self, container: &web_sys::Element) {
        let distance = (max_scroll_top(container) - container.scroll_top()) as f64;
        if let Some(trim) = self.hooks.borrow().as_ref() {
            if distance <= SCROLL_TOP_SLACK_PX as f64
                && trim.window_overgrown.get()
                && !trim_would_rearm_backfill(
                    container.scroll_height(),
                    container.client_height(),
                    trim.window_rendered.get(),
                    INITIAL_WINDOW_ITEMS,
                )
            {
                trim.window_overgrown.set(false);
                let window_anchor = trim.window_anchor.clone();
                let mut window_items = trim.window_items;
                // Deferred: this runs from a raw JS callback with no Dioxus scope,
                // and `window_items` is a signal the render subscribes to. See
                // .claude/rules/dioxus-signal-safety.md.
                crate::util::defer(move || {
                    *window_anchor.borrow_mut() = None;
                    window_items.set(INITIAL_WINDOW_ITEMS);
                });
            }
        }
    }

    /// Put the view where it belongs after a layout or content change.
    pub(super) fn restore(&self) {
        self.take_in_undelivered_scroll();
        self.restore_now()
    }

    /// A `scroll` event is queued for the next frame, and a content change can
    /// land first. If the view has moved by more than a layout clamp could since
    /// it was recorded, that is the reader's scroll and its event is still on its
    /// way: read it now, or the pin is stale and the restore drags them back.
    pub(super) fn take_in_undelivered_scroll(&self) {
        let Some(container) = laid_out_container() else {
            return;
        };
        let top = container.scroll_top();
        if top == self.top.get() {
            return;
        }
        if self.cause_now(&container) == ScrollCause::Reader {
            self.on_scroll();
        }
    }

    /// Who the `scroll` event now pending (or being handled) belongs to.
    fn cause_now(&self, container: &web_sys::Element) -> ScrollCause {
        classify_scroll(
            self.sig.get(),
            read_sig(container),
            self.top.get(),
            container.scroll_top(),
            max_scroll_top(container),
        )
    }

    fn restore_now(&self) {
        // Hidden: keep the pin, the anchor and any pending `force` for the reveal.
        // A hide also ends whatever gesture was going on, and an engine sends no
        // `scrollend` for a scroll a hide cut short, so its settle waits for the
        // reveal (a no-op if there was no gesture).
        let Some(container) = laid_out_container() else {
            self.settle_pending.set(true);
            return;
        };
        let before = self.recorded_edges();
        if self.force.take() {
            self.end_interaction();
            self.snap_and_tell();
            return;
        }
        match self.follow.get() {
            Follow::Seeking => self.seek(&container),
            Follow::Free if self.pinned.get() => self.snap_and_tell(),
            Follow::Free | Follow::Held => {
                self.restore_anchor(&container);
                self.record(&container);
            }
        }
        // Everything that moved the view since the last record was layout (a
        // pending reader move was taken in first), so it is not the gesture's.
        if let Some(from) = self.gesture_from.get() {
            let after = self.recorded_edges();
            self.gesture_from.set(Some(ViewEdges {
                top: from.top + after.top - before.top,
                bottom: from.bottom + after.bottom - before.bottom,
            }));
        }
        if self.settle_pending.take() {
            self.end_gesture();
        }
    }

    /// Snap to the bottom at once, and tell the component it happened (the
    /// opening snap's completion, with its own room check).
    fn snap_and_tell(&self) {
        self.snap_to_bottom(web_sys::ScrollBehavior::Instant);
        let snapped = self
            .hooks
            .borrow()
            .as_ref()
            .map(|h| h.snapped_to_bottom.clone());
        if let Some(snapped) = snapped {
            snapped();
        }
    }

    /// Aim the scroll-to-latest animation at the live end, or finish it when it is
    /// already there. Re-issuing it retargets an animation in flight and restarts
    /// one a hide cut short. Never an opening snap: nothing is told.
    fn seek(&self, container: &web_sys::Element) {
        if at_end(container.scroll_top(), max_scroll_top(container)) {
            self.follow.set(Follow::Free);
        } else {
            let opts = web_sys::ScrollToOptions::new();
            opts.set_top(container.scroll_height() as f64);
            opts.set_behavior(web_sys::ScrollBehavior::Smooth);
            container.scroll_to_with_scroll_to_options(&opts);
        }
        self.record(container);
    }

    /// Scroll the first anchor row that still exists back to its gap. If none
    /// survives, leave the view alone: the next reader scroll captures a new one.
    fn restore_anchor(&self, container: &web_sys::Element) {
        let view = container.get_bounding_client_rect();
        // Newest first, so the first row found is the newest survivor.
        for (key, saved_gap) in self.anchor.borrow().iter() {
            let Some(key) = css_escape(key) else {
                continue;
            };
            let selector = format!("#chat-content [{ANCHOR_ATTR}=\"{key}\"]");
            let Ok(Some(row)) = container.query_selector(&selector) else {
                continue;
            };
            let delta = saved_gap - gap(&view, &row);
            if delta.abs() > SCROLL_TOP_SLACK_PX {
                container.set_scroll_top(container.scroll_top() + delta);
            }
            return;
        }
    }

    /// Read a `scroll` event as layout's doing (restore) or the reader's (capture).
    fn on_scroll(&self) {
        let Some(container) = laid_out_container() else {
            return;
        };
        match self.cause_now(&container) {
            // Nothing to account for, so the reader's intent stays as it was.
            // Still a bottom: a follower's snap trims here.
            ScrollCause::Echo => self.trim_at_bottom(&container),
            ScrollCause::Layout => self.restore_now(),
            ScrollCause::Reader => self.on_reader_scroll(&container),
        }
    }

    /// A scroll nothing else accounts for. During the scroll-to-latest animation
    /// that is mostly the animation itself, which must not unpin; otherwise it
    /// is the reader.
    fn on_reader_scroll(&self, container: &web_sys::Element) {
        if self.follow.get() != Follow::Seeking {
            let from = self
                .gesture_from
                .get()
                .unwrap_or_else(|| self.recorded_edges());
            self.gesture_from.set(Some(from));
            self.capture();
            if at_end(container.scroll_top(), max_scroll_top(container)) {
                // Back at the end: following again, whatever came before.
                self.follow.set(Follow::Free);
                self.gesture_from.set(None);
            } else if moved_up(from, self.recorded_edges()) {
                self.follow.set(Follow::Held);
            }
            return;
        }
        match seek_step(
            self.top.get(),
            container.scroll_top(),
            max_scroll_top(container),
        ) {
            SeekStep::Travelling => self.record(container),
            SeekStep::Arrived => {
                self.follow.set(Follow::Free);
                self.record(container);
                self.trim_at_bottom(container);
            }
            SeekStep::TakenOver => {
                // The reader's gesture, started where the last frame was.
                self.follow.set(Follow::Held);
                self.gesture_from.set(Some(self.recorded_edges()));
                self.capture();
            }
        }
    }

    /// A scroll has come to rest (`scrollend`, or the debounce standing in for
    /// it). Finishes or re-aims a seek whose animation ended short of the end,
    /// and ends a reader gesture. Nothing else: a stale `scrollend` (another
    /// room's, an old gesture's) finds no gesture and does nothing.
    fn settle(&self) {
        self.settle_timer.set(None);
        if self.follow.get() == Follow::Seeking {
            self.restore_now();
            return;
        }
        if self.gesture_from.get().is_none() {
            return;
        }
        if laid_out_container().is_none() {
            self.settle_pending.set(true);
            return;
        }
        self.end_gesture();
    }

    /// The gesture is over: where it came to rest decides the pin, as on `main`.
    /// Measured directly, never through `on_scroll`, because the position has
    /// usually been recorded already and would read as an echo. Does not snap.
    fn end_gesture(&self) {
        if self.follow.get() == Follow::Seeking || self.gesture_from.get().is_none() {
            return;
        }
        self.follow.set(Follow::Free);
        self.gesture_from.set(None);
        self.capture();
    }

    /// A finger on the history stops the scroll-to-latest animation where it is:
    /// that is where the reader now is. Captured directly, not through `on_scroll`,
    /// because nothing may have moved since the last frame was recorded.
    fn on_touch_start(&self) {
        if self.follow.get() != Follow::Seeking || laid_out_container().is_none() {
            return;
        }
        self.follow.set(Follow::Free);
        self.capture();
    }

    /// Scroll to the newest message and re-arm the pin: asking for it is the
    /// clearest statement of intent there is.
    ///
    /// Scrolls the container itself, not the last bubble: `scrollIntoView` aligns
    /// the bubble's top to the container's top and can leave the real bottom
    /// (reactions, sentinel, padding) off-screen, which on a refresh scrolled
    /// only ~70% of the way down.
    pub(super) fn snap_to_bottom(&self, behavior: web_sys::ScrollBehavior) {
        let Some(container) = laid_out_container() else {
            return;
        };
        self.pinned.set(true);
        if behavior == web_sys::ScrollBehavior::Smooth {
            // The animation's frames are not the reader: see `on_reader_scroll`.
            self.end_interaction();
            self.follow.set(Follow::Seeking);
            self.seek(&container);
            return;
        }
        let opts = web_sys::ScrollToOptions::new();
        opts.set_top(container.scroll_height() as f64);
        opts.set_behavior(behavior);
        container.scroll_to_with_scroll_to_options(&opts);
        self.record(&container);
    }

    /// Listen for the reader's scrolls and for layout changes. Idempotent: a
    /// no-op once installed; before the history is in the DOM it does nothing and
    /// the next call retries.
    ///
    /// A hidden history is installed too, so that its reveal is observed; only
    /// its geometry is left unrecorded.
    pub(super) fn install(self: &Rc<Self>, hooks: HistoryHooks) {
        if self.hooks.borrow().is_some() {
            return;
        }
        let container = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("chat-scroll-container"));
        let (Some(container), Some(content)) = (container, chat_content_wrapper()) else {
            return;
        };

        // The ResizeObserver sees the content growing or reflowing, and the
        // container shrinking (the composer growing, #486's third cause), and
        // a hidden container getting its height back. All restore; none may
        // capture.
        let on_resize = {
            let this = self.clone();
            Closure::wrap(Box::new(move |_: js_sys::Array| {
                this.restore();
            }) as Box<dyn FnMut(js_sys::Array)>)
        };
        let Ok(observer) = web_sys::ResizeObserver::new(on_resize.as_ref().unchecked_ref()) else {
            return;
        };
        if let Some(laid_out) = laid_out_container() {
            self.record(&laid_out);
        }
        *self.hooks.borrow_mut() = Some(hooks);
        observer.observe(&content);
        observer.observe(&container);

        // Passive: nothing here calls `preventDefault`, and the jank this
        // replaced (#151) came from work on the scroll path.
        let passive = web_sys::AddEventListenerOptions::new();
        passive.set_passive(true);
        let on_scroll = {
            let this = self.clone();
            Closure::wrap(Box::new(move |_: web_sys::Event| this.on_scroll())
                as Box<dyn FnMut(web_sys::Event)>)
        };
        let _ = container.add_event_listener_with_callback_and_add_event_listener_options(
            "scroll",
            on_scroll.as_ref().unchecked_ref(),
            &passive,
        );

        // `scrollend` fires once the position has come to rest. Where it is
        // missing, a trailing debounce on `scroll` stands in, as on `main`; it
        // cannot tell a paused finger from a lifted one.
        let settle = {
            let this = self.clone();
            Closure::wrap(Box::new(move || this.settle()) as Box<dyn FnMut()>)
        };
        let has_scrollend =
            js_sys::Reflect::has(&container, &JsValue::from_str("onscrollend")).unwrap_or(false);
        let on_settle_signal = if has_scrollend {
            let settle_fn: js_sys::Function =
                settle.as_ref().unchecked_ref::<js_sys::Function>().clone();
            let cb = Closure::wrap(Box::new(move |_: web_sys::Event| {
                let _ = settle_fn.call0(&JsValue::NULL);
            }) as Box<dyn FnMut(web_sys::Event)>);
            let _ = container.add_event_listener_with_callback_and_add_event_listener_options(
                "scrollend",
                cb.as_ref().unchecked_ref(),
                &passive,
            );
            cb
        } else {
            let this = self.clone();
            let settle_fn: js_sys::Function =
                settle.as_ref().unchecked_ref::<js_sys::Function>().clone();
            let cb = Closure::wrap(Box::new(move |_: web_sys::Event| {
                let Some(window) = web_sys::window() else {
                    return;
                };
                if let Some(handle) = this.settle_timer.take() {
                    window.clear_timeout_with_handle(handle);
                }
                if let Ok(handle) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                    &settle_fn,
                    SCROLL_SETTLE_DEBOUNCE_MS,
                ) {
                    this.settle_timer.set(Some(handle));
                }
            }) as Box<dyn FnMut(web_sys::Event)>);
            let _ = container.add_event_listener_with_callback_and_add_event_listener_options(
                "scroll",
                cb.as_ref().unchecked_ref(),
                &passive,
            );
            cb
        };

        let on_touch_start = {
            let this = self.clone();
            Closure::wrap(Box::new(move |_: web_sys::Event| this.on_touch_start())
                as Box<dyn FnMut(web_sys::Event)>)
        };
        let _ = container.add_event_listener_with_callback_and_add_event_listener_options(
            "touchstart",
            on_touch_start.as_ref().unchecked_ref(),
            &passive,
        );

        // Leaked deliberately: `Conversation` mounts once for the app's lifetime
        // (rooms are swapped by CSS, not by unmount) and `use_effect` has no
        // cleanup hook, so there is nothing to disconnect these from.
        on_resize.forget();
        on_scroll.forget();
        settle.forget();
        on_settle_signal.forget();
        on_touch_start.forget();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: (i32, i32) = (0, 500);

    /// `newest_visible_rows` over an array: the closure is all it gets to see.
    fn newest(rows: &[(i32, i32)], view_top: i32, view_bottom: i32, n: usize) -> Vec<usize> {
        newest_visible_rows(rows.len(), |i| rows[i], view_top, view_bottom, n)
    }

    fn sig(content_height: i32, client_height: i32, client_width: i32) -> LayoutSig {
        LayoutSig {
            content_height,
            client_height,
            client_width,
            content_width: client_width,
        }
    }

    #[test]
    fn newest_visible_rows_picks_the_last_row_intersecting_the_viewport() {
        // Rows straddling the top edge, inside, and straddling the bottom edge.
        let rows = [
            (-150, -50),
            (-50, 100),
            (100, 300),
            (300, 450),
            (450, 650),
            (650, 800),
        ];
        assert_eq!(newest(&rows, VIEW.0, VIEW.1, 1), vec![4]);

        // The newest row ends inside the view, so the ones below it are off screen.
        assert_eq!(newest(&rows[..4], VIEW.0, VIEW.1, 1), vec![3]);

        // Only a row straddling the top edge is visible.
        let above = [(-300, -200), (-200, 20), (600, 700)];
        assert_eq!(newest(&above, VIEW.0, VIEW.1, 1), vec![1]);

        // A row taller than the viewport, covering both edges.
        let tall = [(-100, 900)];
        assert_eq!(newest(&tall, VIEW.0, VIEW.1, 1), vec![0]);

        assert_eq!(newest(&[], VIEW.0, VIEW.1, 1), Vec::<usize>::new());

        // Touching an edge is not intersecting it; one pixel of overlap is.
        assert!(newest(&[(-100, 0)], 0, 500, 1).is_empty());
        assert_eq!(newest(&[(0, 100), (500, 600)], 0, 500, 1), vec![0]);
        assert_eq!(newest(&[(-100, 1)], 0, 500, 1), vec![0]);
        assert_eq!(newest(&[(499, 600)], 0, 500, 1), vec![0]);
    }

    #[test]
    fn newest_visible_rows_returns_up_to_n_fallbacks_above_it() {
        let rows = [
            (0, 100),
            (100, 200),
            (200, 300),
            (300, 400),
            (400, 500),
            (500, 600),
        ];
        // Row 4 is the newest visible one; the n rows are it and those above it.
        assert_eq!(newest(&rows, 0, 450, 3), vec![4, 3, 2]);
        // Fewer rows exist than were asked for.
        assert_eq!(newest(&rows[..2], 0, 450, 5), vec![1, 0]);
        // The fallbacks need not be visible themselves.
        assert_eq!(newest(&rows, 250, 450, 4), vec![4, 3, 2, 1]);
        assert_eq!(newest(&rows, 0, 450, 0), Vec::<usize>::new());
    }

    #[test]
    fn newest_visible_rows_reads_only_a_handful_of_rects() {
        // Capture runs on every scroll event, and each rect is a layout read.
        let rows: Vec<(i32, i32)> = (0..10_000).map(|i| (i * 100, i * 100 + 90)).collect();
        let reads = Cell::new(0usize);
        let picked = newest_visible_rows(
            rows.len(),
            |i| {
                reads.set(reads.get() + 1);
                rows[i]
            },
            300_000,
            300_500,
            ANCHOR_FALLBACK_ROWS + 1,
        );
        assert_eq!(picked, vec![3_004, 3_003, 3_002, 3_001, 3_000]);
        // log2(10_000) is about 14: the search, plus the one bottom-edge read.
        assert!(reads.get() <= 16, "read {} rects", reads.get());
    }

    /// A live scroll maximum the recorded top of 2000 can still reach.
    const REACHABLE_MAX: i32 = 2400;

    #[test]
    fn a_change_of_the_wrap_width_alone_is_a_layout_change() {
        // Rewrapping can leave the content's height and the container as they
        // were (the fixture's 1280 -> 700 grows by 0px), so the width the
        // history wraps at has to be part of the signature or the clamp is read
        // as the reader.
        let before = sig(3000, 600, 1000);
        let after = LayoutSig {
            content_width: 700,
            ..before
        };
        assert_ne!(before, after);
        assert_eq!(
            classify_scroll(before, after, 2000, 1944, REACHABLE_MAX),
            ScrollCause::Layout
        );
    }

    #[test]
    fn any_move_with_an_unchanged_layout_and_a_reachable_offset_is_the_reader() {
        let same = sig(3000, 600, 1000);
        let at = |now_top: i32| classify_scroll(same, same, 2000, now_top, REACHABLE_MAX);
        assert_eq!(at(1999), ScrollCause::Reader);
        assert_eq!(at(2001), ScrollCause::Reader);
        assert_eq!(at(100), ScrollCause::Reader);
        // Down to the end the old offset could reach: the reader going there.
        assert_eq!(at(REACHABLE_MAX), ScrollCause::Reader);
    }

    #[test]
    fn the_allowance_boundary_is_layout() {
        let (a, b) = (sig(3000, 600, 1000), sig(3400, 600, 380));
        let at = |now_top: i32| classify_scroll(a, b, 2000, now_top, 2800);
        // Up to the allowance, in either direction, a changed layout is layout's.
        assert_eq!(at(2000 - 8), ScrollCause::Layout);
        assert_eq!(at(2000 + LAYOUT_SHIFT_ALLOWANCE_PX), ScrollCause::Layout);
        assert_eq!(at(2000 - LAYOUT_SHIFT_ALLOWANCE_PX), ScrollCause::Layout);
        // One past it is the reader, upward (the negative move is what catches a
        // dropped `.abs()`) and downward.
        assert_eq!(
            at(2000 + LAYOUT_SHIFT_ALLOWANCE_PX + 1),
            ScrollCause::Reader
        );
        assert_eq!(
            at(2000 - LAYOUT_SHIFT_ALLOWANCE_PX - 1),
            ScrollCause::Reader
        );
    }

    #[test]
    fn a_clamp_to_the_new_end_is_layout_however_far_it_moved() {
        // Widening a long history: the rows above the reader shrink, the range
        // shrinks by 600px, and the browser clamps the view to the new end.
        let (a, b) = (sig(3000, 600, 380), sig(2400, 600, 1000));
        const { assert!(2000 - 1400 > LAYOUT_SHIFT_ALLOWANCE_PX) };
        assert_eq!(classify_scroll(a, b, 2000, 1400, 1400), ScrollCause::Layout);
        // Overflow removed (a positioned overhang going away) clamps the same
        // way with nothing the signature describes changing.
        assert_eq!(classify_scroll(a, a, 2000, 1400, 1400), ScrollCause::Layout);
        assert_eq!(classify_scroll(a, a, 2000, 1950, 1950), ScrollCause::Layout);
    }

    #[test]
    fn the_final_end_clause_needs_an_unreachable_offset_and_a_view_at_the_end() {
        let same = sig(3000, 600, 1000);
        let max = 1400;
        let at = |recorded_top: i32, now_top: i32| {
            classify_scroll(same, same, recorded_top, now_top, max)
        };
        let beyond = max + SCROLL_TOP_SLACK_PX + 1;
        // Past the new end by more than rounding, and now at the end within it.
        assert_eq!(at(beyond, max), ScrollCause::Layout);
        assert_eq!(at(beyond, max - SCROLL_TOP_SLACK_PX), ScrollCause::Layout);
        assert_eq!(at(beyond, max + SCROLL_TOP_SLACK_PX), ScrollCause::Layout);
        // Materially above the end: the reader moved, clamp or not.
        assert_eq!(
            at(beyond, max - SCROLL_TOP_SLACK_PX - 1),
            ScrollCause::Reader
        );
        assert_eq!(at(2000, 1000), ScrollCause::Reader);
        // The distance to the end is bounded both ways: a reading past the end
        // is not "at the end".
        assert_eq!(at(2000, max + 50), ScrollCause::Reader);
        // An old offset within rounding of the new end is not taken as
        // unreachable: nothing provably clamped it.
        assert_eq!(at(max + SCROLL_TOP_SLACK_PX, max), ScrollCause::Reader);
    }

    #[test]
    fn an_intermediate_clamp_is_layout_only_within_the_allowance() {
        // The view clamped during a short intermediate layout and the history
        // then came out taller: it ends BELOW the final end, so the final-end
        // clause cannot see it and only the allowance can.
        let (a, b) = (sig(3000, 600, 1000), sig(3100, 600, 880));
        let max = 2500;
        assert_eq!(classify_scroll(a, b, 2000, 1889, max), ScrollCause::Layout);
        // The documented residual: a larger intermediate clamp reads as the reader.
        assert_eq!(
            classify_scroll(a, b, 2000, 2000 - LAYOUT_SHIFT_ALLOWANCE_PX - 1, max),
            ScrollCause::Reader
        );
        // And with nothing in the signature changed, it is the reader at any size.
        assert_eq!(classify_scroll(a, a, 2000, 1889, max), ScrollCause::Reader);
    }

    #[test]
    fn nothing_moved_and_nothing_resized_is_an_echo() {
        let same = sig(3000, 600, 1000);
        // Unchanged layout, unchanged offset: whatever sent this event, the
        // record already accounts for it.
        assert_eq!(
            classify_scroll(same, same, 2000, 2000, REACHABLE_MAX),
            ScrollCause::Echo
        );
        // Even at the end, where the final-end clause looks.
        assert_eq!(
            classify_scroll(same, same, REACHABLE_MAX, REACHABLE_MAX, REACHABLE_MAX),
            ScrollCause::Echo
        );
        // A changed layout with the offset left where it was is the layout's,
        // not an echo: it still needs a restore.
        assert_eq!(
            classify_scroll(same, sig(3200, 600, 1000), 2000, 2000, REACHABLE_MAX),
            ScrollCause::Layout
        );
        // One pixel of movement is not an echo.
        assert_eq!(
            classify_scroll(same, same, 2000, 1999, REACHABLE_MAX),
            ScrollCause::Reader
        );
    }

    /// A view `height` px tall whose top is at `top`.
    fn edges(top: i32, height: i32) -> ViewEdges {
        ViewEdges {
            top,
            bottom: top + height,
        }
    }

    #[test]
    fn moving_up_is_measured_from_where_the_gesture_started() {
        let from = edges(1000, 600);
        let at = |top: i32| edges(top, 600);
        // Three 1px frames: none is past the slack on its own, the third is
        // past it from where the gesture started.
        assert!(!moved_up(from, at(999)));
        assert!(!moved_up(from, at(1000 - SCROLL_TOP_SLACK_PX)));
        assert!(moved_up(from, at(1000 - SCROLL_TOP_SLACK_PX - 1)));
        // Jitter back within the slack, and any downward move, is not up.
        assert!(!moved_up(from, at(1001)));
        assert!(!moved_up(from, at(1500)));
    }

    #[test]
    fn a_container_growing_over_a_clamped_top_is_not_moving_up() {
        // The composer collapses by 250px: the container grows by that much and
        // the browser clamps `scrollTop` up by the same, so the bottom edge
        // stays put. Past any allowance, so it reaches the reader branch.
        let from = edges(2000, 500);
        assert!(!moved_up(from, edges(1750, 750)));
        // The reader moving up while it grows is still moving up.
        assert!(moved_up(from, edges(1700, 750)));
    }

    #[test]
    fn at_end_allows_rounding_slack_and_no_more() {
        let max = 2400;
        assert!(at_end(max, max));
        assert!(at_end(max - SCROLL_TOP_SLACK_PX, max));
        assert!(!at_end(max - SCROLL_TOP_SLACK_PX - 1, max));
        // Past the end (a stale read against a range that just shrank).
        assert!(at_end(max + 30, max));
    }

    #[test]
    fn a_seek_step_travels_arrives_or_is_taken_over() {
        let max = 2400;
        // Our animation only moves down.
        assert_eq!(seek_step(1000, 1180, max), SeekStep::Travelling);
        // A frame that stalls or jitters within rounding is still travelling.
        assert_eq!(seek_step(1000, 1000, max), SeekStep::Travelling);
        assert_eq!(
            seek_step(1000, 1000 - SCROLL_TOP_SLACK_PX, max),
            SeekStep::Travelling
        );
        // At the live end, within rounding.
        assert_eq!(
            seek_step(2300, max - SCROLL_TOP_SLACK_PX, max),
            SeekStep::Arrived
        );
        assert_eq!(seek_step(2300, max, max), SeekStep::Arrived);
        // Up past rounding from the previous frame: the reader's.
        assert_eq!(
            seek_step(1000, 1000 - SCROLL_TOP_SLACK_PX - 1, max),
            SeekStep::TakenOver
        );
        assert_eq!(seek_step(1000, 200, max), SeekStep::TakenOver);
    }

    #[test]
    fn a_room_switch_forgets_the_old_rooms_position() {
        let history = HistoryScroll::default();
        history.pinned.set(false);
        history.anchor.borrow_mut().push(("m1".into(), 120));
        history.sig.set(sig(3000, 600, 1000));
        history.top.set(2000);
        history.follow.set(Follow::Held);
        history.gesture_from.set(Some(edges(1800, 600)));
        history.settle_pending.set(true);

        history.reset_for_room();

        assert!(history.pinned.get() && history.force.get());
        assert_eq!(history.follow.get(), Follow::Free);
        assert_eq!(history.gesture_from.get(), None);
        assert!(!history.settle_pending.get());
        assert!(history.anchor.borrow().is_empty());
        assert_eq!(history.sig.get(), LayoutSig::default());
        assert_eq!(history.top.get(), 0);
    }
}
