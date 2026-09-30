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
//! Scope: only intercepts clicks on anchors whose `href` is an invite URL
//! for THIS River: same origin, same `/v1/contract/web/<id>` as the page,
//! and `?invitation=` in the query (see [`invitation_code_to_intercept`]).
//! Everything else (Freenet web URLs for other apps, including converted
//! share links that happen to carry `?invitation=`, freenet.org, etc.) is
//! untouched.

use dioxus::logger::tracing::warn;
use dioxus::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// Set by the JS click listener when it intercepts an invite-URL anchor.
/// Carries the raw `<invitation_code>` string (whatever was after
/// `?invitation=`, with any URL fragment stripped). `App` watches this
/// and clears it after consumption.
pub static INTERCEPTED_INVITATION_CODE: GlobalSignal<Option<String>> = Global::new(|| None);

static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Install a document-level `click` listener that intercepts in-page
/// anchor clicks pointing at invite URLs and routes them through the
/// in-app receive-invitation flow instead of letting the browser
/// navigate the iframe.
///
/// Safe to call multiple times — the listener is installed once per page
/// load and ignored thereafter.
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

                // Skeptical-review (#260 P1): only intercept invite URLs for
                // THIS River (same origin AND same web-container contract).
                // A foreign-gateway invite link, or a `?invitation=` link to
                // some other Freenet app, is left alone so it opens in a new
                // tab (which escapes the sandbox via the gateway shell). If
                // we intercepted those, the modal would either fail to parse
                // the code or get stuck "preparing to subscribe" against a
                // contract this gateway doesn't host, and a share link naming
                // app A would open River's invite dialog instead of A.
                let Some(location) = web_sys::window().map(|w| w.location()) else {
                    return;
                };
                let (Ok(origin), Ok(pathname)) = (location.origin(), location.pathname()) else {
                    return;
                };
                let Some(code) = invitation_code_to_intercept(&href, &origin, &pathname) else {
                    return;
                };
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

    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
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

/// The invitation code to handle in-app for a click on `href`, or `None` to
/// let the browser follow the link.
///
/// `origin` and `pathname` are the page's own (`window.location`). The link is
/// intercepted only when it points at this same River: the same origin and the
/// web-container contract id as the page (under `/v1/` or `/v2/`), with
/// `?invitation=<code>` in its query (not merely somewhere in its fragment).
/// A `?invitation=` link to any other contract is some other app's business.
pub(crate) fn invitation_code_to_intercept(
    href: &str,
    origin: &str,
    pathname: &str,
) -> Option<String> {
    // The gateway serves webapps under `/v1/` and `/v2/`. Either route on
    // either side names the same River, so match the contract id, not the
    // route (a v1 invite clicked on a v2 page is still ours).
    const MARKERS: [&str; 2] = ["/v1/contract/web/", "/v2/contract/web/"];
    let strip_marker = |path: &'_ str| -> Option<String> {
        MARKERS
            .iter()
            .find_map(|m| path.strip_prefix(m))
            .map(str::to_string)
    };
    let page_rest = strip_marker(pathname)?;
    let own_id = page_rest.split('/').next()?;
    if own_id.is_empty() {
        return None;
    }
    let href_rest = strip_marker(href.strip_prefix(origin)?)?;
    let after_prefix = href_rest.strip_prefix(own_id)?;
    // The id must end here, not merely share a prefix with a longer one.
    if !after_prefix.starts_with(['/', '?']) {
        return None;
    }
    let before_fragment = after_prefix.split('#').next().unwrap_or("");
    let q_start = before_fragment.find("?invitation=")?;
    // We don't expect `&` in invite URLs but split on it, defensively.
    let code = before_fragment[q_start + "?invitation=".len()..]
        .split('&')
        .next()
        .unwrap_or("");
    if code.is_empty() {
        return None;
    }
    Some(code.to_string())
}

#[cfg(test)]
mod tests {
    use super::invitation_code_to_intercept;

    const ORIGIN: &str = "http://127.0.0.1:7509";
    const RIVER: &str = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";
    const OTHER: &str = "6FzSeAUKcqJrveKyU8RJgGKc5jRB1Z2juvxXtwTA4Em9";

    fn page() -> String {
        format!("/v1/contract/web/{RIVER}/")
    }

    #[test]
    fn own_river_invite_is_intercepted() {
        for href in [
            format!("{ORIGIN}/v1/contract/web/{RIVER}/?invitation=abc"),
            format!("{ORIGIN}/v1/contract/web/{RIVER}/?invitation=abc#frag"),
            format!("{ORIGIN}/v1/contract/web/{RIVER}/?invitation=abc&x=y"),
            format!("{ORIGIN}/v1/contract/web/{RIVER}?invitation=abc"),
        ] {
            assert_eq!(
                invitation_code_to_intercept(&href, ORIGIN, &page()).as_deref(),
                Some("abc"),
                "{href}"
            );
        }
    }

    #[test]
    fn other_apps_and_origins_are_not_intercepted() {
        for href in [
            // Another contract's `?invitation=` (e.g. a converted share link).
            format!("{ORIGIN}/v1/contract/web/{OTHER}/?invitation=abc"),
            // A longer id that merely starts with River's.
            format!("{ORIGIN}/v1/contract/web/{RIVER}x/?invitation=abc"),
            // Another gateway.
            format!("https://gw.example/v1/contract/web/{RIVER}/?invitation=abc"),
            // Only in the fragment, which the page's router owns.
            format!("{ORIGIN}/v1/contract/web/{RIVER}/#?invitation=abc"),
            // No code.
            format!("{ORIGIN}/v1/contract/web/{RIVER}/?invitation="),
            // Not an invite at all.
            format!("{ORIGIN}/v1/contract/web/{RIVER}/?x=1"),
        ] {
            assert_eq!(
                invitation_code_to_intercept(&href, ORIGIN, &page()),
                None,
                "{href}"
            );
        }
    }

    #[test]
    fn v1_and_v2_routes_name_the_same_river() {
        let v2_page = format!("/v2/contract/web/{RIVER}/");
        let v2 = format!("{ORIGIN}/v2/contract/web/{RIVER}/?invitation=abc");
        let v1 = format!("{ORIGIN}/v1/contract/web/{RIVER}/?invitation=abc");
        for (href, page) in [(&v2, &v2_page), (&v1, &v2_page), (&v2, &page())] {
            assert_eq!(
                invitation_code_to_intercept(href, ORIGIN, page).as_deref(),
                Some("abc"),
                "{href} on {page}"
            );
        }
        let other = format!("{ORIGIN}/v2/contract/web/{OTHER}/?invitation=abc");
        assert_eq!(invitation_code_to_intercept(&other, ORIGIN, &v2_page), None);
    }

    #[test]
    fn nothing_is_intercepted_off_a_contract_page() {
        let href = format!("{ORIGIN}/v1/contract/web/{RIVER}/?invitation=abc");
        assert_eq!(invitation_code_to_intercept(&href, ORIGIN, "/"), None);
        assert_eq!(
            invitation_code_to_intercept(&href, ORIGIN, "/v1/contract/web/"),
            None
        );
    }
}
