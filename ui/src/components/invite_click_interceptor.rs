//! Same-origin invite-URL click interceptor.
//!
//! Background — what was breaking (Ivvor's "room invites in DM seem to
//! lock up most of the river UI" report, 2026-05-16):
//!
//! - `message_to_html` linkifies invite URLs in both room messages and DM
//!   bodies, then `finalize_anchors` rewrites the gateway URL to a
//!   same-origin path (`/v1/contract/web/<id>/?invitation=...`) and adds
//!   `target="_blank" rel="noopener noreferrer"`.
//! - The River UI runs inside a gateway iframe sandboxed
//!   `allow-scripts allow-forms allow-popups` (NO
//!   `allow-popups-to-escape-sandbox`, NO `allow-top-navigation`). With
//!   those flags, browsers SUPPRESS `target="_blank"` popups on
//!   same-origin links and fall back to navigating the iframe in place.
//! - In-place navigation re-mounts `App`, restarts the synchronizer,
//!   re-hydrates `ROOMS` from the delegate, and only THEN renders the
//!   `ReceiveInvitationModal` in its "Preparing to subscribe…" state.
//!   To the user it looks like the UI froze for several seconds and
//!   their open DM thread / draft is gone.
//!
//! Fix — intercept the click before the browser navigates. If the
//! anchor's href contains `?invitation=`, extract the code, set
//! [`INTERCEPTED_INVITATION_CODE`], and `preventDefault()` so the
//! iframe stays put. `App` watches the global signal and routes the
//! code through the same `Invitation::from_encoded_string` →
//! `receive_invitation` → `ReceiveInvitationModal` path the URL-bar
//! entry flow uses. The current room / open DM / draft text are
//! preserved because the iframe never reloads.
//!
//! Scope: only intercepts clicks on anchors whose `href` contains
//! `?invitation=`. Non-invite anchors (the `target="_blank"` Freenet
//! web URLs we already shorten, freenet.org, etc.) are untouched.

#[cfg(target_arch = "wasm32")]
use dioxus::logger::tracing::warn;
use dioxus::prelude::*;
#[cfg(target_arch = "wasm32")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;

/// Set by the JS click listener when it intercepts an invite-URL anchor.
/// Carries the raw `<invitation_code>` string (whatever was after
/// `?invitation=`, with any URL fragment stripped). `App` watches this
/// and clears it after consumption.
pub static INTERCEPTED_INVITATION_CODE: GlobalSignal<Option<String>> = Global::new(|| None);

#[cfg(target_arch = "wasm32")]
static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Native (mobile/desktop) builds have no DOM document to attach a click
/// listener to. Invite links are instead handled through the in-app flows;
/// this is a no-op so `App` can call it unconditionally.
#[cfg(not(target_arch = "wasm32"))]
pub fn install_invite_click_interceptor() {}

/// Install a document-level `click` listener that intercepts in-page
/// anchor clicks pointing at invite URLs and routes them through the
/// in-app receive-invitation flow instead of letting the browser
/// navigate the iframe.
///
/// Safe to call multiple times — the listener is installed once per page
/// load and ignored thereafter.
#[cfg(target_arch = "wasm32")]
pub fn install_invite_click_interceptor() {
    if HANDLER_INSTALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let cb = Closure::wrap(Box::new(move |evt: web_sys::Event| {
        // Skeptical-review (#260 P2): don't cancel modifier / non-left
        // clicks. Middle-click and Ctrl/Cmd-click should keep the
        // browser's "open in new tab" UX. Plain left click is the only
        // case the in-iframe fallback navigation hurts.
        if let Some(me) = evt.dyn_ref::<web_sys::MouseEvent>() {
            if me.button() != 0 || me.ctrl_key() || me.meta_key() || me.shift_key() || me.alt_key()
            {
                return;
            }
        }

        let Some(target) = evt.target() else { return };
        // Walk up the DOM looking for an <a>. Use Node→Element coercion
        // since the click target might be a text/span inside the anchor.
        let mut node = target.dyn_into::<web_sys::Element>().ok();
        while let Some(el) = node {
            if el.tag_name().eq_ignore_ascii_case("a") {
                let Ok(anchor) = el.dyn_into::<web_sys::HtmlAnchorElement>() else {
                    return;
                };
                // Use `href` (resolved against base) rather than
                // `get_attribute("href")` so relative URLs (`/v1/...`)
                // and absolute URLs both surface the query string the
                // same way.
                let href = anchor.href();

                // Skeptical-review (#260 P1): only intercept SAME-ORIGIN
                // invite URLs. A foreign-gateway invite link
                // (e.g. `https://other-gw.example/v1/.../?invitation=...`)
                // should be left alone so the user can open it in a new
                // tab (which works in the sandbox via the gateway shell
                // for cross-origin destinations). If we intercepted
                // those, the modal would either fail to parse the code
                // or get stuck "preparing to subscribe" against a
                // contract this gateway doesn't host.
                let same_origin = crate::platform::window()
                    .and_then(|w| w.location().origin().ok())
                    .map(|origin| href.starts_with(&origin) || href.starts_with('/'))
                    .unwrap_or(false);
                if !same_origin {
                    return;
                }

                let Some(q_start) = href.find("?invitation=") else {
                    return;
                };
                let code_with_tail = &href[q_start + "?invitation=".len()..];
                // Strip URL fragment if any. We don't expect `&` in
                // invite URLs but split on it too, defensively.
                let code = code_with_tail
                    .split(['#', '&'])
                    .next()
                    .unwrap_or(code_with_tail)
                    .to_string();
                if code.is_empty() {
                    return;
                }
                evt.prevent_default();
                evt.stop_propagation();
                // `defer` so the signal write happens off the JS event
                // tick — same pattern the rest of the UI uses for
                // signal mutations from JS callbacks (see
                // `defer()` in `util.rs`).
                crate::util::defer(move || {
                    *INTERCEPTED_INVITATION_CODE.write() = Some(code);
                });
                return;
            }
            node = el.parent_element();
        }
    }) as Box<dyn FnMut(web_sys::Event)>);

    if let Some(doc) = crate::platform::window().and_then(|w| w.document()) {
        if let Err(e) = doc.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()) {
            warn!("invite click interceptor: addEventListener failed: {:?}", e);
            HANDLER_INSTALLED.store(false, Ordering::SeqCst);
            return;
        }
        // Leak the closure intentionally — the listener lives for the
        // lifetime of the page.
        cb.forget();
    }
}
