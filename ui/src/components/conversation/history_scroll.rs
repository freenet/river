//! Where the history's view goes: the reader's position is a message, not an offset.
//!
//! We remember the newest row that is visible (`data-anchor-row`) plus up to
//! `ANCHOR_FALLBACK_ROWS` above it, each with its `gap` (the container's bottom
//! edge minus the row's top edge). Layout changes put that row back at its gap;
//! they never re-measure what the reader meant. Measured from the BOTTOM edge on
//! purpose: a growing composer takes height off that edge, so a parked reader's
//! text moves up with it rather than being covered.
//!
//! Three things are kept apart. **Recording** notes the layout signature and
//! `scrollTop` after anything moves the view, ours or not, so the next `scroll`
//! event can be classified. **Capture** takes the reader's position as their
//! intent: the anchor and the pin (within `BOTTOM_THRESHOLD_PX` of the end).
//! **Restore** puts the view where that intent says after a layout or content
//! change, and never changes the anchor or clears the pin.
//!
//! # Follow states
//!
//! | State | Restore does | Ends when |
//! |---|---|---|
//! | `Free` | pinned: snap to the bottom; else the anchor back at its gap | a reader scroll away from the end (`Gesture`), or the button (`Seeking`) |
//! | `Gesture` | as `Free` until `held`; then the anchor back at its gap, even when pinned, never a snap | the gesture settles, or comes back to the end |
//! | `Seeking` | keeps the animation's frame loop running; never an anchor write | it reaches the end, the reader takes over (`Gesture`), a touch, a force, a room switch |
//!
//! * **`Seeking`** is the scroll-to-latest button's animation: our own
//!   `requestAnimationFrame` loop, each frame stepping towards the live end
//!   (`seek_advance`), so an arrival only makes the next step larger. Its frames
//!   are recorded, so their `scroll` events are echoes and cannot unpin the
//!   reader who asked to follow; a Reader event during a seek is the reader's,
//!   and takes over once it has moved up (as `held` below) from the seek's own
//!   `from`: the view at the press, shifted by every frame and layout
//!   correction since but never by the reader, so their 1px moves between
//!   frames add up. The gesture goes on from that `from`. Reaching the end
//!   finishes it and trims, as a snap does. A `touchstart` stops it where it is.
//!   Whatever ends it, or a hide, stops the loop at its next frame. It never
//!   reports an opening snap: only an instant snap does.
//! * **`Gesture`** remembers where it started (`from`, both edges of the view).
//!   It is `held` once both have moved up past rounding from there (`moved_up`),
//!   so an arrival does not yank a reader who has started to look back inside
//!   the band. A layout correction shifts `from` by what it moved. Back at the
//!   end, it is `Free` again at once.
//! * **Settle** (`scrollend`, or the reader's quiet deadline) ends a gesture:
//!   it measures the pin and anchor where the view came to rest, as `main` did,
//!   and does not snap. The quiet deadline is `SCROLL_SETTLE_DEBOUNCE_MS` after
//!   the reader's last move, never after our own work: a reader move restarts a
//!   pending one, while corrections, their echoes and layout work leave it
//!   where it is. Where the browser has no `scrollend`, every reader move arms
//!   it; the deadline cannot tell a paused finger from a lifted one. A
//!   `scrollend` exactly where our latest anchor correction in this gesture
//!   left the view (one that actually moved it; a no-op restore records
//!   nothing), with no reader move since, may be that write's own end, so it
//!   does not settle: the gesture stays held and the deadline is armed instead
//!   (if it is not already), and every such end is refused until the reader
//!   moves or the gesture ends. That is a geometry match, not provenance: the
//!   reader's last end coalesced with the correction looks the same, which is
//!   what the deadline is for. A settle that does end the gesture first puts
//!   back a reflow the ResizeObserver has not reported yet (the signature
//!   differs from the record: an image loading above the view moves the rows,
//!   not `scrollTop`), against the existing anchor, and only then measures.
//!   With no gesture in progress a settle does nothing, which is what makes a
//!   stale `scrollend` (another room's, an old gesture's, a seek frame's)
//!   harmless: room switches, forced snaps and new seeks end the gesture, and
//!   forget its correction and deadline.
//!
//! # Where capture runs
//!
//! A `scroll` classified as the reader's (from the listener or taken in early,
//! below), a gesture's settle, and a touch that stops a seek. The last two
//! capture directly, never through `on_scroll`: the position has usually been
//! recorded already and would classify as an echo.
//!
//! # Classifying a `scroll` event (`classify_scroll`)
//!
//! A geometry heuristic, not provenance.
//!
//! * **Echo**: the signature and `scrollTop` are exactly as recorded. Nothing
//!   moved that is not already accounted for, so the reader's intent stays as it
//!   was. Usually our own write's event (or WebKit's duplicate of an event after
//!   a `wheel`), but that is inferred, not proven. At the bottom it still trims.
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
//! * **Reader**: anything else, handled by the follow state above.
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
//!   and a content change can land first; WebKit can even send a `scrollend`
//!   before the `scroll` of the move it ends. So `restore`, a settle, and the
//!   render before a patch first read a pending reader scroll
//!   (`take_in_undelivered_scroll`, through `on_scroll`), or a stale pin would
//!   drag the reader back down, an anchor would be measured after the patch had
//!   moved the rows, or a gesture would start that nothing settles. The take-in
//!   belongs where we run outside the rendering steps (effects, `scrollend`, the
//!   render body); a rAF callback such as the seek frame runs after the scroll
//!   steps have dispatched the event, so it has nothing to take in.
//! * **Hidden**: the mobile layout hides the history (`display:none`), and every
//!   read is then 0. It stays observed, but nothing measures, records or
//!   restores it until it has height again; the pin, the anchor and any pending
//!   force wait. The ResizeObserver's restore on reveal picks up where it was,
//!   restarts a seek the hide cut short, and then finishes the settle of a
//!   gesture the hide ended (engines send no `scrollend` for it), after putting
//!   the anchor back. A quiet deadline that passes while hidden waits the same
//!   way. Desktop WebKit sends a `scroll` and `scrollend` of its own for the
//!   reveal, which can settle first; it takes the same restore-first path.
//! * **Touch momentum**: a restore that writes `scrollTop` (an anchor
//!   correction) can cut a touch fling short. Holding follows during a gesture
//!   avoids the snaps, not the corrections; this is untested on real devices.
//!
//! **Why this can't latch as #486 did**: the pin comes only from the reader's
//! own positions. Growing content, a growing composer or a rewrap restore
//! instead of measuring, so none of them can clear it, and every gesture ends in
//! a settle that measures again.
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

