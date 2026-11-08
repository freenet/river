//! What the two mobile message surfaces share: the width they appear at, what
//! one is open for, and the modal dialog both are drawn in.
//!
//! Below Tailwind's `md` (48rem) a message row's trailing Reply, Edit and
//! Delete are hidden. A tap on the row opens its **actions sheet**
//! (`mobile_message_actions.rs`); a tap on its reaction + opens a separate
//! **reaction popup** with a text field for the device keyboard's emoji
//! (`mobile_reaction_input.rs`). At 48rem and wider neither exists: the footer
//! and the anchored reaction picker (`reaction_picker.rs`) are as before.
//!
//! Ownership. One [`MobileOpening`] at a time lives in [`MobileState`], which is
//! part of the conversation's [`ReactionSurfaces`], so opening either mobile
//! surface closes the desktop picker and the open reactor list, and opening the
//! picker or a list closes a mobile surface: at most one is open. Groups pass
//! the handle on without reading it (#830); the per-row entries, the hosts and
//! [`MobileSurfaceGuard`] render from it. Every write is deferred
//! (dioxus-signal-safety.md).
//!
//! An opening is one press: it carries a fresh token, the room and which
//! showing of it ([`super::reaction_picker::note_room_shown`]), the room's
//! contract key at the time (a re-key is another generation, which the showing
//! counter does not see), and the row. Work queued for it (a submit, an action,
//! focus) runs only while that same token is still the open one and the
//! opening is still live ([`opening_is_live`]): a dismissal and reopening of the
//! same message is a new token, so an older queued submit does nothing.
//!
//! Both surfaces are a native modal `<dialog>` (`showModal`): it is in the top
//! layer above the header, history, composer and Latest; the rest of the page
//! is inert, so a tap on the backdrop reaches nothing underneath and Tab stays
//! inside (also wrapped here, [`dom::contain_tab`]). Escape closes it natively;
//! the dialog's own `close` event brings the state back in line. While one is
//! open the reader's wheel and one-finger touch scrolling outside its panel are
//! held off ([`dom::hold_background`]), a zoom (Ctrl+wheel, a pinch) is not,
//! and nothing is captured. Arrivals, following, the end hold and reads go on
//! underneath (history-scrolling.md); opening or closing one never moves the
//! history.

use super::reaction_input::ReactionInputError;
use super::reaction_picker::{shown_room, ReactionSurfaces};
use crate::components::app::{MobileView, CURRENT_ROOM, MOBILE_VIEW, ROOMS};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::ContractKey;
use river_core::room_state::message::MessageId;
use std::cell::Cell;

/// Below Tailwind's `md`: the exact complement of `(min-width: 48rem)`, so no
/// fractional width between 767px and 768px belongs to neither. `main.css`
/// uses the same query.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const NARROW_QUERY: &str = "not all and (min-width: 48rem)";

/// The actions sheet's dialog.
pub(super) const SHEET_ID: &str = "mobile-message-actions-sheet";
/// The reaction popup's dialog.
pub(super) const POPUP_ID: &str = "mobile-reaction-popup";
/// The reaction popup's field.
pub(super) const POPUP_INPUT_ID: &str = "mobile-reaction-input";

/// Which mobile surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MobileSurface {
    /// The row's Reply, Edit and Delete.
    Actions,
    /// The reaction field and palette.
    Reaction,
}

impl MobileSurface {
    pub(super) fn dialog_id(self) -> &'static str {
        match self {
            Self::Actions => SHEET_ID,
            Self::Reaction => POPUP_ID,
        }
    }

    /// The row's control this surface opens from, and returns focus to.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn control_selector(self) -> &'static str {
        match self {
            Self::Actions => "[data-testid=\"mobile-message-actions-entry\"]",
            Self::Reaction => "[data-testid=\"message-react-button\"]",
        }
    }
}

/// What a mobile surface is open for: one press on one message.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct MobileOpening {
    pub(super) surface: MobileSurface,
    /// This opening, among every mobile opening on the page.
    pub(super) token: u64,
    room: Option<VerifyingKey>,
    visit: u64,
    /// The room's contract key when it opened: its generation.
    contract: Option<ContractKey>,
    /// The row's `msg-{row}` suffix.
    pub(super) row: String,
    pub(super) message: MessageId,
    /// The message is the reader's own (the sheet offers Edit and Delete).
    /// Shown from this; checked again against the room when an action runs.
    pub(super) own: bool,
    /// The reader's own reaction on it, as its footer last rendered it
    /// (pending ones included). Only labels the popup's submit: the
    /// pending-aware toggle decides from the room itself.
    pub(super) current: Option<String>,
}

