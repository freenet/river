//! The mobile actions sheet: below 48rem, a tap on a signed message opens
//! Reply, and Edit and Delete on your own, for that exact message, and nothing
//! else (no reactions: its + opens the reaction popup,
//! `mobile_reaction_input.rs`). `mobile_surface.rs` has what the two mobile
//! surfaces share.
//!
//! Row taps ([`row_tap_opens_actions`]). One ordinary tap on a message's
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
//! selection themselves are left alone.
//!
//! Keyboard. Each row has one visually hidden native button
//! ([`MobileActionsEntry`]), shown on keyboard focus, which opens the same
//! sheet. The row's own semantics are untouched: no `role="button"` around its
//! Markdown.
//!
//! The sheet focuses its first action, never a field, so it never brings up
//! the software keyboard. Reply, Edit and Delete close it first and then hand
//! over to the existing flows (the composer's reply, the inline editor via
//! `edit_trigger`, the delete confirmation), which take focus; an ordinary
//! dismissal returns focus to the row's entry. What an action does is decided
//! when it runs, from the room as it is then (`Conversation`'s
//! `on_mobile_action`).

#[cfg(target_arch = "wasm32")]
use super::mobile_surface::dom;
use super::mobile_surface::{narrow_now, opening_is_live, MobileOpening, MobileSurface, SHEET_ID};
use super::reaction_picker::ReactionSurfaces;
use crate::components::icons::{
    line_icon, CLOSE_ICON_PATH, DELETE_ICON_PATH, EDIT_ICON_PATH, REPLY_ICON_PATH,
};
use dioxus::prelude::*;
use river_core::room_state::message::MessageId;

/// One of the sheet's actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MobileAction {
    Reply,
    Edit,
    Delete,
}

/// The message's own controls and the marked exclusions: a click inside any
/// of these is that control's, never the row's.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const EXCLUDED: &str = "a, button, input, textarea, select, option, label, summary, details, \
     video, audio, iframe, object, embed, [role=\"button\"], [role=\"link\"], \
     [role=\"menuitem\"], [role=\"dialog\"], [contenteditable=\"\"], [contenteditable=\"true\"], \
     [tabindex]:not([tabindex=\"-1\"]), [data-row-actions=\"exclude\"], [data-member-id]";

/// How far a press may wander and still be a tap, in CSS px.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const TAP_SLOP_PX: f64 = 10.0;

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
}

