//! Where the history's view goes: the reader's position is a message, not an offset.
//!
//! # The saved position
//!
//! We remember the newest row that is visible (`data-anchor-row`) plus up to
//! `ANCHOR_FALLBACK_ROWS` above it. Each [`SavedRow`] keeps its `gap` (the
//! container's bottom edge minus the row's top edge) and its `offset` (the row's
//! top inside `#chat-content`, which scrolling does not change). The gap is
//! measured from the BOTTOM edge on purpose: a growing composer takes height off
//! that edge, so the reader's text moves up with it rather than being covered.
//!
//! One rule holds everywhere, at the very end of the history too: only the
//! reader, or their click on "Scroll to latest messages", moves the view.
//! Arrivals (including the reader's own sends), joins, reactions, edits, late
//! images, resizes and a hide and reveal all put the saved row back at its gap.
//! There is no following mode, and no distance from the end that turns one on.
//!
//! * **Capture** measures the saved rows where the view is now: for a `scroll`
//!   the reader caused, after a room's initial placement, and where a
//!   scroll-to-latest navigation comes to rest or is cut short.
//! * **Restore** puts the first saved row that still exists back at its gap
//!   after a content or layout change. It never captures, so a gap the browser
//!   cannot reach yet (the range is too short) stays saved for when it can.
//!   With no saved row left, the view stays where it is.
//! * **Recording** notes the layout signature and `scrollTop` after anything
//!   moves the view, ours or not, so the next `scroll` event can be classified.
//!
//! An anchor correction is an instant `scrollTop` write. The container keeps
//! `overflow-anchor:none` so browser scroll anchoring does not compete with it,
//! and `scroll-behavior:auto` so those writes stay instant.
//!
//! # Placement
//!
//! A room opened for the first time this session starts at its newest message,
//! once, as soon as it has rows and a laid-out container, and captures there.
//! That is a starting position, not a mode: the next arrival is preserved like
//! any other. Revisiting a room restores the rows saved when the reader left it
//! (`leave_room`; the component restores that room's rendered window too, so
//! the rows exist). If none of them is rendered any more, the revisit is placed
//! like a first open. An empty room places when its first rows arrive. Either
//! way the component is told (`HistoryHooks::positioned`), which is what lets
//! the backfill sentinel mount.
//!
//! # Scroll-to-latest
//!
//! One native smooth `scrollTo` to the end as measured at the click. The browser
//! owns its duration, easing and progress. While it runs:
//!
//! * its `scroll` events capture the position reached (so a hide keeps it);
//! * restores do not write `scrollTop`: content landing below the view moves
//!   nothing on screen, so nothing fights the animation and nothing retargets
//!   it. A reflow ABOVE the view moves the saved row's `offset`; that cancels
//!   the navigation once, keeping the row where the reader last saw it.
//!
//! It finishes at a `scrollend` that finds the view at its destination (or the
//! clamped end), or after `navigation_quiet_ms` with no `scroll` event; the
//! landing is captured, and may be above an end that has moved on since the
//! click. Another click is how the reader asks for that. The reader's own input
//! (`wheel`, `touchstart`, `pointerdown`, `keydown` on the history), a hide, a
//! room switch and a second click each cancel it. Cancelling writes `scrollTop`
//! one pixel off and back: Firefox does not abort a smooth scroll for a write to
//! the position it is already at, and all three engines do for a real move.
//!
//! # Classifying a `scroll` event (`classify_scroll`)
//!
//! A geometry heuristic, not provenance.
//!
//! * **Echo**: the signature and `scrollTop` are exactly as recorded. Nothing
//!   moved that is not already accounted for, so the saved position stays as it
//!   was. Usually our own write's event (or WebKit's duplicate of an event after
//!   a `wheel`), but that is inferred, not proven.
//! * **Layout** (a browser clamp): the recorded `scrollTop` is out of reach of
//!   the live scroll range and the view now sits at its end, whether or not
//!   anything was resized (removing a positioned overhang clamps with nothing
//!   resized); or the signature changed since it was recorded AND `scrollTop`
//!   moved no more than `LAYOUT_SHIFT_ALLOWANCE_PX`, for a clamp taken during a
//!   shorter intermediate layout that ends short of the end; or the container
//!   grew, `scrollTop` went down, and the view's bottom edge stayed where it was
//!   (within rounding), however far: a composer collapsing grows the container
//!   and the browser clamps the top up by the same amount, and content that
//!   arrives before the event is read leaves that clamp short of the end and
//!   past the allowance (a cap-height composer moves it ~400px). It restores.
//! * **Reader**: anything else. It captures.
//!
//! Residuals: a reader who moves less than the allowance in the very frame a
//! layout change lands loses that frame's movement; an intermediate clamp larger
//! than the allowance that also moves the bottom edge is taken as the reader; a
//! reader who scrolls to the end in the frame of a final-end clamp is
//! indistinguishable from it; and so is one who scrolls up by exactly what the
//! container grew in the frame it grows. The signature records the layout's
//! shape (content and container sizes); the scroll range is always read live. It
//! can still be stale when an event is classified: ResizeObserver delivery is
//! asynchronous, a hidden history records nothing, and the sizes read are client
//! sizes, not exactly the boxes the observer watches.
//!
//! # Timing and visibility
//!
//! * **Late scroll events**: a `scroll` event arrives a frame after the scroll,
//!   and a content change can land first. So a restore, the render before a
//!   patch, a room switch and a hide first take in a pending reader scroll
//!   (`take_in_undelivered_scroll`), or the next restore would put the reader
//!   back where their previous event left them.
//! * **Hidden**: the mobile layout hides the history (`display:none`), and every
//!   read is then 0. It stays observed, but nothing measures, records or
//!   restores it until it has height again; the saved rows wait. The first
//!   callback that finds it laid out again (the ResizeObserver, or the reveal's
//!   own `scroll` in desktop WebKit) restores rather than capturing, so a
//!   reveal never turns where the browser put the view into the reader's
//!   choice. A hide also cancels a navigation: the mobile panel buttons call
//!   `before_hide` while the history still has a box to cancel it in, and the
//!   ResizeObserver seeing it hidden forgets the navigation (a breakpoint hide).
//!   A reveal restores; it never resumes an animation.
//!
//! State is `Cell`/`RefCell`, never signals: raw JS callbacks write it.

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

