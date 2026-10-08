//! The Latest control shared by the room history and the DM thread (10c): the
//! on-screen test for the newest item's bottom, the jump to the end, and the
//! button itself. Each caller keeps its own sentinel, observer and handler.

use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaChevronDown;
use dioxus_free_icons::Icon;

/// How far below the visible area (in px) the newest item's bottom may sit and
/// still count as on screen: fractional layout can leave a row that did scroll
/// fully into view a pixel short. Past it the Latest button shows (10c decision
/// 4), and the read rule uses the same edge (decision 5).
#[cfg(target_arch = "wasm32")]
pub const NEWEST_IN_VIEW_SLACK_PX: f64 = 4.0;

/// Is the sentinel `sentinel_id`, whose top edge is the newest item's bottom,
/// on screen in `container`, within [`NEWEST_IN_VIEW_SLACK_PX`]? The same edge,
/// and the same slack, as the Latest button's observer. A live layout read
/// rather than the observer's last report, which lags a patch that just put an
/// arrival below the fold. A container with no height has nothing on screen.
#[cfg(target_arch = "wasm32")]
pub fn sentinel_in_view(container: &web_sys::Element, sentinel_id: &str) -> bool {
    let Some(sentinel) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(sentinel_id))
    else {
        return false;
    };
    let view = container.get_bounding_client_rect();
    let edge = sentinel.get_bounding_client_rect().top();
    view.height() > 0.0 && edge >= view.top() && edge <= view.bottom() + NEWEST_IN_VIEW_SLACK_PX
}

/// Take `container` to its end at once. Never animated (decisions 6, 12): an
/// explicit request lands in the same task, so nothing can arrive or settle
/// mid-flight, and a smooth scroll would leave the end off screen while it ran.
#[cfg(target_arch = "wasm32")]
pub fn scroll_to_end(container: &web_sys::Element) {
    let opts = web_sys::ScrollToOptions::new();
    opts.set_top(container.scroll_height() as f64);
    opts.set_behavior(web_sys::ScrollBehavior::Instant);
    container.scroll_to_with_scroll_to_options(&opts);
}

/// The round Latest button in the corner of a scroll area. Instant and
/// one-shot: a later arrival does not extend it. The caller's observer hides
/// it once the end is in view; nothing hides it optimistically (#402).
#[component]
pub fn LatestButton(
    aria_label: &'static str,
    test_id: &'static str,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    rsx! {
        button {
            class: "absolute bottom-4 right-4 z-30 flex items-center justify-center w-10 h-10 rounded-full bg-panel shadow-lg border border-border text-text-muted hover:text-accent transition-colors",
            "aria-label": aria_label,
            "data-testid": test_id,
            onclick: move |evt| onclick.call(evt),
            Icon { icon: FaChevronDown, width: 18, height: 18 }
        }
    }
}
