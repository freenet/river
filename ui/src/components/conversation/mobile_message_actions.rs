//! How a row opens the mobile message sheet (`mobile_message_sheet.rs`):
//! below 48rem, a tap on a signed message, or its keyboard entry, opens the
//! sheet for that exact message. Its + opens the same sheet
//! (`reaction_picker.rs`); its chips react (`conversation.rs`).
//! `mobile_surface.rs` has the sheet's state and lifecycle.
//!
//! Row taps ([`row_tap`], [`row_clicked`]). One ordinary tap on a message's
//! non-interactive content or background opens it. Anything with a behaviour
//! of its own keeps it and opens nothing: links, mentions and share links in
//! the raw message HTML, buttons (author name, reply strip, React), reaction
//! chips and their reactor lists, fields and the edit form, media and
//! disclosure controls, and anything marked `data-row-actions="exclude"` (the
//! avatar). The click's actual DOM target is checked, ancestors up to the row,
//! because the message body is raw HTML whose delegated handlers Dioxus does
//! not see. A drag or scroll (a press that moved, or one the browser
//! cancelled), a second finger, a press too old to be this click, and a tap
//! that completes a text selection open nothing; touch scrolling and text
//! selection themselves are left alone. A tap or click opens at once, whatever
//! the pointer: the first click of a double-click opens the sheet, and the
//! second is never a tap. Rows take no double-tap zoom below 48rem
//! (`touch-action: manipulation`, `main.css`), so iOS Safari does not hold the
//! click back waiting for a second tap.
//!
//! Keyboard. Each row has one visually hidden native button
//! ([`MobileActionsEntry`]), shown on keyboard focus, which opens the same
//! sheet. The row's own semantics are untouched: no `role="button"` around its
//! Markdown. The entry is also the row's hook into an open sheet: it keeps
//! the sheet's reactions in step with the row, and its unmounting is how the
//! sheet learns its message left.

use super::mobile_surface::{narrow_now, MobileOpening, Origin, SHEET_ID, SHEET_TITLE};
use super::reaction_picker::ReactionSurfaces;
use dioxus::prelude::*;
use river_core::room_state::message::MessageId;

/// The message's own controls and the marked exclusions: a click inside any
/// of these is that control's, never the row's.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const EXCLUDED: &str = "a, button, input, textarea, select, option, label, summary, details, \
     video, audio, iframe, object, embed, [role=\"button\"], [role=\"link\"], \
     [role=\"menuitem\"], [role=\"dialog\"], [contenteditable=\"\"], [contenteditable=\"true\"], \
     [tabindex]:not([tabindex=\"-1\"]), [data-row-actions=\"exclude\"], [data-member-id]";

/// A press on a message row, kept for the click it may become. Not a signal:
/// nothing renders from it.
#[derive(Clone, Debug, PartialEq)]
struct RowPress {
    row: String,
    pointer: i32,
    x: f64,
    y: f64,
    at: f64,
    /// Another finger landed during it (a pinch), or the browser cancelled
    /// it (it became a scroll).
    spoiled: bool,
}

thread_local! {
    static ROW_PRESS: std::cell::RefCell<Option<RowPress>> =
        const { std::cell::RefCell::new(None) };
}

/// A `pointerdown` on `row`. A primary pointer, by definition the first
/// contact down, starts the press. Any other pointer is a second finger,
/// which spoils the press under way: a pinch is no tap.
pub(super) fn note_row_press(row: &str, e: &PointerEvent) {
    let data = e.data();
    let point = data.client_coordinates();
    ROW_PRESS.with(|p| {
        let mut p = p.borrow_mut();
        if !data.is_primary() {
            if let Some(press) = p.as_mut() {
                press.spoiled = true;
            }
            return;
        }
        *p = Some(RowPress {
            row: row.to_string(),
            pointer: data.pointer_id(),
            x: point.x,
            y: point.y,
            at: super::now_ms(),
            spoiled: false,
        });
    });
}

/// A contact on a row was cancelled (`pointercancel`, the browser taking it
/// for a scroll): its press opens nothing.
pub(super) fn note_row_cancel(e: &PointerEvent) {
    let pointer = e.data().pointer_id();
    ROW_PRESS.with(|p| {
        if let Some(p) = p.borrow_mut().as_mut() {
            if p.pointer == pointer {
                p.spoiled = true;
            }
        }
    });
}

/// The message a row opens the sheet for, as the row renders it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct RowTarget {
    /// The row's `msg-{row}` suffix.
    pub(super) row: String,
    pub(super) message: MessageId,
    /// The message is the reader's own (the sheet offers Edit and Delete).
    /// Shown from this; checked again against the room when an action runs.
    pub(super) own: bool,
    /// The reader's own reaction on it, as its footer renders it (pending
    /// ones included). Only marks the current choice: the pending-aware
    /// toggle decides from the room itself.
    pub(super) current: Option<String>,
}

