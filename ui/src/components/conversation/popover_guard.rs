//! Document-level guards for the conversation's two popovers (the reaction picker and the action menu).
//! Raw JS listeners with no Dioxus scope: they make DOM calls and touch a thread-local, never a signal.

use std::cell::Cell;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use super::action_menu::ACTION_MENU_ID;
use super::reaction_picker::REACTION_PICKER_ID;

thread_local! {
    /// Set by a `pointerdown` that light-dismisses an open popover, so the `click` it becomes can be cancelled.
    static SWALLOW: Cell<bool> = const { Cell::new(false) };
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// The open popover and its id, if either is showing.
fn open_popover() -> Option<(web_sys::Element, &'static str)> {
    let doc = web_sys::window()?.document()?;
    [REACTION_PICKER_ID, ACTION_MENU_ID]
        .into_iter()
        .find_map(|id| {
            doc.get_element_by_id(id)
                .filter(|el| el.matches(":popover-open").unwrap_or(false))
                .map(|el| (el, id))
        })
}

/// The open popover, if `event` lands outside it and outside its own invokers (which toggle it themselves).
fn popover_left_by(event: &web_sys::Event) -> Option<web_sys::Element> {
    let (popover, id) = open_popover()?;
    let target: web_sys::Element = event.target()?.dyn_into().ok()?;
    let inside = target
        .closest(&format!("#{id}, [popovertarget=\"{id}\"]"))
        .ok()
        .flatten()
        .is_some();
    (!inside).then_some(popover)
}

fn listen(
    event: &str,
    options: &web_sys::AddEventListenerOptions,
    f: impl FnMut(web_sys::Event) + 'static,
) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let cb = Closure::<dyn FnMut(web_sys::Event)>::new(f);
    let _ = doc.add_event_listener_with_callback_and_add_event_listener_options(
        event,
        cb.as_ref().unchecked_ref(),
        options,
    );
    // `Conversation` mounts once; the listeners live for the page.
    cb.forget();
}

pub(super) fn install_popover_guards() {
    if INSTALLED.with(|i| i.replace(true)) {
        return;
    }
    let passive = web_sys::AddEventListenerOptions::new();
    passive.set_capture(true);
    passive.set_passive(true);
    let active = web_sys::AddEventListenerOptions::new();
    active.set_capture(true);
    active.set_passive(false);

    // Decided here: by the time the `click` fires, light dismiss has already closed the popover.
    listen("pointerdown", &passive, |e| {
        SWALLOW.with(|s| s.set(popover_left_by(&e).is_some()));
    });
    // A touch that turns into a scroll produces no click.
    listen("pointercancel", &passive, |_| {
        SWALLOW.with(|s| s.set(false))
    });
    // Capture on `document` runs before Dioxus's root listener. `detail > 0`: never a keyboard click.
    listen("click", &active, |e| {
        let detail = js_sys::Reflect::get(&e, &JsValue::from_str("detail"))
            .ok()
            .and_then(|d| d.as_f64())
            .unwrap_or(0.0);
        if SWALLOW.with(|s| s.replace(false)) && detail > 0.0 {
            e.prevent_default();
            e.stop_immediate_propagation();
        }
    });
}