/// How many rows above the newest visible one are remembered as fallbacks, for
/// when the anchor row itself is deleted or windowed out before the restore.
const ANCHOR_FALLBACK_ROWS: usize = 4;

/// The most a `scroll` event may move `scrollTop`, after the layout signature
/// changed, and still be read as the browser's clamp rather than the reader.
///
/// Only needed for a clamp that does NOT end at the final end (see the module
/// doc). The 8px (Linux CI) and 56px clamps quoted for this before came from
/// other trees with CSS size containers, and the 111px one from a synthetic
/// test, so they are context rather than measurements of this code.
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

/// One remembered row: its `data-anchor-row` key, its `gap` (the container's
/// bottom edge minus the row's top edge) and its `offset` (the row's top inside
/// `#chat-content`, which only a reflow above it changes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SavedRow {
    key: String,
    gap: i32,
    offset: i32,
}

/// How far the view must move to put the first saved row that still exists
/// (newest first) back at its saved gap; `current_gap` finds a row's gap now,
/// `None` if it is gone. `None` if no saved row survives, including when none
/// was saved.
fn anchor_delta(
    saved: &[SavedRow],
    mut current_gap: impl FnMut(&str) -> Option<i32>,
) -> Option<i32> {
    saved
        .iter()
        .find_map(|row| current_gap(&row.key).map(|now| row.gap - now))
}

