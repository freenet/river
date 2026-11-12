//! Dragging the mobile message sheet down to dismiss it.
//!
//! A pointer that goes down anywhere in the open dialog (the backdrop, the
//! panel, an action, an emoji) and then moves further down than sideways
//! drags the panel with it, by the distance moved, and fades the dim behind
//! it. Released far enough or fast enough ([`dismisses`]) the panel slides
//! away and the sheet closes; otherwise it springs back. Upward it does not
//! move: it never sits higher than at rest, fully visible on the bottom edge.
//! A sideways move is left alone: on the emoji row the browser is already
//! scrolling it (`touch-action: pan-x`, `main.css`). A second finger (a
//! pinch) cancels the drag, and the zoom goes ahead.
//!
//! A click in the dialog that is the tail of the press that opened it does
//! nothing ([`stray_after_open`]): the second click of a double-click, and
//! any pointer's click for a moment after a tap or click opened it, since a
//! phone's second tap lands on the sheet the first one opened.
//!
//! A tap outside slides the panel away the same way ([`animate_out`]); Escape
//! and a choice (an emoji, an action) close it at once. With
//! `prefers-reduced-motion: reduce` nothing slides: a dismissal closes at
//! once.
//!
//! The listeners touch only the DOM and the thread-locals here. The one state
//! change, the close, is deferred (dioxus-signal-safety.md) and names the
//! opening it is for, read from the dialog's `data-opening`, so it never
//! closes a later one. The dialog's `close` listener (Escape) is installed
//! and removed with them ([`install`]).

// Natively only the tests use the decision functions (the DOM is wasm-only).
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use super::TAP_SLOP_PX;

/// A release this far down, or this fraction of the panel's height if that
/// is more, dismisses.
const MIN_DISMISS_PX: f64 = 48.0;
const DISMISS_FRACTION: f64 = 0.4;
/// A release moving down at least this fast (px/ms over the last
/// [`VELOCITY_WINDOW_MS`]) dismisses once it is [`MIN_FLICK_PX`] down.
const FLICK_PX_PER_MS: f64 = 0.5;
const MIN_FLICK_PX: f64 = 16.0;
const VELOCITY_WINDOW_MS: f64 = 100.0;
/// A finger held still this long before it lifts released with no speed.
const STILL_MS: f64 = 80.0;

/// How long after a moved gesture's lift its click may still come.
const CLICK_AFTER_LIFT_MS: f64 = 400.0;

/// The click a gesture that moved sends when it lifts (a mouse's, mostly):
/// not a tap on what it lands on. Only that click: a gesture that ends with
/// none (cancelled, a second finger) leaves nothing armed, so it cannot
/// swallow a later activation that has no press of its own (a key's).
#[derive(Default)]
struct MovedClick {
    until: Option<f64>,
}

impl MovedClick {
    /// A gesture that moved lifted at `now` (ms).
    fn lifted(&mut self, now: f64) {
        self.until = Some(now + CLICK_AFTER_LIFT_MS);
    }

    /// No click can follow: cancelled, a second finger, or a new press.
    fn clear(&mut self) {
        self.until = None;
    }

    /// A click at `now`: whether it is that lift's. Consumes it.
    fn take(&mut self, now: f64) -> bool {
        self.until.take().is_some_and(|until| now <= until)
    }
}

/// How long after a pointer opens the sheet a pointer's click in it is still
/// that press's second tap.
const OPEN_GUARD_MS: f64 = 350.0;

/// Whether a click in the dialog is the tail of the press that opened it,
/// not a choice: the second click of a double-click (`detail` over 1), or a
/// pointer's click `since_pointer_opening` ms after a tap or click opened it
/// (a phone's second tap counts 1). A key's click (`detail` 0) never is.
fn stray_after_open(detail: i32, since_pointer_opening: Option<f64>) -> bool {
    detail > 1 || (detail == 1 && since_pointer_opening.is_some_and(|ms| ms < OPEN_GUARD_MS))
}

