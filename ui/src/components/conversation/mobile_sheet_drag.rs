//! Dragging the mobile message sheet down to dismiss it.
//!
//! A pointer that goes down anywhere in the open dialog (the backdrop, the
//! panel, an action, an emoji) and then moves further down than sideways drags the panel
//! with it, by the distance moved, and fades the dim behind it. Released far
//! enough or fast enough ([`release`]) the panel slides away and the sheet
//! closes; otherwise it springs back. Upward it gives a little and springs
//! back. A sideways move is left alone: on the emoji row the browser is
//! already scrolling it (`touch-action: pan-x`, `main.css`). A second finger
//! (a pinch) cancels the drag, and the zoom goes ahead.
//!
//! A tap outside slides the panel away the same way ([`animate_out`]); Escape
//! and a choice (an emoji, an action) close it at once. With `prefers-reduced-motion: reduce`
//! nothing slides: a dismissal closes at once.
//!
//! The listeners touch only the DOM and the thread-locals here. The one state
//! change, the close, is deferred (dioxus-signal-safety.md) and names the
//! opening it is for, read from the dialog's `data-opening`, so it never
//! closes a later one.

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
/// Upward the panel follows a fifth of the move, at most this far.
const UP_RESISTANCE: f64 = 0.2;
const UP_MAX_PX: f64 = 24.0;

/// Which way a pointer set off, once it is past the tap slop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Axis {
    Vertical,
    Horizontal,
}

/// The way a pointer moved `(dx, dy)` from where it went down is going, or
/// `None` while it is still a tap. A tie is sideways: scrolling the row wins.
pub(super) fn intent(dx: f64, dy: f64) -> Option<Axis> {
    if dx.hypot(dy) <= TAP_SLOP_PX {
        None
    } else if dy.abs() > dx.abs() {
        Some(Axis::Vertical)
    } else {
        Some(Axis::Horizontal)
    }
}

/// Where the panel sits for a pointer `dy` below where it went down.
pub(super) fn offset(dy: f64) -> f64 {
    if dy >= 0.0 {
        dy
    } else {
        (dy * UP_RESISTANCE).max(-UP_MAX_PX)
    }
}

/// The speed (px/ms, down positive) of a release at `released_at`, from the
/// pointer's recent `(time, y)` samples, oldest first: their displacement
/// over their time, or none if the finger was held still for [`STILL_MS`]
/// before it lifted.
pub(super) fn velocity(samples: &[(f64, f64)], released_at: f64) -> f64 {
    let (Some(&(t0, y0)), Some(&(t1, y1))) = (samples.first(), samples.last()) else {
        return 0.0;
    };
    if released_at - t1 > STILL_MS || t1 <= t0 {
        return 0.0;
    }
    (y1 - y0) / (t1 - t0)
}

/// What a release does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Release {
    Dismiss,
    SpringBack,
}

/// A release with the panel `offset` px down, moving at `velocity` px/ms
/// (down is positive), for a panel `panel_height` px tall.
pub(super) fn release(offset: f64, velocity: f64, panel_height: f64) -> Release {
    let far = offset >= MIN_DISMISS_PX.max(panel_height * DISMISS_FRACTION);
    let flick = offset >= MIN_FLICK_PX && velocity >= FLICK_PX_PER_MS;
    if far || flick {
        Release::Dismiss
    } else {
        Release::SpringBack
    }
}

#[cfg(target_arch = "wasm32")]
pub(super) use self::dom::{animate_out, install, reset, Listeners};

#[cfg(target_arch = "wasm32")]
mod dom {
    use super::super::reaction_picker::ReactionSurfaces;
    use super::{intent, offset, release, velocity, Axis, Release, VELOCITY_WINDOW_MS};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    /// The pointer gesture under way in the open dialog.
    struct Gesture {
        pointer: i32,
        x0: f64,
        y0: f64,
        axis: Option<Axis>,
        offset: f64,
        /// `(time, y)` over the last [`VELOCITY_WINDOW_MS`].
        samples: VecDeque<(f64, f64)>,
    }