/// Whether the content above the saved position reflowed since it was captured,
/// and by how much: `current_offset` finds a row's offset in the content now.
///
/// `Some(shift)` when the first surviving saved row moved in the content by more
/// than rounding (`shift` is how far down), and `Some(0)` when rows were saved
/// and none survives (deleted or windowed out). `None` when nothing moved, or
/// nothing was saved to tell by. Scrolling changes no offset, so this is blind
/// to the navigation's own movement, and content appended below the rows does
/// not move them either.
fn reflow_above(
    saved: &[SavedRow],
    mut current_offset: impl FnMut(&str) -> Option<i32>,
) -> Option<i32> {
    if saved.is_empty() {
        return None;
    }
    let shift = saved
        .iter()
        .find_map(|row| current_offset(&row.key).map(|now| now - row.offset));
    match shift {
        None => Some(0),
        Some(shift) if shift.abs() > SCROLL_TOP_SLACK_PX => Some(shift),
        Some(_) => None,
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
    /// In place: restores preserve the saved rows.
    Placed,
    /// No saved position: start at the newest message once it can be measured.
    Latest,
    /// Saved when the reader left this room: put those rows back first.
    Saved,
}

/// The history's scroll state. See the module doc.
pub(super) struct HistoryScroll {
    /// Newest visible row first. Empty until something has been captured.
    anchor: RefCell<Vec<SavedRow>>,
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
            anchor: RefCell::new(Vec::new()),
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
    /// The reader is leaving the current room: what to put back when they
    /// return. Takes in their latest movement first and cancels a navigation
    /// where it is, while the DOM is still this room's.
    pub(super) fn leave_room(&self) -> Vec<SavedRow> {
        #[cfg(target_arch = "wasm32")]
        self.settle_before_leaving();
        self.end_navigation();
        self.anchor.borrow().clone()
    }

    /// A room becomes current: start from what `leave_room` saved for it, or
    /// at its newest message when nothing was.
    pub(super) fn enter_room(&self, saved: Option<Vec<SavedRow>>) {
        self.end_navigation();
        self.visit.set(self.visit.get().wrapping_add(1));
        let saved = saved.unwrap_or_default();
        self.placement.set(if saved.is_empty() {
            Placement::Latest
        } else {
            Placement::Saved
        });
        *self.anchor.borrow_mut() = saved;
        self.sig.set(LayoutSig::default());
        self.top.set(0);
    }

    /// Forget a navigation in flight, and its quiet interval. Does not touch
    /// the view: the callers that need the animation stopped do that first.
    fn end_navigation(&self) {
        self.navigation.set(None);
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
fn element_by_id(id: &str) -> Option<web_sys::Element> {
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
    /// and record.
    fn capture(&self, container: &web_sys::Element) {
        if let (Some(list), Some(content)) =
            (self.rows.borrow().as_ref(), self.content.borrow().as_ref())
        {
            // Relative to the container, the frame `newest_visible_rows` works in.
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
                    let top = row.get_bounding_client_rect().top();
                    Some(SavedRow {
                        key: row.get_attribute(ANCHOR_ATTR)?,
                        gap: (view.bottom() - top).round() as i32,
                        offset: (top - content_top).round() as i32,
                    })
                })
                .collect();
        }
        self.record(container);
    }

    /// Capture where the view has come to rest, and trim the window if that is
    /// the bottom.
    fn capture_at_rest(&self, container: &web_sys::Element) {
        self.capture(container);
        self.trim_at_bottom(container);
    }

    /// A view resting AT the bottom is the one moment a window trim is
    /// invisible: the rows it removes are above the view, and the restore that
    /// follows puts the saved row (at the bottom) back at its gap. Gated at
    /// SCROLL_TOP_SLACK_PX of the end, so a reader resting higher keeps the rows
    /// above them. Skipped when the trimmed tail would leave the backfill
    /// sentinel in range of the bottom, or the two oscillate at render speed
    /// (#505; see `trim_would_rearm_backfill`).
    ///
    /// Runs where the reader's own scroll comes to rest at the end, and where a
    /// scroll-to-latest navigation lands there, never for an arrival. Nothing to
    /// trim (not installed, or the window has not grown past its initial size)
    /// reads no geometry.
    fn trim_at_bottom(&self, container: &web_sys::Element) {
        let hooks = self.hooks.borrow();
        let Some(trim) = hooks.as_ref().filter(|h| h.window_overgrown.get()) else {
            return;
        };
        if view_at_end(container)
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
            let (visit, trimmed_visit) = (self.visit.clone(), self.visit.get());
            // Deferred: this runs from a raw JS callback with no Dioxus scope,
            // and `window_items` is a signal the render subscribes to. See
            // .claude/rules/dioxus-signal-safety.md. A room switch in between
            // (`leave_room`'s own take-in can schedule this) owns the window
            // by then: the room switched to may have had its depth restored.
            crate::util::defer(move || {
                if visit.get() != trimmed_visit {
                    return;
                }
                *window_anchor.borrow_mut() = None;
                window_items.set(INITIAL_WINDOW_ITEMS);
            });
        }
    }

    /// Put the view where it belongs after a layout or content change.
    pub(super) fn restore(&self) {
        self.take_in_undelivered_scroll();
        self.restore_now()
    }

    /// Take in a scroll whose event has not arrived yet, before rendering or
    /// restoring: the reader's is captured, a navigation's recorded as its
    /// progress. Layout movement is left for the restore.
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
        } else if self.cause_now(&container) == ScrollCause::Reader {
            self.capture_at_rest(&container);
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
            // animation alone unless something above it reflowed.
            match self.navigation_reflow(&container) {
                Some(shift) => self.cancel_navigation_for_reflow(&container, shift),
                None => self.record(&container),
            }
            return;
        }
        self.restore_anchor(&container);
        self.record(&container);
    }

    /// A room with no saved position starts at its newest message, once it has
    /// any, and that is captured at once.
    fn place_at_latest(&self, container: &web_sys::Element) {
        if !self.has_rows() {
            self.record(container);
            return;
        }
        container.set_scroll_top(container.scroll_height());
        self.capture(container);
        self.placement.set(Placement::Placed);
        self.tell_positioned();
    }

    /// A revisited room puts its saved rows back, once it has rows. If none of
    /// them is rendered, it starts at its newest message instead. Nothing is
    /// captured: a gap out of reach for now stays saved.
    fn place_saved(&self, container: &web_sys::Element) {
        if !self.has_rows() {
            self.record(container);
            return;
        }
        if self.restore_anchor(container) == AnchorRestore::Missing {
            self.place_at_latest(container);
            return;
        }
        self.record(container);
        self.placement.set(Placement::Placed);
        self.tell_positioned();
    }

    fn tell_positioned(&self) {
        let positioned = self.hooks.borrow().as_ref().map(|h| h.positioned.clone());
        if let Some(positioned) = positioned {
            positioned();
        }
    }

    /// Scroll the first saved row that still exists back to its gap. If none
    /// survives (or none was saved), leave the view alone: the next reader
    /// scroll captures a new position.
    fn restore_anchor(&self, container: &web_sys::Element) -> AnchorRestore {
        let view = container.get_bounding_client_rect();
        let delta = anchor_delta(&self.anchor.borrow(), |key| {
            Some(gap(&view, &self.find_row(container, key)?))
        });
        let restored = anchor_restore_for(delta);
        if let (AnchorRestore::Moved, Some(delta)) = (restored, delta) {
            container.set_scroll_top(container.scroll_top() + delta);
        }
        restored
    }

    /// `reflow_above` for the saved rows as rendered now.
    fn navigation_reflow(&self, container: &web_sys::Element) -> Option<i32> {
        let content_top = self
            .content
            .borrow()
            .as_ref()?
            .get_bounding_client_rect()
            .top();
        reflow_above(&self.anchor.borrow(), |key| {
            let row = self.find_row(container, key)?;
            Some((row.get_bounding_client_rect().top() - content_top).round() as i32)
        })
    }

    /// Something above the view reflowed during a navigation: stop it, keeping
    /// the saved row where the reader last saw it (`shift` is how far the
    /// reflow moved it down), and capture there.
    fn cancel_navigation_for_reflow(&self, container: &web_sys::Element, shift: i32) {
        self.end_navigation();
        if shift == 0 {
            stop_native_scroll(container);
        } else {
            // A real move, so it aborts the animation as it compensates.
            container.set_scroll_top(container.scroll_top() + shift);
        }
        self.capture(container);
    }

    /// Stop a navigation where the view is now, and capture there.
    fn stop_navigation_here(&self, container: &web_sys::Element) {
        self.end_navigation();
        stop_native_scroll(container);
        self.capture_at_rest(container);
    }

    /// A `scroll` during a navigation is its progress: capture it, unless the
    /// saved row moved in the content, which only a reflow above it does.
    fn on_navigation_scroll(&self, container: &web_sys::Element) {
        if let Some(shift) = self.navigation_reflow(container) {
            self.cancel_navigation_for_reflow(container, shift);
            return;
        }
        self.capture(container);
        self.arm_navigation_timer();
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

    /// The navigation has come to rest: capture where it landed. A `scrollend`
    /// counts only at the destination (`navigation_arrived`); the quiet
    /// interval ends it wherever it is. Hidden, the hide has ended it already.
    fn finish_navigation(&self, native_end: bool) {
        let Some(destination) = self.navigation.get() else {
            return;
        };
        let Some(container) = self.laid_out_container() else {
            return;
        };
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
        if let (Some(handle), Some(window)) = (self.navigation_timer.take(), web_sys::window()) {
            window.clear_timeout_with_handle(handle);
        }
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

    /// `before_hide`, for a room switch: the DOM is still the old room's.
    fn settle_before_leaving(&self) {
        self.before_hide();
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

    fn row(key: &str, gap: i32, offset: i32) -> SavedRow {
        SavedRow {
            key: key.into(),
            gap,
            offset,
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
        // Capture runs on every reader scroll event, and each rect is a layout read.
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
        let saved = vec![row("m3", 300, 0), row("m2", 500, 0), row("m1", 700, 0)];
        // Every saved row gone, or none saved: nothing to put back.
        assert_eq!(anchor_delta(&saved, |_| None), None);
        assert_eq!(anchor_delta(&[], |_| Some(0)), None);
        // The newest survivor decides, wherever it is in the list.
        let only_m2 = |key: &str| (key == "m2").then_some(460);
        assert_eq!(anchor_delta(&saved, only_m2), Some(40));
        let all = |key: &str| Some(if key == "m3" { 280 } else { 0 });
        assert_eq!(anchor_delta(&saved, all), Some(20));
        // A survivor already at its gap needs no write, which is not missing.
        assert_eq!(anchor_delta(&saved, |_| Some(300)), Some(0));

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
    fn only_a_reflow_above_the_saved_rows_counts_during_a_navigation() {
        let saved = vec![row("m3", 300, 2000), row("m2", 500, 1800)];
        // Scrolling and content appended below move no offset.
        assert_eq!(
            reflow_above(&saved, |k| Some(if k == "m3" { 2000 } else { 1800 })),
            None
        );
        assert_eq!(
            reflow_above(&saved, |_| Some(2000 + SCROLL_TOP_SLACK_PX)),
            None
        );
        // Something above grew or shrank: the newest survivor says by how much.
        assert_eq!(reflow_above(&saved, |_| Some(2120)), Some(120));
        assert_eq!(
            reflow_above(&saved, |k| (k == "m2").then_some(1700)),
            Some(-100)
        );
        // Every saved row gone is a reflow too, with nothing to compensate by.
        assert_eq!(reflow_above(&saved, |_| None), Some(0));
        // Nothing saved: nothing to tell by, so nothing to cancel for.
        assert_eq!(reflow_above(&[], |_| None), None);
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
        *history.anchor.borrow_mut() = vec![row("a1", 120, 900)];
        history.sig.set(sig(3000, 600, 1000));
        history.top.set(2000);
        history.navigation.set(Some(2400));
        let left = history.leave_room();
        assert_eq!(left, vec![row("a1", 120, 900)]);
        assert_eq!(history.navigation.get(), None);

        let visit = history.visit.get();
        history.enter_room(None);
        assert_ne!(
            history.visit.get(),
            visit,
            "a switch drops trims scheduled before it"
        );
        assert_eq!(history.placement.get(), Placement::Latest);
        assert!(history.anchor.borrow().is_empty());
        assert_eq!(history.sig.get(), LayoutSig::default());
        assert_eq!(history.top.get(), 0);

        // Coming back restores the saved rows first.
        history.enter_room(Some(left));
        assert_eq!(history.placement.get(), Placement::Saved);
        assert_eq!(&*history.anchor.borrow(), &[row("a1", 120, 900)]);

        // An empty saved position is no position.
        history.enter_room(Some(Vec::new()));
        assert_eq!(history.placement.get(), Placement::Latest);
    }

    #[test]
    fn a_hide_forgets_the_navigation_and_keeps_the_saved_rows() {
        let history = HistoryScroll::default();
        history.placement.set(Placement::Placed);
        *history.anchor.borrow_mut() = vec![row("m9", 40, 5000)];
        history.navigation.set(Some(8000));
        history.note_hidden();
        assert!(history.hidden.get());
        assert_eq!(history.navigation.get(), None);
        assert_eq!(&*history.anchor.borrow(), &[row("m9", 40, 5000)]);
        assert_eq!(history.placement.get(), Placement::Placed);
    }
}
