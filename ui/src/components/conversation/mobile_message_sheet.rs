//! The mobile message sheet: below 48rem, a tap on a signed message row or
//! on its reaction +, or its keyboard entry, opens this one sheet for that
//! message (`mobile_surface.rs` has its state and lifecycle;
//! `mobile_message_actions.rs` the row taps and the entry).
//!
//! It is drawn in a native modal `<dialog>`: the dialog (its own `close`
//! event brings the state in line, Tab stays inside, and a downward drag
//! anywhere on it dismisses it, `mobile_sheet_drag.rs`), the backdrop (a tap
//! slides it away), and the panel with its handle.
//!
//! From the top:
//!
//! - **Reactions.** The 16 [`FREQUENT_EMOJIS`] the desktop picker offers, in
//!   one row that scrolls sideways. A tap reacts, or on the reader's current
//!   reaction removes it, and closes the sheet. It takes focus when the sheet
//!   opens (the current reaction, centred in the row, else the first).
//! - **Who reacted.** For a message with reactions, each emoji with the
//!   people behind it, the same names and ⚠ / 🛡 marks as the desktop chip's
//!   list. It is how a phone sees who reacted: a chip tap reacts with its
//!   emoji, so the names are here, behind a row tap or the +.
//! - **Actions.** Reply, and Edit and Delete on your own message. Each closes
//!   the sheet first and then hands over to the existing flow (the composer's
//!   reply, the inline editor via `edit_trigger`, the delete confirmation),
//!   which takes focus. What an action does is decided when it runs, from the
//!   room as it is then (`Conversation`'s `on_mobile_action`).
//!
//! A choice acts once, for the opening it was made in and only while that is
//! live: a duplicate event, or one queued before a dismissal and reopening,
//! does nothing.

use super::emoji_picker::FREQUENT_EMOJIS;
use super::mobile_surface::{dom, SHEET_ID, SHEET_TITLE};
use super::reaction_picker::{choice_label, ReactionSurfaces};
use super::reactor_marks;
use crate::components::icons::{line_icon, DELETE_ICON_PATH, EDIT_ICON_PATH, REPLY_ICON_PATH};
use crate::components::members::DeputyBadge;
use crate::util::confusable::ImpersonationWarning;
use dioxus::prelude::*;
use river_core::room_state::message::MessageId;
use std::cell::Cell;
use std::rc::Rc;

/// What takes focus when it opens: the reader's current reaction, else the
/// first choice.
const FOCUS: &[&str] = &[
    ".mobile-reaction-choice--current",
    ".mobile-reaction-choice",
];

/// One of the sheet's actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MobileAction {
    Reply,
    Edit,
    Delete,
}

/// Someone who reacted, as the row shows them.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Reactor {
    /// Their nickname, or "You".
    pub(super) name: String,
    pub(super) warning: Option<ImpersonationWarning>,
    pub(super) badge: Option<DeputyBadge>,
}

/// One of a message's reactions: the emoji and who made it, in the order
/// the row's chips show them.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SheetReaction {
    pub(super) emoji: String,
    /// The reader is among them.
    pub(super) mine: bool,
    pub(super) reactors: Vec<Reactor>,
}