/// The quiet interval a gesture settles after when no native end does: after
/// the reader's last move, where the browser has no `scrollend` (Safari before
/// 17.4, as on `main`), and after a native end that may be our correction's own.
const SCROLL_SETTLE_DEBOUNCE_MS: i32 = 120;

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

/// `origin` with our own work taken out: shifted by what that work (a seek frame,
/// a restore) moved the view, from `before` to `after`, edge by edge.
fn shift_origin(origin: ViewEdges, before: ViewEdges, after: ViewEdges) -> ViewEdges {
    ViewEdges {
        top: origin.top + after.top - before.top,
        bottom: origin.bottom + after.bottom - before.bottom,
    }
}

/// A reader scroll to `now` during a seek whose direction origin is `from`:
/// once it has moved up from there it is the reader's gesture, from that same
/// origin and already held; until then the seek carries on with its origin
/// unchanged, so the reader's next move adds to this one.
fn seek_after_reader_scroll(from: ViewEdges, now: ViewEdges) -> Follow {
    if moved_up(from, now) {
        Follow::Gesture { from, held: true }
    } else {
        Follow::Seeking { from }
    }
}

/// Our latest anchor correction during a gesture: the view it left, and the
/// reader revision (`HistoryScroll::reader_rev`) it was made in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Correction {
    edges: ViewEdges,
    reader_rev: u32,
}

/// What says a scroll has come to rest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SettleCause {
    /// The browser's `scrollend`.
    Native,
    /// The reader's quiet deadline: `SCROLL_SETTLE_DEBOUNCE_MS` after their last move.
    Quiet,
}

/// Whether a settle from `cause` ends the gesture, with the view at `now` and
/// `reader_rev` the current reader revision. A quiet deadline always does, even
/// at the corrected view: the reader has stopped. A native end with the view's
/// top exactly where our latest correction left it, with no reader move since,
/// may be that correction's own, so it does not; it cannot be told from the
/// reader's last end coalesced with it, which is why the quiet deadline backs it
/// up. The top only, exactly: the bottom edge also moves when the container is
/// resized (the composer growing) before the observer has reported it, and that
/// is not anyone moving the view.
fn settle_ends_gesture(
    cause: SettleCause,
    correction: Option<Correction>,
    now: ViewEdges,
    reader_rev: u32,
) -> bool {
    cause == SettleCause::Quiet
        || correction.is_none_or(|c| c.edges.top != now.top || c.reader_rev != reader_rev)
}

/// Whether a settle that ends the gesture must put the view back first: a
/// gesture is in progress, no forced snap is pending (the restore it is owed
/// snaps anyway), and the layout is not the one last recorded. That is a reflow
/// the ResizeObserver has not reported yet, such as an image loading above the
/// view, which moves the reader's rows without moving `scrollTop`; measured as
/// it is, the settle would save where the reflow pushed them. An unchanged
/// layout has nothing to put back.
fn settle_restores_first(follow: Follow, force: bool, recorded: LayoutSig, now: LayoutSig) -> bool {
    matches!(follow, Follow::Gesture { .. }) && !force && recorded != now
}

/// How long from `now_ms` until the quiet deadline of a reader whose last move
/// was at `last_move_ms`: `SCROLL_SETTLE_DEBOUNCE_MS` after that move, so our
/// own work since does not restart it. Never negative, never longer.
fn quiet_deadline_in(last_move_ms: f64, now_ms: f64) -> i32 {
    let full = f64::from(SCROLL_SETTLE_DEBOUNCE_MS);
    (full - (now_ms - last_move_ms)).clamp(0.0, full).ceil() as i32
}

/// Whether `top` is within rounding of the live end `max`.
fn at_end(top: i32, max: i32) -> bool {
    max - top <= SCROLL_TOP_SLACK_PX
}

/// The scroll-to-latest animation's time constant: each frame covers
/// `1 - exp(-dt / SEEK_TAU_MS)` of what is left. Our own frames, because native
/// smooth scrolling restarted its ease-in on every re-issue in Chromium (317px a
/// frame down to 0-2px) and dipped in WebKit, so an arrival mid-flight stalled it.
const SEEK_TAU_MS: f64 = 80.0;
/// Its slowest speed, so the ease-out has no long tail near the end.
const SEEK_MIN_PX_PER_MS: f64 = 1.5;
/// The frame interval a step is computed for. A zero interval still moves, and
/// a frame after a long stall moves no further than a 64ms one.
const SEEK_MIN_DT_MS: f64 = 1.0;
const SEEK_MAX_DT_MS: f64 = 64.0;