/// Whether a pointer moved `(dx, dy)` from where it went down is going up or
/// down rather than sideways, or `None` while it is still a tap. A tie is
/// sideways: scrolling the row wins.
fn vertical(dx: f64, dy: f64) -> Option<bool> {
    (dx.hypot(dy) > TAP_SLOP_PX).then_some(dy.abs() > dx.abs())
}

/// Where the panel sits for a pointer `dy` below where it went down: with
/// the finger, downward only. It never sits higher than at rest, where it is
/// fully visible on the bottom edge.
fn offset(dy: f64) -> f64 {
    dy.max(0.0)
}

/// The speed (px/ms, down positive) of a release at `released_at`, from the
/// pointer's recent `(time, y)` samples, oldest first: their displacement
/// over their time, or none if the finger was held still for [`STILL_MS`]
/// before it lifted.
fn velocity(samples: &[(f64, f64)], released_at: f64) -> f64 {
    let (Some(&(t0, y0)), Some(&(t1, y1))) = (samples.first(), samples.last()) else {
        return 0.0;
    };
    if released_at - t1 > STILL_MS || t1 <= t0 {
        return 0.0;
    }
    (y1 - y0) / (t1 - t0)
}

/// Whether a release with the panel `offset` px down, moving at `velocity`
/// px/ms (down is positive), dismisses a panel `panel_height` px tall.
/// Otherwise it springs back.
fn dismisses(offset: f64, velocity: f64, panel_height: f64) -> bool {
    let far = offset >= MIN_DISMISS_PX.max(panel_height * DISMISS_FRACTION);
    let flick = offset >= MIN_FLICK_PX && velocity >= FLICK_PX_PER_MS;
    far || flick
}

#[cfg(target_arch = "wasm32")]
pub(super) use self::dom::{animate_out, install, reset, Listeners};