    thread_local! {
        static GESTURE: RefCell<Option<Gesture>> = const { RefCell::new(None) };
        /// A gesture moved past the tap slop: the click that may follow it
        /// (a mouse's, mostly) is not a tap on what it lands on.
        static SUPPRESS_CLICK: Cell<bool> = const { Cell::new(false) };
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

    fn element(id: &str) -> Option<web_sys::HtmlElement> {
        web_sys::window()?
            .document()?
            .get_element_by_id(id)?
            .dyn_into::<web_sys::HtmlElement>()
            .ok()
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
    pub(in super::super) fn animate_out(id: &str, surfaces: ReactionSurfaces) {
        let Some(dialog) = element(id) else {
            return;
        };
        leave(&dialog, surfaces);
    }

    fn leave(dialog: &web_sys::HtmlElement, surfaces: ReactionSurfaces) {
        let Some(token) = opening_token(dialog) else {
            return;
        };
        if dialog.class_list().contains(LEAVING) {
            return;
        }
        let Some(panel) = panel(dialog).filter(|_| !reduced_motion()) else {
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

    /// Clear what a drag or a slide left on dialog `id`, before it shows: a
    /// surface closed mid-drag opens at rest.
    pub(in super::super) fn reset(id: &str) {
        GESTURE.with(|g| g.borrow_mut().take());
        SUPPRESS_CLICK.with(|s| s.set(false));
        let Some(dialog) = element(id) else {
            return;
        };
        let _ = dialog.class_list().remove_2(DRAGGING, LEAVING);
        let _ = dialog.style().remove_property(SHADE);
        if let Some(panel) = panel(&dialog) {
            let _ = panel.style().remove_property("transform");
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
                // A second finger: a pinch, not a drag.
                GESTURE.with(|g| g.borrow_mut().take());
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
        SUPPRESS_CLICK.with(|s| s.set(false));
        let (x, y) = (f64::from(e.client_x()), f64::from(e.client_y()));
        let gesture = Gesture {
            pointer: e.pointer_id(),
            x0: x,
            y0: y,
            axis: None,
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
            if gesture.axis.is_none() {
                match intent(dx, dy)? {
                    Axis::Horizontal => {
                        SUPPRESS_CLICK.with(|s| s.set(true));
                        slot.take();
                        return None;
                    }
                    Axis::Vertical => {
                        gesture.axis = Some(Axis::Vertical);
                        SUPPRESS_CLICK.with(|s| s.set(true));
                        let _ = dialog.set_pointer_capture(gesture.pointer);
                        let _ = dialog.class_list().add_1(DRAGGING);
                    }
                }
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
        if gesture.axis != Some(Axis::Vertical) {
            return;
        }
        let _ = dialog.release_pointer_capture(gesture.pointer);
        let samples: Vec<_> = gesture.samples.drain(..).collect();
        let speed = velocity(&samples, event.time_stamp());
        let height = panel(dialog).map_or(1.0, |p| f64::from(p.offset_height()));
        match release(gesture.offset, speed, height) {
            Release::Dismiss => leave(dialog, surfaces),
            Release::SpringBack => spring_back(dialog),
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
        if take_gesture(e.pointer_id()).is_some_and(|g| g.axis == Some(Axis::Vertical)) {
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

    /// The click after a gesture that moved, inside dialog `id`: swallowed
    /// before anything in the page sees it.
    fn on_click(id: &str, event: web_sys::Event) {
        let inside = event
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.closest(&format!("#{id}")).ok().flatten())
            .is_some();
        if inside && SUPPRESS_CLICK.with(|s| s.replace(false)) {
            event.stop_immediate_propagation();
            event.prevent_default();
        }
    }

    /// Listen for drags on dialog `id`, for as long as the result lives.
    pub(in super::super) fn install(
        id: &'static str,
        surfaces: ReactionSurfaces,
    ) -> Option<Listeners> {
        let dialog = element(id)?;
        let window: web_sys::EventTarget = web_sys::window()?.into();
        let on_dialog = |f: fn(&web_sys::HtmlElement, web_sys::Event)| -> Listener {
            let dialog = dialog.clone();
            Closure::new(move |event: web_sys::Event| f(&dialog, event))
        };
        let up = {
            let dialog = dialog.clone();
            Listener::new(move |event: web_sys::Event| on_up(&dialog, event, surfaces))
        };
        let click = Listener::new(move |event: web_sys::Event| on_click(id, event));
        let target: web_sys::EventTarget = dialog.clone().into();
        let added = vec![
            (target.clone(), "pointerdown", on_dialog(on_down), false),
            (target.clone(), "pointermove", on_dialog(on_move), false),
            (target.clone(), "pointerup", up, false),
            (target.clone(), "pointercancel", on_dialog(on_cancel), false),
            (target, "lostpointercapture", on_dialog(on_cancel), false),
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
pub(super) fn animate_out(_id: &str, _surfaces: super::reaction_picker::ReactionSurfaces) {}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn reset(_id: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_inside_the_slop_is_still_a_tap() {
        assert_eq!(intent(0.0, 0.0), None);
        assert_eq!(intent(6.0, 6.0), None);
        assert_eq!(intent(0.0, TAP_SLOP_PX), None);
    }

    #[test]
    fn past_the_slop_the_larger_move_decides_and_a_tie_scrolls() {
        assert_eq!(intent(3.0, 20.0), Some(Axis::Vertical));
        assert_eq!(intent(3.0, -20.0), Some(Axis::Vertical));
        assert_eq!(intent(20.0, 3.0), Some(Axis::Horizontal));
        assert_eq!(intent(-15.0, 15.0), Some(Axis::Horizontal));
    }

    #[test]
    fn down_follows_the_finger_and_up_resists() {
        assert_eq!(offset(50.0), 50.0);
        assert_eq!(offset(0.0), 0.0);
        assert_eq!(offset(-50.0), -10.0);
        assert_eq!(offset(-500.0), -UP_MAX_PX);
    }

    #[test]
    fn a_release_far_enough_dismisses() {
        // A 90px panel: 40% is 36px, so the 48px floor decides.
        assert_eq!(release(47.0, 0.1, 90.0), Release::SpringBack);
        assert_eq!(release(48.0, 0.0, 90.0), Release::Dismiss);
        // A 200px panel: 40% (80px) decides.
        assert_eq!(release(79.0, 0.0, 200.0), Release::SpringBack);
        assert_eq!(release(80.0, 0.0, 200.0), Release::Dismiss);
    }

    #[test]
    fn a_flick_dismisses_once_it_has_moved_a_little() {
        assert_eq!(release(20.0, 0.6, 90.0), Release::Dismiss);
        assert_eq!(release(20.0, 0.4, 90.0), Release::SpringBack);
        assert_eq!(release(10.0, 2.0, 90.0), Release::SpringBack);
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
    fn upward_never_dismisses() {
        assert_eq!(release(-20.0, -3.0, 90.0), Release::SpringBack);
    }
}
