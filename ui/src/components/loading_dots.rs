//! Decorative loading indicators. The surrounding text describes the work.
use dioxus::prelude::*;

#[component]
pub(crate) fn WaveDots(test_id: Option<&'static str>) -> Element {
    rsx! {
        span { class: "wave-dots", "aria-hidden": "true", "data-testid": test_id,
            for i in 0..10 {
                span { key: "{i}", style: "--i: {i}" }
            }
        }
    }
}

#[component]
pub(crate) fn SmallDots(test_id: Option<&'static str>) -> Element {
    rsx! {
        span { class: "small-dots", "aria-hidden": "true", "data-testid": test_id,
            for i in 0..5 {
                span { key: "{i}", style: "--i: {i}" }
            }
        }
    }
}
