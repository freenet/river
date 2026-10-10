//! The mobile back arrow that returns to the chat from the rooms or members
//! panel. While that panel replaces the chat, nothing else shows an arrival in
//! the open room, so the arrow carries its unread count
//! (`.claude/rules/history-scrolling.md`).

use crate::components::app::document_title::count_unread_in_current_room;
use crate::components::app::{MobileView, MOBILE_VIEW};
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaArrowLeft;
use dioxus_free_icons::Icon;

/// The mobile back arrow to the chat, with the open room's unread count while another panel replaces it.
#[component]
pub fn BackToChatButton(class: &'static str, icon_size: u32, testid: &'static str) -> Element {
    let unread = use_memo(count_unread_in_current_room);

    rsx! {
        button {
            // `relative` anchors the unread badge overlay.
            class: "relative {class}",
            "data-testid": testid,
            // The badge is visual-only, so the count lives in the accessible name.
            "aria-label": if unread() > 0 {
                format!("Back to chat, {} unread", unread())
            } else {
                "Back to chat".to_string()
            },
            onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Chat),
            Icon { icon: FaArrowLeft, width: icon_size, height: icon_size }
            if unread() > 0 {
                span {
                    class: "absolute -top-1.5 -right-1.5 flex items-center justify-center min-w-4 h-4 px-1 rounded-full bg-accent text-white text-[10px] font-semibold leading-none pointer-events-none",
                    "data-testid": "back-to-chat-unread-badge",
                    "aria-hidden": "true",
                    "{unread}"
                }
            }
        }
    }
}
