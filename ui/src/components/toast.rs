//! One app-wide info toast: top-centre, above the ordinary modals, gone after
//! [`TOAST_LIFETIME_MS`], on its action, or on Dismiss.
//!
//! There is one visible toast and no queue: showing a toast replaces the
//! visible one. Every toast gets a fresh id, and every way of closing it
//! (expiry, action, Dismiss) closes only that id, so a stale timer or a late
//! click never closes its replacement.
//!
//! Info only. A toast's action is a shortcut, never the only way to do the
//! thing: archive Undo has the Archived panel's Un-archive. The toast does not
//! pause on hover or focus and never takes focus. Its expiry or dismissal is
//! not a save receipt and gives no permission to reload; the generation-update
//! banner keeps its own lifecycle and is never routed through here.

use crate::components::icons::line_icon;
use dioxus::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

/// How long a toast stays up when nothing closes it first.
pub(crate) const TOAST_LIFETIME_MS: u64 = 5_000;

/// The host's DOM id, for the banner-clearance measurement.
const HOST_ID: &str = "app-toast-host";

/// Space left between the banner and a toast moved below it.
#[cfg(any(target_arch = "wasm32", test))]
const BANNER_GAP_PX: f64 = 8.0;

const CLOSE_ICON_PATH: &str = "M18 6 6 18M6 6l12 12";

/// An action's callback, taken out on its first run.
type RunOnce = Rc<RefCell<Option<Box<dyn FnOnce()>>>>;

/// The one button a toast may carry. It runs at most once, from the click
/// handler, and closes the toast. Anything it writes to global state it
/// defers itself (`.claude/rules/dioxus-signal-safety.md`).
#[derive(Clone)]
pub struct ToastAction {
    label: String,
    run: RunOnce,
}

impl ToastAction {
    pub fn new(label: impl Into<String>, run: impl FnOnce() + 'static) -> Self {
        Self {
            label: label.into(),
            run: Rc::new(RefCell::new(Some(Box::new(run)))),
        }
    }

    fn take_and_run(&self) {
        let run = self.run.borrow_mut().take();
        if let Some(run) = run {
            run();
        }
    }
}

impl PartialEq for ToastAction {
    fn eq(&self, other: &Self) -> bool {
        self.label == other.label && Rc::ptr_eq(&self.run, &other.run)
    }
}

impl std::fmt::Debug for ToastAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToastAction")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Toast {
    id: u64,
    message: String,
    action: Option<ToastAction>,
}

/// The visible toast, and the id the next one gets.
#[derive(Default)]
pub(crate) struct ToastSlot {
    last_id: u64,
    current: Option<Toast>,
}

impl ToastSlot {
    /// Replace whatever is showing; returns the new toast's id.
    fn show(&mut self, message: String, action: Option<ToastAction>) -> u64 {
        self.last_id += 1;
        self.current = Some(Toast {
            id: self.last_id,
            message,
            action,
        });
        self.last_id
    }

    /// Close the visible toast only if it is still `id`.
    fn close(&mut self, id: u64) -> bool {
        if self.current.as_ref().is_some_and(|t| t.id == id) {
            self.current = None;
            true
        } else {
            false
        }
    }
}

static TOAST: GlobalSignal<ToastSlot> = Global::new(ToastSlot::default);

/// Show `message`, replacing any visible toast, and close it after
/// [`TOAST_LIFETIME_MS`] unless something closed it first.
///
/// The id is allocated inside the deferred write, so two calls in one handler
/// cannot read the same slot and both believe they are current.
pub fn show_toast(message: impl Into<String>, action: Option<ToastAction>) {
    let message = message.into();
    crate::util::defer(move || {
        let id = TOAST.with_mut(|slot| slot.show(message, action));
        crate::util::safe_spawn_local(async move {
            crate::util::sleep(crate::util::millis(TOAST_LIFETIME_MS)).await;
            close_toast(id);
        });
    });
}

/// Close toast `id` if it is still the visible one. Safe from any handler or
/// future: the write is deferred.
pub fn close_toast(id: u64) {
    crate::util::defer(move || {
        TOAST.with_mut(|slot| slot.close(id));
    });
}

