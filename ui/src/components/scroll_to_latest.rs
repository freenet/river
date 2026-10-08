//! The Latest control shared by the room history and the DM thread: the
//! sentinel's observer, the on-screen test for the newest item's bottom, the
//! jump to the end, and the button itself. Each caller keeps its own sentinel
//! and handler.

use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaChevronDown;
use dioxus_free_icons::Icon;

/// How far below the visible area (in px) the newest item's bottom may sit and
/// still count as on screen: fractional layout can leave a row that did scroll
/// fully into view a pixel short. Past it the Latest button shows, and the
/// read rule uses the same edge.
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

/// An IntersectionObserver on one sentinel, with the callback it calls.
/// Dropping it disconnects the observer before the callback goes.
#[cfg(target_arch = "wasm32")]
pub struct SentinelObserver {
    observer: web_sys::IntersectionObserver,
    _callback: wasm_bindgen::closure::Closure<dyn FnMut(js_sys::Array)>,
}

#[cfg(target_arch = "wasm32")]
impl Drop for SentinelObserver {
    fn drop(&mut self) {
        self.observer.disconnect();
    }
}

/// Watch `sentinel`, whose top edge is the newest item's bottom, and call
/// `on_change` with whether it is on screen in `root`, within
/// [`NEWEST_IN_VIEW_SLACK_PX`]. The observer reports only on a change, so
/// scrolling does no DOM queries (#151). `on_change` runs from a raw JS
/// callback with no Dioxus scope, so it must `defer` any signal write
/// (.claude/rules/dioxus-signal-safety.md). `None` if the browser refused the
/// observer.
#[cfg(target_arch = "wasm32")]
pub fn observe_sentinel(
    root: &web_sys::Element,
    sentinel: &web_sys::Element,
    mut on_change: impl FnMut(bool) + 'static,
) -> Option<SentinelObserver> {
    use wasm_bindgen::JsCast;

    let callback = wasm_bindgen::closure::Closure::<dyn FnMut(js_sys::Array)>::new(
        move |entries: js_sys::Array| {
            // Several reports can queue between callbacks; the last is current.
            if let Some(entry) = entries
                .iter()
                .last()
                .and_then(|e| e.dyn_into::<web_sys::IntersectionObserverEntry>().ok())
            {
                on_change(entry.is_intersecting());
            }
        },
    );
    let options = web_sys::IntersectionObserverInit::new();
    options.set_root(Some(root));
    // Only fractional-layout slack below the viewport edge: a newest item
    // whose bottom is any further down offers Latest.
    options.set_root_margin(&format!("0px 0px {NEWEST_IN_VIEW_SLACK_PX}px 0px"));
    options.set_threshold(&wasm_bindgen::JsValue::from_f64(0.0));
    let observer = web_sys::IntersectionObserver::new_with_options(
        callback.as_ref().unchecked_ref(),
        &options,
    )
    .ok()?;
    observer.observe(sentinel);
    Some(SentinelObserver {
        observer,
        _callback: callback,
    })
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
