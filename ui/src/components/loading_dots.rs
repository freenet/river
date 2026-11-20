//! Decorative loading indicators. The surrounding text describes the work.
use dioxus::prelude::*;

#[component]
pub(crate) fn WaveDots(test_id: Option<&'static str>) -> Element {
    dots("wave-dots", 10, test_id)
}

#[component]
pub(crate) fn SmallDots(test_id: Option<&'static str>) -> Element {
    dots("small-dots", 5, test_id)
}

fn dots(class: &'static str, count: usize, test_id: Option<&'static str>) -> Element {
    rsx! {
        span { class: class, "aria-hidden": "true", "data-testid": test_id,
            for i in 0..count {
                span { key: "{i}", style: "--i: {i}" }
            }
        }
    }
}