#[cfg(target_arch = "wasm32")]
mod dom {
    use super::super::mobile_surface::dom::sheet;
    use super::super::now_ms;
    use super::super::reaction_picker::ReactionSurfaces;
    use super::{
        dismisses, offset, stray_after_open, velocity, vertical, MovedClick, VELOCITY_WINDOW_MS,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    /// The pointer gesture under way in the open dialog.
    struct Gesture {
        pointer: i32,
        x0: f64,
        y0: f64,
        /// Past the tap slop, either way: its lift's click is no tap.
        moved: bool,
        /// Past the tap slop, going up or down: it drags the panel.
        dragging: bool,
        offset: f64,
        /// `(time, y)` over the last [`VELOCITY_WINDOW_MS`].
        samples: VecDeque<(f64, f64)>,
    }

    thread_local! {
        static GESTURE: RefCell<Option<Gesture>> = const { RefCell::new(None) };
        /// The click the last moved gesture's lift may send.
        static MOVED_CLICK: RefCell<MovedClick> = const { RefCell::new(MovedClick { until: None }) };
        /// When the opening it shows was shown (ms), if a pointer opened it.
        static POINTER_OPENED_AT: Cell<Option<f64>> = const { Cell::new(None) };
    }

    const DRAGGING: &str = "is-dragging";
    const LEAVING: &str = "is-leaving";
    const SHADE: &str = "--sheet-shade";

    type Listener = Closure<dyn FnMut(web_sys::Event)>;

    /// The listeners [`install`] added; dropping it removes them.
    pub(in super::super) struct Listeners {
        added: Vec<(web_sys::EventTarget, &'static str, Listener, bool)>,
    }

    impl Drop for Listeners {
        fn drop(&mut self) {
            for (target, kind, listener, capture) in &self.added {
                let _ = target.remove_event_listener_with_callback_and_bool(
                    kind,
                    listener.as_ref().unchecked_ref(),
                    *capture,
                );
            }
        }
    }

    fn panel(dialog: &web_sys::HtmlElement) -> Option<web_sys::HtmlElement> {
        dialog
            .query_selector(".mobile-sheet-panel")
            .ok()??
            .dyn_into::<web_sys::HtmlElement>()
            .ok()
    }

    fn reduced_motion() -> bool {
        crate::util::media_matches("(prefers-reduced-motion: reduce)").unwrap_or(false)
    }

    /// Put the panel `offset` px down and dim behind it to match.
    fn set_offset(dialog: &web_sys::HtmlElement, offset: f64) {
        let Some(panel) = panel(dialog) else {
            return;
        };
        let _ = panel
            .style()
            .set_property("transform", &format!("translateY({offset}px)"));
        let height = f64::from(panel.offset_height()).max(1.0);
        let shade = 1.0 - (offset / height).clamp(0.0, 1.0);
        let _ = dialog.style().set_property(SHADE, &format!("{shade:.3}"));
    }

    /// Back to rest, through the panel's transition (`main.css`).
    fn spring_back(dialog: &web_sys::HtmlElement) {
        let _ = dialog.class_list().remove_1(DRAGGING);
        if let Some(panel) = panel(dialog) {
            let _ = panel.style().remove_property("transform");
        }
        let _ = dialog.style().remove_property(SHADE);
    }

    /// The opening the dialog shows now, from its `data-opening`.
    fn opening_token(dialog: &web_sys::HtmlElement) -> Option<u64> {
        dialog
            .get_attribute("data-opening")?
            .parse()
            .ok()
            .filter(|token| *token != 0)
    }

    fn close_later(surfaces: ReactionSurfaces, token: u64) {
        crate::util::defer(move || surfaces.dismiss_opening(token));
    }

    /// Slide the panel away, then close the opening it shows. Once: a second
    /// call while it leaves does nothing. With reduced motion, close now.
    pub(in super::super) fn animate_out(surfaces: ReactionSurfaces) {
        let Some(dialog) = sheet() else {
            return;
        };
        let Some(token) = opening_token(&dialog) else {
            return;
        };
        if dialog.class_list().contains(LEAVING) {
            return;
        }
        let Some(panel) = panel(&dialog).filter(|_| !reduced_motion()) else {
            close_later(surfaces, token);
            return;
        };
        let _ = dialog.class_list().remove_1(DRAGGING);
        let _ = dialog.class_list().add_1(LEAVING);
        let _ = panel
            .style()
            .set_property("transform", "translateY(calc(100% + 24px))");
        let _ = dialog.style().set_property(SHADE, "0");
        // Closed when the slide ends, or after a bound if no transition
        // runs; whichever is second finds the opening gone and does nothing.
        let done = Closure::once_into_js(move || close_later(surfaces, token));
        let options = web_sys::AddEventListenerOptions::new();
        options.set_once(true);
        let _ = panel.add_event_listener_with_callback_and_add_event_listener_options(
            "transitionend",
            done.unchecked_ref(),
            &options,
        );
        crate::util::safe_spawn_local(async move {
            crate::util::sleep(crate::util::millis(320)).await;
            close_later(surfaces, token);
        });
    }

    /// Clear what a drag or a slide left on the dialog, before it shows (a
    /// sheet closed mid-drag opens at rest), and note how this opening began.
    pub(in super::super) fn reset(by_pointer: bool) {
        GESTURE.with(|g| g.borrow_mut().take());
        MOVED_CLICK.with(|m| m.borrow_mut().clear());
        POINTER_OPENED_AT.with(|at| at.set(by_pointer.then(now_ms)));
        if let Some(dialog) = sheet() {
            let _ = dialog.class_list().remove_1(LEAVING);
            spring_back(&dialog);
        }
    }

    fn pointer(event: &web_sys::Event) -> Option<&web_sys::PointerEvent> {
        event.dyn_ref::<web_sys::PointerEvent>()
    }

    fn on_down(dialog: &web_sys::HtmlElement, event: web_sys::Event) {
        let Some(e) = pointer(&event) else {
            return;
        };
        let other = GESTURE.with(|g| g.borrow().as_ref().map(|g| g.pointer));
        if let Some(other) = other {
            if other != e.pointer_id() {
                // A second finger: a pinch, not a drag, and no click.
                GESTURE.with(|g| g.borrow_mut().take());
                MOVED_CLICK.with(|m| m.borrow_mut().clear());
                spring_back(dialog);
            }
            return;
        }
        if !e.is_primary() || (e.pointer_type() == "mouse" && e.button() != 0) {
            return;
        }
        if dialog.class_list().contains(LEAVING) {
            return;
        }
        MOVED_CLICK.with(|m| m.borrow_mut().clear());
        let (x, y) = (f64::from(e.client_x()), f64::from(e.client_y()));
        let gesture = Gesture {
            pointer: e.pointer_id(),
            x0: x,
            y0: y,
            moved: false,
            dragging: false,
            offset: 0.0,
            samples: VecDeque::from([(event.time_stamp(), y)]),
        };
        GESTURE.with(|g| *g.borrow_mut() = Some(gesture));
    }

    fn on_move(dialog: &web_sys::HtmlElement, event: web_sys::Event) {
        let Some(e) = pointer(&event) else {
            return;
        };
        // A mouse whose button came up outside the window sends no
        // `pointerup`, only moves with no button down: a cancel.
        if e.pointer_type() == "mouse" && e.buttons() == 0 {
            on_cancel(dialog, event);
            return;
        }
        let (x, y, now) = (
            f64::from(e.client_x()),
            f64::from(e.client_y()),
            event.time_stamp(),
        );
        let moved = GESTURE.with(|g| {
            let mut slot = g.borrow_mut();
            let gesture = slot.as_mut().filter(|g| g.pointer == e.pointer_id())?;
            let (dx, dy) = (x - gesture.x0, y - gesture.y0);
            if !gesture.dragging {
                if gesture.moved {
                    // Sideways: the row's own scrolling, not a drag. Kept
                    // until it lifts or is cancelled.
                    return None;
                }
                let vertical = vertical(dx, dy)?;
                gesture.moved = true;
                if !vertical {
                    return None;
                }
                gesture.dragging = true;
                let _ = dialog.set_pointer_capture(gesture.pointer);
                let _ = dialog.class_list().add_1(DRAGGING);
            }
            gesture.offset = offset(dy);
            gesture.samples.push_back((now, y));
            while gesture
                .samples
                .front()
                .is_some_and(|(t, _)| now - t > VELOCITY_WINDOW_MS)
                && gesture.samples.len() > 2
            {
                gesture.samples.pop_front();
            }
            Some(gesture.offset)
        });
        if let Some(offset) = moved {
            event.prevent_default();
            set_offset(dialog, offset);
        }
    }

    fn on_up(dialog: &web_sys::HtmlElement, event: web_sys::Event, surfaces: ReactionSurfaces) {
        let Some(e) = pointer(&event) else {
            return;
        };
        let Some(mut gesture) = take_gesture(e.pointer_id()) else {
            return;
        };
        if gesture.moved {
            MOVED_CLICK.with(|m| m.borrow_mut().lifted(now_ms()));
        }
        if !gesture.dragging {
            return;
        }
        let _ = dialog.release_pointer_capture(gesture.pointer);
        let speed = velocity(gesture.samples.make_contiguous(), event.time_stamp());
        let height = panel(dialog).map_or(1.0, |p| f64::from(p.offset_height()));
        if dismisses(gesture.offset, speed, height) {
            animate_out(surfaces);
        } else {
            spring_back(dialog);
        }
    }

    /// `pointercancel`, or the dialog's capture lost mid-drag: back to rest.
    /// Only the dialog's own `lostpointercapture`: taking the capture from
    /// the element a touch started on (touch captures implicitly) fires one
    /// there, which bubbles here.
    fn on_cancel(dialog: &web_sys::HtmlElement, event: web_sys::Event) {
        let Some(e) = pointer(&event) else {
            return;
        };
        let on_dialog = event
            .target()
            .is_some_and(|t| t == web_sys::EventTarget::from(dialog.clone()));
        if event.type_() == "lostpointercapture" && !on_dialog {
            return;
        }
        let Some(gesture) = take_gesture(e.pointer_id()) else {
            return;
        };
        // A cancelled gesture sends no click.
        MOVED_CLICK.with(|m| m.borrow_mut().clear());
        if gesture.dragging {
            spring_back(dialog);
        }
    }

    fn take_gesture(pointer: i32) -> Option<Gesture> {
        GESTURE.with(|g| {
            let mut slot = g.borrow_mut();
            if slot.as_ref().is_some_and(|g| g.pointer == pointer) {
                slot.take()
            } else {
                None
            }
        })
    }

    /// A click inside the dialog that is no choice, swallowed before
    /// anything in the page sees it: the click after a gesture that moved,
    /// and the tail of the press that opened the sheet ([`stray_after_open`]).
    fn on_click(event: web_sys::Event) {
        let inside = event
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Node>().ok())
            .is_some_and(|node| sheet().is_some_and(|dialog| dialog.contains(Some(&node))));
        if !inside {
            return;
        }
        let now = now_ms();
        let detail = event
            .dyn_ref::<web_sys::MouseEvent>()
            .map_or(0, |mouse| mouse.detail());
        let since_open = POINTER_OPENED_AT.with(Cell::get).map(|at| now - at);
        // Read first: a moved gesture's click is consumed either way.
        let moved = MOVED_CLICK.with(|m| m.borrow_mut().take(now));
        if moved || stray_after_open(detail, since_open) {
            event.stop_immediate_propagation();
            event.prevent_default();
        }
    }

    /// The dialog's listeners, for as long as the result lives: its drags,
    /// and its closing by itself (Escape), after which the state follows.
    pub(in super::super) fn install(surfaces: ReactionSurfaces) -> Option<Listeners> {
        let dialog = sheet()?;
        let window: web_sys::EventTarget = web_sys::window()?.into();
        let on_dialog = |f: fn(&web_sys::HtmlElement, web_sys::Event)| -> Listener {
            let dialog = dialog.clone();
            Closure::new(move |event: web_sys::Event| f(&dialog, event))
        };
        let up = {
            let dialog = dialog.clone();
            Listener::new(move |event: web_sys::Event| on_up(&dialog, event, surfaces))
        };
        let click = Listener::new(on_click);
        let close = Listener::new(move |_| {
            crate::util::defer(move || surfaces.mobile_dialog_closed());
        });
        let target: web_sys::EventTarget = dialog.clone().into();
        let added = vec![
            (target.clone(), "pointerdown", on_dialog(on_down), false),
            (target.clone(), "pointermove", on_dialog(on_move), false),
            (target.clone(), "pointerup", up, false),
            (target.clone(), "pointercancel", on_dialog(on_cancel), false),
            (
                target.clone(),
                "lostpointercapture",
                on_dialog(on_cancel),
                false,
            ),
            (target, "close", close, false),
            // On the window, capturing: before any handler in the page.
            (window, "click", click, true),
        ];
        for (target, kind, listener, capture) in &added {
            let _ = target.add_event_listener_with_callback_and_bool(
                kind,
                listener.as_ref().unchecked_ref(),
                *capture,
            );
        }
        Some(Listeners { added })
    }
}

/// Native builds have no DOM, and so no dialog to dismiss.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn animate_out(_surfaces: super::reaction_picker::ReactionSurfaces) {}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn reset(_by_pointer: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_inside_the_slop_is_still_a_tap() {
        assert_eq!(vertical(0.0, 0.0), None);
        assert_eq!(vertical(6.0, 6.0), None);
        assert_eq!(vertical(0.0, TAP_SLOP_PX), None);
    }

    #[test]
    fn past_the_slop_the_larger_move_decides_and_a_tie_scrolls() {
        assert_eq!(vertical(3.0, 20.0), Some(true));
        assert_eq!(vertical(3.0, -20.0), Some(true));
        assert_eq!(vertical(20.0, 3.0), Some(false));
        assert_eq!(vertical(-15.0, 15.0), Some(false), "a tie");
    }

    #[test]
    fn down_follows_the_finger_and_up_stays_put() {
        assert_eq!(offset(50.0), 50.0);
        assert_eq!(offset(0.0), 0.0);
        assert_eq!(offset(-50.0), 0.0);
        assert_eq!(offset(-500.0), 0.0);
    }

    #[test]
    fn a_release_far_enough_dismisses() {
        // A 90px panel: 40% is 36px, so the 48px floor decides.
        assert!(!dismisses(47.0, 0.1, 90.0));
        assert!(dismisses(48.0, 0.0, 90.0));
        // A 200px panel: 40% (80px) decides.
        assert!(!dismisses(79.0, 0.0, 200.0));
        assert!(dismisses(80.0, 0.0, 200.0));
    }

    #[test]
    fn a_flick_dismisses_once_it_has_moved_a_little() {
        assert!(dismisses(20.0, 0.6, 90.0));
        assert!(!dismisses(20.0, 0.4, 90.0));
        assert!(!dismisses(10.0, 2.0, 90.0));
    }

    #[test]
    fn speed_is_the_recent_moves_unless_held_still_before_lifting() {
        let moves = [(0.0, 100.0), (10.0, 130.0)];
        assert_eq!(velocity(&moves, 50.0), 3.0, "lifted soon after moving");
        assert_eq!(
            velocity(&moves, 10.0 + STILL_MS + 1.0),
            0.0,
            "held still, then lifted"
        );
        assert_eq!(
            velocity(&[(0.0, 100.0)], 5.0),
            0.0,
            "one sample has no speed"
        );
        assert_eq!(velocity(&[], 5.0), 0.0);
        assert_eq!(
            velocity(&[(0.0, 130.0), (10.0, 100.0)], 20.0),
            -3.0,
            "up is negative"
        );
    }

    #[test]
    fn a_moved_lift_swallows_its_own_click_once() {
        let mut moved = MovedClick::default();
        moved.lifted(1_000.0);
        assert!(moved.take(1_005.0));
        assert!(!moved.take(1_010.0), "only once");
    }

    #[test]
    fn a_click_long_after_the_lift_is_a_tap() {
        let mut moved = MovedClick::default();
        moved.lifted(1_000.0);
        assert!(!moved.take(1_000.0 + CLICK_AFTER_LIFT_MS + 1.0));
    }

    #[test]
    fn a_gesture_that_sends_no_click_swallows_nothing() {
        let mut moved = MovedClick::default();
        assert!(!moved.take(0.0), "nothing moved");
        moved.lifted(1_000.0);
        moved.clear();
        assert!(!moved.take(1_001.0), "cancelled, or a second finger");
    }

    #[test]
    fn the_second_click_of_a_double_click_is_never_a_choice() {
        assert!(stray_after_open(2, None));
        assert!(stray_after_open(3, Some(10_000.0)));
    }

    #[test]
    fn a_pointer_click_just_after_a_pointer_opening_is_its_double_tap() {
        assert!(stray_after_open(1, Some(120.0)));
        assert!(!stray_after_open(1, Some(OPEN_GUARD_MS)), "after the guard");
        assert!(!stray_after_open(1, None), "opened by a key");
    }

    #[test]
    fn a_key_is_never_guarded() {
        assert!(!stray_after_open(0, Some(5.0)));
    }

    #[test]
    fn upward_never_dismisses() {
        assert!(!dismisses(-20.0, -3.0, 90.0));
    }
}
