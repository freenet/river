//! The conversation's one reaction picker, and the footer React that opens it.
//!
//! One `popover="auto"` element for the whole history, opened at the footer
//! React of whichever message it is for ([`ReactionInvoker`]). The browser
//! closes it on Escape and on a press outside it; a pick, a room switch and the
//! message leaving the history close it here. Placement is CSS anchor
//! positioning (`.reaction-picker` in `main.css`): the open React carries
//! `anchor-name`, set here when it opens.
//!
//! What the picker is for is a [`ReactionTarget`]: the room and which showing
//! of it, the message, and the reader's current reaction on it. It lives in
//! `Conversation` ([`ReactionSurfaces`]) and reaches the footers through each
//! message group, which passes it on without reading it (#830): only the
//! invokers and the picker render from it. Every write is deferred
//! (dioxus-signal-safety.md), and a pick acts only on a target still current
//! when the pick runs ([`target_is_live`]), so a pick queued before a room
//! switch, or before its message was deleted, does nothing.
//!
//! While it is open the reader's wheel and one-finger touch scrolling are
//! held off everywhere, over the picker too (it does not scroll, and a wheel
//! there would scroll the history), as the full-screen backdrop it replaced
//! did. A zoom is not held: a wheel with Ctrl down (Ctrl+wheel, or a touchpad
//! pinch) and a touch with a second finger down (a pinch) still reach the
//! browser, and keyboard zoom is never touched. Only those input events are
//! held: arrivals, following, the end hold and reads go on underneath
//! (history-scrolling.md). Closing lets go of all of it at once, a finger the
//! picker captured included.

use super::emoji_picker::FREQUENT_EMOJIS;
use super::ReactionOwnersPopover;
use crate::components::app::{MobileView, CURRENT_ROOM, MOBILE_VIEW};
use crate::components::icons::{line_icon, REACT_ICON_PATH};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::message::MessageId;
use std::cell::Cell;
use std::rc::Rc;

/// The picker element's id.
pub(super) const PICKER_ID: &str = "reaction-picker";

/// The anchor name the open React carries and `.reaction-picker` positions
/// against (`main.css`).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const ANCHOR_NAME: &str = "--reaction-picker";

/// What the picker is open for.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ReactionTarget {
    /// The room, and which showing of it ([`note_room_shown`]): a pick made
    /// for one showing never acts in a later one, even of the same room.
    room: Option<VerifyingKey>,
    visit: u64,
    /// The row's `msg-{row}` suffix.
    row: String,
    message: MessageId,
    /// The reader's own reaction on the message, as its footer last rendered
    /// it (pending ones included). Only highlights the picker: a pick goes
    /// through the pending-aware toggle, which decides from the room itself.
    current: Option<String>,
}

impl ReactionTarget {
    /// The same opening: room, showing and message.
    fn same_opening(&self, other: &Self) -> bool {
        self.room == other.room && self.visit == other.visit && self.row == other.row
    }
}

/// The conversation's reaction surfaces: the picker's target, and the group
/// whose reactor list is open, so that opening either closes the other. Both
/// are `Conversation`'s own signals; the handles compare by identity, so a
/// group's props stay equal (#830).
#[derive(Clone, Copy, PartialEq)]
pub(super) struct ReactionSurfaces {
    target: Signal<Option<ReactionTarget>>,
    open_owner_list: Signal<Option<Signal<Option<ReactionOwnersPopover>>>>,
}

/// `Conversation`'s reaction surfaces, made once.
pub(super) fn use_reaction_surfaces() -> ReactionSurfaces {
    ReactionSurfaces {
        target: use_signal(|| None),
        open_owner_list: use_signal(|| None),
    }
}

impl ReactionSurfaces {
    /// A group opened a reactor list: the picker closes, and the list is
    /// remembered so that opening the picker closes it. From a deferred
    /// callback only.
    pub(super) fn owner_list_opened(mut self, list: Signal<Option<ReactionOwnersPopover>>) {
        self.close_now();
        self.open_owner_list.set(Some(list));
    }

