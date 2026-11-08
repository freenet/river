//! The mobile reaction popup: below 48rem, a message's reaction + opens this,
//! and only this (`mobile_surface.rs` has what the two mobile surfaces share).
//!
//! It holds a labelled text field for the device keyboard's emoji, an explicit
//! React/Remove submit, and "Choose from palette", which shows the 16
//! [`FREQUENT_EMOJIS`] the desktop picker offers. The web cannot open the
//! keyboard on its emoji tab: HTML has no emoji input mode, and the
//! VirtualKeyboard API controls only visibility and geometry. So the field
//! asks for the ordinary keyboard and says where the emoji key is.
//!
//! Focus adapter. The popup's dialog is always mounted. The press on + shows
//! it and focuses the field synchronously, inside that trusted activation
//! ([`open_from_press`]), because a mobile browser opens its software keyboard
//! only for a focus made there; the state is committed afterwards, deferred
//! (dioxus-signal-safety.md). If the press could not show it, the host's
//! effect shows it once the state is in, and the reader taps the visible
//! field: a keyboard that does not come never leaves an invisible field
//! focused or the page locked. Whether the first tap opens the keyboard on a
//! real iOS Safari or Android Chrome, inside the gateway's frame too, needs a
//! physical device and is not established by the browser tests.
//!
//! Input. The field is a local controlled input: `oninput` writes its signal
//! synchronously (the documented exception). Typing, or an IME composition
//! ending, never submits: only the submit button, a palette choice, or Enter
//! once no composition is under way. The text must be one emoji
//! (`reaction_input.rs`); anything else is explained under the field, which
//! stays editable. A submit acts once, through the conversation's
//! pending-aware toggle, for the opening it was made in and only while that is
//! live: a duplicate event, or one queued before a dismissal and reopening,
//! does nothing. Cancel, the backdrop and Escape discard the text. The room
//! composer is never touched.

use super::emoji_picker::FREQUENT_EMOJIS;
use super::mobile_surface::{
    dom, opening_is_live, MobileOpening, MobileSurface, POPUP_ID, POPUP_INPUT_ID,
};
use super::reaction_input::single_emoji;
use super::reaction_picker::ReactionSurfaces;
use crate::components::icons::{line_icon, CLOSE_ICON_PATH};
use dioxus::prelude::*;
use river_core::room_state::message::MessageId;
use std::cell::Cell;
use std::rc::Rc;

/// Called from the + press itself (below 48rem): show the popup and focus its
/// field now, inside the activation, then commit the opening. No signal is
/// written here.
pub(super) fn open_from_press(surfaces: ReactionSurfaces, opening: MobileOpening) {
    #[cfg(target_arch = "wasm32")]
    let shown = dom::show_dialog(POPUP_ID) && dom::focus_id(POPUP_INPUT_ID);
    #[cfg(not(target_arch = "wasm32"))]
    let shown = false;
    if shown {
        SHOWN_BY_PRESS.with(|s| s.set(opening.token));
    }
    let closes = dom::closes_from_here();
    crate::util::defer(move || {
        // Closed since the press. By the reader (Escape, before this ran):
        // nothing to commit. By this code (an earlier opening's close queued
        // before this press): shown again for this one.
        let closed = shown && !dom::dialog_open(POPUP_ID);
        if closed && dom::closes_from_here() == closes {
            return;
        }
        let token = opening.token;
        if surfaces.open_mobile(opening) && (!shown || closed) {
            show_now(token);
        }
    });
}

thread_local! {
    /// The last opening shown, by its press or by a fallback. Nothing shows
    /// that one again: closed since, it was dismissed.
    static SHOWN_BY_PRESS: Cell<u64> = const { Cell::new(0) };
}

/// Show the popup for opening `token` and focus its field: the fallback when
/// the press could not. Recorded as shown, so an effect run after Escape
/// closes it never shows it again.
fn show_now(token: u64) {
    SHOWN_BY_PRESS.with(|s| s.set(token));
    #[cfg(target_arch = "wasm32")]
    if dom::show_dialog(POPUP_ID) {
        dom::focus_id(POPUP_INPUT_ID);
    }
}

/// A submit for opening `token`, run deferred: it reacts with `emoji` only if
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

/// Validate `text` for the open popup, now, and queue its submit; or queue
/// the explanation. Nothing if no popup is open (or it is for a press not yet
/// committed).
fn submit(surfaces: ReactionSurfaces, text: &str, on_react: EventHandler<(MessageId, String)>) {
    let Some(token) = surfaces.mobile_token(MobileSurface::Reaction) else {
        return;
    };
    match single_emoji(text) {
        Ok(emoji) => {
            let emoji = emoji.to_string();
            crate::util::defer(move || dispatch(surfaces, token, emoji, on_react));
        }
        Err(why) => {
            let mut error = surfaces.mobile.error;
            crate::util::defer(move || {
                if surfaces
                    .mobile_opening_for(MobileSurface::Reaction, token)
                    .is_some()
                {
                    error.set(Some(why));
                }
            });
        }
    }
}

