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
//! * **The one heuristic**: a `scroll` event is layout's (a browser clamp) if the
//!   layout signature changed since it was recorded AND `scrollTop` moved no more
//!   than `LAYOUT_SHIFT_ALLOWANCE_PX`. Residual: a reader who moves less than
//!   that in the very frame a layout change lands loses that frame's movement.
//!   The pin can't latch on it, since the next scroll event captures.
//! * **Late scroll events**: a `scroll` event arrives a frame after the scroll,
//!   and a content change can land first. So `restore`, and the render before a
//!   patch, first read a pending reader scroll (`take_in_undelivered_scroll`,
//!   through `on_scroll`), or a stale pin would drag the reader back down, or an
//!   anchor would be measured after the patch had moved the rows.
//! * **Why this can't latch as #486 did**: the pin comes only from the reader's
//!   own positions. Growing content, a growing composer or a rewrap restore
//!   instead of measuring, so none of them can clear it.
//!
//! Known limit: the scroll-to-latest button scrolls smoothly, so captures during
//! its animation read as the reader's, and a message arriving mid-animation
//! lands one row short until the next change.
//!
//! State is `Cell`/`RefCell`, never signals: raw JS callbacks write it.

// Only the wasm build drives the DOM half; natively the pure half is exercised by
// the unit tests.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use super::{WindowAnchor, BOTTOM_THRESHOLD_PX};
use dioxus::prelude::Signal;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[cfg(target_arch = "wasm32")]
use super::{
    chat_content_wrapper, chat_scroll_container, trim_would_rearm_backfill, INITIAL_WINDOW_ITEMS,
    SCROLL_TOP_SLACK_PX,
};
#[cfg(target_arch = "wasm32")]
use dioxus::prelude::WritableExt;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{prelude::*, JsCast};

/// How many rows above the newest visible one are remembered as fallbacks, for
/// when the anchor row itself is deleted or windowed out before the restore.
pub(super) const ANCHOR_FALLBACK_ROWS: usize = 4;

/// The most a `scroll` event may move `scrollTop` and still be read as the
/// browser's clamp after a layout change rather than the reader.
///
/// The clamps measured so far were 8px (Linux CI), 56px (a spike) and 111px (the
/// synthetic clamp test), so this leaves headroom over the largest. A reader who
/// moves less than this in the single frame a layout change lands in loses only
/// that frame's movement, and the pin can't latch because the next scroll event
/// captures.
pub(super) const LAYOUT_SHIFT_ALLOWANCE_PX: i32 = 200;

/// The three numbers that change when the history is laid out again.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(super) struct LayoutSig {
    pub scroll_height: i32,
    pub client_height: i32,
    pub client_width: i32,
}

/// Who a `scroll` event belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ScrollCause {
    /// The reader moved the view: it becomes the new anchor.
    Reader,
    /// A layout change moved it (a browser clamp): put the anchor back.
    Layout,
}

/// Indices of up to `n` rows, newest first, starting at the newest row that
/// intersects the viewport `[view_top, view_bottom]`. The rows above it are
/// fallbacks and need not be visible. Empty if no row is visible.
///
/// `rows` are `(top, bottom)` in document order, so their tops are monotonic
/// and the newest row starting above the viewport's bottom edge is a binary
/// search away. Touching an edge is not intersecting it.
pub(super) fn newest_visible_rows(
    rows: &[(i32, i32)],
    view_top: i32,
    view_bottom: i32,
    n: usize,
) -> Vec<usize> {
    let started = rows.partition_point(|&(top, _)| top < view_bottom);
    let Some(newest) = started.checked_sub(1) else {
        return Vec::new();
    };
    if n == 0 || rows[newest].1 <= view_top {
        return Vec::new();
    }
    (0..=newest).rev().take(n).collect()
}

/// How far to scroll so a row whose top sat `saved_gap` above the viewport's
/// bottom, and now sits `current_gap` above it, is back where it was.
pub(super) fn restore_delta(saved_gap: i32, current_gap: i32) -> i32 {
    saved_gap - current_gap
}

