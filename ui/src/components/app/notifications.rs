//! Browser notification support for new chat messages.
//!
//! Sends desktop notifications when:
//! - Document is not visible (tab hidden or window not focused)
//! - Message is from another user (not self)
//! - Room is not currently active
//! - Permission has been granted

use crate::components::app::{MobileView, CURRENT_ROOM, MOBILE_VIEW, ROOMS};
use crate::room_data::{CurrentRoom, NotificationMode};
use crate::util::ecies::{decrypt_with_symmetric_key, unseal_bytes_with_secrets};
use dioxus::logger::tracing::{debug, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::MemberInfoV1;
use river_core::room_state::message::{AuthorizedMessageV1, MessageId, RoomMessageBody};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MessageEvent, Notification, NotificationOptions, NotificationPermission, VisibilityState,
};

const NOTIFICATION_PROMPTED_KEY: &str = "river_notification_prompted";

/// Test function to verify browser notifications work at all.
/// Can be called from browser console via: river_test_notification()
#[wasm_bindgen]
pub fn river_test_notification() {
    web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
        "[River] Testing notification system...",
    ));

    let permission = get_permission();
    web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(&format!(
        "[River] Current permission: {:?}",
        permission
    )));

    if permission == NotificationPermission::Granted {
        let options = NotificationOptions::new();
        options.set_body("If you see this, notifications are working!");

        match Notification::new_with_options("River Test Notification", &options) {
            Ok(_notification) => {
                web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
                    "[River] Test notification created successfully! You should see it now.",
                ));
            }
            Err(e) => {
                web_sys::console::error_1(&wasm_bindgen::JsValue::from_str(&format!(
                    "[River] Failed to create test notification: {:?}",
                    e
                )));
            }
        }
    } else if permission == NotificationPermission::Default {
        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
            "[River] Permission not yet granted. Requesting permission...",
        ));
        wasm_bindgen_futures::spawn_local(async {
            let granted = request_permission().await;
            if granted {
                web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
                    "[River] Permission granted! Run riverTestNotification() again to test.",
                ));
            } else {
                web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
                    "[River] Permission denied or dismissed.",
                ));
            }
        });
    } else {
        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
            "[River] Notifications are denied. Please enable them in browser settings.",
        ));
    }
}

/// Tracks which rooms have completed initial sync (don't notify for initial message load)
pub static INITIAL_SYNC_COMPLETE: GlobalSignal<HashSet<VerifyingKey>> = Global::new(HashSet::new);

/// Check if the document is currently visible and focused
pub fn is_document_visible() -> bool {
    if let Some(window) = web_sys::window() {
        if let Some(document) = window.document() {
            let is_visible = document.visibility_state() == VisibilityState::Visible;
            let has_focus = document.has_focus().unwrap_or(false);
            return is_visible && has_focus;
        }
    }
    // Default to visible to avoid spurious notifications
    true
}

/// Get current notification permission status
pub fn get_permission() -> NotificationPermission {
    Notification::permission()
}

/// Request notification permission from the user
/// Returns true if permission was granted
pub async fn request_permission() -> bool {
    match Notification::request_permission() {
        Ok(promise) => match JsFuture::from(promise).await {
            Ok(result) => {
                let permission_str = result.as_string().unwrap_or_default();
                info!("Notification permission result: {}", permission_str);
                permission_str == "granted"
            }
            Err(e) => {
                warn!("Failed to request notification permission: {:?}", e);
                false
            }
        },
        Err(e) => {
            warn!("Failed to request notification permission: {:?}", e);
            false
        }
    }
}

/// Check if we've already prompted the user for notification permission
fn has_prompted_for_permission() -> bool {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            return storage
                .get_item(NOTIFICATION_PROMPTED_KEY)
                .ok()
                .flatten()
                .is_some();
        }
    }
    false
}

/// Mark that we've prompted the user for notification permission
fn mark_prompted_for_permission() {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item(NOTIFICATION_PROMPTED_KEY, "true");
        }
    }
}

// --- Gateway shell-bridge notification proxy -----------------------------
//
// In the deployed gateway, River runs inside the shell's sandboxed iframe,
// which has an opaque (null) origin. Browsers block the Notifications API from
// opaque-origin iframes, so River can't request permission or show a
// notification directly (freenet/river#408; the iframe isolation was added in
// freenet-core#3254). Instead we hand the notification to the shell page (which
// is same-origin with the node, a real origin) over the existing
// `__freenet_shell__` postMessage bridge — the same channel that already
// carries title/favicon updates. The shell shows the notification and posts a
// `notification_click` reply back so we can route to the room.
//
// When River is served directly as a top-level page (dev / `no-sync`), it has a
// real origin and uses the Notifications API directly, so the direct path below
// is preserved for that case.

/// Only send the "offer notifications" request to the shell once per session
/// (the iframe's opaque origin can't persist a localStorage flag anyway).
/// Cleared again by a `notification_status` of `default` — see
/// [`status_rearms_auto_prompt`].
static ENABLE_PROMPT_SENT: AtomicBool = AtomicBool::new(false);
/// How many times [`ENABLE_PROMPT_SENT`] has been re-armed this session.
static ENABLE_PROMPT_REARMS: AtomicUsize = AtomicUsize::new(0);
/// Install the `notification_click` listener at most once.
static CLICK_LISTENER_INSTALLED: AtomicBool = AtomicBool::new(false);

/// How many times an unanswered prompt may re-arm the AUTOMATIC ask
/// ([`request_enable_via_shell`], fired when the user sends their first
/// message). One re-arm means at most two automatic asks per session.
///
/// A cap is needed because the shell re-shows its affordance every time River
/// asks: without one, a user who keeps closing the browser's permission dialog
/// would get the bar back on every single message they send. The manual
/// [`request_enable_now`] is deliberately NOT capped — an explicit click is a
/// direct request, not a nag.
const MAX_ENABLE_PROMPT_REARMS: usize = 1;

/// The permission outcomes the gateway shell reports back as
/// `notification_status` over the `__freenet_shell__` bridge.
///
/// River's iframe has an opaque origin and cannot read `Notification.permission`
/// at all, so in the gateway this message is the ONLY way it learns whether a
/// notification it hands to the shell will actually be shown. The variants
/// mirror `notifyStatusToIframe` in freenet-core's `shell_bridge.js`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationStatus {
    /// Permission granted and this contract has the shell's consent.
    Granted,
    /// The browser is blocking notifications for the origin. Only the user can
    /// undo this, in browser settings — asking again cannot help.
    Denied,
    /// The offer was declined ("Not now"), or is inside the shell's snooze
    /// window from an earlier decline.
    Dismissed,
    /// The permission prompt closed with no answer (the shell's `default`).
    /// Nothing is blocked, so asking again is still worthwhile.
    Undecided,
    /// Permission exists but nothing could display the notification — no
    /// service worker on mobile, an insecure-context origin, or no contract
    /// key to gate consent on.
    Undeliverable,
    /// The browser has no Notifications API at all.
    Unsupported,
}

impl NotificationStatus {
    /// Parse the shell's `status` string.
    ///
    /// An unrecognised value yields `None` and is then ignored rather than
    /// guessed at, so a newer shell adding a status can never overwrite a known
    /// one with a wrong reading.
    pub fn from_shell_status(status: &str) -> Option<Self> {
        Some(match status {
            "granted" => Self::Granted,
            "denied" => Self::Denied,
            "dismissed" => Self::Dismissed,
            "default" => Self::Undecided,
            "undeliverable" => Self::Undeliverable,
            "unsupported" => Self::Unsupported,
            _ => return None,
        })
    }
}

/// The last notification status River has learned, or `None` before it has
/// learned any. Written only by [`record_notification_status`].
pub static NOTIFICATION_STATUS: GlobalSignal<Option<NotificationStatus>> = Global::new(|| None);

/// What the bell modal tells the user about browser-level permission, and
/// whether it offers them a button to ask again.
pub struct PermissionNotice {
    pub message: &'static str,
    pub offer_enable: bool,
}

