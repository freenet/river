//! The reaction picker: ONE `popover="auto"` for the whole conversation.
//!
//! Each message's "+" is a `popovertarget` invoker for it, so the browser opens
//! and closes it, light-dismisses it on an outside click or Esc, and makes the
//! clicked "+" its implicit anchor. It lives in the top layer, so no scroll
//! container clips it and no stacking context (the composer's) covers it.
//! Rust keeps one thing: [`PickerTarget`], which message the picker reacts to.

use dioxus::prelude::*;
use river_core::room_state::message::MessageId;
use wasm_bindgen::JsCast;

use super::emoji_picker::FREQUENT_EMOJIS;

/// DOM id of the picker, and the value every "+" names in `popovertarget`.
const REACTION_PICKER_ID: &str = "reaction-picker";

/// Which message the open picker reacts to. Set by the "+" click, cleared when the picker closes.
#[derive(Clone)]
pub(super) struct PickerTarget {
    message_id: MessageId,
    /// The viewer's current reaction on that message; picking it again removes it.
    current: Option<String>,
}

/// The picker, if it is showing; the DOM owns that state.
fn open_picker() -> Option<web_sys::HtmlElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id(REACTION_PICKER_ID)?
        .dyn_into::<web_sys::HtmlElement>()
        .ok()
        .filter(|el| el.matches(":popover-open").unwrap_or(false))
}

fn picker_is_open() -> bool {
    open_picker().is_some()
}

/// Close the picker if it is open; its `toggle` handler then clears the target. A DOM call, not a signal write.
pub(super) fn close_reaction_picker() {
    if let Some(el) = open_picker() {
        let _ = el.hide_popover();
    }
}

#[component]
pub(super) fn ReactionPicker(
    target: Signal<Option<PickerTarget>>,
    on_react: EventHandler<(MessageId, String)>,
) -> Element {
    let current = target.read().as_ref().and_then(|t| t.current.clone());
    rsx! {
        div {
            id: REACTION_PICKER_ID,
            "data-testid": "emoji-picker",
            popover: "auto",
            // No display utility on the root: it would show a closed popover.
            class: "p-1.5 bg-panel text-text rounded-xl shadow-xl border border-border",
            // Fires on every open and close, however it happened; a close clears the target.
            ontoggle: move |_| {
                if !picker_is_open() {
                    let mut target = target;
                    crate::util::defer(move || target.set(None));
                }
            },
            div { class: "grid grid-cols-4 gap-0.5",
                for (emoji, is_current) in FREQUENT_EMOJIS.iter().map(|&e| (e, current.as_deref() == Some(e))) {
                    button {
                        key: "{emoji}",
                        r#type: "button",
                        // Closes the picker natively, after `onclick` has read the target.
                        popovertarget: REACTION_PICKER_ID,
                        popovertargetaction: "hide",
                        class: format!(
                            "p-1 rounded hover:bg-surface transition-colors text-xl leading-none {}",
                            if is_current { "bg-accent/20 ring-2 ring-accent" } else { "" }
                        ),
                        title: if is_current {
                            format!("Remove {emoji} reaction")
                        } else {
                            format!("React with {emoji}")
                        },
                        onclick: move |_| {
                            if let Some(t) = target.peek().clone() {
                                on_react.call((t.message_id, emoji.to_string()));
                            }
                        },
                        "{emoji}"
                    }
                }
            }
        }
    }
}

/// A message's "+": an invoker for the shared picker.
///
/// Its own component so opening the picker re-renders only these buttons.
#[component]
pub(super) fn AddReactionButton(
    message_id: MessageId,
    user_reaction: Option<String>,
    has_reactions: bool,
    picker_target: Signal<Option<PickerTarget>>,
) -> Element {
    let open_for_me = picker_target
        .read()
        .as_ref()
        .is_some_and(|t| t.message_id == message_id);
    rsx! {
        button {
            r#type: "button",
            class: format!(
                "add-reaction-btn inline-flex items-center justify-center text-xl leading-none hover:scale-110 {}",
                if has_reactions { "has-reactions" } else { "" }
            ),
            title: "Add reaction",
            "aria-label": "Add reaction",
            "data-testid": "add-reaction-button",
            popovertarget: REACTION_PICKER_ID,
            // Keeps this "+" shown while its picker is open (main.css) and names its owner.
            "aria-expanded": "{open_for_me}",
            onclick: move |_| {
                // The browser toggles after this handler, so only an opening click names the target.
                if !picker_is_open() {
                    let t = PickerTarget {
                        message_id: message_id.clone(),
                        current: user_reaction.clone(),
                    };
                    let mut picker_target = picker_target;
                    crate::util::defer(move || picker_target.set(Some(t)));
                }
            },
            "+"
        }
    }
}
