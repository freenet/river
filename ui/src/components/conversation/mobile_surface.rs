//! What the two mobile message surfaces share: the width they appear at, what
//! one is open for, and the modal dialog both are drawn in.
//!
//! Below Tailwind's `md` (48rem) a message row's trailing Reply, Edit and
//! Delete are hidden. A tap on the row opens its **actions sheet**
//! (`mobile_message_actions.rs`); a tap on its reaction + opens a separate
//! **reaction popup**: one row of emoji that scrolls sideways
//! (`mobile_reaction_popup.rs`). At 48rem and wider neither exists: the footer
//! and the anchored reaction picker (`reaction_picker.rs`) are as before.
//!
//! Ownership. One [`MobileOpening`] at a time lives in the `mobile` signal of
//! the conversation's [`ReactionSurfaces`], so opening either mobile
//! surface closes the desktop picker and the open reactor list, and opening the
//! picker or a list closes a mobile surface: at most one is open. Groups pass
//! the handle on without reading it (#830); the per-row entries, the hosts and
//! [`MobileSurfaceGuard`] render from it. Every write is deferred
//! (dioxus-signal-safety.md).
//!
//! An opening is one press: it carries a fresh token, the room and which
//! showing of it ([`super::reaction_picker::note_room_shown`]), the room's
//! contract key at the time (a re-key is another generation, which the showing
//! counter does not see), and the row. Work queued for it (a reaction, an
//! action, focus) runs only while that same token is still the open one and the
//! opening is still live ([`opening_is_live`]): a dismissal and reopening of the
//! same message is a new token, so an older queued reaction does nothing.
//!
//! Neither surface has a field, so neither brings up the software keyboard.
//! Each is committed deferred and shown after the render that fills it
//! ([`use_show_on_open`]), focus on one of its buttons.
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

use super::reaction_picker::{shown_room, ReactionSurfaces};
use crate::components::app::{MobileView, CURRENT_ROOM, MOBILE_VIEW, ROOMS};
use crate::components::icons::{line_icon, CLOSE_ICON_PATH};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use freenet_stdlib::prelude::ContractKey;
use river_core::room_state::message::MessageId;
use std::cell::Cell;

/// Below Tailwind's `md`: the exact complement of `(min-width: 48rem)`, so no
/// fractional width between 767px and 768px belongs to neither. `main.css`
/// uses the same query.
const NARROW_QUERY: &str = "not all and (min-width: 48rem)";

/// The actions sheet's dialog.
pub(super) const SHEET_ID: &str = "mobile-message-actions-sheet";
/// The reaction popup's dialog.
pub(super) const POPUP_ID: &str = "mobile-reaction-popup";

/// Which mobile surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MobileSurface {
    /// The row's Reply, Edit and Delete.
    Actions,
    /// The row of reactions.
    Reaction,
}