/// The notice for a given status. Pure, so the "can asking again help?"
/// decision is testable without a browser.
///
/// `offer_enable` is true exactly where a fresh ask can still change the
/// outcome. `Denied` and `Unsupported` are the browser's decision and River
/// cannot re-open them; `Undeliverable` means permission is already granted, so
/// asking for it again would do nothing.
pub fn permission_notice(status: Option<NotificationStatus>) -> PermissionNotice {
    let (message, offer_enable) = match status {
        None => (
            "Desktop notifications are not set up yet for this browser.",
            true,
        ),
        Some(NotificationStatus::Granted) => ("Desktop notifications are enabled.", false),
        Some(NotificationStatus::Denied) => (
            "Your browser is blocking notifications for this site. Turn them back on in the \
             browser's site settings — River can't ask again once they're blocked.",
            false,
        ),
        // Honest about the delay: `dismissed` also covers "inside the gateway's
        // snooze window from an earlier decline", during which asking again
        // provably shows nothing. Promising an immediate retry would put the
        // user back in front of a control that silently does nothing.
        Some(NotificationStatus::Dismissed) => (
            "You chose not to enable notifications. You can ask again, though the prompt may not \
             reappear straight away.",
            true,
        ),
        Some(NotificationStatus::Undecided) => (
            "The notification prompt closed without an answer, so nothing is enabled yet.",
            true,
        ),
        Some(NotificationStatus::Undeliverable) => (
            "This browser granted permission but couldn't display a notification. Unread badges \
             and the tab title still work.",
            false,
        ),
        Some(NotificationStatus::Unsupported) => (
            "This browser doesn't support desktop notifications. Unread badges and the tab title \
             still work.",
            false,
        ),
    };
    PermissionNotice {
        message,
        offer_enable,
    }
}

/// Whether the bell should carry a "these preferences aren't reaching you"
/// marker.
///
/// The per-room modes decide only WHEN River wants to notify. If the browser
/// won't deliver, every one of them is inert — and until now the only place
/// that said so was inside the modal, so a user with a blocked permission saw an
/// ordinary bell reading "Notifications: All messages" and learned nothing
/// unless they went looking. For an issue titled "fails silently", that is the
/// last place the silence lives.
///
/// Deliberately NOT flagged:
/// - [`NotificationMode::Muted`] — the user asked for no notifications, so
///   warning that they won't get any is noise.
/// - [`NotificationStatus::Unsupported`] — the platform has no Notifications API
///   at all (mobile Safari), so the marker could never be cleared by anything
///   the user does. A permanent badge is nagging, not informing; the modal still
///   explains it.
pub fn delivery_is_blocked(mode: NotificationMode, status: Option<NotificationStatus>) -> bool {
    !matches!(mode, NotificationMode::Muted)
        && matches!(
            status,
            Some(NotificationStatus::Denied | NotificationStatus::Undeliverable)
        )
}

/// Whether a reported status should re-arm the automatic enable prompt.
///
/// Only [`NotificationStatus::Undecided`] does: every other outcome is either
/// final (`Granted`, `Denied`, `Unsupported`, `Undeliverable`) or a decision the
/// user actively made (`Dismissed`, which the shell also snoozes for 24h — River
/// re-asking would just bounce off that snooze). Bounded by
/// [`MAX_ENABLE_PROMPT_REARMS`].
fn status_rearms_auto_prompt(status: NotificationStatus, rearms_used: usize) -> bool {
    matches!(status, NotificationStatus::Undecided) && rearms_used < MAX_ENABLE_PROMPT_REARMS
}

/// Whether River is running framed rather than as a top-level page. This
/// detects framing in general (`window.parent !== window`); River only ever
/// runs top-level (dev / `no-sync`) or inside the gateway's sandboxed iframe
/// (opaque origin, no Notifications API), so "framed" reliably means "in the
/// gateway shell" in practice. In the gateway `window.parent` is the shell page
/// — a different window; served directly, `window.parent === window`.
fn is_in_shell_iframe() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    match window.parent() {
        Ok(Some(parent)) => !js_sys::Object::is(parent.as_ref(), window.as_ref()),
        _ => false,
    }
}

/// Post a `__freenet_shell__` message to the parent shell page. Target origin
/// `"*"` because the shell's origin can't be named from here; the shell gates
/// on `event.source` being its own iframe.
fn post_to_shell(msg: &JsValue) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(parent)) = window.parent() {
            let _ = parent.post_message(msg, "*");
        }
    }
}

/// Build a `{__freenet_shell__: true, type: <kind>}` message object.
fn shell_message(kind: &str) -> js_sys::Object {
    let o = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&o, &JsValue::from_str("__freenet_shell__"), &JsValue::TRUE);
    let _ = js_sys::Reflect::set(&o, &JsValue::from_str("type"), &JsValue::from_str(kind));
    o
}

/// Encode a room's owner key as the notification `tag` so a `notification_click`
/// reply from the shell can be routed back to the right room.
fn room_key_to_tag(room_key: &VerifyingKey) -> String {
    bs58::encode(room_key.as_bytes()).into_string()
}

/// Inverse of [`room_key_to_tag`]. Returns `None` for a malformed tag.
fn tag_to_room_key(tag: &str) -> Option<VerifyingKey> {
    let bytes = bs58::decode(tag).into_vec().ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&arr).ok()
}

/// Ask the shell to offer notifications (it owns the real-origin permission
/// prompt). Sent at most once per session.
fn request_enable_via_shell() {
    if ENABLE_PROMPT_SENT.swap(true, Ordering::SeqCst) {
        return;
    }
    post_to_shell(&shell_message("notification_enable_prompt"));
}

/// Ask for notification permission right now, carrying the user's click as the
/// fresh gesture browsers require. Backs the bell modal's "Enable
/// notifications" button.
///
/// Deliberately NOT latched: [`request_enable_via_shell`]'s once-per-session
/// latch exists so sending messages doesn't nag, and an explicit click is
/// exactly the retry that latch must not swallow. Without this, a prompt that
/// was blocked, snoozed, or mis-clicked could not be re-requested for the rest
/// of the session (freenet/river#510).
///
/// MUST be called synchronously from the click handler — see
/// [`request_permission_directly`] for why. It writes no signal synchronously
/// (every write goes through `defer`), so calling it directly from an `onclick`
/// is signal-safe.
pub fn request_enable_now() {
    if is_in_shell_iframe() {
        // Leave the automatic latch SET: the shell is being asked right now, and
        // its `notification_status` reply is what re-arms it.
        ENABLE_PROMPT_SENT.store(true, Ordering::SeqCst);
        post_to_shell(&shell_message("notification_enable_prompt"));
        return;
    }
    request_permission_directly();
}

/// Top-level (dev / `no-sync`) path for [`request_enable_now`]: River has a real
/// origin here, so it can call the Notifications API itself.
///
/// `Notification::request_permission()` is invoked SYNCHRONOUSLY so it runs
/// inside the click's transient user activation — Firefox and Safari reject the
/// request with `NotAllowedError` once that activation has lapsed. Only the
/// awaiting of the already-created promise is deferred, which is safe because
/// the browser has already seen the gesture by then.
fn request_permission_directly() {
    match Notification::request_permission() {
        Ok(promise) => {
            crate::util::safe_spawn_local(async move {
                let status = match JsFuture::from(promise).await {
                    Ok(result) => match result.as_string().as_deref() {
                        Some("granted") => NotificationStatus::Granted,
                        Some("denied") => NotificationStatus::Denied,
                        // "default" — the user closed the dialog without an answer.
                        _ => NotificationStatus::Undecided,
                    },
                    Err(e) => {
                        warn!("Notification permission request failed: {:?}", e);
                        NotificationStatus::Undecided
                    }
                };
                record_notification_status(status);
            });
        }
        Err(e) => {
            warn!("Notifications API unavailable: {:?}", e);
            record_notification_status(NotificationStatus::Unsupported);
        }
    }
}

/// Record a notification status and, when the prompt closed with no answer,
/// re-arm the automatic ask so a later gesture can offer again.
///
/// The re-arm touches only atomics, which need no Dioxus context, so it happens
/// immediately; only the signal write is deferred, as required for a write
/// originating in a JS callback (`.claude/rules/dioxus-signal-safety.md`).
fn record_notification_status(status: NotificationStatus) {
    if status_rearms_auto_prompt(status, ENABLE_PROMPT_REARMS.load(Ordering::SeqCst)) {
        ENABLE_PROMPT_REARMS.fetch_add(1, Ordering::SeqCst);
        ENABLE_PROMPT_SENT.store(false, Ordering::SeqCst);
    }
    crate::util::defer(move || {
        *NOTIFICATION_STATUS.write() = Some(status);
    });
}