/// Mounted once in `App`, after the ordinary modals and the update banner.
///
/// The host is always in the DOM and is the polite status region, so a new
/// toast is announced; the card inside is keyed by id, so its entrance plays
/// for a new toast only.
#[component]
pub fn ToastHost() -> Element {
    // The last toast read cleanly: a contended read keeps it on screen rather
    // than unmounting the card (and replaying its entrance when it comes back).
    let last_good = use_hook(|| Rc::new(RefCell::new(None::<Toast>)));

    crate::util::signal_guard::anchor();
    let toast = match TOAST.try_read() {
        Ok(slot) => {
            let toast = slot.current.clone();
            last_good.borrow_mut().clone_from(&toast);
            toast
        }
        Err(_) => {
            crate::util::signal_guard::schedule_nudge();
            last_good.borrow().clone()
        }
    };

    #[cfg(target_arch = "wasm32")]
    watch_banner_clearance();

    rsx! {
        div {
            id: HOST_ID,
            class: "app-toast-host",
            role: "status",
            "aria-live": "polite",
            "data-testid": "toast-host",
            if let Some(toast) = toast {
                ToastCard { key: "{toast.id}", toast }
            }
        }
    }
}

#[component]
fn ToastCard(toast: Toast) -> Element {
    let id = toast.id;
    let action = toast.action.clone();
    rsx! {
        div { class: "app-toast", "data-testid": "toast",
            span { class: "app-toast-message", "data-testid": "toast-message", "{toast.message}" }
            if let Some(action) = action {
                button {
                    r#type: "button",
                    class: "app-toast-action",
                    "data-testid": "toast-action",
                    onclick: {
                        let action = action.clone();
                        move |_| {
                            action.take_and_run();
                            close_toast(id);
                        }
                    },
                    "{action.label}"
                }
            }
            button {
                r#type: "button",
                class: "app-toast-dismiss",
                "data-testid": "toast-dismiss",
                "aria-label": "Dismiss notification",
                title: "Dismiss",
                onclick: move |_| close_toast(id),
                {line_icon(CLOSE_ICON_PATH, 14)}
            }
        }
    }
}

/// Where the toast's top goes when, at its own place, it would reach the
/// update banner: just below the banner. `None` keeps its own place.
#[cfg(any(target_arch = "wasm32", test))]
fn top_clear_of_banner(
    toast_top: f64,
    toast_bottom: f64,
    banner_top: f64,
    banner_bottom: f64,
) -> Option<f64> {
    let overlaps = toast_bottom > banner_top && toast_top < banner_bottom;
    overlaps.then_some(banner_bottom + BANNER_GAP_PX)
}

/// Keep a wrapped toast clear of the update banner, from both elements'
/// measured sizes. Re-measures when the toast or the banner changes, and when
/// either resizes (wrapping, the countdown's text, the window); the clearance
/// goes when the banner does.
#[cfg(target_arch = "wasm32")]
fn watch_banner_clearance() {
    use crate::components::app::freenet_api::generation_watch::UPDATE_NOTICE;
    use wasm_bindgen::prelude::*;

    /// The observer, its callback, and the (toast id, banner shown) it was
    /// set up for.
    type Watch = (
        web_sys::ResizeObserver,
        Closure<dyn FnMut(js_sys::Array)>,
        (Option<u64>, bool),
    );
    let watch = use_hook(|| Rc::new(RefCell::new(None::<Watch>)));
    {
        let watch = watch.clone();
        use_drop(move || {
            if let Some((observer, _, _)) = watch.borrow_mut().take() {
                observer.disconnect();
            }
        });
    }
    use_effect(move || {
        crate::util::signal_guard::anchor();
        // The effect also re-runs on every countdown tick and nudge; it sets up
        // again only when a toast or the banner comes or goes. The observer
        // covers everything in between.
        let Ok(toast) = TOAST
            .try_read()
            .map(|slot| slot.current.as_ref().map(|t| t.id))
        else {
            crate::util::signal_guard::schedule_nudge();
            return;
        };
        let Ok(banner) = UPDATE_NOTICE.try_read().map(|n| n.is_some()) else {
            crate::util::signal_guard::schedule_nudge();
            return;
        };
        let key = (toast, banner);
        if watch
            .borrow()
            .as_ref()
            .is_some_and(|(_, _, seen)| *seen == key)
        {
            return;
        }
        if let Some((observer, _, _)) = watch.borrow_mut().take() {
            observer.disconnect();
        }
        place_clear_of_banner();
        let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };
        let cb = Closure::wrap(Box::new(move |_: js_sys::Array| place_clear_of_banner())
            as Box<dyn FnMut(js_sys::Array)>);
        if let Ok(observer) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            for el in [doc.get_element_by_id(HOST_ID), update_banner(&doc)]
                .into_iter()
                .flatten()
            {
                observer.observe(&el);
            }
            *watch.borrow_mut() = Some((observer, cb, key));
        }
    });
}

