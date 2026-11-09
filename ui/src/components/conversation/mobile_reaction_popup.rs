//! The mobile reaction popup: below 48rem, a message's reaction + opens this,
//! and only this (`mobile_surface.rs` has what the two mobile surfaces share).
//!
//! It holds the 16 [`FREQUENT_EMOJIS`] the desktop picker offers, in one row
//! that scrolls sideways (the panel is outside the background hold, so a swipe
//! on it scrolls it). A tap on one reacts, or on the reader's current reaction
//! removes it, and closes the popup. It has no title or Close button: a tap
//! outside it, or Escape, dismisses it. It has no field, so it never brings up
//! the software keyboard: like the actions sheet it is committed deferred and
//! shown after the render that fills it, the current reaction (else the first
//! choice) centred in the row and focused.
//!
//! A choice acts once, through the conversation's pending-aware toggle, for
//! the opening it was made in and only while that is live: a duplicate event,
//! or one queued before a dismissal and reopening, does nothing. The room
//! composer is never touched.

use super::emoji_picker::FREQUENT_EMOJIS;
use super::mobile_surface::{
    opening_is_live, use_show_on_open, MobileDialogFrame, MobileOpening, MobileSurface,
};
use super::reaction_picker::{choice_label, ReactionSurfaces};
use dioxus::prelude::*;
use river_core::room_state::message::MessageId;

/// What takes focus when it opens: the reader's current reaction, else the
/// first choice.
const FOCUS: &[&str] = &[
    ".mobile-reaction-choice--current",
    ".mobile-reaction-choice",
];

/// Called from the + press (below 48rem): commit the opening, deferred.
pub(super) fn open(surfaces: ReactionSurfaces, opening: MobileOpening) {
    crate::util::defer(move || {
        surfaces.open_mobile(opening);
    });
}

/// A choice for opening `token`, run deferred: it reacts with `emoji` only if
/// that opening is still the open one and still live, and closes it either
/// way, so a second event from the same opening finds nothing.
fn dispatch(
    surfaces: ReactionSurfaces,
    token: u64,
    emoji: String,
    on_react: EventHandler<(MessageId, String)>,
) {
    let Some(opening) = surfaces.mobile_opening_for(MobileSurface::Reaction, token) else {
        return;
    };
    let live = opening_is_live(&opening);
    surfaces.close_mobile(true);
    if live {
        on_react.call((opening.message, emoji));
    }
}

/// A choice tapped in the open popup: queue its reaction for this opening.
/// Nothing if no popup is open (or it is for a press not yet committed).
fn pick(surfaces: ReactionSurfaces, emoji: String, on_react: EventHandler<(MessageId, String)>) {
    if let Some(token) = surfaces.mobile_token(MobileSurface::Reaction) {
        crate::util::defer(move || dispatch(surfaces, token, emoji, on_react));
    }
}

/// The conversation's one mobile reaction popup. Rendered once by
/// `Conversation`, closed until a + opens it below 48rem.
#[component]
pub(super) fn MobileReactionPopup(
    surfaces: ReactionSurfaces,
    on_react: EventHandler<(MessageId, String)>,
) -> Element {
    // Infallible, deliberately: `Conversation`'s own `use_signal`, written
    // only from deferred callbacks, so no write is live during a render.
    let opening = surfaces
        .mobile
        .read()
        .clone()
        .filter(|o| o.surface == MobileSurface::Reaction);
    let current = opening.as_ref().and_then(|o| o.current.clone());
    let target_row = opening.as_ref().map(|o| o.row.clone()).unwrap_or_default();
    use_show_on_open(surfaces, MobileSurface::Reaction, FOCUS);
    rsx! {
        MobileDialogFrame { surfaces, surface: MobileSurface::Reaction, target_row,
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
                                onclick: {
                                    let emoji = emoji.clone();
                                    move |_| pick(surfaces, emoji.clone(), on_react)
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