/// How far the scroll-to-latest animation moves in one frame of `dt_ms`, with
/// `remaining` px to the live end: an exponential ease-out with a speed floor,
/// at least 1px (rounded up, so it always progresses) and never past the end.
///
/// The speed is proportional to what is left, so an arrival that moves the end
/// away makes the next step larger rather than starting the animation over:
/// about 430ms for 10,000px and 200ms for 500px at 60fps.
fn seek_advance(remaining: i32, dt_ms: f64) -> i32 {
    if remaining <= 0 {
        return 0;
    }
    let dt = dt_ms.clamp(SEEK_MIN_DT_MS, SEEK_MAX_DT_MS);
    let remaining_px = f64::from(remaining);
    let eased = remaining_px * (1.0 - (-dt / SEEK_TAU_MS).exp());
    let floor = remaining_px.min(SEEK_MIN_PX_PER_MS * dt);
    (eased.max(floor).ceil() as i32).clamp(1, remaining)
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

/// What drives the view between reader scrolls (the module doc's table).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Follow {
    /// The pin and the anchor drive restores.
    Free,
    /// A reader gesture: `from` is the view before its first move, shifted by
    /// every layout correction since; `held` once it has moved up from there.
    Gesture { from: ViewEdges, held: bool },
    /// The scroll-to-latest animation is travelling to the end. `from` is the
    /// view when it started, shifted by every frame and layout correction since,
    /// so the reader's own moves are measured from it as a gesture's are.
    Seeking { from: ViewEdges },
}

