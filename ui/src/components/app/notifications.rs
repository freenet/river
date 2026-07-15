//! Browser notification support for new chat messages.
//!
//! Sends desktop notifications when:
//! - Document is not visible (tab hidden or window not focused)
//! - Message is from another user (not self)
//! - Room is not currently active
//! - Permission has been granted

use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::room_data::CurrentRoom;
use crate::util::ecies::{decrypt_with_symmetric_key, unseal_bytes_with_secrets};
use dioxus::logger::tracing::{debug, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::MemberInfoV1;
use river_core::room_state::message::{AuthorizedMessageV1, RoomMessageBody};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
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
static ENABLE_PROMPT_SENT: AtomicBool = AtomicBool::new(false);
/// Install the `notification_click` listener at most once.
static CLICK_LISTENER_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Whether River is running inside the gateway's sandboxed iframe (opaque
/// origin, no Notifications API) rather than as a top-level page. In the
/// gateway `window.parent` is the shell page — a different window; served
/// directly, `window.parent === window`.
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

/// Listen for `notification_click` replies from the shell and route to the room.
/// No-op when not in the shell iframe, and installs the listener at most once.
/// The iframe's window is only reachable by its parent shell, so we don't need
/// a separate source check; the payload is validated defensively.
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
        let body = format!("{}: {}", sender_name, message_preview);
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
    let body = format!("{}: {}", sender_name, message_preview);
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

/// Pure decision for [`crate::room_data::NotificationMode::MentionsAndReplies`]:
/// does `msg` @mention `self_member_id`, OR is it a reply to a message that
/// `self_member_id` authored among `recent`? No globals, so it is unit-testable.
///
/// The reply check resolves the reply's target id against `recent` and compares
/// that message's author to self. If the target has scrolled out of `recent` we
/// cannot confirm authorship, so we conservatively return `false` (miss rather
/// than spuriously notify).
fn mentions_or_replies_to_self(
    msg: &AuthorizedMessageV1,
    self_member_id: MemberId,
    room_secrets: &std::collections::HashMap<u32, [u8; 32]>,
    recent: &[AuthorizedMessageV1],
) -> bool {
    use crate::components::conversation::{decrypt_message_content, extract_reply_context};

    // (a) An @mention of self anywhere in the (decrypted) message text.
    let text = decrypt_message_content(&msg.message.content, room_secrets);
    if river_core::mention::contains_mention_of(&text, self_member_id) {
        return true;
    }

    // (b) A reply to a message authored by self.
    let (_, _, target_id) = extract_reply_context(&msg.message.content, room_secrets);
    if let Some(target_id) = target_id {
        return recent
            .iter()
            .any(|m| m.id() == target_id && m.message.author == self_member_id);
    }
    false
}

/// Whether `msg` should notify the local user under
/// [`crate::room_data::NotificationMode::MentionsAndReplies`]: it either
/// @mentions them or is a reply to one of their own messages.
///
/// Reads `recent_messages` from `ROOMS` (needed for reply-authorship) and
/// delegates the decision to the pure [`mentions_or_replies_to_self`] helper.
fn message_notifies_self(
    msg: &AuthorizedMessageV1,
    self_member_id: MemberId,
    room_key: &VerifyingKey,
    room_secrets: &std::collections::HashMap<u32, [u8; 32]>,
) -> bool {
    if let Ok(rooms) = ROOMS.try_read() {
        if let Some(rd) = rooms.map.get(room_key) {
            return mentions_or_replies_to_self(
                msg,
                self_member_id,
                room_secrets,
                &rd.room_state.recent_messages.messages,
            );
        }
    }
    // ROOMS unreadable / room missing: the @mention check needs no room
    // context, so still honour it with an empty recent-message window.
    mentions_or_replies_to_self(msg, self_member_id, room_secrets, &[])
}

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

    // Filter to messages from other users
    let external_messages: Vec<_> = new_messages
        .iter()
        .filter(|msg| msg.message.author != self_member_id)
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
    let mode = ROOMS
        .read()
        .notification_modes
        .get(room_key)
        .copied()
        .unwrap_or_default();
    let external_messages: Vec<_> = match mode {
        crate::room_data::NotificationMode::All => external_messages,
        crate::room_data::NotificationMode::Muted => {
            info!(
                "Room {:?} is muted, skipping notification",
                MemberId::from(*room_key)
            );
            return;
        }
        crate::room_data::NotificationMode::MentionsAndReplies => external_messages
            .into_iter()
            .filter(|msg| message_notifies_self(msg, self_member_id, room_key, room_secrets))
            .collect(),
    };
    if external_messages.is_empty() {
        info!("No messages match the room's mentions-and-replies filter");
        return;
    }

    // Get room name (decrypt if private)
    let room_name = ROOMS
        .read()
        .map
        .get(room_key)
        .map(|rd| {
            let sealed_name = &rd.room_state.configuration.configuration.display.name;
            match unseal_bytes_with_secrets(sealed_name, room_secrets) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(_) => sealed_name.to_string_lossy(),
            }
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
        .member_info
        .iter()
        .find(|ami| ami.member_info.member_id == msg.message.author)
        .map(|ami| {
            match unseal_bytes_with_secrets(&ami.member_info.preferred_nickname, room_secrets) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(_) => ami.member_info.preferred_nickname.to_string_lossy(),
            }
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