thread_local! {
    static TOKENS: Cell<u64> = const { Cell::new(0) };
}

/// A token no opening on the page has had.
fn next_token() -> u64 {
    TOKENS.with(|t| {
        let next = t.get().wrapping_add(1);
        t.set(next);
        next
    })
}

impl MobileOpening {
    /// A new opening, now: a fresh token, and the room, showing and
    /// generation the history shows.
    pub(super) fn new(
        surface: MobileSurface,
        row: String,
        message: MessageId,
        own: bool,
        current: Option<String>,
    ) -> Self {
        let (room, visit) = shown_room();
        Self {
            surface,
            token: next_token(),
            room,
            visit,
            contract: live_contract_key(),
            row,
            message,
            own,
            current,
        }
    }
}

/// The open mobile surface and the popup's scratch state: the field's text,
/// what it last said about it, and whether the palette is out. All of it is
/// cleared whenever a surface closes, so a new opening starts empty.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct MobileState {
    pub(super) opening: Signal<Option<MobileOpening>>,
    pub(super) text: Signal<String>,
    pub(super) error: Signal<Option<ReactionInputError>>,
    pub(super) palette: Signal<bool>,
}

/// `Conversation`'s mobile state, made once (inside `use_reaction_surfaces`).
pub(super) fn use_mobile_state() -> MobileState {
    MobileState {
        opening: use_signal(|| None),
        text: use_signal(String::new),
        error: use_signal(|| None),
        palette: use_signal(|| false),
    }
}

/// The open room's contract key, read now.
fn live_contract_key() -> Option<ContractKey> {
    let owner = CURRENT_ROOM.try_peek().ok()?.owner_key?;
    ROOMS
        .try_peek()
        .ok()?
        .map
        .get(&owner)
        .map(|room| room.contract_key)
}

/// Whether `opening` still stands: the same showing of the room the history
/// shows, in the same generation, and its row's control still rendered (the
/// message not deleted, out of the rendered range or being edited).
pub(super) fn opening_is_live(opening: &MobileOpening) -> bool {
    let (shown, visit) = shown_room();
    let current_room = CURRENT_ROOM.try_peek().ok().and_then(|r| r.owner_key);
    opening_matches(opening, shown, visit, current_room, live_contract_key())
        && dom::row_control_exists(&opening.row, opening.surface)
}

/// [`opening_is_live`]'s room and generation half, pure for native tests.
fn opening_matches(
    opening: &MobileOpening,
    shown: Option<VerifyingKey>,
    visit: u64,
    current_room: Option<VerifyingKey>,
    contract: Option<ContractKey>,
) -> bool {
    opening.room.is_some()
        && opening.room == shown
        && opening.visit == visit
        && opening.room == current_room
        && opening.contract.is_some()
        && opening.contract == contract
}

impl ReactionSurfaces {
    /// Open `opening`'s surface, closing the desktop picker, a reactor list
    /// and the other mobile surface first. Nothing is opened for an opening
    /// already gone; a popup the press showed for it is put away again.
    /// Whether it is open. From a deferred callback only.
    pub(super) fn open_mobile(self, opening: MobileOpening) -> bool {
        let state = self.mobile;
        let current = state.opening.try_peek().ok().and_then(|o| o.clone());
        if current.as_ref().is_some_and(|o| o.token == opening.token) {
            return true;
        }
        if !narrow_now() || !opening_is_live(&opening) {
            // Only a dialog no opening holds: a press superseded by a later
            // one must not take the later one's dialog away.
            if current
                .as_ref()
                .is_none_or(|o| o.surface != opening.surface)
            {
                dom::close_dialog(opening.surface.dialog_id());
            }
            return false;
        }
        self.close();
        self.close_owner_list_now();
        if let Some(old) = current {
            if old.surface != opening.surface {
                dom::close_dialog(old.surface.dialog_id());
            }
        }
        reset_scratch(state);
        let mut slot = state.opening;
        slot.set(Some(opening));
        dom::hold_background(true);
        dom::track_viewport(true);
        true
    }

