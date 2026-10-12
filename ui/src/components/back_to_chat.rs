//! The mobile back arrow that returns to the chat from the rooms or members
//! panel, and the count badge the hamburger buttons share.

use crate::components::app::document_title::count_unread_in_current_room;
use crate::components::app::{MobileView, MOBILE_VIEW};
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaArrowLeft;
use dioxus_free_icons::Icon;

/// `current_room_badge` is the test id of a badge counting the open room's
/// unread messages under its notification mode, or `None` for no badge. The
/// members panel opts in: while it replaces the chat, Latest is hidden with
/// the chat and so is the room list. The rooms panel does not, since the open
/// room's row there shows the same count beside its arrow.
#[component]
pub fn BackToChatButton(
    class: &'static str,
    icon_size: u32,
    testid: &'static str,
    #[props(default)] current_room_badge: Option<&'static str>,
) -> Element {
    // Unconditional, as hooks must be; the count is per-room memoized and
    // shown only when opted in.
    let current_room_unread = use_memo(count_unread_in_current_room);
    let unread = if current_room_badge.is_some() {
        current_room_unread()
    } else {
        0
    };
    rsx! {
        button {
            // `relative` anchors the unread badge overlay.
            class: "relative {class} rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
            "data-testid": testid,
            // The badge is visual-only, so the count lives in the label.
            "aria-label": if unread > 0 { format!("Back to chat, {unread} unread") } else { "Back to chat".to_string() },
            onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Chat),
            Icon { icon: FaArrowLeft, width: icon_size, height: icon_size }
            if let Some(badge_testid) = current_room_badge {
                CountBadge { count: unread, position: "-top-1.5 -right-2", testid: badge_testid }
            }
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
