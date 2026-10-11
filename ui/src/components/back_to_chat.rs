//! The mobile back arrow that returns to the chat from the rooms or members
//! panel, and the count badge the hamburger buttons share.

use crate::components::app::{MobileView, MOBILE_VIEW};
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaArrowLeft;
use dioxus_free_icons::Icon;

#[component]
pub fn BackToChatButton(class: &'static str, icon_size: u32, testid: &'static str) -> Element {
    rsx! {
        button {
            class: "{class} rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
            "data-testid": testid,
            "aria-label": "Back to chat",
            onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Chat),
            Icon { icon: FaArrowLeft, width: icon_size, height: icon_size }
        }
    }
}

/// A visual-only count on a `relative` button's corner, absent at zero.
#[component]
pub fn CountBadge(count: usize, position: &'static str, testid: &'static str) -> Element {
    rsx! {
        if count > 0 {
            span {
                class: "absolute {position} flex items-center justify-center min-w-4 h-4 px-1 rounded-full bg-accent text-white text-[10px] font-semibold leading-none pointer-events-none",
                "data-testid": testid,
                "aria-hidden": "true",
                "{count}"
            }
        }
    }
}
