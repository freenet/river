//! Document-level click interceptor for `@mention` chips.
//!
//! Mention chips are rendered as `<span class="river-mention"
//! data-member-id="<hex>">@Name</span>` inside message bodies that are injected
//! via `dangerous_inner_html` (see `conversation::message_to_html_with_mentions`).
//! Because that markup is raw HTML, Dioxus can't attach an `onclick` to the
//! chip directly. Instead we install one document-level `click` listener that
//! walks up from the click target to the nearest `[data-member-id]` element and
//! opens the member-info modal — the same delegation pattern used by
//! [`crate::components::invite_click_interceptor`] for invite anchors.

use crate::components::app::MEMBER_INFO_MODAL;
use dioxus::logger::tracing::warn;
use std::sync::atomic::{AtomicBool, Ordering};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Install a document-level `click` listener that opens the member-info modal
/// when a mention chip is clicked. Safe to call multiple times — installed once
/// per page load and ignored thereafter.
pub fn install_mention_click_interceptor() {
    if HANDLER_INSTALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let cb = Closure::wrap(Box::new(move |evt: web_sys::Event| {
        // Only plain left-clicks. Leave modifier clicks alone (consistent with
        // the invite interceptor); a chip has no navigation behaviour to
        // preserve, but we still don't want to hijack e.g. Ctrl-click.
        if let Some(me) = evt.dyn_ref::<web_sys::MouseEvent>() {
            if me.button() != 0 || me.ctrl_key() || me.meta_key() || me.shift_key() || me.alt_key()
            {
                return;
            }
        }

        let Some(target) = evt.target() else { return };
        // The click target may be a text node inside the span; walk up.
        let mut node = target.dyn_into::<web_sys::Element>().ok();
        while let Some(el) = node {
            if let Some(hex) = el.get_attribute("data-member-id") {
                if let Some(member_id) = river_core::mention::member_id_from_hex(&hex) {
                    evt.prevent_default();
                    evt.stop_propagation();
                    // Defer the signal write off the JS event tick — same
                    // pattern the rest of the UI uses for signal mutations
                    // from JS callbacks (see `defer()` in `util.rs`).
                    crate::util::defer(move || {
                        MEMBER_INFO_MODAL.with_mut(|signal| {
                            signal.member = Some(member_id);
                        });
                    });
                }
                return;
            }
            node = el.parent_element();
        }
    }) as Box<dyn FnMut(web_sys::Event)>);

    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
        if let Err(e) = doc.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()) {
            warn!(
                "mention click interceptor: addEventListener failed: {:?}",
                e
            );
            HANDLER_INSTALLED.store(false, Ordering::SeqCst);
            return;
        }
        // Leak the closure intentionally — the listener lives for the lifetime
        // of the page.
        cb.forget();
    }
}