/// Read a `scroll` event as a layout change's doing or the reader's.
///
/// It is layout only if the layout changed since it was last recorded AND
/// `scrollTop` moved no more than `LAYOUT_SHIFT_ALLOWANCE_PX`.
pub(super) fn classify_scroll(
    recorded: LayoutSig,
    now: LayoutSig,
    recorded_top: i32,
    now_top: i32,
) -> ScrollCause {
    if recorded != now && (now_top - recorded_top).abs() <= LAYOUT_SHIFT_ALLOWANCE_PX {
        ScrollCause::Layout
    } else {
        ScrollCause::Reader
    }
}

/// Whether a reader `distance_from_bottom` px above the end is still following.
pub(super) fn is_pinned(distance_from_bottom: f64) -> bool {
    distance_from_bottom <= BOTTOM_THRESHOLD_PX
}

/// What the trim (the window reset at the bottom) needs from the component.
#[derive(Clone)]
pub(super) struct TrimHooks {
    pub window_items: Signal<usize>,
    pub window_anchor: Rc<RefCell<Option<WindowAnchor>>>,
    pub window_overgrown: Rc<Cell<bool>>,
    pub window_rendered: Rc<Cell<usize>>,
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
    /// What `capture` needs, kept from `install` so a restore can first take in a
    /// scroll the browser has not delivered an event for yet.
    trim: RefCell<Option<TrimHooks>>,
}

impl Default for HistoryScroll {
    fn default() -> Self {
        Self {
            anchor: RefCell::new(Vec::new()),
            pinned: Cell::new(true),
            force: Cell::new(false),
            sig: Cell::new(LayoutSig::default()),
            top: Cell::new(0),
            trim: RefCell::new(None),
        }
    }
}

impl HistoryScroll {
    /// Make the next restore go to the bottom.
    pub(super) fn force_next(&self) {
        self.force.set(true);
    }

    /// Whether a forced snap is waiting for the next restore.
    pub(super) fn is_forced(&self) -> bool {
        self.force.get()
    }

    /// A new room opens at its newest message: forget the old room's position.
    pub(super) fn reset_for_room(&self) {
        self.anchor.borrow_mut().clear();
        self.pinned.set(true);
        self.force.set(true);
        self.sig.set(LayoutSig::default());
        self.top.set(0);
    }

    /// Whether the reader is following the newest message. Read by tests only:
    /// production code acts on the pin inside `restore`.
    #[cfg(test)]
    pub(super) fn is_pinned(&self) -> bool {
        self.pinned.get()
    }
}

#[cfg(target_arch = "wasm32")]
const ANCHOR_ROWS: &str = "#chat-content [data-anchor-row]";
#[cfg(target_arch = "wasm32")]
const ANCHOR_ATTR: &str = "data-anchor-row";

#[cfg(target_arch = "wasm32")]
fn read_sig(container: &web_sys::Element) -> LayoutSig {
    LayoutSig {
        scroll_height: container.scroll_height(),
        client_height: container.client_height(),
        client_width: container.client_width(),
    }
}

/// The anchor rows with their `(top, bottom)` relative to the container's top
/// edge, so a sidebar toggle can't skew them.
#[cfg(target_arch = "wasm32")]
struct Rows {
    list: web_sys::NodeList,
    rects: Vec<(i32, i32)>,
    view_bottom: i32,
}

#[cfg(target_arch = "wasm32")]
impl Rows {
    fn read(container: &web_sys::Element) -> Option<Self> {
        let list = container.query_selector_all(ANCHOR_ROWS).ok()?;
        let view = container.get_bounding_client_rect();
        let rects = (0..list.length())
            .map(|i| {
                let el = list
                    .item(i)
                    .unwrap_throw()
                    .unchecked_into::<web_sys::Element>();
                let r = el.get_bounding_client_rect();
                (
                    (r.top() - view.top()).round() as i32,
                    (r.bottom() - view.top()).round() as i32,
                )
            })
            .collect();
        Some(Self {
            list,
            rects,
            view_bottom: (view.bottom() - view.top()).round() as i32,
        })
    }

    fn key(&self, i: usize) -> Option<String> {
        self.list
            .item(i as u32)?
            .unchecked_into::<web_sys::Element>()
            .get_attribute(ANCHOR_ATTR)
    }
}

#[cfg(target_arch = "wasm32")]
impl HistoryScroll {
    /// Record the layout and `scrollTop` as they are now. Read back after any write,
    /// since the browser clamps.
    fn record(&self, container: &web_sys::Element) {
        self.sig.set(read_sig(container));
        self.top.set(container.scroll_top());
    }

