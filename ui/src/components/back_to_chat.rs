//! The mobile back arrow that returns to the chat from the rooms or members
//! panel, and the count badge the hamburger buttons share.

use crate::components::app::document_title::{count_unread_in_current_room, CurrentRoomUnread};
use crate::components::app::{MobileView, MOBILE_VIEW};
use crate::components::room_list::unread_badge_label;
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
    match current_room_badge {
        Some(badge_testid) => rsx! {
            BackToChatWithUnread { class, icon_size, testid, badge_testid }
        },
        None => back_to_chat(class, icon_size, testid, None),
    }
}

/// The opted-in arrow. A component of its own so only it counts: the rooms
/// panel's arrow never shows the count, so it never computes it.
#[component]
fn BackToChatWithUnread(
    class: &'static str,
    icon_size: u32,
    testid: &'static str,
    badge_testid: &'static str,
) -> Element {
    let unread = use_memo(count_unread_in_current_room);
    back_to_chat(class, icon_size, testid, Some((unread(), badge_testid)))
}

fn back_to_chat(
    class: &'static str,
    icon_size: u32,
    testid: &'static str,
    badge: Option<(CurrentRoomUnread, &'static str)>,
) -> Element {
    // The badge is visual-only, so its count lives in the label, worded as
    // the room's row words it.
    let unread_label = badge
        .filter(|(unread, _)| unread.count > 0)
        .map(|(unread, _)| unread_badge_label(unread.count, unread.is_mentions));
    let aria_label = match &unread_label {
        Some(label) => format!("Back to chat, {label}"),
        None => "Back to chat".to_string(),
    };
    rsx! {
        button {
            // `relative` anchors the unread badge overlay.
            class: "relative {class} rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
            "data-testid": testid,
            "aria-label": aria_label,
            title: unread_label,
            onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Chat),
            Icon { icon: FaArrowLeft, width: icon_size, height: icon_size }
            if let Some((unread, badge_testid)) = badge {
                CountBadge { count: unread.count, position: "-top-1.5 -right-2", testid: badge_testid }
            }
        }
    }
}

/// A visual-only count on a `relative` button's corner, absent at zero. The
/// accent text colour as its fill and the panel colour as its text, so it
/// reads in both themes (a white count on the light dark-theme accent would
/// not).
#[component]
pub fn CountBadge(count: usize, position: &'static str, testid: &'static str) -> Element {
    rsx! {
        if count > 0 {
            span {
                class: "absolute {position} flex items-center justify-center min-w-4 h-4 px-1 rounded-full bg-[var(--color-link)] text-[var(--color-panel)] text-[10px] font-semibold leading-none pointer-events-none",
                "data-testid": testid,
                "aria-hidden": "true",
                "{count}"
            }
        }
    }
}