impl MobileSurface {
    fn dialog_id(self) -> &'static str {
        match self {
            Self::Actions => SHEET_ID,
            Self::Reaction => POPUP_ID,
        }
    }

    /// What its frame's test ids and title id start with.
    fn prefix(self) -> &'static str {
        match self {
            Self::Actions => "mobile-message-actions",
            Self::Reaction => "mobile-reaction",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Actions => "Message actions",
            Self::Reaction => "React to message",
        }
    }

    /// Whether its panel shows the title and a Close button. The reaction
    /// popup does not: a tap outside it (or Escape) dismisses it, and the
    /// title is only its accessible name.
    fn has_header(self) -> bool {
        self == Self::Actions
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
    /// (pending ones included). Only marks the popup's current choice: the
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

    /// Whether this is `surface`'s opening for `row`.
    pub(super) fn is_for(&self, surface: MobileSurface, row: &str) -> bool {
        self.surface == surface && self.row == row
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
    /// The open mobile opening, read fallibly (the conversation may have
    /// unmounted).
    fn mobile_opening(self) -> Option<MobileOpening> {
        self.mobile.try_peek().ok().and_then(|o| o.clone())
    }

    /// Open `opening`'s surface, closing the desktop picker, a reactor list
    /// and the other mobile surface first. Nothing is opened for an opening
    /// already gone. Whether it is open. From a deferred callback only.
    pub(super) fn open_mobile(self, opening: MobileOpening) -> bool {
        let current = self.mobile_opening();
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
        let mut slot = self.mobile;
        slot.set(Some(opening));
        dom::hold_background(true);
        dom::track_viewport(true);
        true
    }

    /// Close the open mobile surface and let go of what it held: the dialog,
    /// the input hold and the viewport tracking, then its state.
    /// An ordinary dismissal (`restore`) returns focus, without scrolling, to
    /// the row's control it opened from if that is still live; a handoff
    /// (Reply, Edit, Delete) leaves focus to where it goes. Nothing is open
    /// after it, and with nothing open it changes nothing. From a deferred
    /// callback only; the state is read and written fallibly, since the
    /// conversation may have unmounted.
    pub(super) fn close_mobile(self, restore: bool) {
        let open = self.mobile_opening();
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
        let mut slot = self.mobile;
        // A statement, so the write guard drops before `slot` does.
        let _ = slot.try_write().map(|mut slot| *slot = None);
    }

    /// Dismiss `surface` (Close, the backdrop), from its handler: it closes,
    /// focus back on its control, unless an opening other than the one open
    /// now has replaced it by then. A press not yet committed is closed too.
    pub(super) fn dismiss_mobile(self, surface: MobileSurface) {
        let token = self.mobile_token(surface);
        crate::util::defer(move || {
            if token.is_none_or(|t| self.mobile_opening_for(surface, t).is_some()) {
                self.close_mobile(true);
            }
        });
    }

    /// Close the mobile surface if what it is for has gone. From a deferred
    /// callback only.
    pub(super) fn close_mobile_if_stale(self) {
        if self.mobile_opening().is_some_and(|o| !opening_is_live(&o)) {
            self.close_mobile(false);
        }
    }

    /// `surface`'s dialog closed by itself (Escape): bring the state in line
    /// and return focus. Nothing if it is open again by now (a close and an
    /// opening in one task). From a deferred callback only.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn mobile_dialog_closed(self, surface: MobileSurface) {
        let open = self.mobile_opening();
        if open.is_some_and(|o| o.surface == surface) && !dom::dialog_open(surface.dialog_id()) {
            self.close_mobile(true);
        }
    }

    /// `row`'s control for `surface` is unmounting (from its `use_drop`). If
    /// the opening open for it now is still live when the deferred check runs,
    /// its row only remounted: it stays open, and focus returns to the new
    /// control when it closes. Otherwise it closes, silently.
    pub(super) fn mobile_control_unmounting(self, surface: MobileSurface, row: &str) {
        let Some(token) = self
            .mobile_opening()
            .filter(|o| o.is_for(surface, row))
            .map(|o| o.token)
        else {
            return;
        };
        crate::util::defer(move || {
            if self
                .mobile_opening_for(surface, token)
                .is_some_and(|o| !opening_is_live(&o))
            {
                self.close_mobile(false);
            }
        });
    }

    /// The open opening for `surface`, if `token` is it. Read when queued work
    /// runs.
    pub(super) fn mobile_opening_for(
        self,
        surface: MobileSurface,
        token: u64,
    ) -> Option<MobileOpening> {
        self.mobile_opening()
            .filter(|o| o.surface == surface && o.token == token)
    }

    /// The token of the open `surface`, now. Taken by a press, which queues
    /// work for that opening only.
    pub(super) fn mobile_token(self, surface: MobileSurface) -> Option<u64> {
        self.mobile_opening()
            .filter(|o| o.surface == surface)
            .map(|o| o.token)
    }
}

/// Whether the window is below 48rem now. The authority for a press, which
/// must not wait for [`NARROW`] to catch up with a resize.
pub(super) fn narrow_now() -> bool {
    crate::util::media_matches(NARROW_QUERY).unwrap_or(false)
}