/// The status the UI should display.
pub fn current_notification_status() -> Option<NotificationStatus> {
    // Read unconditionally, even on the path that then ignores the value: the
    // read is what subscribes this render to the signal, so the modal
    // re-renders when a permission request completes. `try_read`, not `read`,
    // because a concurrent write would otherwise panic (dioxus-signal-safety).
    let stored = NOTIFICATION_STATUS.try_read().ok().and_then(|s| *s);
    let framed = is_in_shell_iframe();
    resolve_status(framed, stored, (!framed).then(read_browser_permission))
}

/// Which answer the UI shows: the shell's last report when framed, the
/// browser's own when not.
///
/// Framed, River's origin is opaque and the shell's report is the only source
/// there is. Served top-level the browser is authoritative and STAYS
/// authoritative — the user can flip the permission in site settings at any
/// time, and a remembered answer from an earlier request would then be wrong
/// for the rest of the session. So `stored` is deliberately ignored there
/// rather than preferred; its only job on that path is to trigger the re-render.
///
/// `live` is `None` exactly when framed, since there is nothing to read.
fn resolve_status(
    framed: bool,
    stored: Option<NotificationStatus>,
    live: Option<NotificationStatus>,
) -> Option<NotificationStatus> {
    if framed {
        stored
    } else {
        live
    }
}

/// The browser's own answer. Top-level only — the framed path must not call
/// this, since an opaque origin has no meaningful permission to read.
fn read_browser_permission() -> NotificationStatus {
    if !notification_api_available() {
        return NotificationStatus::Unsupported;
    }
    match get_permission() {
        NotificationPermission::Granted => NotificationStatus::Granted,
        NotificationPermission::Denied => NotificationStatus::Denied,
        _ => NotificationStatus::Undecided,
    }
}

/// Whether this browser exposes the Notifications API at all.
///
/// Must be checked before [`get_permission`]: `Notification::permission()` is a
/// non-throwing binding, so reading `.permission` off an undefined `Notification`
/// raises a JS `ReferenceError` that aborts the WASM module rather than
/// returning an error. Only [`Notification::request_permission`] is a
/// `Result`-returning (try/catch-wrapped) binding.
fn notification_api_available() -> bool {
    js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("Notification"))
        .map(|v| !v.is_undefined() && !v.is_null())
        .unwrap_or(false)
}

/// Proxy a new-message notification to the shell (gateway iframe path).
fn post_notification_to_shell(room_key: VerifyingKey, room_name: &str, body: &str) {
    let o = shell_message("notification");
    let _ = js_sys::Reflect::set(
        &o,
        &JsValue::from_str("title"),
        &JsValue::from_str(room_name),
    );
    let _ = js_sys::Reflect::set(&o, &JsValue::from_str("body"), &JsValue::from_str(body));
    let _ = js_sys::Reflect::set(
        &o,
        &JsValue::from_str("tag"),
        &JsValue::from_str(&room_key_to_tag(&room_key)),
    );
    post_to_shell(&o);
}

/// Listen for the shell's replies: `notification_click` (route to the clicked
/// room) and `notification_status` (record the permission outcome).
/// No-op when not in the shell iframe, and installs the listener at most once.
/// Accepts messages only from the parent shell window and validates the payload
/// defensively.
pub fn install_shell_notification_listener() {
    if !is_in_shell_iframe() {
        return;
    }
    if CLICK_LISTENER_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(window) = web_sys::window() else {
        return;
    };
    let cb = Closure::wrap(Box::new(move |event: MessageEvent| {
        // Defense-in-depth: only act on messages from the parent shell window.
        // In the gateway only the shell holds a handle to this iframe's window,
        // but a source check keeps a stray postMessage from forcing a room
        // switch. Worst case if this ever mis-rejected is a dropped click.
        let from_parent = web_sys::window()
            .and_then(|w| w.parent().ok().flatten())
            .zip(event.source())
            .map(|(parent, src)| js_sys::Object::is(src.as_ref(), parent.as_ref()))
            .unwrap_or(false);
        if !from_parent {
            return;
        }
        let data = event.data();
        let is_shell = js_sys::Reflect::get(&data, &JsValue::from_str("__freenet_shell__"))
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !is_shell {
            return;
        }
        let kind = js_sys::Reflect::get(&data, &JsValue::from_str("type"))
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();
        // The shell reports the outcome of its permission offer here. Without
        // this arm every outcome was discarded identically, so a blocked or
        // dismissed permission failed silently with no explanation and no
        // retry (freenet/river#510).
        if kind == "notification_status" {
            match js_sys::Reflect::get(&data, &JsValue::from_str("status"))
                .ok()
                .and_then(|v| v.as_string())
                .as_deref()
                .and_then(NotificationStatus::from_shell_status)
            {
                Some(status) => record_notification_status(status),
                // `warn!`, not `debug!`: `release_max_level_info` compiles
                // `debug!` out, and this fires exactly when the cross-repo
                // status contract has drifted — the failure that reverts #510
                // wholesale. It has to be diagnosable in the build users run.
                None => warn!("Ignoring unrecognised notification_status from shell"),
            }
            return;
        }
        if kind != "notification_click" {
            return;
        }
        let Some(room_key) = js_sys::Reflect::get(&data, &JsValue::from_str("tag"))
            .ok()
            .and_then(|v| v.as_string())
            .and_then(|tag| tag_to_room_key(&tag))
        else {
            return;
        };
        // Signal mutation from a JS event callback must be deferred to a clean
        // execution context (Dioxus signal-safety rules).
        crate::util::defer(move || {
            *CURRENT_ROOM.write() = CurrentRoom {
                owner_key: Some(room_key),
            };
            // Mirror normal room selection: on mobile, switch to the chat panel
            // so the clicked conversation is actually shown (otherwise the user
            // stays on the Rooms/Members panel and the room may be marked read
            // without ever being displayed).
            *MOBILE_VIEW.write() = MobileView::Chat;
            if let Some(window) = web_sys::window() {
                let _ = window.focus();
            }
        });
    }) as Box<dyn Fn(MessageEvent)>);
    let _ = window.add_event_listener_with_callback("message", cb.as_ref().unchecked_ref());
    cb.forget();
}

/// Request notification permission on first user message.
///
/// This follows best practices:
/// - Only prompts once per browser (persisted in localStorage)
/// - Respects if user already granted or denied permission
/// - Triggered by user action (sending a message) for better UX
pub fn request_permission_on_first_message() {
    // In the gateway iframe we can't use the Notifications API directly; ask the
    // shell (real origin) to offer notifications via its affordance instead.
    if is_in_shell_iframe() {
        request_enable_via_shell();
        return;
    }

    let permission = get_permission();

    // Already have a definitive answer - don't prompt
    if permission == NotificationPermission::Granted {
        debug!("Notification permission already granted");
        return;
    }

    if permission == NotificationPermission::Denied {
        debug!("Notification permission already denied");
        return;
    }

    // Permission is "default" (not yet asked) - check if we've prompted before
    if has_prompted_for_permission() {
        debug!("Already prompted for notification permission previously");
        return;
    }

    // First time - request permission
    info!("Requesting notification permission on first message");
    mark_prompted_for_permission();

    crate::util::safe_spawn_local(async {
        let granted = request_permission().await;
        if granted {
            info!("User granted notification permission");
        } else {
            info!("User did not grant notification permission");
        }
    });
}

/// Every character a renderer may start a new line on, flattened to a space by
/// [`notification_body`]: LF, CR, U+0085 NEXT LINE, U+2028 LINE SEPARATOR and
/// U+2029 PARAGRAPH SEPARATOR.
const NOTIFICATION_LINE_BREAKS: [char; 5] = ['\n', '\r', '\u{0085}', '\u{2028}', '\u{2029}'];

/// Build the `sender: message` body a notification displays.
///
/// The body is ONE flat string, so a message that can start a new line lets its
/// author paint a second line reading exactly like a notification from someone
/// else, including a moderator with a 🛡 in it. Every line-break character is
/// therefore flattened to a space. The sender name needs no such treatment: it
/// arrives already sanitised from
/// [`crate::util::display_name::display_nickname`].
///
/// BOTH notification paths build their body here (the gateway shell bridge in
/// [`show_notification`], and the direct Notifications API in
/// [`create_notification_internal`]), so the guard cannot be half-applied and a
/// third path cannot ship without it. Pinned by
/// `both_notification_paths_go_through_notification_body`.
fn notification_body(sender_name: &str, message_preview: &str) -> String {
    format!(
        "{sender_name}: {}",
        message_preview.replace(NOTIFICATION_LINE_BREAKS, " ")
    )
}

