//! Stroke icons for the inline message action buttons, drawn in `currentColor`.
use dioxus::prelude::*;

/// The 16x16 `currentColor` stroke `<svg>` shell every icon here shares.
fn stroke_svg(size: u32, stroke_width: &'static str, children: Element) -> Element {
    rsx! {
        svg {
            class: "inline-block align-[-0.125em]",
            width: "{size}",
            height: "{size}",
            view_box: "0 0 16 16",
            fill: "none",
            stroke: "currentColor",
            stroke_width: stroke_width,
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            {children}
        }
    }
}

fn stroke_icon(d: &'static str, size: u32) -> Element {
    stroke_svg(size, "2", rsx! { path { d: d } })
}

/// Add-reaction face: an outlined circle, two dot eyes and a smile. A lighter
/// stroke than the action icons, since it sits among emoji chips rather than
/// among the other buttons.
#[component]
pub(super) fn SmileyIcon() -> Element {
    stroke_svg(
        16,
        "1.5",
        rsx! {
            circle { cx: "8", cy: "8", r: "6.25" }
            path { d: "M5.5 9.5a3 3 0 0 0 5 0" }
            path { d: "M6 6.25h.01M10 6.25h.01", stroke_width: "2" }
        },
    )
}

#[component]
pub(super) fn ReplyIcon(#[props(default = 14)] size: u32) -> Element {
    stroke_icon("M6 3 2 7l4 4M2 7h7a5 5 0 0 1 5 5v1", size)
}

#[component]
pub(super) fn EditIcon() -> Element {
    stroke_icon("M10 3l3 3-7 7H3v-3zM8 5l3 3", 14)
}

#[component]
pub(super) fn DeleteIcon() -> Element {
    stroke_icon("M3 5h10M6 5V3h4v2M4.5 5l.7 8h5.6l.7-8", 14)
}