/// Whether the window is below 48rem, for rendering: the invokers' route and
/// labels, and the guard that closes a mobile surface at 48rem. Kept by a
/// `matchMedia` listener ([`crate::util::watch_media`], installed by
/// [`MobileSurfaceGuard`]).
static NARROW: GlobalSignal<bool> = Global::new(narrow_now);

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

/// Closes the open mobile surface when it no longer belongs on screen: the
/// window reaching 48rem, another mobile panel replacing the chat, or the
/// room's generation changing. A room switch closes it from `Conversation`'s
/// room-change path, and its message leaving the history from the row's
/// control unmounting. Rendered once by `Conversation`; renders nothing.
///
/// It does not watch for an unrelated app modal opening, deliberately: while
/// a mobile surface is open the rest of the page is inert under its modal
/// dialog, so no control can open another modal, and the app's other modals
/// open only from such controls (member info, room settings, invite, ban,
/// delete confirmation, DM thread). Paths that open one without a press go
/// through a room switch first (a notification click, an invitation link),
/// which closes the surface in `Conversation`'s room-change path. A modal
/// that could open programmatically over an open surface would need closing
/// here.
#[component]
pub(super) fn MobileSurfaceGuard(
    surfaces: ReactionSurfaces,
    /// The open room's contract key, as `Conversation` renders it.
    contract_key: Option<ContractKey>,
) -> Element {
    crate::util::watch_media(NARROW_QUERY, &NARROW);
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
    let open = surfaces.mobile.read().clone();
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

/// Show `surface`'s dialog once per opening, after the render that filled it,
/// and focus the first of `focus` that matches: a button, so never the
/// keyboard. Once per token: an effect run after Escape closed it, before the
/// state follows, must not show it again.
pub(super) fn use_show_on_open(
    surfaces: ReactionSurfaces,
    surface: MobileSurface,
    focus: &'static [&'static str],
) {
    let shown_token = use_hook(|| std::rc::Rc::new(Cell::new(0u64)));
    use_effect(move || {
        // Infallible, deliberately: `Conversation`'s own `use_signal`, written
        // only from deferred callbacks.
        let token = surfaces
            .mobile
            .read()
            .as_ref()
            .filter(|o| o.surface == surface)
            .map(|o| o.token);
        let Some(token) = token else {
            return;
        };
        let first_run = shown_token.replace(token) != token;
        let id = surface.dialog_id();
        if first_run && !dom::dialog_open(id) && dom::show_dialog(id) {
            dom::focus_in(id, focus);
        }
    });
}