/// Show a notification for new messages in a room
///
/// # Arguments
/// * `room_key` - The room's owner verifying key
/// * `room_name` - Display name of the room
/// * `sender_name` - Name of the message sender
/// * `message_preview` - Truncated message content
pub fn show_notification(
    room_key: VerifyingKey,
    room_name: &str,
    sender_name: &str,
    message_preview: &str,
) {
    // Gateway iframe: hand the notification to the shell (real origin) over the
    // postMessage bridge — the sandboxed iframe can't use the Notifications API.
    if is_in_shell_iframe() {
        let body = notification_body(sender_name, message_preview);
        post_notification_to_shell(room_key, room_name, &body);
        return;
    }

    // Check permission
    let permission = get_permission();
    info!(
        "show_notification called for room '{}', permission: {:?}",
        room_name, permission
    );

    if permission == NotificationPermission::Denied {
        info!("Notifications denied by browser, skipping");
        return;
    }

    // If permission is default (not yet asked), request it first
    if permission == NotificationPermission::Default {
        let room_name = room_name.to_string();
        let sender_name = sender_name.to_string();
        let message_preview = message_preview.to_string();
        wasm_bindgen_futures::spawn_local(async move {
            let granted = request_permission().await;
            if granted {
                // Try again after permission granted
                create_notification_internal(room_key, &room_name, &sender_name, &message_preview);
            }
        });
        return;
    }

    // Permission is granted, show notification
    create_notification_internal(room_key, room_name, sender_name, message_preview);
}

fn create_notification_internal(
    room_key: VerifyingKey,
    room_name: &str,
    sender_name: &str,
    message_preview: &str,
) {
    let body = notification_body(sender_name, message_preview);
    info!(
        "Creating notification - title: '{}', body: '{}'",
        room_name, body
    );

    // Log to browser console for debugging
    let console_msg = format!("[River] Creating notification: {} - {}", room_name, body);
    web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(&console_msg));

    let options = NotificationOptions::new();
    options.set_body(&body);
    // Could add icon here: options.icon("/assets/river_logo.svg");

    match Notification::new_with_options(room_name, &options) {
        Ok(notification) => {
            // Log to browser console for confirmation
            web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(
                "[River] Notification object created successfully",
            ));

            // Set up click handler to focus window and switch to room
            let onclick = Closure::wrap(Box::new(move || {
                info!("Notification clicked, switching to room");

                // Switch to the room
                *CURRENT_ROOM.write() = CurrentRoom {
                    owner_key: Some(room_key),
                };

                // Focus the window
                if let Some(window) = web_sys::window() {
                    let _ = window.focus();
                }
            }) as Box<dyn Fn()>);

            notification.set_onclick(Some(onclick.as_ref().unchecked_ref()));

            // Prevent the closure from being dropped
            onclick.forget();

            info!(
                "Notification created for room: {} (check browser console)",
                room_name
            );
        }
        Err(e) => {
            warn!("Failed to create notification: {:?}", e);
            // Also log to console
            let err_msg = format!("[River] FAILED to create notification: {:?}", e);
            web_sys::console::error_1(&wasm_bindgen::JsValue::from_str(&err_msg));
        }
    }
}

/// Whether `msg` is a candidate for a browser notification to the local
/// user: authored by someone else, and not a room event.
///
/// Room events ("X joined the room", `CONTENT_TYPE_EVENT`) are ambient
/// activity, not something waiting to be read.
/// [`crate::components::app::document_title::count_unread_in_room_data`]
/// excludes them from every unread surface (freenet/river#500), so notifying
/// on one would make the notification and the badge disagree by construction:
/// a join would pop a browser notification while contributing to no badge and
/// no title count.
///
/// Known remaining asymmetry, pre-existing and deliberately unchanged here:
/// action messages (edits, reactions, deletes — `is_action()`) are likewise
/// excluded from the counts but are NOT filtered out of notifications, so a
/// reaction can still notify without badging. Changing that would alter
/// user-visible notification behaviour beyond the scope of #500.
pub(crate) fn is_notifiable_external_message(
    msg: &AuthorizedMessageV1,
    self_member_id: MemberId,
) -> bool {
    msg.message.author != self_member_id && !msg.message.content.is_event()
}

/// Lazily-built index of the ids of messages in `recent` authored by
/// `self_member_id` — the set a reply's target is tested against.
///
/// Exists because `AuthorizedMessageV1::id()` is a 64-byte rolling hash, so
/// the naive `recent.iter().any(|m| m.id() == target)` per reply is O(N)
/// hashes *per candidate message* (O(N²) over a reply-heavy buffer). Scanning
/// a whole room's unread tail (freenet/river#500) makes that the hot path, so
/// the ids are hashed once per scan and reused.
///
/// Built lazily via `OnceCell`: a scan whose candidates contain no replies
/// (the common case) never pays for the index at all.
pub(crate) struct SelfAuthoredIndex<'a> {
    recent: &'a [AuthorizedMessageV1],
    self_member_id: MemberId,
    ids: std::cell::OnceCell<std::collections::HashSet<MessageId>>,
}

impl<'a> SelfAuthoredIndex<'a> {
    pub(crate) fn new(recent: &'a [AuthorizedMessageV1], self_member_id: MemberId) -> Self {
        Self {
            recent,
            self_member_id,
            ids: std::cell::OnceCell::new(),
        }
    }

    /// The member this index treats as "self" — the single source of that
    /// answer for every caller, so no caller can supply a second one.
    pub(crate) fn self_member_id(&self) -> MemberId {
        self.self_member_id
    }

    /// Whether `target_id` names a message in `recent` authored by self.
    fn contains(&self, target_id: &MessageId) -> bool {
        self.ids
            .get_or_init(|| {
                self.recent
                    .iter()
                    .filter(|m| m.message.author == self.self_member_id)
                    .map(|m| m.id())
                    .collect()
            })
            .contains(target_id)
    }
}

/// Pure decision for [`crate::room_data::NotificationMode::MentionsAndReplies`]:
/// does `msg` @mention `self_member_id`, OR is it a reply to a message that
/// `self_member_id` authored among `recent`? No globals, so it is unit-testable.
///
/// The reply check resolves the reply's target id against `recent` and compares
/// that message's author to self. If the target has scrolled out of `recent` we
/// cannot confirm authorship, so we conservatively return `false` (miss rather
/// than spuriously notify).
///
/// Single-message convenience wrapper over
/// [`mentions_or_replies_to_self_indexed`].
///
/// `#[cfg(test)]`: production has no single-message caller left. The
/// notification gate and the unread counter both scan a batch against one
/// `recent`, so both build a [`SelfAuthoredIndex`] once and use the indexed
/// form; re-introducing a per-message wrapper on either path re-introduces the
/// per-message index rebuild it exists to avoid. Kept for the pure-decision
/// tests below, which are about the predicate rather than the batching.
///
/// `pub(crate)`: besides gating browser notifications here, this is the same
/// predicate the unread counters use for rooms in MentionsAndReplies mode
/// (`document_title::count_unread_in_room_data_with_mode`, freenet/river#500),
/// so the badge and the notification can't drift on what a mention or a reply
/// IS.
///
/// They do deliberately differ on one thing: a body that cannot be decrypted
/// at all. The counter counts it (see
/// `document_title::unreadable_body_may_become_readable`) because a wrong
/// number that resolves down is better than a badge that hides a mention. A
/// notification cannot be taken back, and every cold start of a private room
/// has an empty secrets map, so applying the same fail-safe here would fire a
/// burst of "you were mentioned" popups on every reload. The badge
/// over-reports; the notification under-reports; both fail toward the less
/// annoying error.
#[cfg(test)]
pub(crate) fn mentions_or_replies_to_self(
    msg: &AuthorizedMessageV1,
    self_member_id: MemberId,
    room_secrets: &std::collections::HashMap<u32, [u8; 32]>,
    recent: &[AuthorizedMessageV1],
) -> bool {
    mentions_or_replies_to_self_indexed(
        msg,
        room_secrets,
        &SelfAuthoredIndex::new(recent, self_member_id),
    )
}

/// [`mentions_or_replies_to_self`] against a pre-built (or lazily-built)
/// self-authored id index, so a multi-message scan hashes `recent` at most
/// once instead of once per candidate.
pub(crate) fn mentions_or_replies_to_self_indexed(
    msg: &AuthorizedMessageV1,
    room_secrets: &std::collections::HashMap<u32, [u8; 32]>,
    self_authored: &SelfAuthoredIndex<'_>,
) -> bool {
    use crate::components::conversation::decrypt_message_content;

    let text = decrypt_message_content(&msg.message.content, room_secrets);
    text_mentions_or_replies_to_self(msg, &text, room_secrets, self_authored)
}

