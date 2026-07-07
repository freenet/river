use crate::components::members::Invitation;
use crate::components::room_list::receive_invitation_modal::present_invitation;
use dioxus::prelude::*;

/// Modal that lets a user paste a portable invite CODE (the bare
/// `Invitation::to_encoded_string()` base58 string) and join a room, without
/// needing a host-baked `?invitation=...` link.
///
/// This is the receive-side counterpart to the "Portable invite code" field
/// in `InviteMemberModal` (freenet/river#381). It mirrors the "Import ID"
/// affordance in the room list: paste, validate, act. A first-time user on
/// try.freenet.org (or any non-standard host) previously had to hand-edit the
/// host out of an invite link; now the inviter shares a host-independent code
/// and the recipient pastes it here.
///
/// On a successful decode we route through `present_invitation`, the same
/// public entry point the DM invite-card "Accept" button uses. That surfaces
/// the normal `ReceiveInvitationModal` (nickname prompt → accept), so the
/// entire accept flow — re-accept guard, `room_secrets` handling, processed
/// fingerprinting — is reused unchanged. Unparseable input is surfaced
/// inline rather than silently dropped (the click-interceptor logs the same
/// class of failure as "unparseable code").
#[component]
pub fn JoinWithCodeModal(is_active: Signal<bool>) -> Element {
    let mut code_input = use_signal(String::new);
    let mut error_msg = use_signal(|| None::<String>);

    if !*is_active.read() {
        return rsx! {};
    }

    // Signal-safety (see `.claude/rules/dioxus-signal-safety.md` and the
    // deferred local-signal writes in `room_list.rs`): this component reads
    // `is_active`, `code_input`, and `error_msg` during render, so mutations to
    // them from event handlers are wrapped in `crate::util::defer()` to run in a
    // clean Dioxus context (no re-entrant `RefCell` borrow, root scope present).
    // The one exception is the controlled `<textarea>`'s `oninput` below: a
    // deferred write to a controlled input's bound value lags the DOM and drops
    // keystrokes, which is why every text-input handler in the codebase
    // (`ImportIdentityModal`, `receive_invitation_modal`) sets its value signal
    // synchronously.
    let reset_and_close = move || {
        crate::util::defer(move || {
            is_active.set(false);
            error_msg.set(None);
            code_input.set(String::new());
        });
    };

    let handle_join = move |_| {
        // Codes are typically copy/pasted, so tolerate surrounding whitespace
        // and stray newlines that would otherwise break base58 decoding.
        let input = code_input.read().trim().to_string();
        if input.is_empty() {
            crate::util::defer(move || {
                error_msg.set(Some("Please paste an invite code.".to_string()));
            });
            return;
        }
        match Invitation::from_encoded_string(&input) {
            Ok(invitation) => {
                // Hand off to the shared accept flow. `present_invitation`
                // stashes the invitation in localStorage and defers the
                // global-signal write that opens `ReceiveInvitationModal`, so
                // there is nothing further to do here but close.
                present_invitation(invitation);
                reset_and_close();
            }
            Err(e) => {
                let msg = format!("That doesn't look like a valid invite code: {}", e);
                crate::util::defer(move || {
                    error_msg.set(Some(msg));
                });
            }
        }
    };

    rsx! {
        div {
            class: "fixed inset-0 bg-black/50 flex items-center justify-center z-50",
            onclick: move |_| reset_and_close(),
            div {
                "data-testid": "join-with-code-modal",
                class: "bg-panel border border-border rounded-xl shadow-lg p-6 max-w-lg w-full mx-4",
                onclick: move |e| e.stop_propagation(),
                h3 { class: "text-lg font-semibold text-text mb-4",
                    "Enter Invite Code"
                }
                p { class: "text-sm text-text-muted mb-3",
                    "Paste a portable invite code someone shared with you. It works on any host or peer, so you don't need to open a special link."
                }
                textarea {
                    "data-testid": "join-with-code-input",
                    class: "w-full h-32 bg-surface border border-border rounded-lg p-3 text-xs font-mono text-text resize-none",
                    placeholder: "Paste invite code here",
                    value: "{code_input}",
                    oninput: move |e| {
                        // Clear any stale error as soon as the user edits.
                        error_msg.set(None);
                        code_input.set(e.value());
                    },
                }
                if let Some(err) = &*error_msg.read() {
                    div { class: "mt-2 text-sm text-red-400",
                        "{err}"
                    }
                }
                div { class: "flex justify-end gap-3 mt-4",
                    button {
                        class: "px-4 py-2 bg-surface hover:bg-surface-hover text-text text-sm rounded-lg transition-colors border border-border",
                        onclick: move |_| reset_and_close(),
                        "Cancel"
                    }
                    button {
                        "data-testid": "join-with-code-submit-button",
                        class: "px-4 py-2 bg-accent hover:bg-accent-hover text-white text-sm font-medium rounded-lg transition-colors",
                        onclick: handle_join,
                        "Join"
                    }
                }
            }
        }
    }
}
