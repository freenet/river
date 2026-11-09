//! The mobile message sheet: below 48rem, a tap on a signed message row, on
//! its reaction +, on one of its reaction chips, or its keyboard entry opens
//! this one sheet for that message (`mobile_surface.rs` has its state, frame
//! and lifecycle; `mobile_message_actions.rs` the row taps and the entry).
//!
//! From the top:
//!
//! - **Reactions.** The 16 [`FREQUENT_EMOJIS`] the desktop picker offers, in
//!   one row that scrolls sideways. A tap reacts, or on the reader's current
//!   reaction removes it, and closes the sheet. It takes focus when the sheet
//!   opens (the current reaction, centred in the row, else the first), so the
//!   software keyboard never comes up.
//! - **Who reacted.** For a message with reactions, each emoji with the
//!   people behind it, the same names and ⚠ / 🛡 marks as the desktop chip's
//!   list. It is how a phone sees who reacted: a chip tap opens this sheet.
//! - **Actions.** Reply, and Edit and Delete on your own message. Each closes
//!   the sheet first and then hands over to the existing flow (the composer's
//!   reply, the inline editor via `edit_trigger`, the delete confirmation),
//!   which takes focus. What an action does is decided when it runs, from the
//!   room as it is then (`Conversation`'s `on_mobile_action`).
//!
//! A choice acts once, for the opening it was made in and only while that is
//! live: a duplicate event, or one queued before a dismissal and reopening,
//! does nothing. The room composer is never touched.

use super::emoji_picker::FREQUENT_EMOJIS;
use super::mobile_surface::{opening_is_live, use_show_on_open, MobileDialogFrame};
use super::reaction_picker::{choice_label, ReactionSurfaces};
use super::{name_icon_mark, name_mark};
use crate::components::icons::{
    line_icon, DELETE_ICON_PATH, EDIT_ICON_PATH, REPLY_ICON_PATH, SHIELD_ICON_PATH,
};
use crate::components::members::DeputyBadge;
use crate::util::confusable::ImpersonationWarning;
use dioxus::prelude::*;
use river_core::room_state::message::MessageId;

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

/// A choice for opening `token`, run deferred: it reacts with `emoji` only
/// if that opening is still the open one and still live, and closes it
/// either way, focus back on what it opened from, so a second event from the
/// same opening finds nothing.
fn react(
    surfaces: ReactionSurfaces,
    token: u64,
    emoji: String,
    on_react: EventHandler<(MessageId, String)>,
) {
    let Some(opening) = surfaces.mobile_opening_for(token) else {
        return;
    };
    let live = opening_is_live(&opening);
    surfaces.close_mobile(true);
    if live {
        on_react.call((opening.message, emoji));
    }
}

/// An action for opening `token`, run deferred: the sheet closes first (the
/// action's destination takes focus), then, if that opening was still the
/// open one and still live, the action is handed to `Conversation`, which
/// checks it against the room as it is now.
fn act(
    surfaces: ReactionSurfaces,
    token: u64,
    action: MobileAction,
    on_action: Callback<(MobileAction, String, MessageId)>,
) {
    let Some(opening) = surfaces.mobile_opening_for(token) else {
        return;
    };
    let live = opening_is_live(&opening);
    surfaces.close_mobile(false);
    if live {
        on_action.call((action, opening.row, opening.message));
    }
}

/// The conversation's one mobile message sheet. Rendered once by
/// `Conversation`, closed until a row, its +, a chip or its entry opens it
/// below 48rem.
#[component]
pub(super) fn MobileMessageSheet(
    surfaces: ReactionSurfaces,
    on_react: EventHandler<(MessageId, String)>,
    on_action: Callback<(MobileAction, String, MessageId)>,
) -> Element {
    // Infallible, deliberately: `Conversation`'s own `use_signal`, written
    // only from deferred callbacks, so no write is live during a render.
    let opening = surfaces.mobile.read().clone();
    let own = opening.as_ref().is_some_and(|o| o.own);
    let current = opening.as_ref().and_then(|o| o.current.clone());
    let reactions = opening
        .as_ref()
        .map(|o| o.reactions.clone())
        .unwrap_or_default();
    let target_row = opening.as_ref().map(|o| o.row.clone()).unwrap_or_default();
    use_show_on_open(surfaces, FOCUS);
    let pick = move |emoji: String| {
        move |_| {
            if let Some(token) = surfaces.mobile_token() {
                let emoji = emoji.clone();
                crate::util::defer(move || react(surfaces, token, emoji, on_react));
            }
        }
    };
    let action = move |action: MobileAction| {
        move |_| {
            if let Some(token) = surfaces.mobile_token() {
                crate::util::defer(move || act(surfaces, token, action, on_action));
            }
        }
    };
    rsx! {
        MobileDialogFrame { surfaces, target_row,
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
                                        {reactor.warning.as_ref().map(|warning| name_mark("reaction-owner-impersonation-warning", "cursor-default", warning.tooltip(), crate::util::confusable::WARNING_GLYPH))}
                                        {reactor.badge.as_ref().map(|badge| name_icon_mark("reaction-owner-deputy-badge", badge.tooltip(), SHIELD_ICON_PATH))}
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