#[cfg(target_arch = "wasm32")]
fn update_banner(doc: &web_sys::Document) -> Option<web_sys::Element> {
    doc.query_selector("[data-testid='update-banner']")
        .ok()
        .flatten()
}

/// Put the host at its own place, then below the banner if it reaches it.
/// The host has no transform (the card's entrance animates the card), so its
/// box is the toast's laid-out box even mid-entrance. One synchronous pass, so
/// nothing paints in between.
#[cfg(target_arch = "wasm32")]
fn place_clear_of_banner() {
    use wasm_bindgen::JsCast;

    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(host) = doc
        .get_element_by_id(HOST_ID)
        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        return;
    };
    let style = host.style();
    let _ = style.remove_property("top");
    let Some(banner) = update_banner(&doc) else {
        return;
    };
    if host.child_element_count() == 0 {
        return;
    }
    let toast = host.get_bounding_client_rect();
    let banner = banner.get_bounding_client_rect();
    if let Some(top) =
        top_clear_of_banner(toast.top(), toast.bottom(), banner.top(), banner.bottom())
    {
        let _ = style.set_property("top", &format!("{top}px"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_toast_cannot_close_its_replacement() {
        let mut slot = ToastSlot::default();
        let a = slot.show("A".into(), None);
        let b = slot.show("B".into(), None);

        assert!(!slot.close(a), "A's timer or click must not close B");
        assert_eq!(slot.current.as_ref().map(|t| t.message.as_str()), Some("B"));

        assert!(slot.close(b));
        assert!(slot.current.is_none());
    }

    #[test]
    fn closing_twice_is_a_no_op() {
        let mut slot = ToastSlot::default();
        let a = slot.show("A".into(), None);
        assert!(slot.close(a));
        assert!(!slot.close(a));
    }

    #[test]
    fn showing_the_same_message_again_is_a_new_toast() {
        let mut slot = ToastSlot::default();
        let a = slot.show("Same".into(), None);
        let b = slot.show("Same".into(), None);
        assert_ne!(a, b, "the second toast's timer must not be the first's");
        assert!(!slot.close(a));
        assert!(slot.current.is_some());
    }

    #[test]
    fn an_action_runs_once() {
        let runs = Rc::new(std::cell::Cell::new(0));
        let action = ToastAction::new("Undo", {
            let runs = runs.clone();
            move || runs.set(runs.get() + 1)
        });
        action.take_and_run();
        action.clone().take_and_run();
        assert_eq!(runs.get(), 1, "a double click must not undo twice");
    }

    #[test]
    fn a_toast_clear_of_the_banner_keeps_its_place() {
        // Toast 16..60, banner from 72; touching is not reaching.
        assert_eq!(top_clear_of_banner(16.0, 60.0, 72.0, 140.0), None);
        assert_eq!(top_clear_of_banner(16.0, 72.0, 72.0, 140.0), None);
    }

    #[test]
    fn a_toast_reaching_the_banner_moves_below_it() {
        // A wrapped toast 16..90 against a banner 72..140.
        assert_eq!(
            top_clear_of_banner(16.0, 90.0, 72.0, 140.0),
            Some(140.0 + BANNER_GAP_PX)
        );
        // One pixel into it is enough.
        assert_eq!(
            top_clear_of_banner(16.0, 73.0, 72.0, 140.0),
            Some(140.0 + BANNER_GAP_PX)
        );
    }
}
