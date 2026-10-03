//! Keep the reader's place when messages or layout change.
//!
//! Save a visible message because content changes make scroll offsets unreliable.
//! Measure its gap from the bottom so a growing composer does not cover it.
//! New messages wait below the view until the reader chooses to scroll.
//!
//! Background: <https://github.com/freenet/river/pull/732>.

// Only the wasm build drives the DOM half; natively the pure half is exercised by
// the unit tests.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use super::{WindowAnchor, SCROLL_TOP_SLACK_PX};
use dioxus::prelude::Signal;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[cfg(target_arch = "wasm32")]
use super::{trim_would_rearm_backfill, INITIAL_WINDOW_ITEMS};
#[cfg(target_arch = "wasm32")]
use dioxus::prelude::WritableExt;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{prelude::*, JsCast};

/// Allow for browser clamping during reflow so it does not overwrite the
/// reader's saved position, even if later growth leaves the view above the end.
const LAYOUT_SHIFT_ALLOWANCE_PX: i32 = 200;

/// How long a scroll-to-latest navigation may go without a `scroll` event before
/// it counts as finished, where the browser sends no `scrollend` (Safari before
/// 17.4). A native animation sends one every frame while it moves.
const NAVIGATION_QUIET_MS: i32 = 250;

/// The same, where `scrollend` is what finishes it: only a backstop, for a
/// native end that never arrives at the destination (an animation the browser
/// gave up on, say).
const NAVIGATION_QUIET_WITH_SCROLLEND_MS: i32 = 1000;

/// The quiet interval a navigation ends after, given whether the browser sends
/// `scrollend`.
fn navigation_quiet_ms(native_scrollend: bool) -> i32 {
    if native_scrollend {
        NAVIGATION_QUIET_WITH_SCROLLEND_MS
    } else {
        NAVIGATION_QUIET_MS
    }
}

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
    /// The reader moved the view: it becomes the new saved position.
    Reader,
    /// A layout change moved it (a browser clamp): put the saved row back.
    Layout,
}

/// The one remembered row: its `data-anchor-row` key, its `gap` (the container's
/// bottom edge minus the row's top edge) and its `offset` (the row's top inside
/// `#chat-content`, which only a reflow above it changes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SavedAnchor {
    key: String,
    gap: i32,
    offset: i32,
}

/// How far the view must move to put the saved anchor back at its gap.
///
/// `current_gap` finds that row's gap now, `None` if it is not rendered.
/// The result is `None` when nothing was saved or the saved row is gone — that
/// is the missing-anchor transition, not a zero move. A row that is rendered
/// but whose gap is out of the browser's reach is still this anchor: the delta
/// is how far the view would have to move, and the caller keeps the saved gap.
fn anchor_delta(
    saved: Option<&SavedAnchor>,
    mut current_gap: impl FnMut(&str) -> Option<i32>,
) -> Option<i32> {
    let row = saved?;
    current_gap(&row.key).map(|now| row.gap - now)
}

/// What the saved anchor did in the content since it was captured.
///
/// Scrolling changes no offset, so this is blind to a navigation's own
/// movement, and content appended below the anchor does not move it either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnchorReflow {
    /// Nothing was saved, or the saved anchor is still there and has not moved
    /// past rounding.
    Stable,
    /// The saved anchor moved in the content. Positive is downward.
    Shifted(i32),
    /// A saved anchor is no longer rendered. Distinct from [`Shifted`]`(0)`:
    /// a real zero move is [`Stable`].
    Missing,
}

/// Whether the content above the saved anchor reflowed since it was captured.
/// `current_offset` finds the anchor's offset in the content now.
fn reflow_above(
    saved: Option<&SavedAnchor>,
    mut current_offset: impl FnMut(&str) -> Option<i32>,
) -> AnchorReflow {
    let Some(row) = saved else {
        return AnchorReflow::Stable;
    };
    match current_offset(&row.key) {
        None => AnchorReflow::Missing,
        Some(now) => {
            let shift = now - row.offset;
            if shift.abs() > SCROLL_TOP_SLACK_PX {
                AnchorReflow::Shifted(shift)
            } else {
                AnchorReflow::Stable
            }
        }
    }
}

/// What restoring the saved anchor did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnchorRestore {
    /// No saved row survives, or none was saved: nothing was written.
    Missing,
    /// A saved row survives and was already at its gap, within rounding.
    AtGap,
    /// A saved row survives and the view was moved to put it back (as far as
    /// the browser allowed).
    Moved,
}

/// Classify a restore by the delta `anchor_delta` found.
fn anchor_restore_for(delta: Option<i32>) -> AnchorRestore {
    match delta {
        None => AnchorRestore::Missing,
        Some(delta) if delta.abs() <= SCROLL_TOP_SLACK_PX => AnchorRestore::AtGap,
        Some(_) => AnchorRestore::Moved,
    }
}

/// Index of the newest row that intersects the viewport `[view_top, view_bottom]`.
/// `None` if no row is visible.
///
/// There are `len` rows in document order and `rect(i)` gives row `i`'s
/// `(top, bottom)`, so their tops are monotonic and the newest row starting above
/// the viewport's bottom edge is a binary search away. `rect` is a closure, not a
/// slice, because each call is a layout read in production: this makes O(log n)
/// of them plus one for the newest row's bottom edge. Touching an edge is not
/// intersecting it.
fn newest_visible_row(
    len: usize,
    rect: impl Fn(usize) -> (i32, i32),
    view_top: i32,
    view_bottom: i32,
) -> Option<usize> {
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
    let newest = lo.checked_sub(1)?;
    if rect(newest).1 <= view_top {
        return None;
    }
    Some(newest)
}

