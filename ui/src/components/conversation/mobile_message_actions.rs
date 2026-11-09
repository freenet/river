//! How a row opens the mobile message sheet (`mobile_message_sheet.rs`):
//! below 48rem, a tap on a signed message, or its keyboard entry, opens the
//! sheet for that exact message. Its + and its chips open the same sheet
//! (`reaction_picker.rs`, `conversation.rs`). `mobile_surface.rs` has the
//! sheet's state and lifecycle.
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
//! selection themselves are left alone. A finger's tap opens at once; a mouse
//! or pen click waits out the double-click window
//! ([`ROW_DOUBLE_CLICK_WAIT_MS`]) and opens only if no other row click came
//! and nothing was selected meanwhile, so a narrow mouse window keeps double-
//! and triple-click selection (the second click is never a tap, and the
//! sheet never opens under it).
//!
//! Keyboard. Each row has one visually hidden native button
//! ([`MobileActionsEntry`]), shown on keyboard focus, which opens the same
//! sheet. The row's own semantics are untouched: no `role="button"` around its
//! Markdown. The entry is also the row's hook into an open sheet: it keeps
//! the sheet's reactions in step with the row, and its unmounting is how the
//! sheet learns its message left.

use super::mobile_message_sheet::SheetReaction;
use super::mobile_surface::{narrow_now, MobileOpening, Origin, SHEET_ID};
use super::reaction_picker::ReactionSurfaces;
use dioxus::prelude::*;
use river_core::room_state::message::MessageId;
use std::rc::Rc;

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
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
struct RowPress {
    row: String,
    pointer: i32,
    x: f64,
    y: f64,
    at: f64,
    /// Another finger landed during it (a pinch), or the browser cancelled
    /// it (it became a scroll).
    spoiled: bool,
    /// A finger: it opens at once. A mouse or pen waits out the double-click
    /// window ([`ROW_DOUBLE_CLICK_WAIT_MS`]).
    touch: bool,
}

/// How long a mouse or pen click on a row waits before opening its actions,
/// so that the first click of a double- or triple-click (which selects a word
/// or a line) never opens the sheet over the text: any later click on a row
/// cancels it, and so does a selection made meanwhile. 500ms is the Windows
/// default double-click time, as the room description's click uses. A finger
/// opens at once: a double tap does not select a word.
const ROW_DOUBLE_CLICK_WAIT_MS: u64 = 500;

thread_local! {
    static ROW_PRESS: std::cell::RefCell<Option<RowPress>> =
        const { std::cell::RefCell::new(None) };
    /// The touch or pen contacts down on message rows, by pointer id.
    static CONTACTS: std::cell::RefCell<Vec<i32>> = const { std::cell::RefCell::new(Vec::new()) };
    /// How many clicks rows have had. A click waiting out the double-click
    /// window opens only if no row click came after it.
    static ROW_CLICKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A `pointerdown` on `row`. A second contact while one is down spoils the
/// press: a pinch is no tap. A primary pointer is by definition the first
/// contact down, so it starts the contact list afresh: a contact whose
/// `pointerup` never reached a row (its row was replaced mid-touch) cannot
/// leave every later press looking like a pinch.
pub(super) fn note_row_press(row: &str, e: &PointerEvent) {
    let data = e.data();
    let pointer = data.pointer_id();
    let pointer_type = data.pointer_type();
    let contact = matches!(pointer_type.as_str(), "touch" | "pen");
    let primary = data.is_primary();
    let down = CONTACTS.with(|c| {
        let mut c = c.borrow_mut();
        if primary {
            c.clear();
        }
        if contact && !c.contains(&pointer) {
            c.push(pointer);
        }
        c.len()
    });
    let point = data.client_coordinates();
    ROW_PRESS.with(|p| {
        let mut p = p.borrow_mut();
        if down > 1 || !primary {
            // A second finger: the press under way is a pinch, not a tap.
            if let Some(press) = p.as_mut() {
                press.spoiled = true;
            }
            return;
        }
        *p = Some(RowPress {
            row: row.to_string(),
            pointer,
            x: point.x,
            y: point.y,
            at: super::now_ms(),
            spoiled: false,
            touch: pointer_type == "touch",
        });
    });
}

/// A contact on a row ended (`pointerup`) or was cancelled
/// (`pointercancel`, the browser taking it for a scroll): a cancelled press
/// opens nothing.
pub(super) fn note_row_release(e: &PointerEvent, cancelled: bool) {
    let pointer = e.data().pointer_id();
    CONTACTS.with(|c| c.borrow_mut().retain(|&down| down != pointer));
    if cancelled {
        ROW_PRESS.with(|p| {
            if let Some(p) = p.borrow_mut().as_mut() {
                if p.pointer == pointer {
                    p.spoiled = true;
                }
            }
        });
    }
}

/// What a click on a row does to its sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowTap {
    Nothing,
    /// A finger's tap, or a click with no press of its own (assistive
    /// technology): open now.
    Open,
    /// A mouse or pen click: open once the double-click window has passed
    /// with no other row click and no selection.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    OpenAfterWindow,
}