    /// Close the picker. From a deferred callback only. Hidden before the
    /// target is cleared, so the focus return sees what it was for.
    pub(super) fn close_now(mut self) {
        hide_picker();
        if self.target.peek().is_some() {
            self.target.set(None);
        }
    }

    /// Close the picker if what it is for has gone: another room or showing,
    /// or its message is no longer in the history. From a deferred callback
    /// only.
    pub(super) fn close_if_stale(self) {
        let stale = self
            .target
            .peek()
            .as_ref()
            .is_some_and(|target| !target_is_live(target));
        if stale {
            self.close_now();
        }
    }

    /// Close whichever group's reactor list is open. A list its group has
    /// already closed, or a group no longer mounted, is left alone.
    fn close_owner_list_now(mut self) {
        let Some(mut list) = self.open_owner_list.take() else {
            return;
        };
        if let Ok(mut open) = list.try_write() {
            if open.is_some() {
                *open = None;
            }
        };
    }
}

thread_local! {
    /// The room the history shows, and how many times a room has been shown,
    /// set by `Conversation`'s render when the room changes.
    static SHOWN_ROOM: Cell<(Option<VerifyingKey>, u64)> = const { Cell::new((None, 0)) };
}

/// `Conversation` shows `room`, newly: every target made before is stale.
/// Called from its render, where only a `Cell` may be written.
pub(super) fn note_room_shown(room: Option<VerifyingKey>) {
    SHOWN_ROOM.with(|shown| {
        let (_, visit) = shown.get();
        shown.set((room, visit.wrapping_add(1)));
    });
}

fn shown_room() -> (Option<VerifyingKey>, u64) {
    SHOWN_ROOM.with(Cell::get)
}

/// Whether `target` is still what the reader opened the picker for: the same
/// showing of the room the history shows, and its message still in the
/// history with its React (not deleted, not out of the rendered range, not
/// being edited).
fn target_is_live(target: &ReactionTarget) -> bool {
    let (room, visit) = shown_room();
    let current_room = CURRENT_ROOM.try_peek().ok().and_then(|r| r.owner_key);
    target_matches_room(target, room, visit, current_room) && react_button_exists(&target.row)
}

/// [`target_is_live`]'s room half, pure for native tests.
fn target_matches_room(
    target: &ReactionTarget,
    shown: Option<VerifyingKey>,
    visit: u64,
    current_room: Option<VerifyingKey>,
) -> bool {
    target.room == shown && target.visit == visit && target.room == current_room
}

