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

/// How far below the visible area (in px) the room's newest message may sit
/// when an arrival lands and still be followed to the end. The pre-#753
/// `BOTTOM_THRESHOLD_PX`, now measured from the newest message's bottom rather
/// than the scroller's absolute end (`.claude/rules/history-scrolling.md`).
#[cfg(target_arch = "wasm32")]
pub const ROOM_FOLLOW_BAND_PX: f64 = 100.0;

/// The DM thread's [`ROOM_FOLLOW_BAND_PX`]: the pre-#753 `is_near_bottom`
/// tolerance.
#[cfg(target_arch = "wasm32")]
pub const DM_FOLLOW_BAND_PX: f64 = 50.0;

/// Did a newer newest item arrive? Only a strictly greater key counts: an
/// edit, a deletion of the newest and a re-render do not. `first_counts`
/// says whether a first item after none is one: a DM thread opened empty
/// follows its first DMs, but a room's first render is not an arrival.
pub fn is_arrival<K: Ord>(previous: Option<&K>, now: Option<&K>, first_counts: bool) -> bool {
    match (previous, now) {
        (Some(previous), Some(now)) => now > previous,
        (None, Some(_)) => first_counts,
        _ => false,
    }
}

/// A scrolling history whose newest item's bottom is the top edge of a
/// sentinel: the room history or the DM thread.
#[cfg(target_arch = "wasm32")]
pub struct HistoryDom {
    pub container_id: &'static str,
    pub sentinel_id: &'static str,
    /// The modal the history lives in, which does not cover it.
    pub modal_except: Option<&'static str>,
    /// How far below the view the newest item's bottom may sit, before the
    /// patch, for an arrival to follow.
    pub band: f64,
}

#[cfg(target_arch = "wasm32")]
impl HistoryDom {
    pub fn container(&self) -> Option<web_sys::Element> {
        element_by_id(self.container_id)
    }

    pub fn sentinel(&self) -> Option<web_sys::Element> {
        element_by_id(self.sentinel_id)
    }

    /// Does an arrival rendering now follow? Only into a history in the
    /// foreground whose newest item's bottom sat at most `band` px below the
    /// view. Called from render, before the patch.
    pub fn arrival_follows(&self) -> bool {
        self.container().is_some_and(|container| {
            crate::components::foreground::in_foreground(self.modal_except, &container)
                && self.newest_bottom_below_view(&container) <= self.band
        })
    }

    /// Is the history in the foreground with its newest item's bottom on
    /// screen, within [`NEWEST_IN_VIEW_SLACK_PX`]? The same edge, and the same
    /// slack, as the Latest button's observer. A live layout read rather than
    /// the observer's last report, which lags a patch that just put an arrival
    /// below the fold.
    pub fn end_seen(&self) -> bool {
        self.container().is_some_and(|container| {
            crate::components::foreground::in_foreground(self.modal_except, &container)
                && self.sentinel().is_some_and(|sentinel| {
                    let view = container.get_bounding_client_rect();
                    let edge = sentinel.get_bounding_client_rect().top();
                    view.height() > 0.0
                        && edge >= view.top()
                        && edge <= view.bottom() + NEWEST_IN_VIEW_SLACK_PX
                })
        })
    }

    /// How far (px) the newest item's bottom sits below `container`'s visible
    /// bottom: 0 while it is on screen, infinite with no sentinel or no layout.
    fn newest_bottom_below_view(&self, container: &web_sys::Element) -> f64 {
        let Some(sentinel) = self.sentinel() else {
            return f64::INFINITY;
        };
        let view = container.get_bounding_client_rect();
        if view.height() <= 0.0 {
            return f64::INFINITY;
        }
        (sentinel.get_bounding_client_rect().top() - view.bottom()).max(0.0)
    }
}

#[cfg(target_arch = "wasm32")]
fn element_by_id(id: &str) -> Option<web_sys::Element> {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
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
fn observe_sentinel(
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

/// Watch `dom`'s sentinel and publish whether the newest item's bottom is on
/// screen to `in_view`, calling `on_report` with each report first. The write
/// is deferred, and skipped once the signal has gone with its component or
/// would not change. `None` if either element is missing or the browser
/// refused the observer.
#[cfg(target_arch = "wasm32")]
pub fn observe_newest(
    dom: &HistoryDom,
    in_view: Signal<bool>,
    mut on_report: impl FnMut(bool) + 'static,
) -> Option<SentinelObserver> {
    let (root, sentinel) = (dom.container()?, dom.sentinel()?);
    observe_sentinel(&root, &sentinel, move |now| {
        on_report(now);
        crate::util::defer(move || {
            let mut in_view = in_view;
            if in_view.try_peek().is_ok_and(|v| *v != now) {
                if let Ok(mut v) = in_view.try_write() {
                    *v = now;
                }
            }
        });
    })
}

/// Take `container` to its end at once. Never animated
/// (`.claude/rules/history-scrolling.md`): an explicit request lands in the
/// same task, so nothing can arrive or settle mid-flight, and a smooth scroll
/// would leave the end off screen while it ran.
#[cfg(target_arch = "wasm32")]
pub fn scroll_to_end(container: &web_sys::Element) {
    let opts = web_sys::ScrollToOptions::new();
    opts.set_top(container.scroll_height() as f64);
    opts.set_behavior(web_sys::ScrollBehavior::Instant);
    container.scroll_to_with_scroll_to_options(&opts);
}

/// The round Latest button in the corner of a scroll area. Instant and
/// one-shot: it leaves nothing behind, and a later arrival follows only by the
/// ordinary rule (`HistoryDom::arrival_follows`). The caller's observer hides
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_strictly_newer_newest_item_is_an_arrival() {
        for first_counts in [false, true] {
            assert!(is_arrival(Some(&1), Some(&2), first_counts), "a later item");
            assert!(
                !is_arrival(Some(&1), Some(&1), first_counts),
                "an edit or a re-render"
            );
            assert!(
                !is_arrival(Some(&2), Some(&1), first_counts),
                "the newest deleted"
            );
            assert!(
                !is_arrival(Some(&1), None, first_counts),
                "an emptied history"
            );
            assert!(!is_arrival(None::<&i32>, None, first_counts));
            assert_eq!(
                is_arrival(None, Some(&1), first_counts),
                first_counts,
                "a first item"
            );
        }
    }
}