/// [`mentions_or_replies_to_self_indexed`] against a body that has ALREADY
/// been decrypted, so a caller that needed the plaintext for its own reasons
/// does not pay to decrypt the same message again for the mention scan. (A
/// REPLY body is still decrypted a second time inside `extract_reply_target_id`
/// — that is pre-existing, and unchanged either way.)
///
/// This is the single definition of "qualifies" — the notification gate and
/// the unread counter both end up here, so they cannot drift on what counts
/// as a mention or a reply.
pub(crate) fn text_mentions_or_replies_to_self(
    msg: &AuthorizedMessageV1,
    text: &str,
    room_secrets: &std::collections::HashMap<u32, [u8; 32]>,
    self_authored: &SelfAuthoredIndex<'_>,
) -> bool {
    use crate::components::conversation::extract_reply_target_id;

    // "Self" is taken from the index and nowhere else. Passing it separately
    // as well let the mention arm and the reply arm disagree about whose
    // messages these are — the paired-field shape the project's bug-prevention
    // rules call out.
    if river_core::mention::contains_mention_of(text, self_authored.self_member_id()) {
        return true;
    }

    // (b) A reply to a message authored by self.
    if let Some(target_id) = extract_reply_target_id(&msg.message.content, room_secrets) {
        return self_authored.contains(&target_id);
    }
    false
}

// NOTE: there is deliberately no per-message `message_notifies_self` helper
// any more. It re-read `ROOMS` and rebuilt a `SelfAuthoredIndex` once per
// message, so a K-message delta did K signal reads and K index builds;
// `notify_new_messages` now reads once and builds one index for the batch.

/// Process new messages and show notifications if appropriate
/// Called from apply_delta when new messages are received
///
/// # Arguments
/// * `room_key` - The room's owner verifying key
/// * `new_messages` - New messages from the delta
/// * `self_member_id` - The current user's member ID (to filter out own messages)
/// * `member_info` - Member info for looking up sender names
/// * `room_secrets` - Map of secret_version -> decrypted secret for version-aware decryption
pub fn notify_new_messages(
    room_key: &VerifyingKey,
    new_messages: &[AuthorizedMessageV1],
    self_member_id: MemberId,
    member_info: &MemberInfoV1,
    room_secrets: &std::collections::HashMap<u32, [u8; 32]>,
) {
    // Skip if this room hasn't completed initial sync
    if !INITIAL_SYNC_COMPLETE.read().contains(room_key) {
        info!(
            "Initial sync not complete for room {:?}, skipping notification",
            MemberId::from(*room_key)
        );
        return;
    }
    info!(
        "notify_new_messages called for room {:?} with {} messages",
        MemberId::from(*room_key),
        new_messages.len()
    );

    // Skip if this is the currently active room AND document is visible
    // (user is looking at this room right now)
    let doc_visible = is_document_visible();
    if doc_visible {
        if let Some(current_key) = CURRENT_ROOM.read().owner_key {
            if current_key == *room_key {
                info!(
                    "Room {:?} is currently active and visible, skipping notification",
                    MemberId::from(*room_key)
                );
                return;
            }
        }
    }
    info!(
        "Document visible: {}, proceeding with notification check",
        doc_visible
    );

    // Skip if document is visible and focused AND we're in a different room
    // This prevents notifications while actively using River, but allows them
    // when the app is in background (minimized, different tab, etc.)
    // Note: We still notify for other rooms because users want to know about
    // activity in other conversations even while using the app.
    // Uncomment this block to suppress ALL notifications while app is focused:
    // if is_document_visible() {
    //     debug!("Document visible, skipping notification");
    //     return;
    // }

    // Filter to messages from other users, excluding room events.
    let external_messages: Vec<_> = new_messages
        .iter()
        .filter(|msg| is_notifiable_external_message(msg, self_member_id))
        .collect();

    info!(
        "Filtered to {} external messages (from {} total, self_member_id: {:?})",
        external_messages.len(),
        new_messages.len(),
        self_member_id
    );

    if external_messages.is_empty() {
        info!("No external messages to notify about");
        return;
    }

    // Apply the per-room notification preference (a local user setting).
    //
    // Read ONCE, and fallibly: `.read()` panics on a re-entrant borrow
    // (`.claude/rules/dioxus-signal-safety.md`), and this runs from
    // `apply_delta`. The fallback is `return`, NOT `unwrap_or_default()` — the
    // default is `All`, so defaulting here would notify a MUTED room, which is
    // exactly the setting the user chose to prevent. A missed notification on
    // an unreadable signal is the safe direction.
    //
    // The room's recent messages come out of the same read, so the per-message
    // mention check below does not re-read `ROOMS` (and does not rebuild its
    // self-authored index) once per message.
    let Ok(rooms) = ROOMS.try_read() else {
        warn!("ROOMS unreadable while deciding notifications; skipping");
        return;
    };
    let mode = rooms
        .notification_modes
        .get(room_key)
        .copied()
        .unwrap_or_default();
    // Only MentionsAndReplies needs the buffer, and it is the whole
    // `max_recent_messages` window (ciphertext included), so `All` — the
    // default — must not pay a deep clone of it on every delta.
    let recent: Vec<AuthorizedMessageV1> =
        if mode == crate::room_data::NotificationMode::MentionsAndReplies {
            rooms
                .map
                .get(room_key)
                .map(|rd| rd.room_state.recent_messages.messages.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
    drop(rooms);
    let external_messages: Vec<_> = match mode {
        crate::room_data::NotificationMode::All => external_messages,
        crate::room_data::NotificationMode::Muted => {
            info!(
                "Room {:?} is muted, skipping notification",
                MemberId::from(*room_key)
            );
            return;
        }
        crate::room_data::NotificationMode::MentionsAndReplies => {
            let self_authored = SelfAuthoredIndex::new(&recent, self_member_id);
            external_messages
                .into_iter()
                .filter(|msg| {
                    mentions_or_replies_to_self_indexed(msg, room_secrets, &self_authored)
                })
                .collect()
        }
    };
    if external_messages.is_empty() {
        info!("No messages match the room's mentions-and-replies filter");
        return;
    }

    // Get room name (decrypt if private). Fallible for the same reason as the
    // mode read above; unlike the mode, an unavailable NAME is cosmetic, so
    // the fallback is the generic label rather than a `return`.
    let room_name = ROOMS
        .try_read()
        .ok()
        .and_then(|rooms| {
            rooms.map.get(room_key).map(|rd| {
                let sealed_name = &rd.room_state.configuration.configuration.display.name;
                match unseal_bytes_with_secrets(sealed_name, room_secrets) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                    Err(_) => sealed_name.to_string_lossy(),
                }
            })
        })
        .unwrap_or_else(|| "Room".to_string());

    // For multiple messages, show a summary
    if external_messages.len() > 1 {
        show_notification(
            *room_key,
            &room_name,
            "",
            &format!("{} new messages", external_messages.len()),
        );
        return;
    }

    // Single message - show sender and preview
    let msg = external_messages[0];

    // Get sender name
    let sender_name = member_info
        .canonical(msg.message.author)
        .map(|ami| {
            crate::util::display_name::display_nickname(
                &ami.member_info.preferred_nickname,
                room_secrets,
            )
        })
        .unwrap_or_else(|| "Someone".to_string());

    // Get message preview (decrypt if needed)
    let preview = get_message_preview(&msg.message.content, room_secrets);

    show_notification(*room_key, &room_name, &sender_name, &preview);
}

/// Extract a preview from message content, decrypting if necessary
fn get_message_preview(
    content: &RoomMessageBody,
    room_secrets: &std::collections::HashMap<u32, [u8; 32]>,
) -> String {
    use river_core::room_state::content::{
        ActionContentV1, EventContentV1, ReplyContentV1, TextContentV1, ACTION_TYPE_DELETE,
        ACTION_TYPE_EDIT, ACTION_TYPE_REACTION, ACTION_TYPE_REMOVE_REACTION, CONTENT_TYPE_ACTION,
        CONTENT_TYPE_EVENT, CONTENT_TYPE_REPLY, CONTENT_TYPE_TEXT, EVENT_TYPE_JOIN,
    };

    let text = match content {
        RoomMessageBody::Public {
            content_type, data, ..
        } => match *content_type {
            CONTENT_TYPE_TEXT => TextContentV1::decode(data)
                .map(|t| t.text)
                .unwrap_or_else(|_| "[Failed to decode message]".to_string()),
            CONTENT_TYPE_ACTION => ActionContentV1::decode(data)
                .map(|action| match action.action_type {
                    ACTION_TYPE_EDIT => "[Edited a message]".to_string(),
                    ACTION_TYPE_DELETE => "[Deleted a message]".to_string(),
                    ACTION_TYPE_REACTION => action
                        .reaction_payload()
                        .map(|p| format!("Reacted with {}", p.emoji))
                        .unwrap_or_else(|| "[Reacted]".to_string()),
                    ACTION_TYPE_REMOVE_REACTION => "[Removed a reaction]".to_string(),
                    _ => "[Unknown action]".to_string(),
                })
                .unwrap_or_else(|_| "[Action]".to_string()),
            CONTENT_TYPE_REPLY => ReplyContentV1::decode(data)
                .map(|r| r.text)
                .unwrap_or_else(|_| "[Failed to decode reply]".to_string()),
            CONTENT_TYPE_EVENT => EventContentV1::decode(data)
                .map(|event| match event.event_type {
                    EVENT_TYPE_JOIN => "joined the room".to_string(),
                    _ => format!("[Event type {}]", event.event_type),
                })
                .unwrap_or_else(|_| "[Event]".to_string()),
            _ => "[Unknown message type]".to_string(),
        },
        RoomMessageBody::Private {
            content_type,
            ciphertext,
            nonce,
            secret_version,
            ..
        } => {
            // Look up the secret for this message's version
            if let Some(secret) = room_secrets.get(secret_version) {
                decrypt_with_symmetric_key(secret, ciphertext.as_slice(), nonce)
                    .map(|bytes| match *content_type {
                        CONTENT_TYPE_TEXT => TextContentV1::decode(&bytes)
                            .map(|t| t.text)
                            .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).to_string()),
                        CONTENT_TYPE_REPLY => ReplyContentV1::decode(&bytes)
                            .map(|r| r.text)
                            .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).to_string()),
                        CONTENT_TYPE_EVENT => EventContentV1::decode(&bytes)
                            .map(|event| match event.event_type {
                                EVENT_TYPE_JOIN => "joined the room".to_string(),
                                _ => format!("[Event type {}]", event.event_type),
                            })
                            .unwrap_or_else(|_| "[Event]".to_string()),
                        CONTENT_TYPE_ACTION => "[Action]".to_string(),
                        _ => String::from_utf8_lossy(&bytes).to_string(),
                    })
                    .unwrap_or_else(|_| "[Encrypted message]".to_string())
            } else {
                "[Encrypted message]".to_string()
            }
        }
    };

    // Truncate to ~50 chars for notification (use truncate_str to avoid
    // panicking on multi-byte emoji at the boundary).
    if text.len() > 50 {
        format!("{}...", crate::util::truncate_str(&text, 47))
    } else {
        text
    }
}