/// The conversation's one mobile message sheet. Rendered once by
/// `Conversation`, closed until a row, its + or its entry opens it below
/// 48rem.
#[component]
pub(super) fn MobileMessageSheet(
    surfaces: ReactionSurfaces,
    on_react: EventHandler<(MessageId, String)>,
    on_action: Callback<(MobileAction, String, MessageId)>,
) -> Element {
    let opening = surfaces.mobile_for_render();
    let target = opening.as_ref().map(|o| &o.target);
    let own = target.is_some_and(|t| t.own);
    let current = target.and_then(|t| t.current.clone());
    let reactions = target.map(|t| t.reactions.clone()).unwrap_or_default();
    let target_row = target.map(|t| t.row.clone()).unwrap_or_default();
    // The drag reads it from the DOM, so a slide away closes only the
    // opening it began on.
    let token = opening.as_ref().map_or(0, |o| o.token);
    // Show the sheet once per opening, after the render that filled it, and
    // focus the first of `FOCUS` that matches: a button, so never the
    // keyboard. A new opening starts at rest, even in a dialog still open:
    // one still sliding away from an earlier opening comes back. Once per
    // token: an effect run after Escape closed it, before the state follows,
    // must not show it again.
    let shown_token = use_hook(|| Rc::new(Cell::new(0u64)));
    use_effect(move || {
        let Some(opening) = surfaces.mobile_for_render() else {
            return;
        };
        if shown_token.replace(opening.token) == opening.token {
            return;
        }
        super::mobile_sheet_drag::reset();
        dom::mark_opened_by(opening.by_pointer);
        if !dom::dialog_open() && dom::show_dialog() {
            dom::focus_in(FOCUS);
        }
    });
    // The drag's listeners, for as long as the sheet lives.
    #[cfg(target_arch = "wasm32")]
    let drag_listeners = use_hook(|| {
        Rc::new(std::cell::RefCell::new(
            None::<super::mobile_sheet_drag::Listeners>,
        ))
    });
    // The dialog closing by itself (Escape): the state follows. The listener
    // lives as long as the sheet.
    #[cfg(target_arch = "wasm32")]
    let close_listener = use_hook(|| {
        Rc::new(std::cell::RefCell::new(
            None::<wasm_bindgen::closure::Closure<dyn FnMut()>>,
        ))
    });
    // A choice acts deferred, for the opening it was made in and only if
    // that is still open and live then ([`ReactionSurfaces::take_mobile`]).
    // An emoji reacts, focus back on what the sheet opened from.
    let pick = move |emoji: String| {
        move |_| {
            if let Some(token) = surfaces.mobile_token() {
                let emoji = emoji.clone();
                crate::util::defer(move || {
                    if let Some(opening) = surfaces.take_mobile(token, true) {
                        on_react.call((opening.target.message, emoji));
                    }
                });
            }
        }
    };
    // An action is handed to `Conversation`, which checks it against the
    // room as it is now; its destination takes focus.
    let action = move |action: MobileAction| {
        move |_| {
            if let Some(token) = surfaces.mobile_token() {
                crate::util::defer(move || {
                    if let Some(opening) = surfaces.take_mobile(token, false) {
                        on_action.call((action, opening.target.row, opening.target.message));
                    }
                });
            }
        }
    };
    rsx! {
        dialog {
            id: SHEET_ID,
            "data-testid": SHEET_ID,
            class: "mobile-sheet",
            "aria-label": SHEET_TITLE,
            "data-target-row": "{target_row}",
            "data-opening": "{token}",
            onmounted: move |_| {
                #[cfg(target_arch = "wasm32")]
                {
                    let listener = dom::on_dialog_close(move || {
                        crate::util::defer(move || surfaces.mobile_dialog_closed());
                    });
                    *close_listener.borrow_mut() = listener;
                    *drag_listeners.borrow_mut() = super::mobile_sheet_drag::install(surfaces);
                }
            },
            onkeydown: move |e: KeyboardEvent| {
                // A key in it: from now on its focus shows, however it opened.
                dom::mark_opened_by(false);
                #[cfg(target_arch = "wasm32")]
                if e.key() == Key::Tab && dom::contain_tab(e.modifiers().shift()) {
                    e.prevent_default();
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = e;
            },
            div {
                "data-testid": "{SHEET_ID}-backdrop",
                class: "mobile-sheet-backdrop",
                onclick: move |_| super::mobile_sheet_drag::animate_out(surfaces),
            }
            div { class: "mobile-sheet-panel",
                div { class: "mobile-sheet-handle", "aria-hidden": "true" }
                div {
                    "data-testid": "mobile-reaction-palette",
                    class: "mobile-reaction-palette",
                    role: "group",
                    "aria-label": "Reactions",
                    for emoji in FREQUENT_EMOJIS.iter() {
                        {
                            let emoji = emoji.to_string();
                            let is_current = current.as_deref() == Some(emoji.as_str());
                            rsx! {
                                button {
                                    key: "{emoji}",
                                    r#type: "button",
                                    class: if is_current { "mobile-reaction-choice mobile-reaction-choice--current" } else { "mobile-reaction-choice" },
                                    title: choice_label(&emoji, is_current),
                                    "aria-label": choice_label(&emoji, is_current),
                                    onclick: pick(emoji.clone()),
                                    "{emoji}"
                                }
                            }
                        }
                    }
                }
                if !reactions.is_empty() {
                    ul {
                        "data-testid": "mobile-sheet-reactors",
                        class: "mobile-sheet-reactors",
                        "aria-label": "Who reacted",
                        for reaction in reactions.iter() {
                            li {
                                key: "{reaction.emoji}",
                                "data-testid": "mobile-sheet-reaction",
                                class: "mobile-sheet-reaction",
                                span { class: "mobile-sheet-reaction-emoji", "{reaction.emoji}" }
                                span { class: "mobile-sheet-reaction-names",
                                    for (index, reactor) in reaction.reactors.iter().enumerate() {
                                        span {
                                            key: "{index}",
                                            class: "mobile-sheet-reactor",
                                            span { "data-testid": "mobile-sheet-reactor", "{reactor.name}" }
                                            {reactor_marks(reactor)}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "mobile-sheet-actions",
                    button {
                        r#type: "button",
                        "data-testid": "mobile-message-action-reply",
                        class: "mobile-sheet-action",
                        onclick: action(MobileAction::Reply),
                        {line_icon(REPLY_ICON_PATH, 20)}
                        span { "Reply" }
                    }
                    if own {
                        button {
                            r#type: "button",
                            "data-testid": "mobile-message-action-edit",
                            class: "mobile-sheet-action",
                            onclick: action(MobileAction::Edit),
                            {line_icon(EDIT_ICON_PATH, 20)}
                            span { "Edit" }
                        }
                        button {
                            r#type: "button",
                            "data-testid": "mobile-message-action-delete",
                            class: "mobile-sheet-action mobile-sheet-action--danger",
                            onclick: action(MobileAction::Delete),
                            {line_icon(DELETE_ICON_PATH, 20)}
                            span { "Delete" }
                        }
                    }
                }
            }
        }
    }
}