    /// Close the open mobile surface and let go of what it held: the dialog,
    /// the input hold and the viewport tracking, then its state and scratch.
    /// An ordinary dismissal (`restore`) returns focus, without scrolling, to
    /// the row's control it opened from if that is still live; a handoff
    /// (Reply, Edit, Delete) leaves focus to where it goes. From a deferred
    /// callback only; the state is read and written fallibly, since the
    /// conversation may have unmounted.
    pub(super) fn close_mobile(self, restore: bool) {
        let state = self.mobile;
        let open = state.opening.try_peek().ok().and_then(|o| o.clone());
        dom::close_dialog(SHEET_ID);
        dom::close_dialog(POPUP_ID);
        dom::hold_background(false);
        dom::track_viewport(false);
        let Some(open) = open else {
            return;
        };
        if restore && opening_is_live(&open) {
            dom::focus_control(&open.row, open.surface);
        }
        let mut slot = state.opening;
        if let Ok(mut slot) = slot.try_write() {
            *slot = None;
        }
        reset_scratch(state);
    }

    /// Close the mobile surface if what it is for has gone. From a deferred
    /// callback only.
    pub(super) fn close_mobile_if_stale(self) {
        let stale = self
            .mobile
            .opening
            .try_peek()
            .ok()
            .and_then(|o| o.clone())
            .is_some_and(|o| !opening_is_live(&o));
        if stale {
            self.close_mobile(false);
        }
    }

    /// `surface`'s dialog closed by itself (Escape): bring the state in line
    /// and return focus. Nothing if it is open again by now (a close and an
    /// opening in one task). From a deferred callback only.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(super) fn mobile_dialog_closed(self, surface: MobileSurface) {
        let open = self.mobile.opening.try_peek().ok().and_then(|o| o.clone());
        if open.is_some_and(|o| o.surface == surface) && !dom::dialog_open(surface.dialog_id()) {
            self.close_mobile(true);
        }
    }

    /// `row`'s control for `surface` unmounted while opening `token` was
    /// open. If the opening is still live its row only remounted: it stays
    /// open, and focus returns to the new control when it closes. Otherwise
    /// it closes, silently. From a deferred callback only.
    pub(super) fn mobile_control_dropped(self, surface: MobileSurface, token: u64) {
        let open = self.mobile.opening.try_peek().ok().and_then(|o| o.clone());
        if let Some(open) = open {
            if open.surface == surface && open.token == token && !opening_is_live(&open) {
                self.close_mobile(false);
            }
        }
    }

    /// The open opening for `surface`, if `token` is it. Read when queued work
    /// runs.
    pub(super) fn mobile_opening_for(
        self,
        surface: MobileSurface,
        token: u64,
    ) -> Option<MobileOpening> {
        self.mobile
            .opening
            .try_peek()
            .ok()
            .and_then(|o| o.clone())
            .filter(|o| o.surface == surface && o.token == token)
    }

    /// The token of the open `surface`, now. Taken by a press, which queues
    /// work for that opening only.
    pub(super) fn mobile_token(self, surface: MobileSurface) -> Option<u64> {
        self.mobile
            .opening
            .try_peek()
            .ok()
            .and_then(|o| o.as_ref().filter(|o| o.surface == surface).map(|o| o.token))
    }
}

/// Clear the popup's scratch, writing only what is not already clear (a
/// write notifies even when it changes nothing).
fn reset_scratch(state: MobileState) {
    let (mut text, mut error, mut palette) = (state.text, state.error, state.palette);
    if text.try_peek().is_ok_and(|t| !t.is_empty()) {
        if let Ok(mut text) = text.try_write() {
            text.clear();
        }
    }
    if error.try_peek().is_ok_and(|e| e.is_some()) {
        if let Ok(mut error) = error.try_write() {
            *error = None;
        }
    }
    if palette.try_peek().is_ok_and(|p| *p) {
        if let Ok(mut palette) = palette.try_write() {
            *palette = false;
        }
    }
}

/// Whether the window is below 48rem now. The authority for a press, which
/// must not wait for [`NARROW`] to catch up with a resize.
pub(super) fn narrow_now() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.match_media(NARROW_QUERY).ok().flatten())
            .is_some_and(|m| m.matches())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