/// Mark a room as having completed initial sync
/// Should be called after first successful state load
pub fn mark_initial_sync_complete(room_key: &VerifyingKey) {
    INITIAL_SYNC_COMPLETE.write().insert(*room_key);
    info!(
        "Marked initial sync complete for room: {:?}",
        MemberId::from(*room_key)
    );
}

#[cfg(test)]
mod notify_gate_tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::message::MessageV1;
    use std::collections::HashMap;
    use std::time::SystemTime;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn id_of(sk: &SigningKey) -> MemberId {
        MemberId::from(&sk.verifying_key())
    }

    fn public_msg(author_sk: &SigningKey, content: RoomMessageBody) -> AuthorizedMessageV1 {
        let author = id_of(author_sk);
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: author,
                author,
                content,
                time: SystemTime::UNIX_EPOCH,
            },
            author_sk,
        )
    }

    /// A notification body renders as one flat `sender: message` line. Any
    /// character the renderer breaks on lets a message author paint a second
    /// line that reads as a notification from someone else, which is the exact
    /// impersonation this guard exists to stop.
    #[test]
    fn notification_body_flattens_every_line_break() {
        for (label, c) in [
            ("LINE FEED", '\n'),
            ("CARRIAGE RETURN", '\r'),
            ("NEXT LINE", '\u{0085}'),
            ("LINE SEPARATOR", '\u{2028}'),
            ("PARAGRAPH SEPARATOR", '\u{2029}'),
        ] {
            let forged = format!("hi{c}Ian Clarke \u{1F6E1}: everyone is banned");
            let body = notification_body("Mallory", &forged);
            assert!(
                !body.contains(c),
                "U+{:04X} {label} survived into a notification body: {body:?}",
                u32::from(c)
            );
            assert!(
                body.starts_with("Mallory: hi Ian"),
                "U+{:04X} {label} was not flattened to a space: {body:?}",
                u32::from(c)
            );
        }
    }

    #[test]
    fn notification_body_leaves_ordinary_text_alone() {
        assert_eq!(
            notification_body("Alice", "hello there"),
            "Alice: hello there"
        );
        // The multi-message summary path passes an empty sender name.
        assert_eq!(notification_body("", "3 new messages"), ": 3 new messages");
    }

    /// Both notification paths must build their body through
    /// [`notification_body`]. They are `wasm`-only, so no behavioural test in
    /// this crate can reach them: re-inlining the `format!` on either one
    /// re-opens the hole on that path alone and every other test stays green.
    #[test]
    fn both_notification_paths_go_through_notification_body() {
        let source = include_str!("notifications.rs");
        let (production, _) = source
            .split_once("mod notify_gate_tests")
            .expect("this test module's own declaration marks the end of production code");

        assert_eq!(
            production
                .matches("notification_body(sender_name, message_preview)")
                .count(),
            2,
            "expected exactly two call sites (`show_notification` and \
             `create_notification_internal`); a path stopped delegating"
        );
        assert!(
            !production.contains("\"{}: {}\""),
            "a notification path re-inlined the `sender: message` join, which \
             skips the line-break strip that only `notification_body` applies"
        );
    }

    /// Every status the shell can report, so a matrix test cannot silently skip
    /// a variant that a later edit adds.
    const ALL_STATUSES: [NotificationStatus; 6] = [
        NotificationStatus::Granted,
        NotificationStatus::Denied,
        NotificationStatus::Dismissed,
        NotificationStatus::Undecided,
        NotificationStatus::Undeliverable,
        NotificationStatus::Unsupported,
    ];

    /// The exact strings freenet-core's shell sends (`notifyStatusToIframe` in
    /// `shell_bridge.js`). This is a cross-repo wire contract: a rename on
    /// either side reverts #510 exactly — every status parses as `None`, is
    /// ignored, and the user is back to a silent failure with no retry.
    #[test]
    fn every_shell_status_string_parses() {
        for (sent, expected) in [
            ("granted", NotificationStatus::Granted),
            ("denied", NotificationStatus::Denied),
            ("dismissed", NotificationStatus::Dismissed),
            ("default", NotificationStatus::Undecided),
            ("undeliverable", NotificationStatus::Undeliverable),
            ("unsupported", NotificationStatus::Unsupported),
        ] {
            assert_eq!(
                NotificationStatus::from_shell_status(sent),
                Some(expected),
                "the shell sends {sent:?}; River must understand it"
            );
        }
    }

    /// A status River doesn't know is ignored, not guessed at: overwriting a
    /// known state with a wrong reading is worse than keeping the old one.
    #[test]
    fn an_unrecognised_status_is_ignored_rather_than_guessed() {
        // Matching is deliberately case- and whitespace-sensitive: the shell
        // sends exact lowercase tokens, so anything else is a different message.
        for junk in ["", "GRANTED", "granted ", "prompt", "blocked", "undefined"] {
            assert_eq!(
                NotificationStatus::from_shell_status(junk),
                None,
                "{junk:?} is not a status the shell sends"
            );
        }
    }

    /// Only an unanswered prompt re-arms the automatic ask. Re-arming on
    /// `Dismissed` would fight the shell's own 24h snooze (River would re-ask on
    /// every message and the shell would bounce each one straight back), and
    /// re-arming on a final answer would nag about a decision already made.
    #[test]
    fn only_an_unanswered_prompt_rearms_the_automatic_ask() {
        for status in ALL_STATUSES {
            let should_rearm = status == NotificationStatus::Undecided;
            assert_eq!(
                status_rearms_auto_prompt(status, 0),
                should_rearm,
                "{status:?} re-arm decision"
            );
        }
    }

    /// The re-arm is bounded, so a user who keeps closing the browser dialog
    /// isn't shown the shell's bar again on every message they send.
    #[test]
    fn the_automatic_rearm_is_bounded() {
        // Concrete numbers, NOT `MAX_ENABLE_PROMPT_REARMS` itself: phrased
        // against the constant this test passes for ANY cap, including no cap
        // at all — it stayed green under a mutation that raised the cap to
        // `usize::MAX`.
        assert!(
            MAX_ENABLE_PROMPT_REARMS <= 2,
            "more than a couple of automatic re-asks per session is a nag; the \
             manual button is the unbounded path"
        );
        assert!(
            status_rearms_auto_prompt(NotificationStatus::Undecided, 0),
            "the first re-arm must be allowed, or an unanswered prompt is final \
             and #510's no-retry complaint stands"
        );
        assert!(
            !status_rearms_auto_prompt(NotificationStatus::Undecided, 3),
            "the automatic ask must stop re-arming; otherwise the shell shows \
             its bar again on every message the user sends"
        );
    }

    /// Served top-level, the browser's live answer wins over anything River
    /// remembered. A user who flips the permission in site settings would
    /// otherwise be told "your browser is blocking notifications" for the rest
    /// of the session, with the Enable button withheld — a stale silent failure
    /// of the same shape #510 removes.
    #[test]
    fn the_browser_answer_wins_over_a_remembered_one_when_unframed() {
        assert_eq!(
            resolve_status(
                false,
                Some(NotificationStatus::Denied),
                Some(NotificationStatus::Granted)
            ),
            Some(NotificationStatus::Granted),
            "a remembered Denied must not survive the user allowing notifications"
        );
        assert_eq!(
            resolve_status(
                false,
                Some(NotificationStatus::Granted),
                Some(NotificationStatus::Denied)
            ),
            Some(NotificationStatus::Denied),
            "a remembered Granted must not survive the user blocking notifications"
        );
    }

    /// Framed, the shell's report is the only source there is: River's origin is
    /// opaque, so there is no browser answer to read.
    #[test]
    fn the_shell_report_is_the_only_source_when_framed() {
        assert_eq!(
            resolve_status(true, Some(NotificationStatus::Denied), None),
            Some(NotificationStatus::Denied)
        );
        assert_eq!(
            resolve_status(true, None, None),
            None,
            "nothing reported yet is its own state, not a guess"
        );
    }

    /// The retry button appears exactly where asking again can change the
    /// answer. Offering it against a browser-level block or an already-granted
    /// permission gives the user a button that does nothing — the same silent
    /// dead end #510 is about, just moved.
    #[test]
    fn enable_is_offered_exactly_where_asking_again_can_help() {
        assert!(
            permission_notice(None).offer_enable,
            "nothing known yet: the user must be able to ask"
        );
        for (status, offered) in [
            (NotificationStatus::Granted, false),
            (NotificationStatus::Denied, false),
            (NotificationStatus::Dismissed, true),
            (NotificationStatus::Undecided, true),
            (NotificationStatus::Undeliverable, false),
            (NotificationStatus::Unsupported, false),
        ] {
            assert_eq!(
                permission_notice(Some(status)).offer_enable,
                offered,
                "{status:?} enable-offer decision"
            );
        }
    }

    /// Every state says something, and no two states say the SAME thing —
    /// mapping a blocked permission onto the "enabled" text would leave the user
    /// with a confidently wrong answer, which is worse than the silence #510
    /// replaces.
    #[test]
    fn every_state_has_its_own_explanation() {
        let mut seen: Vec<&str> = Vec::new();
        for status in ALL_STATUSES.map(Some).into_iter().chain([None]) {
            let message = permission_notice(status).message;
            assert!(
                !message.trim().is_empty(),
                "{status:?} explains nothing to the user"
            );
            assert!(
                !seen.contains(&message),
                "{status:?} reuses another state's explanation: {message:?}"
            );
            seen.push(message);
        }
    }

    /// The listener body is a JS `MessageEvent` closure, so no native test in
    /// this crate can reach it: deleting the `notification_status` arm restores
    /// #510 exactly (every outcome discarded, nothing surfaced, no retry) with
    /// every behavioural test above still green. Pinned by source instead.
    #[test]
    fn the_shell_listener_consumes_notification_status() {
        let production = production_source();
        assert!(
            production.contains("ifkind==\"notification_status\""),
            "the shell listener stopped handling notification_status; #510 is back"
        );
        assert!(
            production.contains("Some(status)=>record_notification_status(status)"),
            "a parsed status is no longer recorded"
        );
    }

    /// The bell marker fires exactly where the user's own setting says they want
    /// notifications and the browser will not deliver them.
    #[test]
    fn the_bell_is_marked_only_when_a_wanted_notification_cannot_arrive() {
        use crate::room_data::NotificationMode::*;

        for mode in [All, MentionsAndReplies] {
            for (status, blocked) in [
                (Some(NotificationStatus::Denied), true),
                (Some(NotificationStatus::Undeliverable), true),
                (Some(NotificationStatus::Granted), false),
                (Some(NotificationStatus::Dismissed), false),
                (Some(NotificationStatus::Undecided), false),
                // Unsupported is excluded on purpose: nothing the user does can
                // clear it, so the marker would be permanent nagging.
                (Some(NotificationStatus::Unsupported), false),
                (None, false),
            ] {
                assert_eq!(
                    delivery_is_blocked(mode, status),
                    blocked,
                    "mode {mode:?} with status {status:?}"
                );
            }
        }
    }

    /// A muted room is never marked: the user asked for no notifications, so
    /// telling them they won't get any is noise, not information.
    #[test]
    fn a_muted_room_is_never_marked_as_blocked() {
        for status in ALL_STATUSES {
            assert!(
                !delivery_is_blocked(crate::room_data::NotificationMode::Muted, Some(status)),
                "muted room marked for status {status:?}"
            );
        }
    }

    /// The re-arm itself — the "reset the flag" half of #510 — is two statements
    /// against process-global atomics, which no behavioural test in this crate
    /// can drive (native tests run in parallel threads and would corrupt each
    /// other's view of the statics). Mutation testing showed BOTH lines could be
    /// deleted with the whole suite still green, so each shipped a silent
    /// regression:
    ///
    /// - Without `ENABLE_PROMPT_SENT.store(false, ...)` an unanswered prompt
    ///   never re-arms, which is precisely the "no way to retry" complaint.
    /// - Without `ENABLE_PROMPT_REARMS.fetch_add(1, ...)` the counter stays at
    ///   0, so `rearms_used < MAX` is always true and the re-arm becomes
    ///   UNBOUNDED — the shell's affordance bar returns on every message the
    ///   user sends, the exact nag the cap exists to prevent.
    ///
    /// `the_automatic_rearm_is_bounded` does NOT cover the second one: it feeds
    /// the predicate a caller-supplied count, so it passes with the increment
    /// deleted. A source pin has no parallelism problem, so it is the right
    /// instrument here.
    #[test]
    fn an_unanswered_prompt_actually_rearms_the_flag_it_bounds() {
        let production = production_source();

        assert!(
            production.contains("ENABLE_PROMPT_SENT.store(false,Ordering::SeqCst);"),
            "nothing clears ENABLE_PROMPT_SENT, so an unanswered prompt can \
             never be re-offered — #510's 'no way to retry' is back"
        );
        assert!(
            production.contains("ENABLE_PROMPT_REARMS.fetch_add(1,Ordering::SeqCst);"),
            "nothing advances ENABLE_PROMPT_REARMS, so the cap never binds and \
             the shell's bar returns on every message sent"
        );
        // Both statements, in order, behind the bounded predicate: the pin is
        // about the wiring, not merely about the two lines existing somewhere.
        assert!(
            production.contains(
                "ifstatus_rearms_auto_prompt(status,ENABLE_PROMPT_REARMS.load(Ordering::SeqCst)){\
                 ENABLE_PROMPT_REARMS.fetch_add(1,Ordering::SeqCst);\
                 ENABLE_PROMPT_SENT.store(false,Ordering::SeqCst);}"
            ),
            "the re-arm is no longer gated by status_rearms_auto_prompt, or the \
             two statements it guards have moved apart"
        );
    }

    /// The status write originates in a JS callback, so it must go through
    /// `crate::util::defer` — a direct write is the Firefox-mobile RefCell
    /// re-entrancy crash path (`.claude/rules/dioxus-signal-safety.md`).
    /// Routing every write through `record_notification_status` also keeps the
    /// re-arm from being bypassed by a second writer.
    #[test]
    fn the_status_write_is_deferred_and_has_one_writer() {
        let production = production_source();
        assert!(
            production.contains(
                "crate::util::defer(move||{*NOTIFICATION_STATUS.write()=Some(status);});"
            ),
            "the NOTIFICATION_STATUS write must stay inside crate::util::defer"
        );
        assert_eq!(
            production.matches("NOTIFICATION_STATUS.write()").count(),
            1,
            "NOTIFICATION_STATUS must have exactly one writer \
             (record_notification_status), so no path can skip the re-arm"
        );
    }

    /// `Notification::request_permission()` must be called BEFORE the spawn, so
    /// it runs inside the click's transient user activation. Moving it inside
    /// `safe_spawn_local` (a `setTimeout(0)`) loses that activation and Firefox
    /// and Safari reject the request with `NotAllowedError`, which the caller
    /// only sees as a warning — the exact latent failure #510 describes for the
    /// existing auto path.
    #[test]
    fn the_direct_permission_request_stays_inside_the_click_gesture() {
        let source = include_str!("notifications.rs");
        let (production, _) = source
            .split_once("mod notify_gate_tests")
            .expect("this test module's own declaration marks the end of production code");
        let body = production
            .split_once("fn request_permission_directly() {")
            .expect("request_permission_directly is the direct-path entry point")
            .1
            .split_once("\n}\n")
            .expect("a top-level fn ends with a column-zero closing brace")
            .0;
        let body: String = body.chars().filter(|c| !c.is_whitespace()).collect();

        let request = body
            .find("Notification::request_permission()")
            .expect("the direct path must request permission");
        let spawn = body
            .find("safe_spawn_local")
            .expect("the promise must be awaited off the click stack");
        assert!(
            request < spawn,
            "request_permission() moved inside the spawn, so the browser no \
             longer sees the click's user activation"
        );
    }

    /// This file's production source with whitespace stripped, so the source
    /// pins above survive `cargo fmt` re-wrapping.
    fn production_source() -> String {
        let source = include_str!("notifications.rs");
        let (production, _) = source
            .split_once("mod notify_gate_tests")
            .expect("this test module's own declaration marks the end of production code");
        production.chars().filter(|c| !c.is_whitespace()).collect()
    }

    #[test]
    fn notification_tag_round_trips_room_key() {
        // The gateway iframe path encodes the room's owner key as the
        // notification `tag`; a `notification_click` reply carries it back and
        // must decode to the same key so we route to the right room (#408).
        let owner = key(9).verifying_key();
        let tag = room_key_to_tag(&owner);
        assert_eq!(tag_to_room_key(&tag), Some(owner));
    }

    #[test]
    fn malformed_notification_tag_is_rejected() {
        assert_eq!(tag_to_room_key(""), None);
        // Contains characters outside the base58 alphabet (0, O, I, l).
        assert_eq!(tag_to_room_key("0OIl"), None);
        // Valid base58 but the wrong byte length (not a 32-byte key).
        assert_eq!(
            tag_to_room_key(&bs58::encode([1u8; 16]).into_string()),
            None
        );
    }

    #[test]
    fn notifies_on_mention_of_self() {
        let me = key(1);
        let other = key(2);
        let secrets = HashMap::new();
        let text = format!(
            "hey {}!",
            river_core::mention::encode_mention(id_of(&me), "Me")
        );
        let msg = public_msg(&other, RoomMessageBody::public(text));
        assert!(mentions_or_replies_to_self(&msg, id_of(&me), &secrets, &[]));
    }

    #[test]
    fn does_not_notify_on_mention_of_someone_else() {
        let me = key(1);
        let other = key(2);
        let secrets = HashMap::new();
        let text = format!(
            "hey {}!",
            river_core::mention::encode_mention(id_of(&other), "Other")
        );
        let msg = public_msg(&other, RoomMessageBody::public(text));
        assert!(!mentions_or_replies_to_self(
            &msg,
            id_of(&me),
            &secrets,
            &[]
        ));
    }

    #[test]
    fn notifies_on_reply_to_own_message() {
        let me = key(1);
        let other = key(2);
        let secrets = HashMap::new();
        let target = public_msg(&me, RoomMessageBody::public("my message".to_string()));
        let reply = public_msg(
            &other,
            RoomMessageBody::reply(
                "agreed".to_string(),
                target.id(),
                "Me".to_string(),
                "my message".to_string(),
            ),
        );
        assert!(mentions_or_replies_to_self(
            &reply,
            id_of(&me),
            &secrets,
            &[target]
        ));
    }

    #[test]
    fn does_not_notify_on_reply_to_other_message() {
        let me = key(1);
        let other = key(2);
        let secrets = HashMap::new();
        // Target authored by `other`, not by `me`.
        let target = public_msg(&other, RoomMessageBody::public("their message".to_string()));
        let reply = public_msg(
            &me,
            RoomMessageBody::reply(
                "ok".to_string(),
                target.id(),
                "Other".to_string(),
                "their message".to_string(),
            ),
        );
        assert!(!mentions_or_replies_to_self(
            &reply,
            id_of(&me),
            &secrets,
            &[target]
        ));
    }

    #[test]
    fn does_not_notify_when_reply_target_scrolled_out() {
        let me = key(1);
        let other = key(2);
        let secrets = HashMap::new();
        let target = public_msg(&me, RoomMessageBody::public("old message".to_string()));
        let reply = public_msg(
            &other,
            RoomMessageBody::reply(
                "re".to_string(),
                target.id(),
                "Me".to_string(),
                "old message".to_string(),
            ),
        );
        // Target not in the recent window → conservatively no notification.
        assert!(!mentions_or_replies_to_self(
            &reply,
            id_of(&me),
            &secrets,
            &[]
        ));
    }

    #[test]
    fn room_events_are_not_notifiable() {
        // freenet/river#500: room events are excluded from every unread
        // count, so notifying on one would make the notification and the
        // badge disagree by construction (a join pops a notification while
        // contributing to no badge).
        let me = key(1);
        let other = key(2);
        let join = public_msg(&other, RoomMessageBody::join_event());
        assert!(!is_notifiable_external_message(&join, id_of(&me)));
        // An ordinary message from someone else still notifies…
        let chat = public_msg(&other, RoomMessageBody::public("hello".to_string()));
        assert!(is_notifiable_external_message(&chat, id_of(&me)));
        // …and self-authored messages still don't.
        let mine = public_msg(&me, RoomMessageBody::public("hello".to_string()));
        assert!(!is_notifiable_external_message(&mine, id_of(&me)));
    }

    /// `notify_new_messages` is `wasm`-only, so no behavioural test in this
    /// crate can reach it: re-inlining the author-only filter would silently
    /// restore join-event notifications with every other test still green.
    #[test]
    fn notify_new_messages_filters_through_the_notifiable_predicate() {
        let source = include_str!("notifications.rs");
        let (production, _) = source
            .split_once("mod notify_gate_tests")
            .expect("this test module's own declaration marks the end of production code");
        // Whitespace-stripped: rustfmt re-wraps a growing call's arguments, and
        // a contiguous-text needle would go silently vacuous the first time it
        // did (this repo has been bitten by exactly that).
        let squashed: String = production.chars().filter(|c| !c.is_whitespace()).collect();

        assert!(
            squashed.contains("is_notifiable_external_message(msg,self_member_id)"),
            "the new-message filter stopped delegating to \
             `is_notifiable_external_message`, so room events can notify again"
        );
        // The pre-fix predicate, re-inlined, would drop the event exclusion.
        assert!(
            !squashed.contains("filter(|msg|msg.message.author!=self_member_id)"),
            "the author-only filter was re-inlined, which skips the room-event \
             exclusion that keeps notifications and unread badges in agreement"
        );
    }

    #[test]
    fn plain_message_does_not_notify() {
        let me = key(1);
        let other = key(2);
        let secrets = HashMap::new();
        let msg = public_msg(&other, RoomMessageBody::public("just chatting".to_string()));
        assert!(!mentions_or_replies_to_self(
            &msg,
            id_of(&me),
            &secrets,
            &[]
        ));
    }
}