impl Follow {
    /// Our own work (a seek frame, a restore) moved the view from `before` to
    /// `after`: take that out of a gesture's or a seek's direction origin.
    fn after_own_work(self, before: ViewEdges, after: ViewEdges) -> Self {
        match self {
            Follow::Gesture { from, held } => Follow::Gesture {
                from: shift_origin(from, before, after),
                held,
            },
            Follow::Seeking { from } => Follow::Seeking {
                from: shift_origin(from, before, after),
            },
            Follow::Free => Follow::Free,
        }
    }
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
    /// A gesture ended while the history was hidden; finish it on the first
    /// laid-out restore.
    settle_pending: Cell<bool>,
    /// The reader's quiet deadline (see "Settle"), while one is pending.
    settle_timer: Cell<Option<i32>>,
    /// The deadline's callback, made once by `install`.
    settle_quiet: RefCell<Option<js_sys::Function>>,
    /// Whether the browser sends `scrollend`; without it every reader move arms
    /// the quiet deadline.
    native_settle: Cell<bool>,
    /// Counts the reader's moves, so a correction can say which one it followed.
    reader_rev: Cell<u32>,
    /// When the reader last moved, on the page's clock (ms).
    last_reader_move: Cell<f64>,
    /// Our latest anchor correction in the gesture in progress.
    correction: Cell<Option<Correction>>,
    /// The scroll-to-latest animation's frame callback, made once by `install`.
    seek_frame: RefCell<Option<js_sys::Function>>,
    /// The animation frame asked for and not run yet, so `seek` never asks twice.
    seek_raf: Cell<Option<i32>>,
    /// When the animation's previous frame ran; `None` before a seek's first
    /// frame, and after the loop stops.
    seek_prev_t: Cell<Option<f64>>,
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
            pinned: Cell::new(true),
            force: Cell::new(false),
            sig: Cell::new(LayoutSig::default()),
            top: Cell::new(0),
            follow: Cell::new(Follow::Free),
            settle_pending: Cell::new(false),
            settle_timer: Cell::new(None),
            settle_quiet: RefCell::new(None),
            native_settle: Cell::new(true),
            reader_rev: Cell::new(0),
            last_reader_move: Cell::new(0.0),
            correction: Cell::new(None),
            seek_frame: RefCell::new(None),
            seek_raf: Cell::new(None),
            seek_prev_t: Cell::new(None),
            container: RefCell::new(None),
            content: RefCell::new(None),
            rows: RefCell::new(None),
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
        self.settle_pending.set(false);
        self.correction.set(None);
        self.cancel_settle_timer();
    }

    /// The reader moved the view: a new revision, which no earlier correction
    /// matches.
    fn note_reader_move(&self) {
        self.reader_rev.set(self.reader_rev.get().wrapping_add(1));
    }

    /// An anchor correction moved the view to `edges`. Only a gesture's: with
    /// none in progress there is nothing for its end to settle.
    fn note_correction(&self, edges: ViewEdges) {
        if matches!(self.follow.get(), Follow::Gesture { .. }) {
            self.correction.set(Some(Correction {
                edges,
                reader_rev: self.reader_rev.get(),
            }));
        }
    }

    /// Whether a settle from `cause`, with the view at `now`, ends the gesture.
    fn settle_allowed(&self, cause: SettleCause, now: ViewEdges) -> bool {
        settle_ends_gesture(cause, self.correction.get(), now, self.reader_rev.get())
    }

    fn cancel_settle_timer(&self) {
        #[cfg(target_arch = "wasm32")]
        if let (Some(handle), Some(window)) = (self.settle_timer.take(), web_sys::window()) {
            window.clear_timeout_with_handle(handle);
        }
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

    /// The view's edges as last recorded.
    fn recorded_edges(&self) -> ViewEdges {
        let top = self.top.get();
        ViewEdges {
            top,
            bottom: top + self.sig.get().client_height,
        }
    }

    /// The view's edges as they are now.
    fn live_edges(&self, container: &web_sys::Element) -> ViewEdges {
        let top = container.scroll_top();
        ViewEdges {
            top,
            bottom: top + container.client_height(),
        }
    }

    /// Record the layout and `scrollTop` as they are now. Read back after any write,
    /// since the browser clamps.
    fn record(&self, container: &web_sys::Element) {
        self.sig.set(self.read_sig(container));
        self.top.set(container.scroll_top());
    }

    /// Our own work moved the view from `before` to where it is now recorded:
    /// take that out of the follow state's direction origin.
    fn take_out_own_work(&self, before: ViewEdges) {
        let after = self.recorded_edges();
        self.follow
            .set(self.follow.get().after_own_work(before, after));
    }

    /// Take the reader's position as the new truth (see "Where capture runs").
    fn capture(&self, container: &web_sys::Element) {
        let distance = (max_scroll_top(container) - container.scroll_top()) as f64;
        self.pinned.set(is_pinned(distance));
        if let Some(list) = self.rows.borrow().as_ref() {
            // Relative to the container, the frame `newest_visible_rows` works in.
            // Each call is a layout read, and the search makes few of them.
            let view = container.get_bounding_client_rect();
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
                    Some((row.get_attribute(ANCHOR_ATTR)?, gap(&view, &row)))
                })
                .collect();
        }
        self.record(container);
        self.trim_at_bottom(container);
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

    /// Read a reader scroll whose `scroll` event has not arrived yet (see "Late
    /// scroll events"): the view has moved by more than a layout clamp could
    /// since it was recorded.
    pub(super) fn take_in_undelivered_scroll(&self) {
        let Some(container) = self.laid_out_container() else {
            return;
        };
        if container.scroll_top() == self.top.get() {
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
            self.read_sig(container),
            self.top.get(),
            container.scroll_top(),
            max_scroll_top(container),
        )
    }

    fn restore_now(&self) {
        // Hidden: everything waits for the reveal, including the settle of a
        // gesture the hide ended (a no-op if there was none).
        let Some(container) = self.laid_out_container() else {
            self.settle_pending.set(true);
            return;
        };
        if self.force.take() {
            self.end_interaction();
            self.snap_and_tell(&container);
            return;
        }
        self.restore_position(&container);
        if self.settle_pending.take() {
            self.end_gesture();
        }
    }

    /// The view where the follow state says, with what that moved taken out of
    /// its direction origin. The caller owns a forced snap and a pending settle.
    fn restore_position(&self, container: &web_sys::Element) {
        let before = self.recorded_edges();
        match self.follow.get() {
            Follow::Seeking { .. } => self.seek(container),
            Follow::Free | Follow::Gesture { held: false, .. } if self.pinned.get() => {
                self.snap_and_tell(container)
            }
            Follow::Free | Follow::Gesture { .. } => {
                let moved = self.restore_anchor(container);
                self.record(container);
                if moved {
                    self.note_correction(self.recorded_edges());
                }
            }
        }
        // Everything that moved the view since the last record was layout (a
        // pending reader move was taken in first), so it is not the gesture's
        // or the seek's.
        self.take_out_own_work(before);
    }

    /// Snap to the bottom at once and re-arm the pin. Scrolls the container
    /// itself: `scrollIntoView` on the last bubble aligns its top and could leave
    /// the real bottom (reactions, sentinel, padding) off-screen.
    fn snap_instant(&self, container: &web_sys::Element) {
        self.pinned.set(true);
        container.set_scroll_top(container.scroll_height());
        self.record(container);
    }

    /// Snap, and tell the component it happened (the opening snap's completion,
    /// with its own room check).
    fn snap_and_tell(&self, container: &web_sys::Element) {
        self.snap_instant(container);
        let snapped = self
            .hooks
            .borrow()
            .as_ref()
            .map(|h| h.snapped_to_bottom.clone());
        if let Some(snapped) = snapped {
            snapped();
        }
    }

    /// The scroll-to-latest button: re-arm the pin (asking for the newest
    /// message is the clearest statement of intent there is) and start `Seeking`.
    pub(super) fn seek_to_latest(&self) {
        let Some(container) = self.laid_out_container() else {
            return;
        };
        self.pinned.set(true);
        self.end_interaction();
        self.follow.set(Follow::Seeking {
            from: self.live_edges(&container),
        });
        self.seek_prev_t.set(None);
        self.seek(&container);
    }

    /// Keep the scroll-to-latest animation running; its next frame steps or
    /// finishes it. Idempotent (every frame reads the live end), and restarts a
    /// loop a hide stopped.
    fn seek(&self, container: &web_sys::Element) {
        self.request_seek_frame();
        self.record(container);
    }

    /// The scroll-to-latest animation has reached the end: following again, and
    /// a bottom, so it trims as a snap does.
    fn finish_seek(&self, container: &web_sys::Element) {
        self.follow.set(Follow::Free);
        self.record(container);
        self.trim_at_bottom(container);
    }

    fn request_seek_frame(&self) {
        if self.seek_raf.get().is_some() {
            return;
        }
        let frame = self.seek_frame.borrow().clone();
        let (Some(window), Some(frame)) = (web_sys::window(), frame) else {
            return;
        };
        if let Ok(handle) = window.request_animation_frame(&frame) {
            self.seek_raf.set(Some(handle));
        }
    }

    /// One frame of the scroll-to-latest animation: a step towards the live end,
    /// recorded so that its own `scroll` event is an echo. Stops unless still
    /// `Seeking` and laid out.
    fn on_seek_frame(&self, t: f64) {
        self.seek_raf.set(None);
        let prev_t = self.seek_prev_t.take();
        if !matches!(self.follow.get(), Follow::Seeking { .. }) {
            return;
        }
        let Some(container) = self.laid_out_container() else {
            return;
        };
        // From the record, as a restore does: a container resize since then is
        // ours to take out too. The reader's moves are in it already, since
        // their `scroll` events run before animation frames.
        let before = self.recorded_edges();
        let dt = prev_t.map_or(16.0, |prev| t - prev);
        let top = container.scroll_top();
        let max = max_scroll_top(&container);
        container.set_scroll_top(top + seek_advance(max - top, dt));
        let moved = container.scroll_top() != top;
        if !moved {
            // Rounding at a fractional device scale can swallow a small step.
            container.set_scroll_top(max);
        }
        if !moved || view_at_end(&container) {
            self.finish_seek(&container);
        } else {
            self.record(&container);
            self.take_out_own_work(before);
            self.seek_prev_t.set(Some(t));
            self.request_seek_frame();
        }
    }

    /// Scroll the first anchor row that still exists back to its gap. If none
    /// survives, leave the view alone: the next reader scroll captures a new one.
    /// Whether that moved the view.
    fn restore_anchor(&self, container: &web_sys::Element) -> bool {
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
            if delta.abs() <= SCROLL_TOP_SLACK_PX {
                return false;
            }
            let top = container.scroll_top();
            container.set_scroll_top(top + delta);
            return container.scroll_top() != top;
        }
        false
    }

    /// Read a `scroll` event as layout's doing (restore) or the reader's (capture).
    fn on_scroll(&self) {
        let Some(container) = self.laid_out_container() else {
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

    /// A scroll nothing else accounts for: the reader's. During a seek it takes
    /// over only once it has moved up from the seek's origin; anything else is
    /// recorded and left to the next frame, the origin kept as it was.
    fn on_reader_scroll(&self, container: &web_sys::Element) {
        self.note_reader_move();
        self.last_reader_move.set(js_sys::Date::now());
        // Without `scrollend` every move arms the quiet deadline; with it, only
        // a deadline already pending (a correction's end was refused) restarts.
        if !self.native_settle.get() || self.settle_timer.get().is_some() {
            self.arm_quiet_deadline();
        }
        if let Follow::Seeking { from } = self.follow.get() {
            let follow = seek_after_reader_scroll(from, self.live_edges(container));
            self.follow.set(follow);
            if matches!(follow, Follow::Seeking { .. }) {
                self.record(container);
                return;
            }
            // Taken over: the gesture goes on from the seek's origin.
        }
        let (from, held) = match self.follow.get() {
            Follow::Gesture { from, held } => (from, held),
            _ => (self.recorded_edges(), false),
        };
        self.capture(container);
        let follow = if at_end(self.top.get(), max_scroll_top(container)) {
            // Back at the end: following again, whatever came before.
            self.correction.set(None);
            self.cancel_settle_timer();
            Follow::Free
        } else {
            let held = held || moved_up(from, self.recorded_edges());
            Follow::Gesture { from, held }
        };
        self.follow.set(follow);
    }

    /// The browser says a scroll has come to rest: see "Settle". Not if the end
    /// may be our latest correction's own; then the gesture stays held until the
    /// reader's quiet deadline, which is armed if it is not already (and is not
    /// moved: it runs from the reader's last move).
    fn settle_native(&self) {
        self.take_in_undelivered_scroll();
        if let Some(container) = self.laid_out_container() {
            if !self.settle_allowed(SettleCause::Native, self.live_edges(&container)) {
                if self.settle_timer.get().is_none() {
                    self.arm_quiet_deadline();
                }
                return;
            }
        }
        self.settle_eligible();
    }

    /// The reader's quiet deadline has passed: the gesture settles wherever the
    /// view is. Hidden, it waits for the reveal's restore, as any settle does.
    /// Unless the take-in finds the reader moved since their last delivered
    /// scroll (a new revision) and the gesture goes on: that move is the
    /// reader's latest, so a full quiet interval runs from it instead. Armed
    /// here explicitly, since with `scrollend` the move's own intake does not
    /// (the fired handle is already gone); a move that came back to the end has
    /// made the follow `Free` and settles nothing.
    fn settle_quiet(&self) {
        // The callback running now has fired.
        self.settle_timer.set(None);
        let reader_rev = self.reader_rev.get();
        self.take_in_undelivered_scroll();
        if self.reader_rev.get() != reader_rev
            && matches!(self.follow.get(), Follow::Gesture { .. })
        {
            self.arm_quiet_deadline();
            return;
        }
        self.settle_eligible();
    }

    /// A settle that ends the gesture, the reader's pending scroll taken in. A
    /// reflow no observer has reported yet is put back first, against the
    /// reader's existing anchor (`settle_restores_first`), so the capture
    /// measures the reader's place and not where the reflow pushed their rows.
    /// That restore may correct the view; the gesture ends right after, which
    /// drops the record, so the correction's own end meets no gesture.
    fn settle_eligible(&self) {
        self.cancel_settle_timer();
        if let Some(container) = self.laid_out_container() {
            if settle_restores_first(
                self.follow.get(),
                self.force.get(),
                self.sig.get(),
                self.read_sig(&container),
            ) {
                self.restore_position(&container);
            }
        }
        self.end_gesture();
    }

    /// Arm the quiet deadline, `SCROLL_SETTLE_DEBOUNCE_MS` after the reader's
    /// last move, replacing any pending one.
    fn arm_quiet_deadline(&self) {
        self.cancel_settle_timer();
        let callback = self.settle_quiet.borrow().clone();
        let (Some(window), Some(callback)) = (web_sys::window(), callback) else {
            return;
        };
        let delay = quiet_deadline_in(self.last_reader_move.get(), js_sys::Date::now());
        if let Ok(handle) =
            window.set_timeout_with_callback_and_timeout_and_arguments_0(&callback, delay)
        {
            self.settle_timer.set(Some(handle));
        }
    }

    /// The gesture is over: where it came to rest decides the pin. Does not
    /// snap. Hidden, it waits for the reveal's restore. Completing it cancels
    /// its quiet deadline: a reveal ends a gesture with no settle of its own,
    /// and a handle left behind would be taken for the next gesture's.
    fn end_gesture(&self) {
        if !matches!(self.follow.get(), Follow::Gesture { .. }) {
            return;
        }
        let Some(container) = self.laid_out_container() else {
            self.settle_pending.set(true);
            return;
        };
        self.cancel_settle_timer();
        self.follow.set(Follow::Free);
        self.correction.set(None);
        self.capture(&container);
    }

    /// A finger on the history stops the scroll-to-latest animation where it is:
    /// that is where the reader now is.
    fn on_touch_start(&self) {
        if !matches!(self.follow.get(), Follow::Seeking { .. }) {
            return;
        }
        let Some(container) = self.laid_out_container() else {
            return;
        };
        self.follow.set(Follow::Free);
        self.capture(&container);
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

        // The ResizeObserver sees the content growing or reflowing, and the
        // container shrinking (the composer growing, #486's third cause), and
        // a hidden container getting its height back. All restore; none may
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
        *self.seek_frame.borrow_mut() = Some(
            Closure::<dyn FnMut(f64)>::new(move |t| this.on_seek_frame(t))
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

        let this = self.clone();
        *self.settle_quiet.borrow_mut() = Some(
            Closure::<dyn FnMut()>::new(move || this.settle_quiet())
                .into_js_value()
                .unchecked_into(),
        );
        let native =
            js_sys::Reflect::has(&container, &JsValue::from_str("onscrollend")).unwrap_or(false);
        self.native_settle.set(native);
        if native {
            let this = self.clone();
            listen(
                "scrollend",
                &Closure::<dyn FnMut()>::new(move || this.settle_native()).into_js_value(),
            );
        }

        let this = self.clone();
        listen(
            "touchstart",
            &Closure::<dyn FnMut()>::new(move || this.on_touch_start()).into_js_value(),
        );
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
        // The documented residual: a larger intermediate clamp that moves the
        // bottom edge too (the container is unchanged here) reads as the reader.
        assert_eq!(
            classify_scroll(a, b, 2000, 2000 - LAYOUT_SHIFT_ALLOWANCE_PX - 1, max),
            ScrollCause::Reader
        );
        // And with nothing in the signature changed, it is the reader at any size.
        assert_eq!(classify_scroll(a, a, 2000, 1889, max), ScrollCause::Reader);
    }

    /// A pinned view at the end of a 3000px history in a 400px container (top
    /// 2600, bottom edge 3000), then a 406px composer collapse that the browser
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
        // stays put. `classify_scroll` reads that alone as layout; with any other
        // movement in the same event it reaches the reader branch, where only
        // the reader's own part may count as moving up.
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

    /// Frames of `dt_ms` until a seek that starts `remaining` px from the end
    /// gets there.
    fn frames_to_arrive(mut remaining: i32, dt_ms: f64) -> usize {
        let mut frames = 0;
        while remaining > 0 {
            remaining -= seek_advance(remaining, dt_ms);
            frames += 1;
            assert!(frames < 1_000, "the seek never arrived");
        }
        frames
    }

    #[test]
    fn a_seek_frame_always_progresses_and_never_overshoots() {
        for dt in [1.0, 16.0, 33.0, 64.0] {
            for remaining in [1, 2, 3, 7, 50, 119, 120, 121, 500, 10_000, 1_000_000] {
                let step = seek_advance(remaining, dt);
                assert!(
                    (1..=remaining).contains(&step),
                    "{remaining}px at {dt}ms stepped {step}px"
                );
            }
        }
        // Already there, or past it (a range that just shrank): nothing to do.
        assert_eq!(seek_advance(0, 16.0), 0);
        assert_eq!(seek_advance(-40, 16.0), 0);
    }

    #[test]
    fn a_seek_has_no_long_tail() {
        // 60fps frames: a long trip and a short one both finish quickly.
        assert!(frames_to_arrive(10_000, 16.0) < 40);
        assert!(frames_to_arrive(500, 16.0) < 16);
    }

    #[test]
    fn a_seek_goes_faster_when_the_end_moves_away() {
        // An arrival makes the remaining distance larger: the next step must
        // not be smaller than it would have been, or the animation stalls.
        for dt in [1.0, 16.0, 64.0] {
            let mut prev = 0;
            for remaining in 1..20_000 {
                let step = seek_advance(remaining, dt);
                assert!(
                    step >= prev,
                    "at {dt}ms, {remaining}px stepped {step}px, less than {prev}px"
                );
                prev = step;
            }
        }
    }

    #[test]
    fn a_seek_frames_length_is_clamped() {
        for remaining in [5, 300, 8_000] {
            // A zero or negative interval still moves, as a 1ms frame does.
            assert_eq!(seek_advance(remaining, 0.0), seek_advance(remaining, 1.0));
            assert_eq!(seek_advance(remaining, -5.0), seek_advance(remaining, 1.0));
            // A frame after a long stall moves no further than a 64ms one.
            assert_eq!(
                seek_advance(remaining, 500.0),
                seek_advance(remaining, 64.0)
            );
        }
    }

    /// A seek on the pure half: the follow state and the view, through our own
    /// work and the reader's moves in the order the DOM half would see them.
    struct SeekRun {
        follow: Follow,
        view: ViewEdges,
    }

    impl SeekRun {
        fn start(view: ViewEdges) -> Self {
            Self {
                follow: Follow::Seeking { from: view },
                view,
            }
        }

        /// Our own work (a frame, a restore) left the view at `after`.
        fn own(&mut self, after: ViewEdges) {
            self.follow = self.follow.after_own_work(self.view, after);
            self.view = after;
        }

        /// A seek frame that moved the view down `px`.
        fn frame(&mut self, px: i32) {
            let height = self.view.bottom - self.view.top;
            self.own(edges(self.view.top + px, height));
        }

        /// The reader moved the view by `px` (negative is up).
        fn reader(&mut self, px: i32) {
            self.view = ViewEdges {
                top: self.view.top + px,
                bottom: self.view.bottom + px,
            };
            if let Follow::Seeking { from } = self.follow {
                self.follow = seek_after_reader_scroll(from, self.view);
            }
        }

        fn seeking(&self) -> bool {
            matches!(self.follow, Follow::Seeking { .. })
        }
    }

    #[test]
    fn small_reader_moves_between_seek_frames_add_up_to_a_takeover() {
        let mut run = SeekRun::start(edges(1000, 600));
        // Each 1px move is under the slack on its own, and every frame between
        // them moves the view hundreds of pixels the other way.
        for (i, frame) in [300, 250, 200].into_iter().enumerate() {
            run.frame(frame);
            run.reader(-1);
            let moved = i as i32 + 1;
            assert_eq!(
                run.seeking(),
                moved <= SCROLL_TOP_SLACK_PX,
                "after {moved} 1px moves"
            );
        }
        // Taken over: the gesture's origin is the seek's, with our frames taken
        // out, and it is already held.
        assert_eq!(
            run.follow,
            Follow::Gesture {
                from: edges(1000 + 300 + 250 + 200, 600),
                held: true,
            }
        );
    }

    #[test]
    fn our_own_seek_frames_alone_never_take_over() {
        let mut run = SeekRun::start(edges(1000, 600));
        // Large and small steps, a 1px step, and a frame the end clamped short.
        for px in [1200, 800, 3, 1, 450] {
            run.frame(px);
            assert!(run.seeking(), "a {px}px frame of our own took over");
        }
        // Nothing the reader did is in the origin.
        assert_eq!(run.follow, Follow::Seeking { from: run.view });
    }

    #[test]
    fn downward_or_rounding_sized_reader_movement_keeps_the_seek() {
        let mut run = SeekRun::start(edges(1000, 600));
        run.frame(300);
        run.reader(-SCROLL_TOP_SLACK_PX);
        run.frame(300);
        assert!(run.seeking(), "a move of the slack itself took over");
        // Down 5px and back up 5px: net, still only the slack.
        run.reader(5);
        run.frame(200);
        run.reader(-5);
        assert!(run.seeking(), "net movement within the slack took over");
        run.reader(-1);
        assert!(
            !run.seeking(),
            "net movement past the slack did not take over"
        );
    }

    #[test]
    fn a_container_change_during_a_seek_keeps_the_two_edge_rule() {
        // The composer collapses under a seek: the container grows 250px and the
        // browser clamps the top up by as much, so the bottom edge stays put. Our
        // restore records it, and it is not the reader.
        let mut run = SeekRun::start(edges(1000, 500));
        run.frame(300);
        run.own(edges(1300 - 250, 750));
        assert!(run.seeking());
        // The reader then moves 1px at a time: only past the slack, from the
        // origin with the clamp taken out, does it take over.
        run.reader(-1);
        run.frame(100);
        run.reader(-1);
        assert!(run.seeking());
        run.reader(-1);
        assert!(!run.seeking());

        // The same change read with a reader event (both edges in one
        // `scroll`): the bottom edge held, so it is not moving up.
        let mut run = SeekRun::start(edges(2000, 500));
        run.view = edges(1750, 750);
        let Follow::Seeking { from } = run.follow else {
            unreachable!()
        };
        assert_eq!(
            seek_after_reader_scroll(from, run.view),
            Follow::Seeking { from }
        );
    }

    /// A held gesture whose view a correction has just moved to `CORRECTED`.
    const CORRECTED: ViewEdges = ViewEdges {
        top: 2300,
        bottom: 2900,
    };

    fn corrected_gesture() -> HistoryScroll {
        let history = HistoryScroll::default();
        history.note_reader_move();
        history.follow.set(Follow::Gesture {
            from: edges(2000, 600),
            held: true,
        });
        history.note_correction(CORRECTED);
        history
    }

    #[test]
    fn a_native_end_at_our_correction_does_not_settle_the_gesture() {
        let history = corrected_gesture();
        assert!(!history.settle_allowed(SettleCause::Native, CORRECTED));
        // Every end that matches is refused, not just the first: an engine can
        // send more than one for a write.
        assert!(!history.settle_allowed(SettleCause::Native, CORRECTED));
        // Any other top is an end the correction cannot have caused.
        for other in [edges(2299, 600), edges(2301, 600)] {
            assert!(
                history.settle_allowed(SettleCause::Native, other),
                "{other:?}"
            );
        }
    }

    #[test]
    fn a_container_height_change_alone_keeps_our_corrections_end_refused() {
        // The container resized under the corrected view before its end was
        // read: only the bottom edge moved, and nobody moved the view.
        let history = corrected_gesture();
        let height = CORRECTED.bottom - CORRECTED.top;
        for resized in [height - 40, height - 1, height + 1, height + 40] {
            assert!(
                !history.settle_allowed(SettleCause::Native, edges(CORRECTED.top, resized)),
                "{resized}px tall"
            );
        }
        // With the same resize, another top is still an end it cannot have caused.
        for other in [
            edges(CORRECTED.top - 1, height - 40),
            edges(CORRECTED.top + 1, height + 40),
        ] {
            assert!(
                history.settle_allowed(SettleCause::Native, other),
                "{other:?}"
            );
        }
        // A reader move since lets it settle at the corrected top, resized or not.
        history.note_reader_move();
        assert!(history.settle_allowed(SettleCause::Native, edges(CORRECTED.top, height - 40)));
        // And the quiet deadline settles there whatever the height.
        let history = corrected_gesture();
        assert!(history.settle_allowed(SettleCause::Quiet, edges(CORRECTED.top, height + 40)));
    }

    #[test]
    fn a_reader_move_after_a_correction_lets_its_native_end_settle() {
        let history = corrected_gesture();
        // Away and back: the view is where the correction left it, but this end
        // can be the reader's.
        history.note_reader_move();
        history.note_reader_move();
        assert!(history.settle_allowed(SettleCause::Native, CORRECTED));
    }

    #[test]
    fn the_quiet_deadline_settles_even_at_the_corrected_view() {
        let history = corrected_gesture();
        assert!(history.settle_allowed(SettleCause::Quiet, CORRECTED));
    }

    #[test]
    fn stale_interaction_work_cannot_hold_a_later_gesture() {
        // A room switch, a forced snap or a new seek ends the interaction.
        let history = corrected_gesture();
        history.end_interaction();
        assert_eq!(history.correction.get(), None);
        assert!(history.settle_allowed(SettleCause::Native, CORRECTED));
        // So does the room reset, through it.
        let history = corrected_gesture();
        history.reset_for_room();
        assert_eq!(history.correction.get(), None);
        // A correction with no gesture in progress is no gesture's: a parked
        // reader's restore installs nothing a later gesture could match.
        let history = HistoryScroll::default();
        history.note_correction(CORRECTED);
        history.note_reader_move();
        history.follow.set(Follow::Gesture {
            from: edges(2000, 600),
            held: true,
        });
        assert!(history.settle_allowed(SettleCause::Native, CORRECTED));
    }

    #[test]
    fn the_quiet_deadline_runs_from_the_readers_last_move() {
        let full = SCROLL_SETTLE_DEBOUNCE_MS;
        assert_eq!(quiet_deadline_in(1_000.0, 1_000.0), full);
        // A correction 60ms after the move does not restart the interval.
        assert_eq!(quiet_deadline_in(1_000.0, 1_060.0), full - 60);
        assert_eq!(quiet_deadline_in(1_000.0, 1_000.0 + f64::from(full)), 0);
        // Already past it: at once, never a negative delay.
        assert_eq!(quiet_deadline_in(1_000.0, 5_000.0), 0);
        // A clock that went backwards never makes it longer than the interval.
        assert_eq!(quiet_deadline_in(1_000.0, 900.0), full);
        // Fractional clocks round up, so it is never early.
        assert_eq!(quiet_deadline_in(1_000.0, 1_000.5), full);
    }

    #[test]
    fn a_settle_puts_back_a_reflow_the_observer_has_not_reported() {
        let recorded = sig(3000, 600, 1000);
        let grown = sig(3300, 600, 1000);
        for held in [true, false] {
            let gesture = Follow::Gesture {
                from: edges(2000, 600),
                held,
            };
            assert!(settle_restores_first(gesture, false, recorded, grown));
            // Nothing changed since the record: nothing to put back.
            assert!(!settle_restores_first(gesture, false, recorded, recorded));
            // A forced snap is owed: its restore goes to the bottom anyway.
            assert!(!settle_restores_first(gesture, true, recorded, grown));
        }
        // With no gesture in progress a settle does nothing, so neither does
        // this: a stale end stays harmless.
        for idle in [
            Follow::Free,
            Follow::Seeking {
                from: edges(2000, 600),
            },
        ] {
            assert!(!settle_restores_first(idle, false, recorded, grown));
        }
    }

    #[test]
    fn a_room_switch_forgets_the_old_rooms_position() {
        let history = HistoryScroll::default();
        history.pinned.set(false);
        history.anchor.borrow_mut().push(("m1".into(), 120));
        history.sig.set(sig(3000, 600, 1000));
        history.top.set(2000);
        history.follow.set(Follow::Gesture {
            from: edges(1800, 600),
            held: true,
        });
        history.settle_pending.set(true);

        history.reset_for_room();

        assert!(history.pinned.get() && history.force.get());
        assert_eq!(history.follow.get(), Follow::Free);
        assert!(!history.settle_pending.get());
        assert!(history.anchor.borrow().is_empty());
        assert_eq!(history.sig.get(), LayoutSig::default());
        assert_eq!(history.top.get(), 0);
    }
}