/// Whether the window is below 48rem, for rendering: the invokers' route and
/// labels, and the guard that closes a mobile surface at 48rem. Kept by a
/// `matchMedia` listener ([`install_narrow_listener`]).
pub(super) static NARROW: GlobalSignal<bool> = Global::new(narrow_now);

/// [`NARROW`], read fallibly: a contended read answers from the media query
/// itself, and this render, holding no subscription, is woken by the nudge
/// (freenet/river#555).
pub(super) fn narrow_for_render() -> bool {
    match NARROW.try_read() {
        Ok(narrow) => *narrow,
        Err(_) => {
            crate::util::signal_guard::anchor();
            crate::util::signal_guard::schedule_nudge();
            narrow_now()
        }
    }
}

/// Keep [`NARROW`] current as the window crosses 48rem. The write is deferred
/// (dioxus-signal-safety.md). Installs once.
fn install_narrow_listener() {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        thread_local! {
            static INSTALLED: Cell<bool> = const { Cell::new(false) };
        }
        if INSTALLED.with(|i| i.replace(true)) {
            return;
        }
        let Some(query) =
            web_sys::window().and_then(|w| w.match_media(NARROW_QUERY).ok().flatten())
        else {
            return;
        };
        let watched = query.clone();
        let on_change = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
            let narrow = watched.matches();
            crate::util::defer(move || {
                if *NARROW.peek() != narrow {
                    *NARROW.write() = narrow;
                }
            });
        });
        let _ =
            query.add_event_listener_with_callback("change", on_change.as_ref().unchecked_ref());
        // Lives as long as the page.
        on_change.forget();
    }
}

/// Closes the open mobile surface when it no longer belongs on screen: the
/// window reaching 48rem, another mobile panel replacing the chat, or the
/// room's generation changing. A room switch closes it from `Conversation`'s
/// room-change path, and its message leaving the history from the row's
/// control unmounting. Rendered once by `Conversation`; renders nothing.
#[component]
pub(super) fn MobileSurfaceGuard(
    surfaces: ReactionSurfaces,
    /// The open room's contract key, as `Conversation` renders it.
    contract_key: Option<ContractKey>,
) -> Element {
    install_narrow_listener();
    let narrow = narrow_for_render();
    let in_chat = match MOBILE_VIEW.try_read() {
        Ok(view) => *view == MobileView::Chat,
        Err(_) => {
            crate::util::signal_guard::anchor();
            crate::util::signal_guard::schedule_nudge();
            true
        }
    };
    // Infallible, deliberately: `Conversation`'s own `use_signal`, written
    // only from deferred callbacks, so no write is live during a render.
    let open = surfaces.mobile.opening.read().clone();
    if let Some(open) = open {
        if !narrow || !in_chat || open.contract != contract_key {
            crate::util::defer(move || {
                let still_open = surfaces.mobile_opening_for(open.surface, open.token);
                let gone = !narrow_now()
                    || MOBILE_VIEW
                        .try_peek()
                        .is_ok_and(|view| *view != MobileView::Chat)
                    || open.contract != live_contract_key();
                if still_open.is_some() && gone {
                    surfaces.close_mobile(false);
                }
            });
        }
    }
    // Unmounted open: nothing held outlives it.
    use_drop(|| {
        dom::hold_background(false);
        dom::track_viewport(false);
    });
    rsx! {}
}

/// Whether a mobile surface's dialog is open, for the reload's busy check and
/// the reactor list's focus handoff.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn any_open() -> bool {
    dom::dialog_open(SHEET_ID) || dom::dialog_open(POPUP_ID)
}

#[cfg(target_arch = "wasm32")]
pub(super) mod dom {
    use super::super::focus_without_scrolling;
    use super::{MobileSurface, POPUP_ID, SHEET_ID};
    use std::cell::{Cell, RefCell};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    fn document() -> Option<web_sys::Document> {
        web_sys::window()?.document()
    }

    fn element(id: &str) -> Option<web_sys::HtmlElement> {
        document()?
            .get_element_by_id(id)?
            .dyn_into::<web_sys::HtmlElement>()
            .ok()
    }

    pub(crate) fn dialog_open(id: &str) -> bool {
        element(id).is_some_and(|d| d.has_attribute("open"))
    }