    /// Take the reader's position as the new truth. Only ever called from
    /// `on_scroll`, for a scroll the reader made.
    fn capture(&self, trim: &TrimHooks) {
        let Some(container) = chat_scroll_container() else {
            return;
        };
        let sig = read_sig(&container);
        let top = container.scroll_top();
        let distance = (sig.scroll_height - sig.client_height - top) as f64;
        self.pinned.set(is_pinned(distance));
        if let Some(rows) = Rows::read(&container) {
            let picked =
                newest_visible_rows(&rows.rects, 0, rows.view_bottom, ANCHOR_FALLBACK_ROWS + 1);
            *self.anchor.borrow_mut() = picked
                .into_iter()
                .filter_map(|i| Some((rows.key(i)?, rows.view_bottom - rows.rects[i].0)))
                .collect();
        }
        self.sig.set(sig);
        self.top.set(top);

        // A capture landing AT the bottom is the ONE moment a window trim is
        // provably invisible: the rows it removes are above the view, so the
        // browser's clamp keeps the same tail glued to the bottom edge (and the
        // ResizeObserver's restore keeps a pinned reader there). Gated at
        // SCROLL_TOP_SLACK_PX, not BOTTOM_THRESHOLD_PX: a reader parked 100px up
        // still counts as pinned, and a trim from there would yank them to the
        // exact bottom. Skipped when the trimmed tail would leave the backfill
        // sentinel in range of the bottom, or the two oscillate at render speed
        // (#505; see `trim_would_rearm_backfill`).
        if distance <= SCROLL_TOP_SLACK_PX as f64
            && trim.window_overgrown.get()
            && !trim_would_rearm_backfill(
                sig.scroll_height,
                sig.client_height,
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

    /// Put the view where it belongs after a layout or content change. Returns
    /// whether it snapped to the bottom.
    pub(super) fn restore(&self) -> bool {
        self.take_in_undelivered_scroll();
        self.restore_now()
    }

    /// A `scroll` event is queued for the next frame, and a content change can
    /// land first. If the view has moved by more than a layout clamp could since
    /// it was recorded, that is the reader's scroll and its event is still on its
    /// way: read it now, or the pin is stale and the restore drags them back.
    pub(super) fn take_in_undelivered_scroll(&self) {
        let Some(container) = chat_scroll_container() else {
            return;
        };
        let top = container.scroll_top();
        if top == self.top.get() {
            return;
        }
        let cause = classify_scroll(self.sig.get(), read_sig(&container), self.top.get(), top);
        if cause == ScrollCause::Reader {
            if let Some(trim) = self.trim.borrow().clone() {
                self.on_scroll(&trim);
            }
        }
    }

    fn restore_now(&self) -> bool {
        let Some(container) = chat_scroll_container() else {
            return false;
        };
        if self.force.take() || self.pinned.get() {
            self.snap_to_bottom(web_sys::ScrollBehavior::Instant);
            return true;
        }
        self.restore_anchor(&container);
        self.record(&container);
        false
    }

    /// Scroll the first anchor row that still exists back to its gap. If none
    /// survives, leave the view alone: the next reader scroll captures a new one.
    fn restore_anchor(&self, container: &web_sys::Element) {
        let anchor = self.anchor.borrow();
        if anchor.is_empty() {
            return;
        }
        let Ok(list) = container.query_selector_all(ANCHOR_ROWS) else {
            return;
        };
        let view_bottom = container.get_bounding_client_rect().bottom();
        // Newest first, and the anchor's newest row is the last of the rows that
        // survive, so scanning from the end finds the first survivor quickly.
        for i in (0..list.length()).rev() {
            let el = list
                .item(i)
                .unwrap_throw()
                .unchecked_into::<web_sys::Element>();
            let Some(key) = el.get_attribute(ANCHOR_ATTR) else {
                continue;
            };
            let Some((_, saved_gap)) = anchor.iter().find(|(k, _)| *k == key) else {
                continue;
            };
            let current_gap = (view_bottom - el.get_bounding_client_rect().top()).round() as i32;
            let delta = restore_delta(*saved_gap, current_gap);
            if delta.abs() > SCROLL_TOP_SLACK_PX {
                container.set_scroll_top(container.scroll_top() + delta);
            }
            return;
        }
    }

    /// Read a `scroll` event as layout's doing (restore) or the reader's (capture).
    fn on_scroll(&self, trim: &TrimHooks) {
        let Some(container) = chat_scroll_container() else {
            return;
        };
        let cause = classify_scroll(
            self.sig.get(),
            read_sig(&container),
            self.top.get(),
            container.scroll_top(),
        );
        match cause {
            ScrollCause::Layout => {
                self.restore_now();
            }
            ScrollCause::Reader => self.capture(trim),
        }
    }

    /// Scroll to the newest message and re-arm the pin: asking for it is the
    /// clearest statement of intent there is.
    pub(super) fn snap_to_bottom(&self, behavior: web_sys::ScrollBehavior) {
        let Some(container) = chat_scroll_container() else {
            return;
        };
        self.pinned.set(true);
        let opts = web_sys::ScrollToOptions::new();
        opts.set_top(container.scroll_height() as f64);
        opts.set_behavior(behavior);
        container.scroll_to_with_scroll_to_options(&opts);
        self.record(&container);
    }

    /// Listen for the reader's scrolls and for layout changes. Returns `false` if
    /// the history is not in the DOM yet, so the caller retries.
    #[must_use]
    pub(super) fn install(self: &Rc<Self>, trim: TrimHooks) -> bool {
        let (Some(container), Some(content)) = (chat_scroll_container(), chat_content_wrapper())
        else {
            return false;
        };

        // The ResizeObserver sees the content growing or reflowing, and the
        // container shrinking (the composer growing, #486's third cause). Both
        // restore; neither may capture.
        let on_resize = {
            let this = self.clone();
            Closure::wrap(Box::new(move |_: js_sys::Array| {
                this.restore();
            }) as Box<dyn FnMut(js_sys::Array)>)
        };
        let Ok(observer) = web_sys::ResizeObserver::new(on_resize.as_ref().unchecked_ref()) else {
            return false;
        };
        self.record(&container);
        *self.trim.borrow_mut() = Some(trim.clone());
        observer.observe(&content);
        observer.observe(&container);

        // Passive: nothing here calls `preventDefault`, and the jank this
        // replaced (#151) came from work on the scroll path.
        let passive = web_sys::AddEventListenerOptions::new();
        passive.set_passive(true);
        let on_scroll = {
            let this = self.clone();
            Closure::wrap(Box::new(move |_: web_sys::Event| this.on_scroll(&trim))
                as Box<dyn FnMut(web_sys::Event)>)
        };
        let _ = container.add_event_listener_with_callback_and_add_event_listener_options(
            "scroll",
            on_scroll.as_ref().unchecked_ref(),
            &passive,
        );

        // Leaked deliberately: `Conversation` mounts once for the app's lifetime
        // (rooms are swapped by CSS, not by unmount) and `use_effect` has no
        // cleanup hook, so there is nothing to disconnect these from.
        on_resize.forget();
        on_scroll.forget();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: (i32, i32) = (0, 500);

    fn sig(scroll_height: i32, client_height: i32, client_width: i32) -> LayoutSig {
        LayoutSig {
            scroll_height,
            client_height,
            client_width,
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
        assert_eq!(newest_visible_rows(&rows, VIEW.0, VIEW.1, 1), vec![4]);

        // The newest row ends inside the view, so the ones below it are off screen.
        assert_eq!(newest_visible_rows(&rows[..4], VIEW.0, VIEW.1, 1), vec![3]);

        // Only a row straddling the top edge is visible.
        let above = [(-300, -200), (-200, 20), (600, 700)];
        assert_eq!(newest_visible_rows(&above, VIEW.0, VIEW.1, 1), vec![1]);

        // A row taller than the viewport, covering both edges.
        let tall = [(-100, 900)];
        assert_eq!(newest_visible_rows(&tall, VIEW.0, VIEW.1, 1), vec![0]);

        assert_eq!(
            newest_visible_rows(&[], VIEW.0, VIEW.1, 1),
            Vec::<usize>::new()
        );
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
        assert_eq!(newest_visible_rows(&rows, 0, 450, 3), vec![4, 3, 2]);
        // Fewer rows exist than were asked for.
        assert_eq!(newest_visible_rows(&rows[..2], 0, 450, 5), vec![1, 0]);
        // The fallbacks need not be visible themselves.
        assert_eq!(newest_visible_rows(&rows, 250, 450, 4), vec![4, 3, 2, 1]);
        assert_eq!(newest_visible_rows(&rows, 0, 450, 0), Vec::<usize>::new());
    }

    #[test]
    fn a_row_touching_the_edge_by_one_pixel_is_not_visible() {
        // Its bottom edge is the viewport's top edge: no overlap at all.
        assert!(newest_visible_rows(&[(-100, 0)], 0, 500, 1).is_empty());
        // Its top edge is the viewport's bottom edge.
        assert_eq!(
            newest_visible_rows(&[(0, 100), (500, 600)], 0, 500, 1),
            vec![0]
        );
        // One pixel of overlap is visible.
        assert_eq!(newest_visible_rows(&[(-100, 1)], 0, 500, 1), vec![0]);
        assert_eq!(newest_visible_rows(&[(499, 600)], 0, 500, 1), vec![0]);
    }

    #[test]
    fn restore_delta_moves_the_row_back_to_its_gap() {
        // The row's top sat 200px above the viewport's bottom. Content above it grew
        // by 40, so it is now 160 above the bottom: scroll down 40 to bring it back.
        assert_eq!(restore_delta(200, 160), 40);
        // Content above it shrank by 40: the row is 240 above the bottom, scroll up.
        assert_eq!(restore_delta(200, 240), -40);
        assert_eq!(restore_delta(200, 200), 0);
    }

    #[test]
    fn a_small_move_with_a_changed_layout_is_layout() {
        assert_eq!(
            classify_scroll(sig(3000, 600, 1000), sig(3400, 600, 380), 2000, 1992),
            ScrollCause::Layout
        );
    }

    #[test]
    fn a_large_move_with_a_changed_layout_is_the_reader() {
        assert_eq!(
            classify_scroll(
                sig(3000, 600, 1000),
                sig(3400, 600, 380),
                2000,
                2000 - LAYOUT_SHIFT_ALLOWANCE_PX - 1
            ),
            ScrollCause::Reader
        );
    }

    #[test]
    fn any_move_with_an_unchanged_layout_is_the_reader() {
        let same = sig(3000, 600, 1000);
        assert_eq!(classify_scroll(same, same, 2000, 1999), ScrollCause::Reader);
        assert_eq!(classify_scroll(same, same, 2000, 2000), ScrollCause::Reader);
        assert_eq!(classify_scroll(same, same, 2000, 100), ScrollCause::Reader);
    }

    #[test]
    fn the_allowance_boundary_is_layout() {
        let (a, b) = (sig(3000, 600, 1000), sig(3400, 600, 1000));
        assert_eq!(
            classify_scroll(a, b, 2000, 2000 + LAYOUT_SHIFT_ALLOWANCE_PX),
            ScrollCause::Layout
        );
        assert_eq!(
            classify_scroll(a, b, 2000, 2000 - LAYOUT_SHIFT_ALLOWANCE_PX),
            ScrollCause::Layout
        );
        assert_eq!(
            classify_scroll(a, b, 2000, 2000 + LAYOUT_SHIFT_ALLOWANCE_PX + 1),
            ScrollCause::Reader
        );
    }

    #[test]
    fn a_fresh_history_is_pinned_and_unforced() {
        let history = HistoryScroll::default();
        assert!(history.is_pinned());
        assert!(!history.is_forced());
        assert!(history.anchor.borrow().is_empty());
    }

    #[test]
    fn a_room_switch_forgets_the_old_rooms_position() {
        let history = HistoryScroll::default();
        history.pinned.set(false);
        history.anchor.borrow_mut().push(("m1".into(), 120));
        history.sig.set(sig(3000, 600, 1000));
        history.top.set(2000);

        history.reset_for_room();

        assert!(history.is_pinned() && history.is_forced());
        assert!(history.anchor.borrow().is_empty());
        assert_eq!(history.sig.get(), LayoutSig::default());
        assert_eq!(history.top.get(), 0);
    }

    #[test]
    fn force_next_raises_the_force_and_nothing_else() {
        let history = HistoryScroll::default();
        history.pinned.set(false);
        history.force_next();
        assert!(history.is_forced());
        assert!(!history.is_pinned());
    }

    #[test]
    fn pinned_at_and_under_the_threshold_only() {
        assert!(is_pinned(0.0));
        assert!(is_pinned(99.9));
        assert!(is_pinned(100.0));
        assert!(!is_pinned(100.1));
        assert!(!is_pinned(101.0));
    }
}