/// The message a row opens the sheet for, as the row renders it.
#[derive(Clone)]
pub(super) struct RowTarget {
    pub(super) row: String,
    pub(super) message: MessageId,
    /// The message is the reader's own.
    pub(super) own: bool,
    /// The reader's own reaction on it.
    pub(super) current: Option<String>,
    pub(super) reactions: Rc<Vec<SheetReaction>>,
}

impl RowTarget {
    fn opening(self, origin: Origin) -> MobileOpening {
        MobileOpening::new(
            origin,
            self.row,
            self.message,
            self.own,
            self.current,
            self.reactions,
        )
    }
}

/// Open the sheet for `target`, from `origin`'s handler. The opening is
/// committed deferred.
pub(super) fn open_sheet(surfaces: ReactionSurfaces, target: RowTarget, origin: Origin) {
    let opening = target.opening(origin);
    crate::util::defer(move || {
        surfaces.open_mobile(opening);
    });
}

/// A click on `target`'s row, from its handler: opens its sheet now, after
/// the double-click window, or not at all ([`row_tap`]). Every row click,
/// whatever it does, cancels a click still waiting. A row being edited opens
/// nothing.
pub(super) fn row_clicked(
    surfaces: ReactionSurfaces,
    e: &MouseEvent,
    target: RowTarget,
    editing: bool,
) {
    let click = ROW_CLICKS.with(|c| {
        let next = c.get().wrapping_add(1);
        c.set(next);
        next
    });
    let tap = row_tap(e, &target.row);
    if editing || tap == RowTap::Nothing {
        return;
    }
    if tap == RowTap::Open {
        open_sheet(surfaces, target, Origin::Entry);
        return;
    }
    crate::util::safe_spawn_local(async move {
        crate::util::sleep(crate::util::millis(ROW_DOUBLE_CLICK_WAIT_MS)).await;
        let still = ROW_CLICKS.with(|c| c.get()) == click
            && narrow_now()
            && !row_has_selection(&target.row);
        if still {
            open_sheet(surfaces, target, Origin::Entry);
        }
    });
}

/// Whether a non-empty text selection touches `row`.
fn row_has_selection(row: &str) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        let Some(row_el) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id(&format!("msg-{row}")))
        else {
            return false;
        };
        web_sys::window()
            .and_then(|w| w.get_selection().ok().flatten())
            .is_some_and(|s| {
                !s.is_collapsed()
                    && (s.anchor_node().is_some_and(|n| row_el.contains(Some(&n)))
                        || s.focus_node().is_some_and(|n| row_el.contains(Some(&n))))
            })
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = row;
        false
    }
}

