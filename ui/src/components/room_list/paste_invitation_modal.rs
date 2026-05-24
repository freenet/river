//! Modal for accepting an invitation by pasting either the full URL
//! or the bare invitation code.
//!
//! On web River reads invitations from `window.location().search()` —
//! the URL bar IS the invitation surface. On Android there's no URL
//! bar, no DOM `window` (`platform::window()` returns `None`), and
//! the click interceptor is a no-op. Without this modal, an Android
//! user has no way to act on an invitation URL sent to them out-of-
//! band (Signal, email, etc.) other than installing River on a desktop
//! browser first. Section 8.2 of the openspec change calls this out.
//!
//! The modal accepts both:
//!   - A full URL: `http://.../?invitation=N4so...` (anything containing
//!     `?invitation=` is treated as a URL; everything after that prefix
//!     up to the first `#` or `&` is the code)
//!   - The bare base58 code: `N4so...`
//!
//! On success it calls [`crate::components::room_list::receive_invitation_modal::present_invitation`],
//! which is the same path the in-app DM-thread "Accept" button takes —
//! deferred signal write into `PRESENT_INVITATION_REQUEST`, picked up by
//! `App`'s bridge effect, opens `ReceiveInvitationModal`.

use crate::components::members::Invitation;
use crate::components::room_list::receive_invitation_modal::present_invitation;
use dioxus::prelude::*;

/// Extract the invitation code from arbitrary user-pasted input.
///
/// Pure helper so it can be unit-tested without a Dioxus runtime.
pub(super) fn extract_code(input: &str) -> &str {
    let trimmed = input.trim();
    match trimmed.find("?invitation=") {
        Some(idx) => {
            let after = &trimmed[idx + "?invitation=".len()..];
            // Stop at fragment / next query param boundary
            match after.find(['#', '&']) {
                Some(end) => &after[..end],
                None => after,
            }
        }
        None => trimmed,
    }
}

#[component]
pub fn PasteInvitationModal(is_active: Signal<bool>) -> Element {
    let mut input = use_signal(String::new);
    let mut error_msg = use_signal(|| None::<String>);

    if !*is_active.read() {
        return rsx! {};
    }

    let close = move |_| {
        is_active.set(false);
        input.set(String::new());
        error_msg.set(None);
    };

    let handle_join = move |_| {
        let raw = input.read().clone();
        let code = extract_code(&raw);
        if code.is_empty() {
            error_msg.set(Some("Paste an invitation URL or code first.".to_string()));
            return;
        }
        match Invitation::from_encoded_string(code) {
            Ok(inv) => {
                // present_invitation defers the signal write via
                // crate::util::defer (setTimeout on wasm, synchronous on
                // native — safe from this onclick handler context).
                //
                // KNOWN ANDROID CRASH (TODO): on Android the subsequent
                // render of ReceiveInvitationModal triggers a panic
                // inside dioxus / wry's
                // `Java_dev_dioxus_main_RustWebViewClient_handleRequest`
                // JNI callback (panic_cannot_unwind at the FFI
                // boundary → SIGABRT). The decode + handoff path here
                // works fine; the crash happens downstream when the
                // newly-rendered modal causes the WebView to fetch a
                // resource the custom-protocol handler doesn't expect.
                // Reproduced on Pixel 10 Pro XL with the full public-
                // chat invitation pasted via the system clipboard.
                // Investigation: the originating panic message wasn't
                // captured in logcat by the time the device dropped
                // off USB; will need to dig into dioxus-desktop /
                // wry's asset handler under
                // `target_os = "android"` next session.
                present_invitation(inv);
                // Close immediately; the global ReceiveInvitationModal
                // will open as soon as the bridge effect in App sees
                // PRESENT_INVITATION_REQUEST update.
                is_active.set(false);
                input.set(String::new());
                error_msg.set(None);
            }
            Err(e) => {
                error_msg.set(Some(format!(
                    "Couldn't decode invitation: {e}. Make sure you pasted the full URL or the code after '?invitation='."
                )));
            }
        }
    };

    rsx! {
        div {
            class: "fixed inset-0 bg-black/50 flex items-center justify-center z-50",
            onclick: close,
            div {
                class: "bg-panel border border-border rounded-xl shadow-lg p-6 max-w-lg w-full mx-4",
                onclick: move |e| e.stop_propagation(),
                h3 { class: "text-lg font-semibold text-text mb-4", "Join via Invitation" }
                p { class: "text-sm text-text-muted mb-3",
                    "Paste an invitation URL someone shared with you, or just the invitation code."
                }
                textarea {
                    class: "w-full h-32 bg-surface border border-border rounded-lg p-3 text-xs font-mono text-text resize-none",
                    placeholder: "https://freenet.org/.../?invitation=N4so…\n  or just\nN4so…",
                    value: "{input}",
                    oninput: move |e| input.set(e.value()),
                }
                if let Some(err) = &*error_msg.read() {
                    div { class: "mt-2 text-sm text-red-400", "{err}" }
                }
                div { class: "flex justify-end gap-3 mt-4",
                    button {
                        class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text text-sm rounded-lg transition-colors border border-border",
                        onclick: close,
                        "Cancel"
                    }
                    button {
                        class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white text-sm font-medium rounded-lg transition-colors",
                        onclick: handle_join,
                        "Join"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::extract_code;

    #[test]
    fn full_url_returns_code_only() {
        let url = "http://127.0.0.1:7509/v1/contract/web/raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv/?invitation=N4soWF6CXpiN4qAw";
        assert_eq!(extract_code(url), "N4soWF6CXpiN4qAw");
    }

    #[test]
    fn url_with_fragment_drops_fragment() {
        let url = "http://example.com/?invitation=ABC#stuff";
        assert_eq!(extract_code(url), "ABC");
    }

    #[test]
    fn url_with_extra_query_param_drops_it() {
        let url = "http://example.com/?invitation=ABC&utm_source=signal";
        assert_eq!(extract_code(url), "ABC");
    }

    #[test]
    fn bare_code_passes_through() {
        assert_eq!(extract_code("N4soWF6CXpiN4qAw"), "N4soWF6CXpiN4qAw");
    }

    #[test]
    fn surrounding_whitespace_trimmed() {
        assert_eq!(extract_code("  N4soWF6\n"), "N4soWF6");
    }

    #[test]
    fn empty_input_yields_empty_string() {
        assert_eq!(extract_code(""), "");
        assert_eq!(extract_code("   \n  "), "");
    }
}