/// Dismiss the popup for opening `token` (Close, the backdrop) without
/// reacting, focus back on its +.
fn dismiss(surfaces: ReactionSurfaces) {
    let token = surfaces.mobile_token(MobileSurface::Reaction);
    crate::util::defer(move || {
        if let Some(token) = token {
            if surfaces
                .mobile_opening_for(MobileSurface::Reaction, token)
                .is_none()
            {
                return;
            }
        }
        surfaces.close_mobile(true);
    });
}

/// Whether a keydown belongs to an input method: `keyCode` 229, which WebKit
/// and Blink give the key that confirms a conversion even after the
/// composition has ended.
fn is_ime_key(e: &KeyboardEvent) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        e.data()
            .downcast::<web_sys::KeyboardEvent>()
            .is_some_and(|k| k.key_code() == 229)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = e;
        false
    }
}

/// The conversation's one mobile reaction popup. Rendered once by
/// `Conversation`, closed until a + opens it below 48rem.
#[component]
pub(super) fn MobileReactionPopup(
    surfaces: ReactionSurfaces,
    on_react: EventHandler<(MessageId, String)>,
) -> Element {
    // Infallible, deliberately: `Conversation`'s own `use_signal`s, written
    // only from deferred callbacks and this component's own `oninput`, so no
    // write is live during a render.
    let opening = surfaces
        .mobile
        .opening
        .read()
        .clone()
        .filter(|o| o.surface == MobileSurface::Reaction);
    let mut text = surfaces.mobile.text;
    let mut error = surfaces.mobile.error;
    let mut palette = surfaces.mobile.palette;
    let typed = text.read().clone();
    let explanation = (*error.read()).map(|why| why.explanation());
    let palette_open = *palette.read();
    let current = opening.as_ref().and_then(|o| o.current.clone());
    let target_row = opening.as_ref().map(|o| o.row.clone()).unwrap_or_default();
    // What the submit does, from the field and the reaction the footer shows:
    // the same emoji again removes it. The toggle decides from the room.
    let removes = single_emoji(&typed)
        .ok()
        .is_some_and(|emoji| current.as_deref() == Some(emoji));
    let submit_label = if removes { "Remove" } else { "React" };
    // Whether an IME composition is under way: Enter then confirms it, and
    // must not submit. A `Cell`: nothing renders from it.
    let composing = use_hook(|| Rc::new(Cell::new(false)));

    // The fallback show: an opening committed while its dialog is not open
    // (the press could not show it). Once per opening: an effect run after
    // Escape closed it, before the state follows, must not show it again.
    // Infallible read, as above.
    let shown_token = use_hook(|| Rc::new(Cell::new(0u64)));
    use_effect(move || {
        let token = surfaces
            .mobile
            .opening
            .read()
            .as_ref()
            .filter(|o| o.surface == MobileSurface::Reaction)
            .map(|o| o.token);
        if let Some(token) = token {
            let by_press = SHOWN_BY_PRESS.with(|s| s.get()) == token;
            if shown_token.replace(token) != token && !by_press && !dom::dialog_open(POPUP_ID) {
                show_now(token);
            }
        }
    });
    // The dialog closing by itself (Escape): the state follows.
    #[cfg(target_arch = "wasm32")]
    let close_listener = use_hook(|| {
        Rc::new(std::cell::RefCell::new(
            None::<(
                web_sys::HtmlElement,
                wasm_bindgen::closure::Closure<dyn FnMut()>,
            )>,
        ))
    });

    let (start, end, enter) = (composing.clone(), composing.clone(), composing);
    rsx! {
        dialog {
            id: POPUP_ID,
            "data-testid": "mobile-reaction-popup",
            class: "mobile-sheet",
            "aria-labelledby": "mobile-reaction-title",
            "data-target-row": "{target_row}",
            onmounted: move |_| {
                #[cfg(target_arch = "wasm32")]
                {
                    let listener = dom::on_dialog_close(POPUP_ID, move || {
                        crate::util::defer(move || {
                            surfaces.mobile_dialog_closed(MobileSurface::Reaction)
                        });
                    });
                    *close_listener.borrow_mut() = listener;
                }
            },
            onkeydown: move |e: KeyboardEvent| {
                #[cfg(target_arch = "wasm32")]
                if e.key() == Key::Tab && dom::contain_tab(POPUP_ID, e.modifiers().shift()) {
                    e.prevent_default();
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = e;
            },
            div {
                "data-testid": "mobile-reaction-backdrop",
                class: "mobile-sheet-backdrop",
                onclick: move |_| dismiss(surfaces),
            }
            div { class: "mobile-sheet-panel",
                div { class: "mobile-sheet-handle", "aria-hidden": "true" }
                div { class: "mobile-sheet-header",
                    h2 { id: "mobile-reaction-title", class: "mobile-sheet-title", "React to message" }
                    button {
                        r#type: "button",
                        "data-testid": "mobile-reaction-close",
                        class: "mobile-sheet-close",
                        "aria-label": "Close",
                        title: "Close",
                        onclick: move |_| dismiss(surfaces),
                        {line_icon(CLOSE_ICON_PATH, 18)}
                    }
                }
                label { r#for: POPUP_INPUT_ID, class: "mobile-reaction-label", "Emoji" }
                input {
                    id: POPUP_INPUT_ID,
                    "data-testid": "mobile-reaction-input",
                    class: "mobile-reaction-input",
                    r#type: "text",
                    value: "{typed}",
                    autocomplete: "off",
                    "autocorrect": "off",
                    "autocapitalize": "off",
                    spellcheck: "false",
                    "enterkeyhint": "send",
                    "aria-describedby": if explanation.is_some() { "mobile-reaction-hint mobile-reaction-error" } else { "mobile-reaction-hint" },
                    "aria-invalid": if explanation.is_some() { "true" } else { "false" },
                    // The documented exception: a local controlled input's
                    // signal is written synchronously, so a re-render never
                    // writes an older value back over what was typed.
                    // Only that signal: the explanation's clearing is
                    // deferred like any other write from a handler.
                    oninput: move |e| {
                        text.set(e.value());
                        if error.peek().is_some() {
                            crate::util::defer(move || {
                                if error.peek().is_some() {
                                    error.set(None);
                                }
                            });
                        }
                    },
                    oncompositionstart: move |_| start.set(true),
                    // Held a task past the composition's end: WebKit fires
                    // `compositionend` before the Enter keydown that
                    // confirmed the conversion, and that keydown says
                    // `isComposing: false`.
                    oncompositionend: move |_| {
                        let end = end.clone();
                        crate::util::defer(move || end.set(false));
                    },
                    onkeydown: move |e: KeyboardEvent| {
                        if e.key() != Key::Enter
                            || e.is_composing()
                            || enter.get()
                            || is_ime_key(&e)
                        {
                            return;
                        }
                        e.prevent_default();
                        let value = text.peek().clone();
                        submit(surfaces, &value, on_react);
                    },
                }
                p { id: "mobile-reaction-hint", class: "mobile-reaction-hint",
                    "Use your keyboard\u{2019}s emoji key, or choose from the palette."
                }
                if let Some(explanation) = explanation {
                    p {
                        id: "mobile-reaction-error",
                        "data-testid": "mobile-reaction-error",
                        class: "mobile-reaction-error",
                        role: "alert",
                        "{explanation}"
                    }
                }
                div { class: "mobile-reaction-buttons",
                    button {
                        r#type: "button",
                        "data-testid": "mobile-reaction-submit",
                        class: "mobile-sheet-primary",
                        onclick: move |_| {
                            let value = text.peek().clone();
                            submit(surfaces, &value, on_react);
                        },
                        "{submit_label}"
                    }
                    button {
                        r#type: "button",
                        "data-testid": "mobile-reaction-palette-toggle",
                        class: "mobile-sheet-secondary",
                        "aria-expanded": "{palette_open}",
                        "aria-controls": "mobile-reaction-palette",
                        onclick: move |_| {
                            let open = !palette_open;
                            crate::util::defer(move || {
                                if *palette.peek() != open {
                                    palette.set(open);
                                }
                            });
                        },
                        "Choose from palette"
                    }
                }
                if palette_open {
                    div {
                        id: "mobile-reaction-palette",
                        "data-testid": "mobile-reaction-palette",
                        class: "mobile-reaction-palette",
                        role: "group",
                        "aria-label": "Palette",
                        for emoji in FREQUENT_EMOJIS.iter() {
                            {
                                let emoji = emoji.to_string();
                                let is_current = current.as_deref() == Some(emoji.as_str());
                                rsx! {
                                    button {
                                        key: "{emoji}",
                                        r#type: "button",
                                        class: if is_current { "mobile-reaction-choice mobile-reaction-choice--current" } else { "mobile-reaction-choice" },
                                        title: if is_current { format!("Remove {emoji} reaction") } else { format!("React with {emoji}") },
                                        "aria-label": if is_current { format!("Remove {emoji} reaction") } else { format!("React with {emoji}") },
                                        onclick: {
                                            let emoji = emoji.clone();
                                            move |_| submit(surfaces, &emoji, on_react)
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
    }
}