    /// The row's control for `surface`, as the document has it now.
    fn row_control(row: &str, surface: MobileSurface) -> Option<web_sys::HtmlElement> {
        document()?
            .get_element_by_id(&format!("msg-{row}"))?
            .query_selector(surface.control_selector())
            .ok()??
            .dyn_into::<web_sys::HtmlElement>()
            .ok()
    }

    pub(crate) fn row_control_exists(row: &str, surface: MobileSurface) -> bool {
        row_control(row, surface).is_some()
    }

    pub(crate) fn focus_control(row: &str, surface: MobileSurface) {
        if let Some(control) = row_control(row, surface) {
            focus_without_scrolling(&control);
        }
    }

    /// Show the dialog `id` as a modal. A browser without `showModal` (or
    /// one that throws) gets it shown in place instead, still usable.
    /// Whether it is open.
    pub(crate) fn show_dialog(id: &str) -> bool {
        let Some(dialog) = element(id) else {
            return false;
        };
        if dialog.has_attribute("open") {
            return true;
        }
        let shown = js_sys::Reflect::get(&dialog, &"showModal".into())
            .ok()
            .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
            .is_some_and(|f| f.call0(&dialog).is_ok());
        if !shown {
            let _ = dialog.set_attribute("open", "");
        }
        dialog.has_attribute("open")
    }

    thread_local! {
        /// How many times a dialog was closed from here, not by the reader.
        static CLOSES: Cell<u64> = const { Cell::new(0) };
    }

    /// How many dialogs this code has closed so far: a dialog found closed
    /// with the count unchanged was closed by the reader (Escape).
    pub(crate) fn closes_from_here() -> u64 {
        CLOSES.with(Cell::get)
    }

    pub(crate) fn close_dialog(id: &str) {
        let Some(dialog) = element(id) else {
            return;
        };
        if !dialog.has_attribute("open") {
            return;
        }
        CLOSES.with(|c| c.set(c.get().wrapping_add(1)));
        let closed = js_sys::Reflect::get(&dialog, &"close".into())
            .ok()
            .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
            .is_some_and(|f| f.call0(&dialog).is_ok());
        if !closed || dialog.has_attribute("open") {
            let _ = dialog.remove_attribute("open");
        }
    }

    /// Focus the element with `id`, without scrolling.
    pub(crate) fn focus_id(id: &str) -> bool {
        element(id).is_some_and(|el| focus_without_scrolling(&el))
    }

    /// Focus the first element in dialog `id` matching `selector`.
    pub(crate) fn focus_in(id: &str, selector: &str) {
        let target = element(id).and_then(|d| d.query_selector(selector).ok().flatten());
        if let Some(target) = target {
            focus_without_scrolling(&target);
        }
    }