/// Read a `scroll` event as an echo, a layout change's doing or the reader's
/// (the module doc has the rules and what each gets wrong). `max` is the live
/// scroll range (`max_scroll_top`), never a recorded one.
///
/// Layout is any of three clauses: **final-end clamp**, the recorded `scrollTop`
/// is past `max` by more than rounding and the view sits at `max` (within
/// rounding, either side), whether or not the signature changed; **changed
/// layout, small move**, the signature changed AND `scrollTop` moved no more than
/// `LAYOUT_SHIFT_ALLOWANCE_PX`; or **container grew over a clamped top**, the
/// container is taller, `scrollTop` is lower, and the view's bottom edge is where
/// it was (within rounding), however far the top moved.
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
    let bottom_edge_held = (now_top + now.client_height - (recorded_top + recorded.client_height))
        .abs()
        <= SCROLL_TOP_SLACK_PX;
    let grew_over_a_clamped_top =
        now.client_height > recorded.client_height && now_top < recorded_top && bottom_edge_held;
    if clamped_to_end || small_layout_move || grew_over_a_clamped_top {
        ScrollCause::Layout
    } else {
        ScrollCause::Reader
    }
}

/// Whether `top` is within rounding of the live end `max`.
fn at_end(top: i32, max: i32) -> bool {
    max - top <= SCROLL_TOP_SLACK_PX
}

/// Whether a native `scrollend` during a navigation to `destination` finishes
/// it: the view is at the destination, or at the live end `max` if that is now
/// nearer (a range that shrank under the animation). Any other end (one left
/// over from a scroll the click replaced, say) leaves the navigation to its
/// quiet interval.
fn navigation_arrived(top: i32, destination: i32, max: i32) -> bool {
    at_end(top, destination.min(max))
}

/// What the history needs from the component: the trim's window state (the
/// window reset at the bottom), and who to tell when a room's position is in
/// place.
#[derive(Clone)]
pub(super) struct HistoryHooks {
    pub window_items: Signal<usize>,
    pub window_anchor: Rc<RefCell<Option<WindowAnchor>>>,
    pub window_overgrown: Rc<Cell<bool>>,
    pub window_rendered: Rc<Cell<usize>>,
    /// The current room's initial placement or saved position has been put in
    /// place. Called from raw JS callbacks too, so it may only defer signal work.
    pub positioned: Rc<dyn Fn()>,
}

/// What the current room's view still needs before ordinary preservation runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Placement {
    /// In place: restores preserve the saved anchor.
    Placed,
    /// No saved position: start at the newest message once it can be measured.
    Latest,
    /// Saved when the reader left this room: put that anchor back first.
    Saved,
}

/// The history's scroll state. See the module doc.
pub(super) struct HistoryScroll {
    /// The one newest visible row. `None` until something has been captured,
    /// and again once that row is gone and the latest landing has not yet.
    anchor: RefCell<Option<SavedAnchor>>,
    placement: Cell<Placement>,
    /// Which room visit this is: `enter_room` counts up. Shared with deferred
    /// work, so a trim scheduled for one room is dropped if it runs after the
    /// switch to another (it would reset that room's restored window).
    visit: Rc<Cell<u64>>,
    /// The layout and `scrollTop` as last accounted for, to classify a `scroll`.
    sig: Cell<LayoutSig>,
    top: Cell<i32>,
    /// A callback found the history hidden since it was last laid out: the
    /// first laid-out one restores rather than captures.
    hidden: Cell<bool>,
    /// The end measured at the click, while a scroll-to-latest navigation runs.
    navigation: Cell<Option<i32>>,
    /// The navigation's quiet interval, while one is pending.
    navigation_timer: Cell<Option<i32>>,
    /// The quiet interval's callback, made once by `install`.
    navigation_quiet: RefCell<Option<js_sys::Function>>,
    /// Whether the browser sends `scrollend`.
    native_scrollend: Cell<bool>,
    /// `#chat-scroll-container` and `#chat-content`, found once by `install`:
    /// `Conversation` mounts once, so these are the elements the listeners and
    /// the ResizeObserver are bound to for the app's lifetime.
    container: RefCell<Option<web_sys::Element>>,
    content: RefCell<Option<web_sys::Element>>,
    /// `content`'s `anchor-row` elements: a live collection in tree order, so
    /// nothing invalidates it; relies on `#chat-content` living as long as the app.
    rows: RefCell<Option<web_sys::HtmlCollection>>,
    /// Set by `install` once the listeners exist, so `is_some` also means
    /// "installed".
    hooks: RefCell<Option<HistoryHooks>>,
}

impl Default for HistoryScroll {
    fn default() -> Self {
        Self {
            anchor: RefCell::new(None),
            placement: Cell::new(Placement::Latest),
            visit: Rc::new(Cell::new(0)),
            sig: Cell::new(LayoutSig::default()),
            top: Cell::new(0),
            hidden: Cell::new(false),
            navigation: Cell::new(None),
            navigation_timer: Cell::new(None),
            navigation_quiet: RefCell::new(None),
            native_scrollend: Cell::new(true),
            container: RefCell::new(None),
            content: RefCell::new(None),
            rows: RefCell::new(None),
            hooks: RefCell::new(None),
        }
    }
}

impl HistoryScroll {
    /// The reader is leaving the current room: the one anchor to put back when
    /// they return. Takes in their latest movement first and cancels a
    /// navigation where it is, while the DOM is still this room's.
    pub(super) fn leave_room(&self) -> Option<SavedAnchor> {
        // `before_hide`, for a room switch: the DOM is still the old room's.
        #[cfg(target_arch = "wasm32")]
        self.before_hide();
        self.end_navigation();
        self.anchor.borrow().clone()
    }

    /// The saved anchor's key, for the render to keep that row inside the
    /// window when the window's own head key is gone.
    pub(super) fn reading_anchor_key(&self) -> Option<String> {
        self.anchor
            .borrow()
            .as_ref()
            .map(|anchor| anchor.key.clone())
    }

    /// A room becomes current: start from what `leave_room` saved for it, or
    /// at its newest message when nothing was.
    pub(super) fn enter_room(&self, saved: Option<SavedAnchor>) {
        self.end_navigation();
        self.visit.set(self.visit.get().wrapping_add(1));
        self.placement.set(if saved.is_some() {
            Placement::Saved
        } else {
            Placement::Latest
        });
        *self.anchor.borrow_mut() = saved;
        self.sig.set(LayoutSig::default());
        self.top.set(0);
    }

    /// Forget a navigation in flight, and its quiet interval. Does not touch
    /// the view: the callers that need the animation stopped do that first.
    fn end_navigation(&self) {
        self.navigation.set(None);
        self.clear_navigation_timer();
    }

