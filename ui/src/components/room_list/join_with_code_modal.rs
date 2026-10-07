#[cfg(target_arch = "wasm32")]
use super::invite_qr_scan::Capture;
use super::invite_qr_scan::{self, PREVIEW_ID};
use crate::components::members::Invitation;
use crate::components::room_list::receive_invitation_modal::present_invitation;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::FaCamera;
use dioxus_free_icons::Icon;
use std::rc::Rc;

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
/// Scan QR code (freenet/river#741) reads that same string off the inviter's
/// screen. The camera path calls [`accept_invite_code`], so a scan and a paste
/// join through one decoder.
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
    let mut scan_error = use_signal(|| None::<String>);
    let mut scanning = use_signal(|| false);
    let scan_session = use_hook(|| Rc::new(invite_qr_scan::ScanSession::new())).clone();

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
    let session_for_close = scan_session.clone();
    use_effect(move || {
        // Anchor before the fallible read. A contended `try_read` that is the
        // effect's only read would otherwise drop the subscription
        // (freenet/river#555, and the #741 review).
        crate::util::signal_guard::anchor();
        let Ok(active) = is_active.try_read() else {
            crate::util::signal_guard::schedule_nudge();
            return;
        };
        if !*active {
            session_for_close.stop();
        }
    });
    let session_for_drop = scan_session.clone();
    use_drop(move || session_for_drop.stop());

    if !*is_active.read() {
        return rsx! {};
    }

    let session_for_reset = scan_session.clone();
    // The backdrop, Cancel, and a successful paste each call this. `Rc` keeps
    // one closure instead of moving it into the first handler.
    let reset_and_close = Rc::new(move || {
        session_for_reset.stop();
        crate::util::defer(move || {
            is_active.set(false);
            error_msg.set(None);
            scan_error.set(None);
            scanning.set(false);
            code_input.set(String::new());
        });
    });

    let reset_for_join = reset_and_close.clone();
    let handle_join = move |_| {
        let input = code_input.read().clone();
        match accept_invite_code(&input) {
            Ok(()) => reset_for_join(),
            Err(msg) => {
                crate::util::defer(move || {
                    error_msg.set(Some(msg));
                });
            }
        }
    };

    #[cfg(target_arch = "wasm32")]
    let session_for_scan = scan_session.clone();
    let start_scan = move |_| {
        if !invite_qr_scan::detector_available() {
            crate::util::defer(move || {
                scan_error.set(Some(
                    "This browser cannot scan QR codes. Paste the code instead.".to_string(),
                ));
            });
            return;
        }
        #[cfg(target_arch = "wasm32")]
        {
            // Opaque shell iframe: getUserMedia throws SecurityError and the
            // browser never shows a prompt. Open the camera app instead.
            // This click is the user gesture the file input needs.
            if invite_qr_scan::camera_prompt_unavailable() {
                match invite_qr_scan::open_still_capture() {
                    Ok(()) => {
                        crate::util::defer(move || {
                            scan_error.set(None);
                            error_msg.set(None);
                        });
                    }
                    Err(msg) => {
                        crate::util::defer(move || scan_error.set(Some(msg)));
                    }
                }
                return;
            }
            let promise = match invite_qr_scan::request_rear_camera() {
                Ok(promise) => promise,
                Err(msg) => {
                    crate::util::defer(move || scan_error.set(Some(msg)));
                    return;
                }
            };
            // The promise is created in this click, so the permission prompt
            // stays tied to the tap. The preview element appears on the
            // deferred render below; the task waits for it.
            let generation = session_for_scan.begin();
            let session = session_for_scan.clone();
            let session_after = session.clone();
            crate::util::safe_spawn_local(async move {
                let outcome = invite_qr_scan::capture_code(session, generation, promise).await;
                // Checked after the await. A Stop or a newer scan during the
                // poll must not open an invitation or clear the new scan's flag.
                let still = session_after.generation_is(generation);
                match outcome {
                    Capture::Code(raw) if still => {
                        let extracted = crate::invite_qr::invitation_text_from_scan(&raw);
                        crate::util::defer(move || {
                            scanning.set(false);
                            match accept_invite_code(&extracted) {
                                Ok(()) => {
                                    is_active.set(false);
                                    error_msg.set(None);
                                    scan_error.set(None);
                                    code_input.set(String::new());
                                }
                                Err(msg) => {
                                    code_input.set(extracted);
                                    error_msg.set(Some(msg));
                                }
                            }
                        });
                    }
                    Capture::Code(_) | Capture::Cancelled => {}
                    Capture::Failed(msg) if still => {
                        crate::util::defer(move || {
                            scanning.set(false);
                            scan_error.set(Some(msg));
                        });
                    }
                    Capture::Failed(_) => {}
                }
            });
            crate::util::defer(move || {
                scan_error.set(None);
                error_msg.set(None);
                scanning.set(true);
            });
        }
    };

    #[cfg(target_arch = "wasm32")]
    let session_for_photo = scan_session.clone();
    let session_for_stop = scan_session;
    let stop_scan = move |_| {
        session_for_stop.stop();
        crate::util::defer(move || scanning.set(false));
    };

    rsx! {
        div {
            class: "fixed inset-0 bg-black/50 flex items-center justify-center z-50",
            onclick: {
                let reset_and_close = reset_and_close.clone();
                move |_| reset_and_close()
            },
            div {
                "data-testid": "join-with-code-modal",
                class: "bg-panel border border-border rounded-xl shadow-lg p-6 max-w-lg w-full mx-4 max-h-[90vh] overflow-y-auto",
                onclick: move |e| e.stop_propagation(),
                h3 { class: "text-lg font-semibold text-text mb-4",
                    "Enter Invite Code"
                }
                p { class: "text-sm text-text-muted mb-3",
                    "Paste a portable invite code someone shared with you, or scan their QR code. It works on any host or peer, so you don't need to open a special link."
                }
                if *scanning.read() {
                    video {
                        id: PREVIEW_ID,
                        "data-testid": "join-with-code-scan-preview",
                        class: "w-full max-h-64 bg-black rounded-lg object-contain",
                        autoplay: true,
                        muted: true,
                        playsinline: true,
                    }
                    p { class: "text-xs text-text-muted mt-2",
                        "Point the camera at the invitation QR code."
                    }
                    button {
                        "data-testid": "join-with-code-scan-stop-button",
                        class: "mt-3 w-full px-4 py-2 bg-surface hover:bg-surface-hover text-text text-sm rounded-lg transition-colors border border-border",
                        onclick: stop_scan,
                        "Stop scanning"
                    }
                } else if invite_qr_scan::detector_available() {
                    // Browsers without BarcodeDetector (iOS Safari, Firefox,
                    // desktop Chrome on Linux and Windows) keep the paste box
                    // only. A Scan button there can only report that scanning
                    // is unavailable (freenet/river#741 review).
                    if invite_qr_scan::camera_prompt_unavailable() {
                        p { class: "text-xs text-text-muted mb-3",
                            "Opens your camera to take a photo of the QR code."
                        }
                    }
                    button {
                        "data-testid": "join-with-code-scan-button",
                        class: "w-full mb-3 px-4 py-2 bg-surface hover:bg-surface-hover text-text text-sm rounded-lg transition-colors border border-border flex items-center justify-center gap-2",
                        onclick: start_scan,
                        Icon { icon: FaCamera, width: 14, height: 14 }
                        span { "Scan QR code" }
                    }
                    input {
                        id: invite_qr_scan::STILL_INPUT_ID,
                        "data-testid": "join-with-code-scan-file",
                        r#type: "file",
                        accept: "image/*",
                        capture: "environment",
                        style: "position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);white-space:nowrap;border:0",
                        onchange: move |evt| {
                            #[cfg(target_arch = "wasm32")]
                            {
                                let Some(file) = evt.files().into_iter().next().and_then(|data| {
                                    data.inner().downcast_ref::<web_sys::File>().cloned()
                                }) else {
                                    return;
                                };
                                if let Some(input) = invite_qr_scan::still_input() {
                                    input.set_value("");
                                }
                                // Same generation as a live scan: closing the
                                // modal or starting another scan invalidates
                                // this photo before its decode finishes.
                                let generation = session_for_photo.begin();
                                let session = session_for_photo.clone();
                                crate::util::safe_spawn_local(async move {
                                    let decoded = invite_qr_scan::decode_still(file).await;
                                    if !session.generation_is(generation) {
                                        return;
                                    }
                                    crate::util::defer(move || match decoded {
                                        Ok(raw) => {
                                            let extracted =
                                                crate::invite_qr::invitation_text_from_scan(&raw);
                                            scanning.set(false);
                                            match accept_invite_code(&extracted) {
                                                Ok(()) => {
                                                    is_active.set(false);
                                                    error_msg.set(None);
                                                    scan_error.set(None);
                                                    code_input.set(String::new());
                                                }
                                                Err(msg) => {
                                                    code_input.set(extracted);
                                                    error_msg.set(Some(msg));
                                                    scan_error.set(None);
                                                }
                                            }
                                        }
                                        Err(msg) => scan_error.set(Some(msg)),
                                    });
                                });
                            }
                            #[cfg(not(target_arch = "wasm32"))]
                            {
                                let _ = evt;
                            }
                        },
                    }
                }
                if let Some(err) = scan_error.read().as_ref() {
                    div {
                        "data-testid": "join-with-code-scan-error",
                        class: "mb-3 text-sm text-red-400",
                        "{err}"
                    }
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

/// Decode a pasted or scanned invite and open the shared accept modal.
///
/// A scan of the invite *link* is reduced to its `invitation` parameter first.
/// The QR we draw is the bare code; this keeps a photographed link working too.
fn accept_invite_code(raw: &str) -> Result<(), String> {
    let input = crate::invite_qr::invitation_text_from_scan(raw);
    if input.is_empty() {
        return Err("Please paste an invite code.".to_string());
    }
    match Invitation::from_encoded_string(&input) {
        Ok(invitation) => {
            present_invitation(invitation);
            Ok(())
        }
        Err(e) => Err(format!("That doesn't look like a valid invite code: {e}")),
    }
}
