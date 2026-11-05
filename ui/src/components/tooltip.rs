//! An explanation that appears at once on hover or keyboard focus, and
//! optionally copies diagnostic details when its host is clicked.
//!
//! With `copy`, the host is a real `type="button"`: clicking it (or the
//! tooltip, which is its child) copies the text through the sandbox-safe
//! clipboard helper and says so in the app toast. Without `copy` the host is a
//! plain `span`, so it can sit inside another button (a room row) without
//! nesting interactive controls; it then also shows while that ancestor
//! button is hovered anywhere or has keyboard focus.
//!
//! The card is a child of its host. A copy card takes the pointer, so moving
//! onto it keeps the host hovered; its padding touches the host and bridges
//! the gap. Any other card passes pointer input through to whatever it covers
//! (a room row's card overlaps the neighbouring rows), so it hides once the
//! pointer leaves its source row unless that row has keyboard focus. Where CSS
//! anchor positioning is supported it is `position: fixed`, anchored to the
//! host, and flips to stay on screen, which also escapes a scrolling rail's
//! clipping. Elsewhere it falls back to `position: absolute` above the host.
//! Either way it is linked by `aria-describedby`, so the explanation does not
//! depend on it being painted. It is shown and hidden with `display`, not
//! `visibility`, which Gecko leaves stale on a flipped anchored card. Styles:
//! `.tooltip-*` in `assets/main.css`.

use dioxus::prelude::*;

/// What the toast says after a copy.
const COPIED_MESSAGE: &str = "Error details copied";

thread_local! {
    static NEXT_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn next_tooltip_id() -> String {
    NEXT_ID.with(|n| {
        let id = n.get() + 1;
        n.set(id);
        format!("river-tooltip-{id}")
    })
}

#[component]
pub fn WithTooltip(
    /// The explanation, clamped to three lines.
    tip: String,
    /// Text a click copies; makes the host a button.
    #[props(default)]
    copy: Option<String>,
    /// Classes for the host.
    #[props(default)]
    class: String,
    /// `data-testid` of the host; the card gets `<id>-tooltip`.
    #[props(default)]
    test_id: Option<String>,
    /// Any other host attributes (`aria-label`, `aria-busy`, `role`, …).
    #[props(extends = GlobalAttributes)]
    attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let id = use_hook(next_tooltip_id);
    let anchor = format!("--{id}");
    let tip_test_id = test_id.as_ref().map(|t| format!("{t}-tooltip"));
    let copyable = copy.is_some();

    let card = rsx! {
        span {
            id: "{id}",
            role: "tooltip",
            class: "tooltip-card",
            style: "position-anchor: {anchor};",
            "data-testid": tip_test_id,
            span { class: "tooltip-box",
                span { class: "tooltip-text", "{tip}" }
                if copyable {
                    span { class: "tooltip-hint", "Click to copy details" }
                }
            }
        }
    };

    match copy {
        Some(text) => rsx! {
            button {
                r#type: "button",
                class: "tooltip-host tooltip-host--copy {class}",
                style: "anchor-name: {anchor};",
                "aria-describedby": "{id}",
                "data-testid": test_id,
                onclick: move |_| {
                    // No signal is written here: the clipboard helper is a
                    // plain DOM call and `show_toast` defers its own write.
                    crate::util::copy_to_clipboard(&text);
                    crate::components::toast::show_toast(COPIED_MESSAGE, None);
                },
                ..attributes,
                {children}
                {card}
            }
        },
        None => rsx! {
            span {
                class: "tooltip-host {class}",
                style: "anchor-name: {anchor};",
                "aria-describedby": "{id}",
                "data-testid": test_id,
                ..attributes,
                {children}
                {card}
            }
        },
    }
}