    fn clear_navigation_timer(&self) {
        #[cfg(target_arch = "wasm32")]
        if let (Some(handle), Some(window)) = (self.navigation_timer.take(), web_sys::window()) {
            window.clear_timeout_with_handle(handle);
        }
    }

    /// The history was found hidden: nothing can be measured until it is laid
    /// out again, and a navigation in flight is over. What its last `scroll`
    /// captured is kept.
    fn note_hidden(&self) {
        self.hidden.set(true);
        self.end_navigation();
    }
}

#[cfg(target_arch = "wasm32")]
pub(super) fn element_by_id(id: &str) -> Option<web_sys::Element> {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
}

#[cfg(target_arch = "wasm32")]
const ANCHOR_ATTR: &str = "data-anchor-row";

/// The furthest `scrollTop` can go right now, read live. Never negative.
#[cfg(target_arch = "wasm32")]
fn max_scroll_top(container: &web_sys::Element) -> i32 {
    (container.scroll_height() - container.client_height()).max(0)
}

/// Whether the view is at the live end, within rounding.
#[cfg(target_arch = "wasm32")]
fn view_at_end(container: &web_sys::Element) -> bool {
    at_end(container.scroll_top(), max_scroll_top(container))
}

/// Whether trimming a window of `rendered` items back to its initial size would
/// go unseen now: the view rests at the live end, and the retained tail would
/// not leave the backfill sentinel in range of it (#505; see
/// `trim_would_rearm_backfill`).
#[cfg(target_arch = "wasm32")]
fn trim_is_invisible(container: &web_sys::Element, rendered: usize) -> bool {
    view_at_end(container)
        && !trim_would_rearm_backfill(
            container.scroll_height(),
            container.client_height(),
            rendered,
            INITIAL_WINDOW_ITEMS,
        )
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

/// Whether the reader asked the system for reduced motion, through `Reflect`
/// because web-sys's `MediaQueryList` feature is not enabled. `false` if it
/// cannot be told.
#[cfg(target_arch = "wasm32")]
fn prefers_reduced_motion() -> bool {
    (|| {
        let window = js_sys::global();
        let match_media: js_sys::Function =
            js_sys::Reflect::get(&window, &JsValue::from_str("matchMedia"))
                .ok()?
                .dyn_into()
                .ok()?;
        let list = match_media
            .call1(
                &window,
                &JsValue::from_str("(prefers-reduced-motion: reduce)"),
            )
            .ok()?;
        js_sys::Reflect::get(&list, &JsValue::from_str("matches"))
            .ok()?
            .as_bool()
    })()
    .unwrap_or(false)
}

/// Stop a native smooth scroll where it is. A write to the position it is
/// already at does not abort it in Firefox, so move a pixel and back; all three
/// engines abort for a real move. Never past the range, so no clamp is involved.
#[cfg(target_arch = "wasm32")]
fn stop_native_scroll(container: &web_sys::Element) {
    let top = container.scroll_top();
    let nudge = if top < max_scroll_top(container) {
        top + 1
    } else {
        top - 1
    };
    if nudge < 0 {
        // No range at all, so nothing can be animating.
        return;
    }
    container.set_scroll_top(nudge);
    container.set_scroll_top(top);
}

#[cfg(target_arch = "wasm32")]
impl HistoryScroll {
    /// The scroll container, if it has a layout box to measure. Hidden, every
    /// read is 0, which must not be taken for where the reader is.
    fn laid_out_container(&self) -> Option<web_sys::Element> {
        self.container
            .borrow()
            .clone()
            .filter(|c| c.client_height() > 0)
    }

    fn read_sig(&self, container: &web_sys::Element) -> LayoutSig {
        let (content_height, content_width) =
            self.content.borrow().as_ref().map_or((0, 0), |content| {
                (content.client_height(), content.client_width())
            });
        LayoutSig {
            content_height,
            client_height: container.client_height(),
            client_width: container.client_width(),
            content_width,
        }
    }

    /// Record the layout and `scrollTop` as they are now. Read back after any
    /// write, since the browser clamps.
    fn record(&self, container: &web_sys::Element) {
        self.sig.set(self.read_sig(container));
        self.top.set(container.scroll_top());
    }

    /// Who the `scroll` event now pending (or being handled) belongs to.
    fn cause_now(&self, container: &web_sys::Element) -> ScrollCause {
        classify_scroll(
            self.sig.get(),
            self.read_sig(container),
            self.top.get(),
            container.scroll_top(),
            max_scroll_top(container),
        )
    }

    /// Whether the current room has any rows rendered.
    fn has_rows(&self) -> bool {
        self.rows.borrow().as_ref().is_some_and(|r| r.length() > 0)
    }

    /// The rendered row whose `data-anchor-row` is `key`.
    fn find_row(&self, container: &web_sys::Element, key: &str) -> Option<web_sys::Element> {
        let key = css_escape(key)?;
        let selector = format!("#chat-content [{ANCHOR_ATTR}=\"{key}\"]");
        container.query_selector(&selector).ok()?
    }

    /// Measure the saved rows where the view is now (see "The saved position"),
    /// and record. With no rows rendered it only records: an empty render is
    /// not a position, and the saved anchor waits for the rows.
    fn capture(&self, container: &web_sys::Element) {
        if !self.has_rows() {
            self.record(container);
            return;
        }
        if let (Some(list), Some(content)) =
            (self.rows.borrow().as_ref(), self.content.borrow().as_ref())
        {
            // Relative to the container, the frame `newest_visible_row` works in.
            // Each call is a layout read, and the search makes few of them.
            let view = container.get_bounding_client_rect();
            let content_top = content.get_bounding_client_rect().top();
            let view_bottom = (view.bottom() - view.top()).round() as i32;
            let item = |i: usize| list.item(i as u32).unwrap_throw();
            let rect = |i: usize| {
                let r = item(i).get_bounding_client_rect();
                (
                    (r.top() - view.top()).round() as i32,
                    (r.bottom() - view.top()).round() as i32,
                )
            };
            let picked = newest_visible_row(list.length() as usize, rect, 0, view_bottom);
            *self.anchor.borrow_mut() = picked.and_then(|i| {
                let row = item(i);
                let top = row.get_bounding_client_rect().top();
                Some(SavedAnchor {
                    key: row.get_attribute(ANCHOR_ATTR)?,
                    gap: (view.bottom() - top).round() as i32,
                    offset: (top - content_top).round() as i32,
                })
            });
        }
        self.record(container);
    }

    /// Capture where the view has come to rest, and trim the window if that is
    /// the bottom. With no rows rendered it only records, and schedules no trim
    /// from a history clamped to its empty height.
    fn capture_at_rest(&self, container: &web_sys::Element) {
        if !self.has_rows() {
            self.record(container);
            return;
        }
        self.capture(container);
        self.trim_at_bottom(container);
    }

    /// A view resting AT the bottom is the one moment a window trim is
    /// invisible: the rows it removes are above the view, and the restore that
    /// follows puts the saved anchor (at the bottom) back at its gap. Gated at
    /// SCROLL_TOP_SLACK_PX of the end, so a reader resting higher keeps the rows
    /// above them. Skipped when the trimmed tail would leave the backfill
    /// sentinel in range of the bottom, or the two oscillate at render speed
    /// (#505; see `trim_would_rearm_backfill`).
    ///
    /// Runs where the reader's own scroll comes to rest at the end, where a
    /// scroll-to-latest navigation lands there, and where a placement at the
    /// latest message lands, never for an arrival. Nothing to trim (not
    /// installed, or the window has not grown past its initial size) reads no
    /// geometry.
    ///
    /// The trim is decided here and applied on a later task, so the deferred
    /// callback decides it again against the history as it is then: the same
    /// room visit, the window still overgrown, rows rendered in a laid-out
    /// container, and `trim_is_invisible`. A callback that finds any of those
    /// gone changes nothing, so the window stays eligible for the next rest at
    /// the end; several queued callbacks trim once, since the first to trim
    /// clears `window_overgrown`.
    fn trim_at_bottom(&self, container: &web_sys::Element) {
        let hooks = self.hooks.borrow();
        let Some(trim) = hooks.as_ref().filter(|h| h.window_overgrown.get()) else {
            return;
        };
        if !trim_is_invisible(container, trim.window_rendered.get()) {
            return;
        }
        let window_overgrown = trim.window_overgrown.clone();
        let window_rendered = trim.window_rendered.clone();
        let window_anchor = trim.window_anchor.clone();
        let mut window_items = trim.window_items;
        let container = container.clone();
        let rows = self.rows.borrow().clone();
        let (visit, trimmed_visit) = (self.visit.clone(), self.visit.get());
        // Deferred: this runs from a raw JS callback with no Dioxus scope, and
        // `window_items` is a signal the render subscribes to. See
        // .claude/rules/dioxus-signal-safety.md. A room switch in between
        // (`leave_room`'s own take-in can schedule this) owns the window by
        // then: the room switched to may have had its depth restored, so the
        // visit is checked before any shared handle is read.
        crate::util::defer(move || {
            if visit.get() != trimmed_visit || !window_overgrown.get() {
                return;
            }
            let has_rows = rows.as_ref().is_some_and(|rows| rows.length() > 0);
            let laid_out = container.client_width() > 0 && container.client_height() > 0;
            if !has_rows || !laid_out || !trim_is_invisible(&container, window_rendered.get()) {
                return;
            }
            window_overgrown.set(false);
            *window_anchor.borrow_mut() = None;
            window_items.set(INITIAL_WINDOW_ITEMS);
        });
    }

    /// Put the view where it belongs after a layout or content change.
    pub(super) fn restore(&self) {
        self.take_in_undelivered_scroll();
        self.restore_now()
    }

    /// Take in a scroll whose event has not arrived yet, before rendering or
    /// restoring: the reader's is captured, a navigation's recorded as its
    /// progress. Layout movement is recorded without changing the anchor;
    /// the restore still puts that anchor back. Otherwise a clamp observed
    /// before a refill could be captured as Reader against the refilled DOM.
    pub(super) fn take_in_undelivered_scroll(&self) {
        let Some(container) = self.laid_out_container() else {
            return;
        };
        if self.hidden.get()
            || self.placement.get() != Placement::Placed
            || container.scroll_top() == self.top.get()
        {
            return;
        }
        if self.navigation.get().is_some() {
            self.on_navigation_scroll(&container);
        } else {
            match self.cause_now(&container) {
                ScrollCause::Echo => {}
                ScrollCause::Layout => self.record(&container),
                ScrollCause::Reader => self.capture_at_rest(&container),
            }
        }
    }

    fn restore_now(&self) {
        // Hidden: everything waits for the reveal.
        let Some(container) = self.laid_out_container() else {
            self.note_hidden();
            return;
        };
        self.hidden.set(false);
        match self.placement.get() {
            Placement::Latest => return self.place_at_latest(&container),
            Placement::Saved => return self.place_saved(&container),
            Placement::Placed => {}
        }
        if self.navigation.get().is_some() {
            // Content below the view moves nothing on screen; leave the
            // animation alone unless something above it reflowed, the anchor
            // itself is gone, or no rows render at all.
            if !self.handle_navigation_reflow(&container) {
                self.record(&container);
            }
            return;
        }
        if self.restore_anchor(&container) == AnchorRestore::Missing {
            self.land_on_latest(&container);
            return;
        }
        self.record(&container);
    }

    /// A room with no saved position starts at its newest message, once it has
    /// any, and that is captured at once. That is a rest at the end, so an
    /// overgrown window trims there, whether or not the write produced a
    /// `scroll` the reader is credited with.
    fn place_at_latest(&self, container: &web_sys::Element) {
        if !self.has_rows() {
            self.record(container);
            return;
        }
        container.set_scroll_top(container.scroll_height());
        self.capture_at_rest(container);
        self.placement.set(Placement::Placed);
        self.tell_positioned();
    }

    /// A revisited room puts its saved anchor back, once it has rows. If that
    /// anchor is not rendered, the room starts at its newest message instead.
    /// Nothing is captured on a successful restore: a gap out of reach for now
    /// stays saved.
    fn place_saved(&self, container: &web_sys::Element) {
        if !self.has_rows() {
            self.record(container);
            return;
        }
        if self.restore_anchor(container) == AnchorRestore::Missing {
            self.land_on_latest(container);
            return;
        }
        self.record(container);
        self.placement.set(Placement::Placed);
        self.tell_positioned();
    }

    /// The saved anchor is gone. End any navigation, forget it, and place at
    /// the current end. The landing capture is the new single anchor; a later
    /// arrival preserves it. With no rows rendered nothing is gone yet: it only
    /// records, and the anchor waits for the rows.
    fn land_on_latest(&self, container: &web_sys::Element) {
        if !self.has_rows() {
            self.record(container);
            return;
        }
        let navigating = self.navigation.get().is_some();
        self.end_navigation();
        *self.anchor.borrow_mut() = None;
        if navigating {
            // A write to the position a smooth scroll is already at does not
            // abort it in Firefox. Stop it before the jump to the end.
            stop_native_scroll(container);
        }
        self.place_at_latest(container);
    }

    fn tell_positioned(&self) {
        let positioned = self.hooks.borrow().as_ref().map(|h| h.positioned.clone());
        if let Some(positioned) = positioned {
            positioned();
        }
    }

    /// Scroll the saved anchor back to its gap. If it is gone (or none was
    /// saved), write nothing: the caller places at the latest message.
    fn restore_anchor(&self, container: &web_sys::Element) -> AnchorRestore {
        let view = container.get_bounding_client_rect();
        let delta = anchor_delta(self.anchor.borrow().as_ref(), |key| {
            Some(gap(&view, &self.find_row(container, key)?))
        });
        let restored = anchor_restore_for(delta);
        if let (AnchorRestore::Moved, Some(delta)) = (restored, delta) {
            container.set_scroll_top(container.scroll_top() + delta);
        }
        restored
    }

    /// [`reflow_above`] for the saved anchor as rendered now.
    fn navigation_reflow(&self, container: &web_sys::Element) -> AnchorReflow {
        let Some(content_top) = self
            .content
            .borrow()
            .as_ref()
            .map(|content| content.get_bounding_client_rect().top())
        else {
            return AnchorReflow::Stable;
        };
        reflow_above(self.anchor.borrow().as_ref(), |key| {
            let row = self.find_row(container, key)?;
            Some((row.get_bounding_client_rect().top() - content_top).round() as i32)
        })
    }

    /// Something above the view reflowed during a navigation: stop it, keeping
    /// the saved anchor where the reader last saw it (`shift` is how far the
    /// reflow moved it down), and capture there.
    fn cancel_navigation_for_reflow(&self, container: &web_sys::Element, shift: i32) {
        let corrected = container.scroll_top() + shift;
        self.end_navigation();
        // The correction is not relied on to abort the animation: it can clamp
        // to the offset the view is already at (a negative shift at the top),
        // and Firefox does not abort a smooth scroll for that.
        stop_native_scroll(container);
        container.set_scroll_top(corrected);
        self.capture(container);
    }

    /// Stop a navigation where the view is now, and capture there.
    fn stop_navigation_here(&self, container: &web_sys::Element) {
        self.end_navigation();
        stop_native_scroll(container);
        self.capture_at_rest(container);
    }

    /// What a navigation does about the history as it is now, before any
    /// caller captures. `true` means it was handled and the caller stops:
    ///
    /// * no rows render: end the navigation and stop the animation where it is,
    ///   recording only. There is no row to measure a reflow by or to land on,
    ///   and the saved anchor waits for the rows; their return restores, it
    ///   never resumes or retargets the animation;
    /// * the saved anchor moved in the content (a reflow above it): cancel,
    ///   keeping it where the reader last saw it;
    /// * the saved anchor is gone: the missing-anchor transition.
    ///
    /// `false`: rows render and the anchor is where it was.
    fn handle_navigation_reflow(&self, container: &web_sys::Element) -> bool {
        if !self.has_rows() {
            self.end_navigation();
            stop_native_scroll(container);
            self.record(container);
            return true;
        }
        match self.navigation_reflow(container) {
            AnchorReflow::Shifted(shift) => self.cancel_navigation_for_reflow(container, shift),
            AnchorReflow::Missing => self.land_on_latest(container),
            AnchorReflow::Stable => return false,
        }
        true
    }

    /// A `scroll` during a navigation is its progress: capture it, unless
    /// `handle_navigation_reflow` dealt with it.
    fn on_navigation_scroll(&self, container: &web_sys::Element) {
        if !self.handle_navigation_reflow(container) {
            self.capture(container);
            self.arm_navigation_timer();
        }
    }

    /// Read a `scroll` event as layout's doing (restore) or the reader's
    /// (capture). A navigation's own events are its progress.
    fn on_scroll(&self) {
        let Some(container) = self.laid_out_container() else {
            self.note_hidden();
            return;
        };
        // A reveal (or a placement still to happen) restores first: where the
        // browser put the view is not the reader's choice.
        if self.hidden.get() || self.placement.get() != Placement::Placed {
            self.restore_now();
            return;
        }
        if self.navigation.get().is_some() {
            self.on_navigation_scroll(&container);
            return;
        }
        match self.cause_now(&container) {
            ScrollCause::Echo => {}
            ScrollCause::Layout => self.restore_now(),
            ScrollCause::Reader => self.capture_at_rest(&container),
        }
    }

    /// "Scroll to latest messages": one native smooth scroll to the end as it
    /// is now. Already there, nothing animates; with reduced motion it jumps.
    pub(super) fn navigate_to_latest(&self) {
        let Some(container) = self.laid_out_container() else {
            return;
        };
        // No rows render: there is nothing to land on or to capture, and the
        // saved anchor waits for the rows. The click asks for nothing.
        if !self.has_rows() {
            self.record(&container);
            return;
        }
        if self.hidden.get() || self.placement.get() != Placement::Placed {
            self.restore_now();
        } else {
            self.take_in_undelivered_scroll();
        }
        // A second click replaces the first: its own end, measured now.
        self.end_navigation();
        let destination = max_scroll_top(&container);
        if at_end(container.scroll_top(), destination) {
            self.capture_at_rest(&container);
            return;
        }
        if prefers_reduced_motion() {
            container.set_scroll_top(destination);
            self.capture_at_rest(&container);
            return;
        }
        // Fresh rows to tell a reflow above by.
        self.capture(&container);
        self.navigation.set(Some(destination));
        let options = web_sys::ScrollToOptions::new();
        options.set_top(f64::from(destination));
        options.set_behavior(web_sys::ScrollBehavior::Smooth);
        container.scroll_to_with_scroll_to_options(&options);
        self.arm_navigation_timer();
    }

    /// The navigation has come to rest: capture where it landed. A reflow,
    /// a missing anchor or an empty render since its last `scroll` is handled
    /// first (`handle_navigation_reflow`), at either kind of end, so the
    /// capture never takes a displaced view for the landing. A `scrollend`
    /// then counts only at the destination (`navigation_arrived`); the quiet
    /// interval ends it wherever it is. Hidden, the hide has ended it already.
    fn finish_navigation(&self, native_end: bool) {
        let Some(destination) = self.navigation.get() else {
            return;
        };
        let Some(container) = self.laid_out_container() else {
            return;
        };
        if self.handle_navigation_reflow(&container) {
            return;
        }
        if native_end
            && !navigation_arrived(
                container.scroll_top(),
                destination,
                max_scroll_top(&container),
            )
        {
            return;
        }
        self.end_navigation();
        self.capture_at_rest(&container);
    }

    /// The navigation's quiet interval has passed with no `scroll`.
    fn on_navigation_quiet(&self) {
        // The callback running now has fired.
        self.navigation_timer.set(None);
        self.finish_navigation(false);
    }

    /// (Re)start the navigation's quiet interval.
    fn arm_navigation_timer(&self) {
        self.clear_navigation_timer();
        let callback = self.navigation_quiet.borrow().clone();
        let (Some(window), Some(callback)) = (web_sys::window(), callback) else {
            return;
        };
        if let Ok(handle) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
            &callback,
            navigation_quiet_ms(self.native_scrollend.get()),
        ) {
            self.navigation_timer.set(Some(handle));
        }
    }

    /// The reader's own input on the history (wheel, touch, pointer, key) takes
    /// over from a navigation in flight.
    fn on_reader_input(&self) {
        if self.navigation.get().is_none() {
            return;
        }
        if let Some(container) = self.laid_out_container() {
            self.stop_navigation_here(&container);
        }
    }

    /// The chat is about to be hidden (a mobile panel button): take in the
    /// reader's latest movement and cancel a navigation while the history
    /// still has a box to cancel it in.
    pub(super) fn before_hide(&self) {
        let Some(container) = self.laid_out_container() else {
            return;
        };
        if self.hidden.get() || self.placement.get() != Placement::Placed {
            return;
        }
        self.take_in_undelivered_scroll();
        if self.navigation.get().is_some() {
            self.stop_navigation_here(&container);
        }
    }

    /// Listen for the reader's scrolls and input and for layout changes.
    /// Idempotent: a no-op once installed; before the history is in the DOM it
    /// does nothing and the next call retries.
    ///
    /// A hidden history is installed too, so that its reveal is observed; only
    /// its geometry is left unrecorded.
    pub(super) fn install(self: &Rc<Self>, hooks: HistoryHooks) {
        if self.hooks.borrow().is_some() {
            return;
        }
        let (Some(container), Some(content)) = (
            element_by_id("chat-scroll-container"),
            element_by_id("chat-content"),
        ) else {
            return;
        };

        // Every closure here is leaked (`into_js_value`) deliberately:
        // `Conversation` mounts once for the app's lifetime (rooms are swapped
        // by CSS, not by unmount) and `use_effect` has no cleanup hook, so there
        // is nothing to disconnect them from.

        // The ResizeObserver sees the content growing or reflowing, the
        // container shrinking (the composer growing, #486's third cause), and
        // the container hidden or given its height back. All restore; none may
        // capture.
        let this = self.clone();
        let on_resize = Closure::<dyn FnMut()>::new(move || this.restore()).into_js_value();
        let Ok(observer) = web_sys::ResizeObserver::new(on_resize.unchecked_ref()) else {
            return;
        };
        *self.container.borrow_mut() = Some(container.clone());
        *self.content.borrow_mut() = Some(content.clone());
        *self.rows.borrow_mut() = Some(content.get_elements_by_class_name("anchor-row"));
        if let Some(laid_out) = self.laid_out_container() {
            self.record(&laid_out);
        }
        *self.hooks.borrow_mut() = Some(hooks);
        observer.observe(&content);
        observer.observe(&container);

        let this = self.clone();
        *self.navigation_quiet.borrow_mut() = Some(
            Closure::<dyn FnMut()>::new(move || this.on_navigation_quiet())
                .into_js_value()
                .unchecked_into(),
        );

        // Passive: nothing here calls `preventDefault`, and the jank this
        // replaced (#151) came from work on the scroll path.
        let passive = web_sys::AddEventListenerOptions::new();
        passive.set_passive(true);
        let listen = |event: &str, callback: &JsValue| {
            let _ = container.add_event_listener_with_callback_and_add_event_listener_options(
                event,
                callback.unchecked_ref(),
                &passive,
            );
        };

        let this = self.clone();
        listen(
            "scroll",
            &Closure::<dyn FnMut()>::new(move || this.on_scroll()).into_js_value(),
        );

        let native =
            js_sys::Reflect::has(&container, &JsValue::from_str("onscrollend")).unwrap_or(false);
        self.native_scrollend.set(native);
        if native {
            let this = self.clone();
            listen(
                "scrollend",
                &Closure::<dyn FnMut()>::new(move || this.finish_navigation(true)).into_js_value(),
            );
        }

        for event in ["wheel", "touchstart", "pointerdown", "keydown"] {
            let this = self.clone();
            listen(
                event,
                &Closure::<dyn FnMut()>::new(move || this.on_reader_input()).into_js_value(),
            );
        }

        #[cfg(all(feature = "example-data", feature = "no-sync"))]
        {
            let take_in = self.clone();
            let restore = self.clone();
            crate::test_hooks::install_history_scroll_probe(
                move || take_in.take_in_undelivered_scroll(),
                move || restore.restore(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: (i32, i32) = (0, 500);

    /// `newest_visible_row` over an array: the closure is all it gets to see.
    fn newest(rows: &[(i32, i32)], view_top: i32, view_bottom: i32) -> Option<usize> {
        newest_visible_row(rows.len(), |i| rows[i], view_top, view_bottom)
    }

    fn sig(content_height: i32, client_height: i32, client_width: i32) -> LayoutSig {
        LayoutSig {
            content_height,
            client_height,
            client_width,
            content_width: client_width,
        }
    }

    fn row(key: &str, gap: i32, offset: i32) -> SavedAnchor {
        SavedAnchor {
            key: key.into(),
            gap,
            offset,
        }
    }

    #[test]
    fn newest_visible_row_picks_the_last_row_intersecting_the_viewport() {
        // Rows straddling the top edge, inside, and straddling the bottom edge.
        let rows = [
            (-150, -50),
            (-50, 100),
            (100, 300),
            (300, 450),
            (450, 650),
            (650, 800),
        ];
        assert_eq!(newest(&rows, VIEW.0, VIEW.1), Some(4));

        // The newest row ends inside the view, so the ones below it are off screen.
        assert_eq!(newest(&rows[..4], VIEW.0, VIEW.1), Some(3));

        // Only a row straddling the top edge is visible.
        let above = [(-300, -200), (-200, 20), (600, 700)];
        assert_eq!(newest(&above, VIEW.0, VIEW.1), Some(1));

        // A row taller than the viewport, covering both edges.
        let tall = [(-100, 900)];
        assert_eq!(newest(&tall, VIEW.0, VIEW.1), Some(0));

        assert_eq!(newest(&[], VIEW.0, VIEW.1), None);

        // Touching an edge is not intersecting it; one pixel of overlap is.
        assert_eq!(newest(&[(-100, 0)], 0, 500), None);
        assert_eq!(newest(&[(0, 100), (500, 600)], 0, 500), Some(0));
        assert_eq!(newest(&[(-100, 1)], 0, 500), Some(0));
        assert_eq!(newest(&[(499, 600)], 0, 500), Some(0));
    }

    #[test]
    fn newest_visible_row_reads_only_a_handful_of_rects() {
        // Capture runs on every reader scroll event, and each rect is a layout read.
        let rows: Vec<(i32, i32)> = (0..10_000).map(|i| (i * 100, i * 100 + 90)).collect();
        let reads = Cell::new(0usize);
        let picked = newest_visible_row(
            rows.len(),
            |i| {
                reads.set(reads.get() + 1);
                rows[i]
            },
            300_000,
            300_500,
        );
        assert_eq!(picked, Some(3_004));
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
        // A short intermediate layout can clamp the view and then leave the
        // history taller, so the offset ends below the final end. The final-end
        // clause cannot see that clamp; only the allowance can.
        let (grown, narrower) = (sig(3000, 600, 1000), sig(3100, 600, 880));
        let below_end = 2500;
        let within = 1889;
        assert!(within < below_end);
        assert_eq!(
            classify_scroll(grown, narrower, 2000, within, below_end),
            ScrollCause::Layout
        );
        // The documented residual: a larger intermediate clamp that moves the
        // bottom edge too (the container is unchanged here) reads as the reader.
        assert_eq!(
            classify_scroll(
                grown,
                narrower,
                2000,
                2000 - LAYOUT_SHIFT_ALLOWANCE_PX - 1,
                below_end,
            ),
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

    /// A view at the end of a 3000px history in a 400px container (top 2600,
    /// bottom edge 3000), then a 406px composer collapse that the browser
    /// clamps by the same 406px, and a 700px arrival before the event is read.
    const COLLAPSED: i32 = 406;
    const BEFORE_COLLAPSE: (i32, i32) = (3000, 400);
    const ARRIVAL: i32 = 700;

    /// `classify_scroll` for that collapse with the view now at `now_top`.
    fn after_collapse(now_top: i32, client_height: i32) -> ScrollCause {
        let (content, client) = BEFORE_COLLAPSE;
        let now = sig(content + ARRIVAL, client_height, 1000);
        let max = content + ARRIVAL - client_height;
        classify_scroll(
            sig(content, client, 1000),
            now,
            content - client,
            now_top,
            max,
        )
    }

    #[test]
    fn a_container_growing_over_a_clamped_top_is_layout_however_far_it_moved() {
        // The bottom edge stayed at 3000, but the arrival put the end 700px below
        // it, so the final-end clause cannot see the clamp, and it is past the
        // allowance.
        let (content, client) = BEFORE_COLLAPSE;
        let top = content - client - COLLAPSED;
        const { assert!(COLLAPSED > LAYOUT_SHIFT_ALLOWANCE_PX) };
        assert!(content + ARRIVAL - (client + COLLAPSED) - top > SCROLL_TOP_SLACK_PX);
        assert_eq!(after_collapse(top, client + COLLAPSED), ScrollCause::Layout);
    }

    #[test]
    fn a_container_growing_while_the_reader_moves_is_still_the_reader() {
        let (content, client) = BEFORE_COLLAPSE;
        let clamped = content - client - COLLAPSED;
        let grown = client + COLLAPSED;
        // Both edges up: the reader looking back as the composer collapses.
        assert_eq!(after_collapse(clamped - 300, grown), ScrollCause::Reader);
        // The same move with the container unchanged: both edges up, past the
        // allowance.
        assert_eq!(after_collapse(clamped, client), ScrollCause::Reader);
        // The bottom edge is allowed rounding and no more, either way.
        assert_eq!(
            after_collapse(clamped - SCROLL_TOP_SLACK_PX, grown),
            ScrollCause::Layout
        );
        assert_eq!(
            after_collapse(clamped + SCROLL_TOP_SLACK_PX, grown),
            ScrollCause::Layout
        );
        assert_eq!(
            after_collapse(clamped - SCROLL_TOP_SLACK_PX - 1, grown),
            ScrollCause::Reader
        );
        assert_eq!(
            after_collapse(clamped + SCROLL_TOP_SLACK_PX + 1, grown),
            ScrollCause::Reader
        );
    }

    #[test]
    fn a_container_shrinking_under_a_top_moving_down_is_the_reader() {
        // The mirror image keeps the bottom edge too, but a composer growing
        // never moves `scrollTop`: the top moving down 406px is the reader's.
        let (content, client) = BEFORE_COLLAPSE;
        let (from_top, from_client) = (content - client - COLLAPSED, client + COLLAPSED);
        let now = sig(content, client, 1000);
        assert_eq!(
            classify_scroll(
                sig(content, from_client, 1000),
                now,
                from_top,
                from_top + COLLAPSED,
                content - client + 300,
            ),
            ScrollCause::Reader
        );
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
    fn a_restore_distinguishes_a_missing_anchor_from_one_at_its_gap() {
        let saved = row("m3", 300, 0);
        // The saved row is gone, or none was saved: nothing to put back.
        assert_eq!(anchor_delta(Some(&saved), |_| None), None);
        assert_eq!(anchor_delta(None, |_| Some(0)), None);
        // The same row, moved off its gap. A gap the browser cannot reach is
        // still this anchor, not a different row.
        assert_eq!(anchor_delta(Some(&saved), |_| Some(260)), Some(40));
        assert_eq!(anchor_delta(Some(&saved), |_| Some(3_000)), Some(-2_700));
        // Already at its gap: no write, which is not missing.
        assert_eq!(anchor_delta(Some(&saved), |_| Some(300)), Some(0));

        assert_eq!(anchor_restore_for(None), AnchorRestore::Missing);
        assert_eq!(anchor_restore_for(Some(0)), AnchorRestore::AtGap);
        assert_eq!(
            anchor_restore_for(Some(-SCROLL_TOP_SLACK_PX)),
            AnchorRestore::AtGap
        );
        assert_eq!(
            anchor_restore_for(Some(SCROLL_TOP_SLACK_PX + 1)),
            AnchorRestore::Moved
        );
        assert_eq!(
            anchor_restore_for(Some(-SCROLL_TOP_SLACK_PX - 1)),
            AnchorRestore::Moved
        );
    }

    #[test]
    fn only_a_reflow_above_the_saved_anchor_counts_during_a_navigation() {
        let saved = row("m3", 300, 2000);
        // Scrolling and content appended below move no offset.
        assert_eq!(
            reflow_above(Some(&saved), |_| Some(2000)),
            AnchorReflow::Stable
        );
        assert_eq!(
            reflow_above(Some(&saved), |_| Some(2000 + SCROLL_TOP_SLACK_PX)),
            AnchorReflow::Stable
        );
        // Something above grew or shrank.
        assert_eq!(
            reflow_above(Some(&saved), |_| Some(2120)),
            AnchorReflow::Shifted(120)
        );
        assert_eq!(
            reflow_above(Some(&saved), |_| Some(1900)),
            AnchorReflow::Shifted(-100)
        );
        // The saved anchor is gone. That is not a zero-pixel reflow.
        assert_eq!(reflow_above(Some(&saved), |_| None), AnchorReflow::Missing);
        // Nothing saved: nothing to tell by, so nothing to cancel for.
        assert_eq!(reflow_above(None, |_| None), AnchorReflow::Stable);
    }

    #[test]
    fn a_scrollend_finishes_a_navigation_only_at_its_destination() {
        // At the destination measured at the click, within rounding.
        assert!(navigation_arrived(2000, 2000, 2600));
        assert!(navigation_arrived(2000 - SCROLL_TOP_SLACK_PX, 2000, 2600));
        // Short of it: an end left over from a scroll the click replaced.
        assert!(!navigation_arrived(1500, 2000, 2600));
        // The range shrank under the animation: its clamped end is arrival.
        assert!(navigation_arrived(1400, 2000, 1400));
        // An end that moved on after the click does not make it further to go.
        assert!(navigation_arrived(2000, 2000, 9000));
    }

    #[test]
    fn the_quiet_interval_is_a_backstop_where_scrollend_exists() {
        assert_eq!(navigation_quiet_ms(false), NAVIGATION_QUIET_MS);
        assert_eq!(
            navigation_quiet_ms(true),
            NAVIGATION_QUIET_WITH_SCROLLEND_MS
        );
        const { assert!(NAVIGATION_QUIET_MS < NAVIGATION_QUIET_WITH_SCROLLEND_MS) };
    }

    #[test]
    fn a_room_starts_at_its_newest_message_unless_it_was_visited() {
        let history = HistoryScroll::default();
        assert_eq!(history.placement.get(), Placement::Latest);

        // Leaving keeps what was captured; entering another room forgets the
        // geometry and starts that room at its newest message.
        *history.anchor.borrow_mut() = Some(row("a1", 120, 900));
        history.sig.set(sig(3000, 600, 1000));
        history.top.set(2000);
        history.navigation.set(Some(2400));
        let left = history.leave_room();
        assert_eq!(left, Some(row("a1", 120, 900)));
        assert_eq!(history.navigation.get(), None);

        let visit = history.visit.get();
        history.enter_room(None);
        assert_ne!(
            history.visit.get(),
            visit,
            "a switch drops trims scheduled before it"
        );
        assert_eq!(history.placement.get(), Placement::Latest);
        assert!(history.anchor.borrow().is_none());
        assert_eq!(history.sig.get(), LayoutSig::default());
        assert_eq!(history.top.get(), 0);

        // Coming back restores the saved anchor first.
        history.enter_room(left);
        assert_eq!(history.placement.get(), Placement::Saved);
        assert_eq!(history.anchor.borrow().as_ref(), Some(&row("a1", 120, 900)));

        // No saved anchor is no position.
        history.enter_room(None);
        assert_eq!(history.placement.get(), Placement::Latest);
        assert!(history.anchor.borrow().is_none());
    }

    #[test]
    fn a_hide_forgets_the_navigation_and_keeps_the_saved_anchor() {
        let history = HistoryScroll::default();
        history.placement.set(Placement::Placed);
        *history.anchor.borrow_mut() = Some(row("m9", 40, 5000));
        history.navigation.set(Some(8000));
        history.note_hidden();
        assert!(history.hidden.get());
        assert_eq!(history.navigation.get(), None);
        assert_eq!(history.anchor.borrow().as_ref(), Some(&row("m9", 40, 5000)));
        assert_eq!(history.placement.get(), Placement::Placed);
    }
}