    /// Keep Tab inside dialog `id`'s panel: from its last control Tab goes
    /// to the first, from the first Shift+Tab goes to the last. Whether it
    /// moved focus (the caller then prevents the default).
    pub(crate) fn contain_tab(id: &str, backwards: bool) -> bool {
        let Some(dialog) = element(id) else {
            return false;
        };
        let Ok(list) = dialog.query_selector_all(
            "button:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex=\"-1\"])",
        ) else {
            return false;
        };
        let controls: Vec<web_sys::HtmlElement> = (0..list.length())
            .filter_map(|i| list.item(i))
            .filter_map(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
            .filter(|el| el.offset_width() > 0 || el.offset_height() > 0)
            .collect();
        let (Some(first), Some(last)) = (controls.first(), controls.last()) else {
            return false;
        };
        let active = document().and_then(|d| d.active_element());
        let inside = active
            .as_ref()
            .is_some_and(|a| controls.iter().any(|c| a.is_same_node(Some(c))));
        let at =
            |el: &web_sys::HtmlElement| active.as_ref().is_some_and(|a| a.is_same_node(Some(el)));
        let next = if !inside {
            Some(if backwards { last } else { first })
        } else if backwards && at(first) {
            Some(last)
        } else if !backwards && at(last) {
            Some(first)
        } else {
            None
        };
        match next {
            Some(el) => focus_without_scrolling(el),
            None => false,
        }
    }

    /// The listener that holds scrolling input while a surface is open.
    type Blocker = Closure<dyn FnMut(web_sys::Event)>;

    thread_local! {
        static BLOCKER: RefCell<Option<Blocker>> = const { RefCell::new(None) };
        static BLOCKING: Cell<bool> = const { Cell::new(false) };
        static VIEWPORT: RefCell<Option<Closure<dyn FnMut()>>> = const { RefCell::new(None) };
    }

    const HELD_KINDS: [&str; 2] = ["wheel", "touchmove"];

    /// Hold the reader's wheel and one-finger touch scrolling outside the
    /// open surface's panel, so the history behind it stays put while the
    /// panel itself scrolls and its field takes selections. A zoom is let
    /// through: a wheel with Ctrl down (Ctrl+wheel, a touchpad pinch) and a
    /// touch with more than one finger. Nothing is captured, so turning it
    /// off is only removing the listeners; one left behind holds nothing
    /// while no surface is open.
    pub(crate) fn hold_background(on: bool) {
        if on == BLOCKING.with(Cell::get) {
            return;
        }
        let Some(window) = web_sys::window() else {
            return;
        };
        BLOCKER.with(|blocker| {
            let mut blocker = blocker.borrow_mut();
            let callback = blocker.get_or_insert_with(|| Closure::new(on_held_input));
            let function: &js_sys::Function = callback.as_ref().unchecked_ref();
            for kind in HELD_KINDS {
                if on {
                    let options = web_sys::AddEventListenerOptions::new();
                    options.set_capture(true);
                    options.set_passive(false);
                    let _ = window.add_event_listener_with_callback_and_add_event_listener_options(
                        kind, function, &options,
                    );
                } else {
                    let _ =
                        window.remove_event_listener_with_callback_and_bool(kind, function, true);
                }
            }
        });
        BLOCKING.with(|b| b.set(on));
    }

    fn on_held_input(event: web_sys::Event) {
        if !super::any_open() {
            return;
        }
        let in_panel = event
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.closest(".mobile-sheet-panel").ok().flatten())
            .is_some();
        if in_panel {
            return;
        }
        let zoom = match event.type_().as_str() {
            "wheel" => event
                .dyn_ref::<web_sys::MouseEvent>()
                .is_some_and(web_sys::MouseEvent::ctrl_key),
            _ => touches(&event) > 1,
        };
        if !zoom && event.cancelable() {
            event.prevent_default();
        }
    }

    fn touches(event: &web_sys::Event) -> u32 {
        js_sys::Reflect::get(event, &"touches".into())
            .and_then(|list| js_sys::Reflect::get(&list, &"length".into()))
            .ok()
            .and_then(|n| n.as_f64())
            .map_or(0, |n| n as u32)
    }

    /// While a surface is open, follow the visual viewport: the software
    /// keyboard (and a pinch zoom) shrinks it without shrinking the layout
    /// viewport the dialog is laid out in, so the panel is lifted by the part
    /// of the layout viewport the visual one no longer covers
    /// (`--mobile-sheet-keyboard`) and capped to the visual height
    /// (`--mobile-sheet-viewport`). A browser without `visualViewport`
    /// keeps the CSS fallbacks.
    pub(crate) fn track_viewport(on: bool) {
        let Some(window) = web_sys::window() else {
            return;
        };
        let Ok(viewport) = js_sys::Reflect::get(&window, &"visualViewport".into()) else {
            return;
        };
        if viewport.is_undefined() || viewport.is_null() {
            return;
        }
        let target: web_sys::EventTarget = viewport.unchecked_into();
        VIEWPORT.with(|slot| {
            let mut slot = slot.borrow_mut();
            if on {
                if slot.is_some() {
                    return;
                }
                let update = Closure::<dyn FnMut()>::new(update_viewport);
                for kind in ["resize", "scroll"] {
                    let _ = target
                        .add_event_listener_with_callback(kind, update.as_ref().unchecked_ref());
                }
                *slot = Some(update);
                update_viewport();
            } else if let Some(update) = slot.take() {
                for kind in ["resize", "scroll"] {
                    let _ = target
                        .remove_event_listener_with_callback(kind, update.as_ref().unchecked_ref());
                }
                if let Some(root) = document().and_then(|d| d.document_element()) {
                    if let Ok(root) = root.dyn_into::<web_sys::HtmlElement>() {
                        let _ = root.style().remove_property("--mobile-sheet-keyboard");
                        let _ = root.style().remove_property("--mobile-sheet-viewport");
                    }
                }
            }
        });
    }