/// The footer React of one message: the only way into the picker, and where
/// it opens. A message's footer shows it unless the message is being edited.
#[component]
pub(super) fn ReactionInvoker(
    surfaces: ReactionSurfaces,
    /// The row's `msg-{row}` suffix.
    row: String,
    message: MessageId,
    /// The reader's own reaction on the message, if any.
    current: Option<String>,
) -> Element {
    // This invoker's own signal read: no other footer renders when the
    // picker opens or closes. Infallible, deliberately: `target` is
    // `Conversation`'s own `use_signal`, written only from deferred callbacks,
    // so no write can be live during a render (dioxus-signal-safety.md).
    let (expanded, current_stale) = match surfaces.target.read().as_ref() {
        Some(target) if target.row == row => (true, target.current != current),
        _ => (false, false),
    };
    // A reaction landing while the picker is open (the reader's own, from
    // another device or still being signed) moves the highlight with it.
    if current_stale {
        let (row, current) = (row.clone(), current.clone());
        let mut target = surfaces.target;
        crate::util::defer(move || {
            let stale = target
                .peek()
                .as_ref()
                .is_some_and(|t| t.row == row && t.current != current);
            if stale {
                target.with_mut(|t| {
                    if let Some(t) = t.as_mut() {
                        t.current = current;
                    }
                });
            }
        });
    }
    // The message leaving the history (deleted, even optimistically, or out
    // of the rendered range) takes its picker with it. A rollback renders a
    // new invoker with nothing open: the picker is not reopened, and a pick
    // queued meanwhile finds no target.
    {
        let row = row.clone();
        use_drop(move || {
            crate::util::defer(move || {
                let mine = surfaces
                    .target
                    .peek()
                    .as_ref()
                    .is_some_and(|target| target.row == row);
                if mine {
                    surfaces.close_now();
                }
            });
        });
    }
    let press_row = row.clone();
    rsx! {
        button {
            r#type: "button",
            "data-testid": "message-react-button",
            class: "msg-action",
            title: "React",
            "aria-label": "React",
            "aria-haspopup": "true",
            "aria-controls": PICKER_ID,
            "aria-expanded": "{expanded}",
            onpointerdown: move |_| record_press(&press_row, surfaces),
            onclick: move |_| {
                // A press on React while its picker is open closes it, as the
                // old backdrop did. A key press keeps it open, as it did.
                if take_press_while_open(&row) {
                    crate::util::defer(move || surfaces.close_now());
                    return;
                }
                let target = ReactionTarget {
                    room: shown_room().0,
                    visit: shown_room().1,
                    row: row.clone(),
                    message: message.clone(),
                    current: current.clone(),
                };
                crate::util::defer(move || open_for(surfaces, target));
            },
            {line_icon(REACT_ICON_PATH, 18)}
        }
    }
}

/// Open the picker for `target`, or move it there. The target is committed
/// before the picker shows, so nothing ever shows the previous one. From a
/// deferred callback only.
fn open_for(mut surfaces: ReactionSurfaces, target: ReactionTarget) {
    if !target_is_live(&target) {
        return;
    }
    surfaces.close_owner_list_now();
    if surfaces.target.peek().as_ref() != Some(&target) {
        surfaces.target.set(Some(target.clone()));
    }
    show_picker_at(&target.row);
}

thread_local! {
    /// The row whose React was last pressed while its picker was open, and
    /// when (ms). Read by that React's click.
    static PRESSED_WHILE_OPEN: std::cell::RefCell<Option<(String, f64)>> =
        const { std::cell::RefCell::new(None) };
}

/// A press older than this belongs to another gesture.
const PRESS_FRESH_MS: f64 = 1500.0;

fn record_press(row: &str, surfaces: ReactionSurfaces) {
    let open_here = is_open()
        && surfaces
            .target
            .peek()
            .as_ref()
            .is_some_and(|target| target.row == row);
    PRESSED_WHILE_OPEN.with(|p| *p.borrow_mut() = open_here.then(|| (row.to_string(), now_ms())));
}

fn take_press_while_open(row: &str) -> bool {
    PRESSED_WHILE_OPEN
        .with(|p| p.borrow_mut().take())
        .is_some_and(|(pressed, at)| pressed == row && now_ms() - at <= PRESS_FRESH_MS)
}