/// Whether a click came from a pointer (a tap or a mouse click) rather than
/// a key: a key's activation click has `detail` 0.
fn pointer_click(e: &MouseEvent) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        e.data()
            .downcast::<web_sys::MouseEvent>()
            .is_some_and(|m| m.detail() > 0)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = e;
        false
    }
}

/// Open the sheet for `target`, from the handler of `origin`'s click `e`. The
/// opening is committed deferred.
pub(super) fn open_sheet(
    surfaces: ReactionSurfaces,
    target: RowTarget,
    origin: Origin,
    e: &MouseEvent,
) {
    let opening = MobileOpening::new(target, origin, pointer_click(e));
    crate::util::defer(move || surfaces.open_mobile(opening));
}

/// A click on `target`'s row, from its handler: opens its sheet at once, or
/// not at all ([`row_tap`]). A row being edited has no entry, so nothing
/// opens for it (`opening_is_live`).
pub(super) fn row_clicked(surfaces: ReactionSurfaces, e: &MouseEvent, target: RowTarget) {
    if row_tap(e, &target.row) {
        open_sheet(surfaces, target, Origin::Entry, e);
    }
}

/// Whether a non-empty text selection touches the row `row_el`.
#[cfg(target_arch = "wasm32")]
fn row_has_selection(row_el: &web_sys::Element) -> bool {
    web_sys::window()
        .and_then(|w| w.get_selection().ok().flatten())
        .is_some_and(|s| {
            !s.is_collapsed()
                && (s.anchor_node().is_some_and(|n| row_el.contains(Some(&n)))
                    || s.focus_node().is_some_and(|n| row_el.contains(Some(&n))))
        })
}

/// Whether this click on `row` is a tap that opens its sheet: below 48rem,
/// a plain primary single click, on nothing interactive, completing no
/// selection, from a press on this row that neither moved nor was spoiled. A
/// click with no press of its own (assistive technology) counts. Consumes
/// the press.
fn row_tap(e: &MouseEvent, row: &str) -> bool {
    let press = ROW_PRESS.with(|p| p.borrow_mut().take());
    if !narrow_now() {
        return false;
    }
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        let Some(mouse) = e.data().downcast::<web_sys::MouseEvent>().cloned() else {
            return false;
        };
        // The second click of a double-click (or third of a triple) is the
        // reader selecting a word or line: never a tap.
        if mouse.button() != 0
            || mouse.detail() > 1
            || mouse.ctrl_key()
            || mouse.meta_key()
            || mouse.shift_key()
            || mouse.alt_key()
        {
            return false;
        }
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            return false;
        };
        let Some(row_el) = document.get_element_by_id(&format!("msg-{row}")) else {
            return false;
        };
        // The target may be a text node's parent, deep in the raw HTML.
        let Some(target) = mouse
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Node>().ok())
        else {
            return false;
        };
        let target = match target.dyn_ref::<web_sys::Element>() {
            Some(el) => el.clone(),
            None => match target.parent_element() {
                Some(el) => el,
                None => return false,
            },
        };
        if !row_el.contains(Some(&target)) {
            return false;
        }
        let excluded = target
            .closest(EXCLUDED)
            .ok()
            .flatten()
            .is_some_and(|control| row_el.contains(Some(&control)));
        if excluded {
            return false;
        }
        if row_has_selection(&row_el) {
            return false;
        }
        press_makes_tap(
            press.as_ref(),
            row,
            f64::from(mouse.client_x()),
            f64::from(mouse.client_y()),
            super::now_ms(),
        )
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (e, row, press);
        false
    }
}

