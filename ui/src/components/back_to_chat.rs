//! The mobile back arrow that returns to the chat from the rooms or members
//! panel. While that panel replaces the chat, nothing else shows an arrival in
//! the open room, so the arrow carries its unread count
//! (`.claude/rules/history-scrolling.md`).

use crate::components::app::document_title::count_unread_in_current_room;
use crate::components::app::{MobileView, MOBILE_VIEW};
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaArrowLeft;
use dioxus_free_icons::Icon;

#[component]
pub fn BackToChatButton(class: &'static str, icon_size: u32, testid: &'static str) -> Element {
    let unread = use_memo(count_unread_in_current_room);
    let count = unread();

    rsx! {
        button {
            // `relative` anchors the unread badge overlay.
            class: "relative {class} rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
            "data-testid": testid,
            // The badge is visual-only, so the count lives in the accessible name.
            "aria-label": if count > 0 {
                format!("Back to chat, {count} unread")
            } else {
                "Back to chat".to_string()
            },
            onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Chat),
            Icon { icon: FaArrowLeft, width: icon_size, height: icon_size }
            CountBadge { count, position: "-top-1.5 -right-1.5", testid: "back-to-chat-unread-badge" }
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