/// Whether this click on `row` is a tap that opens its sheet: below 48rem,
/// a plain primary single click, on nothing interactive, completing no
/// selection, from a press on this row that neither moved nor was spoiled. A
/// click with no press of its own (assistive technology) counts. Consumes
/// the press.
fn row_tap(e: &MouseEvent, row: &str) -> RowTap {
    let press = ROW_PRESS.with(|p| p.borrow_mut().take());
    if !narrow_now() {
        return RowTap::Nothing;
    }
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        let Some(mouse) = e.data().downcast::<web_sys::MouseEvent>().cloned() else {
            return RowTap::Nothing;
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
            return RowTap::Nothing;
        }
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            return RowTap::Nothing;
        };
        let Some(row_el) = document.get_element_by_id(&format!("msg-{row}")) else {
            return RowTap::Nothing;
        };
        // The target may be a text node's parent, deep in the raw HTML.
        let Some(target) = mouse
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Node>().ok())
        else {
            return RowTap::Nothing;
        };
        let target = match target.dyn_ref::<web_sys::Element>() {
            Some(el) => el.clone(),
            None => match target.parent_element() {
                Some(el) => el,
                None => return RowTap::Nothing,
            },
        };
        if !row_el.contains(Some(&target)) {
            return RowTap::Nothing;
        }
        let excluded = target
            .closest(EXCLUDED)
            .ok()
            .flatten()
            .is_some_and(|control| row_el.contains(Some(&control)));
        if excluded {
            return RowTap::Nothing;
        }
        if row_has_selection(row) {
            return RowTap::Nothing;
        }
        let tap = press_makes_tap(
            press.as_ref(),
            row,
            f64::from(mouse.client_x()),
            f64::from(mouse.client_y()),
            super::now_ms(),
        );
        match press {
            _ if !tap => RowTap::Nothing,
            Some(p) if !p.touch => RowTap::OpenAfterWindow,
            _ => RowTap::Open,
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (e, row, press);
        RowTap::Nothing
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
/// keeps the sheet's current reaction and reactor list in step with the row
/// (a reaction landing from another device, or one still being signed).
#[component]
pub(super) fn MobileActionsEntry(
    surfaces: ReactionSurfaces,
    /// The row's `msg-{row}` suffix.
    row: String,
    message: MessageId,
    /// The message is the reader's own.
    own: bool,
    /// The reader's own reaction on it, if any.
    current: Option<String>,
    /// Its reactions, as its chips show them.
    reactions: Rc<Vec<SheetReaction>>,
) -> Element {
    // Infallible, deliberately: `Conversation`'s own `use_signal`, written
    // only from deferred callbacks.
    let (expanded, stale) = match surfaces.mobile.read().as_ref() {
        Some(open) if open.is_for(&row) => {
            (true, open.current != current || open.reactions != reactions)
        }
        _ => (false, false),
    };
    if stale {
        let (row, current, reactions) = (row.clone(), current.clone(), reactions.clone());
        let mut opening = surfaces.mobile;
        crate::util::defer(move || {
            let stale = opening.peek().as_ref().is_some_and(|o| {
                o.is_for(&row) && (o.current != current || o.reactions != reactions)
            });
            if stale {
                opening.with_mut(|o| {
                    if let Some(o) = o.as_mut() {
                        o.current = current;
                        o.reactions = reactions;
                    }
                });
            }
        });
    }
    // Its row leaving the history closes a sheet open for it; the row only
    // remounting keeps it, and focus returns to the new control.
    {
        let row = row.clone();
        use_drop(move || surfaces.mobile_control_unmounting(&row));
    }
    rsx! {
        button {
            r#type: "button",
            "data-testid": "mobile-message-actions-entry",
            class: "mobile-actions-entry",
            "aria-label": "Message options",
            "aria-haspopup": "dialog",
            "aria-controls": SHEET_ID,
            "aria-expanded": "{expanded}",
            onclick: move |_| {
                if !narrow_now() {
                    return;
                }
                let target = RowTarget {
                    row: row.clone(),
                    message: message.clone(),
                    own,
                    current: current.clone(),
                    reactions: reactions.clone(),
                };
                open_sheet(surfaces, target, Origin::Entry);
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
            touch: true,
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