/// [`row_tap`]'s press half, pure for native tests: no press is
/// a click from elsewhere (it counts); a press counts only if it is this
/// row's, fresh, unspoiled and has not moved past a tap's slop.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn press_makes_tap(press: Option<&RowPress>, row: &str, x: f64, y: f64, now: f64) -> bool {
    match press {
        None => true,
        Some(p) => {
            p.row == row
                && !p.spoiled
                && now - p.at <= super::PRESS_FRESH_MS
                && (x - p.x).hypot(y - p.y) <= super::TAP_SLOP_PX
        }
    }
}

/// A row's keyboard way into its sheet: a native button, visually hidden
/// until it has keyboard focus, below 48rem only (`main.css`). Rendered in
/// every signed row not being edited. While the sheet is open for its row it
/// keeps the sheet's current reaction in step with the row
/// (a reaction landing from another device, or one still being signed).
#[component]
pub(super) fn MobileActionsEntry(
    surfaces: ReactionSurfaces,
    /// The row's message, as the row renders it.
    target: RowTarget,
) -> Element {
    // The row's current reaction is not the one `open` has.
    let differs = |open: &MobileOpening, target: &RowTarget| open.target.current != target.current;
    let (expanded, stale) = match surfaces.mobile_for_render() {
        Some(open) if open.is_for(&target.row) => (true, differs(&open, &target)),
        _ => (false, false),
    };
    if stale {
        let target = target.clone();
        let mut opening = surfaces.mobile;
        crate::util::defer(move || {
            let stale = opening
                .peek()
                .as_ref()
                .is_some_and(|o| o.is_for(&target.row) && differs(o, &target));
            if stale {
                opening.with_mut(|o| {
                    if let Some(o) = o.as_mut() {
                        o.target.current = target.current;
                    }
                });
            }
        });
    }
    // Its row leaving the history closes a sheet open for it; the row only
    // remounting keeps it, and focus returns to the new control.
    {
        let row = target.row.clone();
        use_drop(move || surfaces.mobile_control_unmounting(&row));
    }
    rsx! {
        button {
            r#type: "button",
            "data-testid": "mobile-message-actions-entry",
            class: "mobile-actions-entry",
            "aria-label": SHEET_TITLE,
            "aria-haspopup": "dialog",
            "aria-controls": SHEET_ID,
            "aria-expanded": "{expanded}",
            onclick: move |e: MouseEvent| {
                open_sheet(surfaces, target.clone(), Origin::Entry, &e);
            },
            "Options"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(row: &str, x: f64, y: f64, at: f64) -> RowPress {
        RowPress {
            row: row.into(),
            pointer: 1,
            x,
            y,
            at,
            spoiled: false,
        }
    }

    /// A tap is a fresh, still, unspoiled press on the same row; a click
    /// with no press of its own (assistive technology) counts too.
    #[test]
    fn only_a_still_fresh_press_on_this_row_is_a_tap() {
        let p = press("m", 10.0, 10.0, 1000.0);
        assert!(press_makes_tap(Some(&p), "m", 12.0, 14.0, 1100.0));
        assert!(press_makes_tap(None, "m", 0.0, 0.0, 0.0), "no press");
        assert!(
            !press_makes_tap(Some(&p), "n", 10.0, 10.0, 1100.0),
            "another row"
        );
        assert!(
            !press_makes_tap(Some(&p), "m", 30.0, 10.0, 1100.0),
            "moved: a drag"
        );
        assert!(
            !press_makes_tap(
                Some(&p),
                "m",
                10.0,
                10.0,
                1000.0 + super::super::PRESS_FRESH_MS + 1.0
            ),
            "too old to be this click"
        );
        let spoiled = RowPress { spoiled: true, ..p };
        assert!(
            !press_makes_tap(Some(&spoiled), "m", 10.0, 10.0, 1100.0),
            "cancelled or pinched"
        );
    }
}