    fn update_viewport() {
        let Some(window) = web_sys::window() else {
            return;
        };
        let number = |object: &JsValue, key: &str| {
            js_sys::Reflect::get(object, &key.into())
                .ok()
                .and_then(|v| v.as_f64())
        };
        let Ok(viewport) = js_sys::Reflect::get(&window, &"visualViewport".into()) else {
            return;
        };
        let (Some(height), Some(offset_top)) =
            (number(&viewport, "height"), number(&viewport, "offsetTop"))
        else {
            return;
        };
        let Some(layout_height) = window.inner_height().ok().and_then(|h| h.as_f64()) else {
            return;
        };
        let keyboard = (layout_height - height - offset_top).max(0.0);
        let Some(root) = document()
            .and_then(|d| d.document_element())
            .and_then(|r| r.dyn_into::<web_sys::HtmlElement>().ok())
        else {
            return;
        };
        let _ = root
            .style()
            .set_property("--mobile-sheet-keyboard", &format!("{keyboard}px"));
        let _ = root
            .style()
            .set_property("--mobile-sheet-viewport", &format!("{height}px"));
    }

    /// Listen for dialog `id` closing by itself (Escape), for as long as the
    /// returned closure lives. `on_close` runs from the listener; it defers
    /// any signal write.
    pub(crate) fn on_dialog_close(
        id: &str,
        on_close: impl FnMut() + 'static,
    ) -> Option<(web_sys::HtmlElement, Closure<dyn FnMut()>)> {
        let dialog = element(id)?;
        let listener = Closure::<dyn FnMut()>::new(on_close);
        dialog
            .add_event_listener_with_callback("close", listener.as_ref().unchecked_ref())
            .ok()?;
        Some((dialog, listener))
    }

    // Both ids are used by the busy check.
    const _: [&str; 2] = [SHEET_ID, POPUP_ID];
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) mod dom {
    use super::MobileSurface;
    pub(crate) fn dialog_open(_id: &str) -> bool {
        false
    }
    pub(crate) fn row_control_exists(_row: &str, _surface: MobileSurface) -> bool {
        true
    }
    pub(crate) fn focus_control(_row: &str, _surface: MobileSurface) {}
    pub(crate) fn close_dialog(_id: &str) {}
    pub(crate) fn closes_from_here() -> u64 {
        0
    }
    pub(crate) fn hold_background(_on: bool) {}
    pub(crate) fn track_viewport(_on: bool) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> VerifyingKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32]).verifying_key()
    }

    fn contract(seed: u8) -> ContractKey {
        use freenet_stdlib::prelude::{ContractCode, Parameters};
        let code = ContractCode::from(vec![seed; 8]);
        ContractKey::from_params_and_code(Parameters::from(vec![seed]), &code)
    }

    fn opening(
        room: Option<VerifyingKey>,
        visit: u64,
        contract: Option<ContractKey>,
    ) -> MobileOpening {
        MobileOpening {
            surface: MobileSurface::Reaction,
            token: 1,
            room,
            visit,
            contract,
            row: "m".into(),
            message: MessageId(freenet_scaffold::util::FastHash(7)),
            own: false,
            current: None,
        }
    }

    /// An opening belongs to one showing of one room in one generation: a
    /// switch away and back, or a re-key of the same room, ends it.
    #[test]
    fn an_opening_is_live_only_in_its_showing_and_generation() {
        let (a, b) = (Some(key(1)), Some(key(2)));
        let (g1, g2) = (Some(contract(1)), Some(contract(2)));
        let o = opening(a, 3, g1);
        assert!(opening_matches(&o, a, 3, a, g1));
        assert!(!opening_matches(&o, b, 4, b, g1), "another room");
        assert!(
            !opening_matches(&o, a, 5, a, g1),
            "the same room, shown again"
        );
        assert!(
            !opening_matches(&o, a, 3, b, g1),
            "switched, not yet rendered"
        );
        assert!(!opening_matches(&o, a, 3, a, g2), "the room re-keyed");
        assert!(
            !opening_matches(&o, a, 3, a, None),
            "the room's data unreadable"
        );
        assert!(
            !opening_matches(&opening(None, 3, g1), None, 3, None, g1),
            "no room"
        );
    }

    /// Every press is a new opening, even of the same message.
    #[test]
    fn each_opening_has_its_own_token() {
        let first = next_token();
        let second = next_token();
        assert_ne!(first, second);
    }
}