thread_local! {
    static ROW_PRESS: std::cell::RefCell<Option<RowPress>> =
        const { std::cell::RefCell::new(None) };
    /// The touch or pen contacts down on message rows, by pointer id.
    static CONTACTS: std::cell::RefCell<Vec<i32>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A `pointerdown` on `row`. A second contact while one is down spoils the
/// press: a pinch is no tap.
pub(super) fn note_row_press(row: &str, e: &PointerEvent) {
    let data = e.data();
    let pointer = data.pointer_id();
    let contact = matches!(data.pointer_type().as_str(), "touch" | "pen");
    let down = CONTACTS.with(|c| {
        let mut c = c.borrow_mut();
        if contact && !c.contains(&pointer) {
            c.push(pointer);
        }
        c.len()
    });
    let point = data.client_coordinates();
    ROW_PRESS.with(|p| {
        let mut p = p.borrow_mut();
        if down > 1 || !data.is_primary() {
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

/// Whether this click on `row` is a tap that opens its actions: below 48rem,
/// a plain primary click, on nothing interactive, completing no selection,
/// from a press on this row that neither moved nor was spoiled. A click with
/// no press of its own (assistive technology) counts. Consumes the press.
pub(super) fn row_tap_opens_actions(e: &MouseEvent, row: &str) -> bool {
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
        if mouse.button() != 0
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
        let selecting = web_sys::window()
            .and_then(|w| w.get_selection().ok().flatten())
            .is_some_and(|s| {
                !s.is_collapsed()
                    && (s.anchor_node().is_some_and(|n| row_el.contains(Some(&n)))
                        || s.focus_node().is_some_and(|n| row_el.contains(Some(&n))))
            });
        if selecting {
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

/// [`row_tap_opens_actions`]'s press half, pure for native tests: no press is
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
                && (x - p.x).hypot(y - p.y) <= TAP_SLOP_PX
        }
    }
}

/// Open the actions sheet for `opening`, from a row tap or the entry. From
/// the event handler; the opening is committed deferred.
pub(super) fn open_actions(surfaces: ReactionSurfaces, opening: MobileOpening) {
    crate::util::defer(move || {
        surfaces.open_mobile(opening);
    });
}

/// A row's keyboard way into its actions sheet: a native button, visually
/// hidden until it has keyboard focus, below 48rem only (`main.css`). Rendered
/// in every signed row not being edited.
#[component]
pub(super) fn MobileActionsEntry(
    surfaces: ReactionSurfaces,
    /// The row's `msg-{row}` suffix.
    row: String,
    message: MessageId,
    /// The message is the reader's own.
    own: bool,
) -> Element {
    // Infallible, deliberately: `Conversation`'s own `use_signal`, written
    // only from deferred callbacks.
    let expanded = surfaces
        .mobile
        .opening
        .read()
        .as_ref()
        .is_some_and(|o| o.surface == MobileSurface::Actions && o.row == row);
    // Its row leaving the history closes a sheet open for it; the row only
    // remounting keeps it, and focus returns to the new entry.
    {
        let row = row.clone();
        use_drop(move || {
            let Ok(open) = surfaces.mobile.opening.try_peek() else {
                return;
            };
            if let Some(token) = open
                .as_ref()
                .filter(|o| o.surface == MobileSurface::Actions && o.row == row)
                .map(|o| o.token)
            {
                crate::util::defer(move || {
                    surfaces.mobile_control_dropped(MobileSurface::Actions, token)
                });
            }
        });
    }
    rsx! {
        button {
            r#type: "button",
            "data-testid": "mobile-message-actions-entry",
            class: "mobile-actions-entry",
            "aria-label": "Message actions",
            "aria-haspopup": "dialog",
            "aria-controls": SHEET_ID,
            "aria-expanded": "{expanded}",
            onclick: move |_| {
                if !narrow_now() {
                    return;
                }
                let opening = MobileOpening::new(
                    MobileSurface::Actions,
                    row.clone(),
                    message.clone(),
                    own,
                    None,
                );
                open_actions(surfaces, opening);
            },
            "Actions"
        }
    }
}

/// An action for opening `token`, run deferred: the sheet closes first, then,
/// if that opening was still the open one and still live, the action is
/// handed to `Conversation`, which checks it against the room as it is now.
fn dispatch(
    surfaces: ReactionSurfaces,
    token: u64,
    action: MobileAction,
    on_action: Callback<(MobileAction, String, MessageId)>,
) {
    let Some(opening) = surfaces.mobile_opening_for(MobileSurface::Actions, token) else {
        return;
    };
    let live = opening_is_live(&opening);
    surfaces.close_mobile(!live);
    if live {
        on_action.call((action, opening.row, opening.message));
    }
}

/// The conversation's one mobile actions sheet. Rendered once by
/// `Conversation`, closed until a row tap or entry opens it below 48rem.
#[component]
pub(super) fn MobileMessageActionsSheet(
    surfaces: ReactionSurfaces,
    on_action: Callback<(MobileAction, String, MessageId)>,
) -> Element {
    // Infallible, deliberately: `Conversation`'s own `use_signal`, written
    // only from deferred callbacks, so no write is live during a render.
    let opening = surfaces
        .mobile
        .opening
        .read()
        .clone()
        .filter(|o| o.surface == MobileSurface::Actions);
    let own = opening.as_ref().is_some_and(|o| o.own);
    let target_row = opening.as_ref().map(|o| o.row.clone()).unwrap_or_default();
    // Shown once its contents are this opening's: after the render that put
    // them in. Its first action takes focus; a button, so no keyboard. Once
    // per opening: an effect run after Escape closed it, before the state
    // follows, must not show it again.
    let shown_token = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(0u64)));
    use_effect(move || {
        let token = surfaces
            .mobile
            .opening
            .read()
            .as_ref()
            .filter(|o| o.surface == MobileSurface::Actions)
            .map(|o| o.token);
        let Some(token) = token else {
            return;
        };
        let first_run = shown_token.replace(token) != token;
        #[cfg(target_arch = "wasm32")]
        if first_run && !dom::dialog_open(SHEET_ID) && dom::show_dialog(SHEET_ID) {
            dom::focus_in(SHEET_ID, "[data-testid=\"mobile-message-action-reply\"]");
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = first_run;
    });
    #[cfg(target_arch = "wasm32")]
    let close_listener = use_hook(|| {
        std::rc::Rc::new(std::cell::RefCell::new(
            None::<(
                web_sys::HtmlElement,
                wasm_bindgen::closure::Closure<dyn FnMut()>,
            )>,
        ))
    });
    let act = move |action: MobileAction| {
        move |_| {
            if let Some(token) = surfaces.mobile_token(MobileSurface::Actions) {
                crate::util::defer(move || dispatch(surfaces, token, action, on_action));
            }
        }
    };
    let dismiss = move |_| {
        let token = surfaces.mobile_token(MobileSurface::Actions);
        crate::util::defer(move || {
            if token.is_some_and(|t| {
                surfaces
                    .mobile_opening_for(MobileSurface::Actions, t)
                    .is_some()
            }) {
                surfaces.close_mobile(true);
            }
        });
    };
    rsx! {
        dialog {
            id: SHEET_ID,
            "data-testid": "mobile-message-actions-sheet",
            class: "mobile-sheet",
            "aria-labelledby": "mobile-message-actions-title",
            "data-target-row": "{target_row}",
            onmounted: move |_| {
                #[cfg(target_arch = "wasm32")]
                {
                    let listener = dom::on_dialog_close(SHEET_ID, move || {
                        crate::util::defer(move || {
                            surfaces.mobile_dialog_closed(MobileSurface::Actions)
                        });
                    });
                    *close_listener.borrow_mut() = listener;
                }
            },
            onkeydown: move |e: KeyboardEvent| {
                #[cfg(target_arch = "wasm32")]
                if e.key() == Key::Tab && dom::contain_tab(SHEET_ID, e.modifiers().shift()) {
                    e.prevent_default();
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = e;
            },
            div {
                "data-testid": "mobile-message-actions-backdrop",
                class: "mobile-sheet-backdrop",
                onclick: dismiss,
            }
            div { class: "mobile-sheet-panel",
                div { class: "mobile-sheet-handle", "aria-hidden": "true" }
                div { class: "mobile-sheet-header",
                    h2 { id: "mobile-message-actions-title", class: "mobile-sheet-title", "Message actions" }
                    button {
                        r#type: "button",
                        "data-testid": "mobile-message-actions-close",
                        class: "mobile-sheet-close",
                        "aria-label": "Close",
                        title: "Close",
                        onclick: dismiss,
                        {line_icon(CLOSE_ICON_PATH, 18)}
                    }
                }
                div { class: "mobile-sheet-actions",
                    button {
                        r#type: "button",
                        "data-testid": "mobile-message-action-reply",
                        class: "mobile-sheet-action",
                        onclick: act(MobileAction::Reply),
                        {line_icon(REPLY_ICON_PATH, 20)}
                        span { "Reply" }
                    }
                    if own {
                        button {
                            r#type: "button",
                            "data-testid": "mobile-message-action-edit",
                            class: "mobile-sheet-action",
                            onclick: act(MobileAction::Edit),
                            {line_icon(EDIT_ICON_PATH, 20)}
                            span { "Edit" }
                        }
                        button {
                            r#type: "button",
                            "data-testid": "mobile-message-action-delete",
                            class: "mobile-sheet-action mobile-sheet-action--danger",
                            onclick: act(MobileAction::Delete),
                            {line_icon(DELETE_ICON_PATH, 20)}
                            span { "Delete" }
                        }
                    }
                }
            }
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