/// The conversation's one reaction picker. Rendered once by `Conversation`,
/// closed until a React opens it.
#[component]
pub(super) fn ReactionPicker(
    surfaces: ReactionSurfaces,
    on_react: EventHandler<(MessageId, String)>,
) -> Element {
    let current = surfaces
        .target
        .read()
        .as_ref()
        .and_then(|target| target.current.clone());
    // Another mobile panel replacing the chat hides the React the picker
    // hangs from: close it, as a tap there would have.
    let last_view = use_hook(|| Rc::new(Cell::new(None::<MobileView>)));
    match MOBILE_VIEW.try_read().map(|view| *view) {
        Ok(view) => {
            let left_chat =
                last_view.replace(Some(view)) == Some(MobileView::Chat) && view != MobileView::Chat;
            if left_chat {
                crate::util::defer(move || surfaces.close_now());
            }
        }
        Err(_) => {
            // This render holds no MOBILE_VIEW subscription; the nudge renders
            // it again (freenet/river#555).
            crate::util::signal_guard::anchor();
            crate::util::signal_guard::schedule_nudge();
        }
    }
    // Unmounted open: nothing held or captured outlives it.
    use_drop(|| {
        hold_scrolling(false);
        clear_anchor();
    });
    rsx! {
        div {
            id: PICKER_ID,
            "data-testid": "emoji-picker",
            popover: "auto",
            class: "reaction-picker",
            "aria-label": "Reactions",
            // Closing with focus inside (a pick from the keyboard, Escape on
            // an emoji): back to the React it opened from, without scrolling,
            // if that is still there. Synchronous, before the browser would
            // focus whatever had focus when it opened. The input hold goes
            // here too, however it closes (a pick, Escape, a press outside,
            // another React), with any finger it captured: `toggle` comes a
            // task later.
            onbeforetoggle: move |e: Event<ToggleData>| {
                if new_state(&e).as_deref() == Some("closed") {
                    return_focus(surfaces);
                    hold_scrolling(false);
                }
            },
            ontoggle: move |_| {
                // Whatever it is now: toggles are coalesced, so a close and a
                // reopen in one task (another React) arrive as one.
                let open = is_open();
                hold_scrolling(open);
                if !open {
                    // Checked again when it runs: a React pressed meanwhile
                    // may have opened it for another message.
                    crate::util::defer(move || {
                        if !is_open() {
                            clear_anchor();
                            let mut target = surfaces.target;
                            if target.peek().is_some() {
                                target.set(None);
                            }
                        }
                    });
                }
            },
            div { class: "reaction-picker-grid",
                for emoji in FREQUENT_EMOJIS.iter() {
                    {
                        let emoji = emoji.to_string();
                        let is_current = current.as_deref() == Some(emoji.as_str());
                        rsx! {
                            button {
                                key: "{emoji}",
                                r#type: "button",
                                class: if is_current { "reaction-picker-emoji reaction-picker-emoji--current" } else { "reaction-picker-emoji" },
                                title: if is_current { format!("Remove {emoji} reaction") } else { format!("React with {emoji}") },
                                onclick: {
                                    let emoji = emoji.clone();
                                    move |_| {
                                        let picked_for = surfaces.target.peek().clone();
                                        let emoji = emoji.clone();
                                        crate::util::defer(move || pick(surfaces, picked_for, emoji, on_react));
                                    }
                                },
                                "{emoji}"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A pick, run deferred: it acts only on the target it was made for, and
/// only while that is still live; never on whatever is open by then.
fn pick(
    surfaces: ReactionSurfaces,
    picked_for: Option<ReactionTarget>,
    emoji: String,
    on_react: EventHandler<(MessageId, String)>,
) {
    let Some(target) = surfaces.target.peek().clone() else {
        return;
    };
    let same = picked_for
        .as_ref()
        .is_some_and(|picked| picked.same_opening(&target));
    if !same {
        return;
    }
    if target_is_live(&target) {
        on_react.call((target.message.clone(), emoji));
    }
    surfaces.close_now();
}

fn new_state(e: &Event<ToggleData>) -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        let data = e.data();
        let event = data.downcast::<web_sys::Event>()?;
        js_sys::Reflect::get(event, &"newState".into())
            .ok()?
            .as_string()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = e;
        None
    }
}

#[cfg(target_arch = "wasm32")]
mod dom {
    use super::{ReactionSurfaces, ANCHOR_NAME, PICKER_ID};
    use dioxus::prelude::ReadableExt;
    use std::cell::RefCell;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    /// The listener that holds scrolling input while the picker is open.
    type Blocker = Closure<dyn FnMut(web_sys::Event)>;

    thread_local! {
        /// The React the picker hangs from (`anchor-name`).
        static ANCHORED: RefCell<Option<web_sys::HtmlElement>> = const { RefCell::new(None) };
        /// The wheel and touch blocker, made once, and whether it is on.
        static BLOCKER: RefCell<Option<Blocker>> = const { RefCell::new(None) };
        static BLOCKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn document() -> Option<web_sys::Document> {
        web_sys::window()?.document()
    }

    fn picker() -> Option<web_sys::HtmlElement> {
        document()?
            .get_element_by_id(PICKER_ID)?
            .dyn_into::<web_sys::HtmlElement>()
            .ok()
    }

    pub(super) fn react_button(row: &str) -> Option<web_sys::HtmlElement> {
        document()?
            .get_element_by_id(&format!("msg-{row}"))?
            .query_selector("[data-testid=\"message-react-button\"]")
            .ok()??
            .dyn_into::<web_sys::HtmlElement>()
            .ok()
    }

    pub(crate) fn is_open() -> bool {
        picker().is_some_and(|p| p.matches(":popover-open").unwrap_or(false))
    }

    pub(super) fn hide_picker() {
        if let Some(p) = picker() {
            if p.matches(":popover-open").unwrap_or(false) {
                let _ = p.hide_popover();
            }
        }
    }

    pub(super) fn clear_anchor() {
        if let Some(old) = ANCHORED.with(|a| a.borrow_mut().take()) {
            let _ = old.style().remove_property("anchor-name");
        }
    }

    /// Hang the picker from `row`'s React and show it, with that React as its
    /// invoker (`source`): its contents follow the React in the tab order, and
    /// a press on it is not a press outside.
    pub(super) fn show_picker_at(row: &str) {
        let (Some(p), Some(button)) = (picker(), react_button(row)) else {
            return;
        };
        let same = ANCHORED.with(|a| a.borrow().as_ref() == Some(&button));
        let open = p.matches(":popover-open").unwrap_or(false);
        if open && same {
            return;
        }
        if open {
            // Shown from another React: shown again, so the invoker is this one.
            let _ = p.hide_popover();
        }
        clear_anchor();
        let _ = button.style().set_property("anchor-name", ANCHOR_NAME);
        ANCHORED.with(|a| *a.borrow_mut() = Some(button.clone()));
        let options = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&options, &"source".into(), &button);
        let shown = js_sys::Reflect::get(&p, &"showPopover".into())
            .ok()
            .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
            .map(|f| f.call1(&p, &options).is_ok())
            .unwrap_or(false);
        if !shown {
            let _ = p.show_popover();
        }
    }

    /// Focus inside the closing picker goes back to the React it is open for,
    /// without scrolling, if that is still live; else it is let go, never sent
    /// to a React that has left.
    pub(super) fn return_focus(surfaces: ReactionSurfaces) {
        let Some(doc) = document() else {
            return;
        };
        let Some(active) = doc.active_element() else {
            return;
        };
        let inside = active
            .closest(&format!("#{PICKER_ID}"))
            .ok()
            .flatten()
            .is_some();
        if !inside {
            return;
        }
        let live = surfaces
            .target
            .peek()
            .clone()
            .filter(super::target_is_live)
            .and_then(|target| react_button(&target.row));
        match live {
            Some(button) => {
                let options = web_sys::FocusOptions::new();
                options.set_prevent_scroll(true);
                let _ = button.focus_with_options(&options);
            }
            None => {
                if let Some(active) = active.dyn_ref::<web_sys::HtmlElement>() {
                    let _ = active.blur();
                }
            }
        }
    }

    /// Every event kind the hold listens to, added and removed together.
    const HELD_KINDS: [&str; 7] = [
        "wheel",
        "touchmove",
        "pointerdown",
        "pointermove",
        "pointerup",
        "pointercancel",
        "lostpointercapture",
    ];

    /// Hold the reader's wheel and one-finger touch scrolling while the
    /// picker is open, wherever it lands, the picker included: the picker has
    /// no scrollport, so a wheel let through over it scrolls the history. A
    /// zoom is let through: a wheel with Ctrl down (Ctrl+wheel, a touchpad
    /// pinch), and a touch with more than one finger down (a pinch), from the
    /// moment the second finger lands until every finger has lifted. Keyboard
    /// zoom has no listener here. Programmatic scrolling (following an
    /// arrival, the end hold) is untouched.
    ///
    /// A finger dragged outside the picker scrolls nothing, and must not close
    /// it either: the old backdrop took no click from a drag. The browser
    /// closes a `popover="auto"` when a press starts and ends outside it, so
    /// once a one-finger press has moved past a tap's slop its pointer is
    /// captured to the picker, and the press ends there. A tap still closes
    /// it. A second finger landing lets that capture go.
    ///
    /// Off is complete whatever the bookkeeping says, so it is safe from the
    /// closing `beforetoggle`, the `toggle` after it and teardown alike: the
    /// listeners are removed, a capture the picker took is released, and no
    /// press or contact is remembered. Should a removal fail, the listener
    /// left behind holds nothing while the picker is closed
    /// (`on_held_input`).
    pub(super) fn hold_scrolling(on: bool) {
        if on && BLOCKING.with(|b| b.get()) {
            return;
        }
        if let Some(window) = web_sys::window() {
            BLOCKER.with(|blocker| {
                let mut blocker = blocker.borrow_mut();
                if !on && blocker.is_none() {
                    return;
                }
                let callback = blocker.get_or_insert_with(|| Closure::new(on_held_input));
                let function: &js_sys::Function = callback.as_ref().unchecked_ref();
                for kind in HELD_KINDS {
                    if on {
                        let options = web_sys::AddEventListenerOptions::new();
                        options.set_capture(true);
                        options.set_passive(false);
                        let _ = window
                            .add_event_listener_with_callback_and_add_event_listener_options(
                                kind, function, &options,
                            );
                    } else {
                        let _ = window
                            .remove_event_listener_with_callback_and_bool(kind, function, true);
                    }
                }
            });
        }
        BLOCKING.with(|b| b.set(on));
        if !on {
            release_capture();
        }
        TOUCH_PRESS.with(|p| p.set(None));
        CONTACTS.with(|c| c.borrow_mut().clear());
        MULTI_TOUCH.with(|m| m.set(false));
    }

    thread_local! {
        /// The one-finger touch or pen press under way while the picker is
        /// open: its pointer id and where it started. Gone once it is
        /// captured, ends, or a second finger lands.
        static TOUCH_PRESS: std::cell::Cell<Option<(i32, f64, f64)>> =
            const { std::cell::Cell::new(None) };
        /// The touch and pen contacts down, by pointer id.
        static CONTACTS: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
        /// Whether a second contact has landed since none was down: a pinch,
        /// let through until every finger has lifted.
        static MULTI_TOUCH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// The pointer the picker captured, and the picker it was captured
        /// to (kept, so teardown can release it after the element is gone).
        static CAPTURED: RefCell<Option<(web_sys::HtmlElement, i32)>> =
            const { RefCell::new(None) };
    }

    /// How far a finger may wander and still be a tap, in CSS px.
    const TAP_SLOP_PX: f64 = 10.0;

    fn on_held_input(event: web_sys::Event) {
        let kind = event.type_();
        let pointer = event.dyn_ref::<web_sys::PointerEvent>();
        let contact = pointer.filter(|p| matches!(p.pointer_type().as_str(), "touch" | "pen"));
        // Bookkeeping only, open or not: a contact ending, or the picker's
        // capture going. Completion releases a capture by itself, so it is
        // only forgotten here, after the release has gone to the picker.
        match kind.as_str() {
            "pointerup" | "pointercancel" => {
                if let Some(contact) = contact {
                    contact_ended(contact.pointer_id());
                }
                return;
            }
            "lostpointercapture" => {
                // The picker's own loss only: taking a finger from the
                // element it was pressed on (a touch's implicit capture)
                // fires one there too.
                let from_picker = event
                    .target()
                    .and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok())
                    .is_some_and(|el| {
                        CAPTURED.with(|c| c.borrow().as_ref().is_some_and(|(p, _)| *p == el))
                    });
                if let (Some(pointer), true) = (pointer, from_picker) {
                    forget_capture(pointer.pointer_id());
                }
                return;
            }
            _ => {}
        }
        // A listener left attached after the picker closed (its removal
        // failed) holds nothing: no input is cancelled, nothing captured.
        if !is_open() {
            return;
        }
        match kind.as_str() {
            "wheel" => {
                if event.cancelable() && !is_zoom(&event) {
                    event.prevent_default();
                }
            }
            "touchmove" => {
                if touches(&event) > 1 {
                    multi_touch();
                }
                if event.cancelable() && !MULTI_TOUCH.with(|m| m.get()) {
                    event.prevent_default();
                }
            }
            "pointerdown" => {
                let Some(contact) = contact else {
                    return;
                };
                let id = contact.pointer_id();
                let down = CONTACTS.with(|c| {
                    let mut c = c.borrow_mut();
                    if !c.contains(&id) {
                        c.push(id);
                    }
                    c.len()
                });
                if down > 1 || !contact.is_primary() {
                    multi_touch();
                    return;
                }
                let over_picker = event
                    .target()
                    .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                    .and_then(|el| el.closest(&format!("#{PICKER_ID}")).ok().flatten())
                    .is_some();
                let press = (!over_picker && !MULTI_TOUCH.with(|m| m.get())).then(|| {
                    (
                        id,
                        f64::from(contact.client_x()),
                        f64::from(contact.client_y()),
                    )
                });
                TOUCH_PRESS.with(|p| p.set(press));
            }
            "pointermove" => {
                let Some(contact) = contact else {
                    return;
                };
                let Some((pressed, x0, y0)) = TOUCH_PRESS.with(|p| p.get()) else {
                    return;
                };
                let (id, x, y) = (
                    contact.pointer_id(),
                    f64::from(contact.client_x()),
                    f64::from(contact.client_y()),
                );
                if pressed != id || (x - x0).hypot(y - y0) <= TAP_SLOP_PX {
                    return;
                }
                TOUCH_PRESS.with(|p| p.set(None));
                capture(id);
            }
            _ => {}
        }
    }

    /// A second finger: a pinch. Nothing is held or captured until every
    /// finger has lifted.
    fn multi_touch() {
        MULTI_TOUCH.with(|m| m.set(true));
        TOUCH_PRESS.with(|p| p.set(None));
        release_capture();
    }

    fn contact_ended(id: i32) {
        let none_left = CONTACTS.with(|c| {
            let mut c = c.borrow_mut();
            c.retain(|&down| down != id);
            c.is_empty()
        });
        if none_left {
            MULTI_TOUCH.with(|m| m.set(false));
        }
        TOUCH_PRESS.with(|p| {
            if p.get().is_some_and(|(pressed, ..)| pressed == id) {
                p.set(None);
            }
        });
        forget_capture(id);
    }

    /// Capture `id` to the picker, remembering it only if the capture took.
    fn capture(id: i32) {
        let Some(p) = picker() else {
            return;
        };
        if p.set_pointer_capture(id).is_ok() && p.has_pointer_capture(id) {
            CAPTURED.with(|c| *c.borrow_mut() = Some((p, id)));
        }
    }

    /// Release the capture the picker took, if it still holds it.
    fn release_capture() {
        if let Some((p, id)) = CAPTURED.with(|c| c.borrow_mut().take()) {
            if p.has_pointer_capture(id) {
                let _ = p.release_pointer_capture(id);
            }
        }
    }

    fn forget_capture(id: i32) {
        CAPTURED.with(|c| {
            let mut c = c.borrow_mut();
            if c.as_ref().is_some_and(|(_, captured)| *captured == id) {
                *c = None;
            }
        });
    }

    /// How many fingers a touch event says are down.
    fn touches(event: &web_sys::Event) -> u32 {
        js_sys::Reflect::get(event, &"touches".into())
            .and_then(|list| js_sys::Reflect::get(&list, &"length".into()))
            .ok()
            .and_then(|n| n.as_f64())
            .map_or(0, |n| n as u32)
    }

    /// A wheel with Ctrl down: the browser zooms on it (Ctrl+wheel, and a
    /// touchpad pinch arrives as one), so it is never held.
    fn is_zoom(event: &web_sys::Event) -> bool {
        event.type_() == "wheel"
            && event
                .dyn_ref::<web_sys::MouseEvent>()
                .is_some_and(web_sys::MouseEvent::ctrl_key)
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) use dom::is_open;
#[cfg(target_arch = "wasm32")]
use dom::{clear_anchor, hide_picker, hold_scrolling, return_focus, show_picker_at};

#[cfg(target_arch = "wasm32")]
fn react_button_exists(row: &str) -> bool {
    dom::react_button(row).is_some()
}

#[cfg(target_arch = "wasm32")]
fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|window| window.performance())
        .map_or(0.0, |performance| performance.now())
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::ReactionSurfaces;
    pub(crate) fn is_open() -> bool {
        false
    }
    pub(super) fn hide_picker() {}
    pub(super) fn clear_anchor() {}
    pub(super) fn show_picker_at(_row: &str) {}
    pub(super) fn return_focus(_surfaces: ReactionSurfaces) {}
    pub(super) fn hold_scrolling(_on: bool) {}
    pub(super) fn react_button_exists(_row: &str) -> bool {
        true
    }
    pub(super) fn now_ms() -> f64 {
        0.0
    }
}
#[cfg(not(target_arch = "wasm32"))]
#[allow(unused_imports)]
pub(crate) use native::is_open;
#[cfg(not(target_arch = "wasm32"))]
use native::{
    clear_anchor, hide_picker, hold_scrolling, now_ms, react_button_exists, return_focus,
    show_picker_at,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> VerifyingKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32]).verifying_key()
    }

    fn target(room: Option<VerifyingKey>, visit: u64, row: &str) -> ReactionTarget {
        ReactionTarget {
            room,
            visit,
            row: row.to_string(),
            message: MessageId(freenet_scaffold::util::FastHash(7)),
            current: None,
        }
    }

    /// A target belongs to one showing of one room: the room the history
    /// shows, the showing it was opened in, and the room still current when
    /// a pick runs. A switch away and back is a new showing.
    #[test]
    fn a_target_is_live_only_in_the_showing_it_was_opened_in() {
        let (a, b) = (Some(key(1)), Some(key(2)));
        let t = target(a, 3, "m");
        assert!(target_matches_room(&t, a, 3, a));
        assert!(!target_matches_room(&t, b, 4, b), "another room");
        assert!(
            !target_matches_room(&t, a, 5, a),
            "the same room, shown again"
        );
        assert!(
            !target_matches_room(&t, a, 3, b),
            "switched, not yet rendered"
        );
    }

    /// A pick acts on the opening it was made in, whatever the reader's
    /// reaction on the message became since.
    #[test]
    fn an_opening_is_its_room_showing_and_message() {
        let a = Some(key(1));
        let t = target(a, 3, "m");
        let mut highlighted = t.clone();
        highlighted.current = Some("👍".into());
        assert!(t.same_opening(&highlighted));
        assert!(!t.same_opening(&target(a, 3, "n")), "another message");
        assert!(!t.same_opening(&target(a, 4, "m")), "another showing");
    }

    /// Each showing of a room is a new visit.
    #[test]
    fn showing_a_room_starts_a_new_visit() {
        let a = Some(key(1));
        note_room_shown(a);
        let first = shown_room();
        note_room_shown(a);
        let second = shown_room();
        assert_eq!(first.0, a);
        assert_eq!(second.1, first.1.wrapping_add(1));
    }
}