/// The modal dialog both mobile surfaces are drawn in: the dialog (its own
/// `close` event brings the state in line, and Tab stays inside), the
/// backdrop (dismisses), and the panel with its handle and, for the actions
/// sheet, a header with its title and Close (also dismisses). Each surface
/// renders only its body, as `children`, after that.
#[component]
pub(super) fn MobileDialogFrame(
    surfaces: ReactionSurfaces,
    surface: MobileSurface,
    /// The open opening's row, if it is this surface's.
    target_row: String,
    children: Element,
) -> Element {
    let (id, prefix, title) = (surface.dialog_id(), surface.prefix(), surface.title());
    let header = surface.has_header();
    // The dialog closing by itself (Escape): the state follows. The listener
    // lives as long as the frame.
    #[cfg(target_arch = "wasm32")]
    let close_listener = use_hook(|| {
        std::rc::Rc::new(std::cell::RefCell::new(
            None::<wasm_bindgen::closure::Closure<dyn FnMut()>>,
        ))
    });
    rsx! {
        dialog {
            id: "{id}",
            "data-testid": "{id}",
            class: "mobile-sheet",
            "aria-labelledby": if header { "{prefix}-title" },
            "aria-label": if !header { "{title}" },
            "data-target-row": "{target_row}",
            onmounted: move |_| {
                #[cfg(target_arch = "wasm32")]
                {
                    let listener = dom::on_dialog_close(id, move || {
                        crate::util::defer(move || surfaces.mobile_dialog_closed(surface));
                    });
                    *close_listener.borrow_mut() = listener;
                }
            },
            onkeydown: move |e: KeyboardEvent| {
                #[cfg(target_arch = "wasm32")]
                if e.key() == Key::Tab && dom::contain_tab(id, e.modifiers().shift()) {
                    e.prevent_default();
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = e;
            },
            div {
                "data-testid": "{prefix}-backdrop",
                class: "mobile-sheet-backdrop",
                onclick: move |_| surfaces.dismiss_mobile(surface),
            }
            div { class: "mobile-sheet-panel",
                div { class: "mobile-sheet-handle", "aria-hidden": "true" }
                if header {
                    div { class: "mobile-sheet-header",
                        h2 { id: "{prefix}-title", class: "mobile-sheet-title", "{title}" }
                        button {
                            r#type: "button",
                            "data-testid": "{prefix}-close",
                            class: "mobile-sheet-close",
                            "aria-label": "Close",
                            title: "Close",
                            onclick: move |_| surfaces.dismiss_mobile(surface),
                            {line_icon(CLOSE_ICON_PATH, 18)}
                        }
                    }
                }
                {children}
            }
        }
    }
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
    use super::MobileSurface;
    use std::cell::RefCell;
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
    pub(crate) fn row_control(row: &str, surface: MobileSurface) -> Option<web_sys::HtmlElement> {
        document()?
            .get_element_by_id(&format!("msg-{row}"))?
            .query_selector(surface.control_selector())
            .ok()??
            .dyn_into::<web_sys::HtmlElement>()
            .ok()
    }

    pub(super) fn row_control_exists(row: &str, surface: MobileSurface) -> bool {
        row_control(row, surface).is_some()
    }

    pub(super) fn focus_control(row: &str, surface: MobileSurface) {
        if let Some(control) = row_control(row, surface) {
            focus_without_scrolling(&control);
        }
    }

    /// Show the dialog `id` as a modal. A browser without `showModal` (or
    /// one that throws) gets it shown in place instead, still usable.
    /// Whether it is open.
    pub(super) fn show_dialog(id: &str) -> bool {
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

    pub(super) fn close_dialog(id: &str) {
        let Some(dialog) = element(id) else {
            return;
        };
        if !dialog.has_attribute("open") {
            return;
        }
        let closed = js_sys::Reflect::get(&dialog, &"close".into())
            .ok()
            .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
            .is_some_and(|f| f.call0(&dialog).is_ok());
        if !closed || dialog.has_attribute("open") {
            let _ = dialog.remove_attribute("open");
        }
    }

    /// Focus, without scrolling, the first element in dialog `id` matching
    /// the first of `selectors` that matches anything. One inside a row that
    /// scrolls sideways is centred in it first.
    pub(super) fn focus_in(id: &str, selectors: &[&str]) {
        let Some(dialog) = element(id) else {
            return;
        };
        let target = selectors.iter().find_map(|selector| {
            dialog
                .query_selector(selector)
                .ok()
                .flatten()?
                .dyn_into::<web_sys::HtmlElement>()
                .ok()
        });
        if let Some(target) = target {
            centre_in_row(&target);
            focus_without_scrolling(&target);
        }
    }

    /// Scroll `el`'s parent sideways so `el` sits in its middle (the browser
    /// clamps it at either end). Nothing for a parent that does not scroll.
    fn centre_in_row(el: &web_sys::HtmlElement) {
        let Some(row) = el.parent_element() else {
            return;
        };
        if row.scroll_width() <= row.client_width() {
            return;
        }
        let (choice, view) = (
            el.get_bounding_client_rect(),
            row.get_bounding_client_rect(),
        );
        let by = (choice.left() + choice.width() / 2.0) - (view.left() + view.width() / 2.0);
        row.set_scroll_left(row.scroll_left() + by.round() as i32);
    }

    /// Keep Tab inside dialog `id`'s panel: from its last control Tab goes
    /// to the first, from the first Shift+Tab goes to the last. Whether it
    /// moved focus (the caller then prevents the default).
    pub(super) fn contain_tab(id: &str, backwards: bool) -> bool {
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
    pub(super) fn hold_background(on: bool) {
        BLOCKER.with(|blocker| {
            let mut blocker = blocker.borrow_mut();
            let callback = blocker.get_or_insert_with(|| Closure::new(on_held_input));
            set_window_capture_listeners(&HELD_KINDS, callback.as_ref().unchecked_ref(), on);
        });
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
            "wheel" => is_zoom(&event),
            _ => touches(&event) > 1,
        };
        if !zoom && event.cancelable() {
            event.prevent_default();
        }
    }

    /// Add (`on`) or remove `callback` as a capturing, non-passive window
    /// listener for each of `kinds`. Adding it again is a no-op (the DOM
    /// keeps one listener per callback and phase), and so is removing one
    /// that is not there.
    pub(crate) fn set_window_capture_listeners(
        kinds: &[&str],
        callback: &js_sys::Function,
        on: bool,
    ) {
        let Some(window) = web_sys::window() else {
            return;
        };
        for &kind in kinds {
            if on {
                let options = web_sys::AddEventListenerOptions::new();
                options.set_capture(true);
                options.set_passive(false);
                let _ = window.add_event_listener_with_callback_and_add_event_listener_options(
                    kind, callback, &options,
                );
            } else {
                let _ = window.remove_event_listener_with_callback_and_bool(kind, callback, true);
            }
        }
    }

    /// How many fingers a touch event says are down.
    pub(crate) fn touches(event: &web_sys::Event) -> u32 {
        js_sys::Reflect::get(event, &"touches".into())
            .and_then(|list| js_sys::Reflect::get(&list, &"length".into()))
            .ok()
            .and_then(|n| n.as_f64())
            .map_or(0, |n| n as u32)
    }

    /// A wheel with Ctrl down: the browser zooms on it (Ctrl+wheel, and a
    /// touchpad pinch arrives as one), so it is never held.
    pub(crate) fn is_zoom(wheel: &web_sys::Event) -> bool {
        wheel
            .dyn_ref::<web_sys::MouseEvent>()
            .is_some_and(web_sys::MouseEvent::ctrl_key)
    }

    /// While a surface is open, follow the visual viewport: the software
    /// keyboard (and a pinch zoom) shrinks it without shrinking the layout
    /// viewport the dialog is laid out in, so the panel is lifted by the part
    /// of the layout viewport the visual one no longer covers
    /// (`--mobile-sheet-keyboard`) and capped to the visual height
    /// (`--mobile-sheet-viewport`). A browser without `visualViewport`
    /// keeps the CSS fallbacks.
    pub(super) fn track_viewport(on: bool) {
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
    pub(super) fn on_dialog_close(
        id: &str,
        on_close: impl FnMut() + 'static,
    ) -> Option<Closure<dyn FnMut()>> {
        let listener = Closure::<dyn FnMut()>::new(on_close);
        element(id)?
            .add_event_listener_with_callback("close", listener.as_ref().unchecked_ref())
            .ok()?;
        Some(listener)
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) mod dom {
    use super::MobileSurface;
    pub(crate) fn dialog_open(_id: &str) -> bool {
        false
    }
    pub(super) fn row_control_exists(_row: &str, _surface: MobileSurface) -> bool {
        true
    }
    pub(super) fn focus_control(_row: &str, _surface: MobileSurface) {}
    pub(super) fn close_dialog(_id: &str) {}
    pub(super) fn show_dialog(_id: &str) -> bool {
        false
    }
    pub(super) fn focus_in(_id: &str, _selectors: &[&str]) {}
    pub(super) fn hold_background(_on: bool) {}
    pub(super) fn track_viewport(_on: bool) {}
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
}
