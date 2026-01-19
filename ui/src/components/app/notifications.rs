//! Browser notification support for new chat messages.
//!
//! Sends desktop notifications when:
//! - Document is not visible (tab hidden or window not focused)
//! - Message is from another user (not self)
//! - Room is not currently active
//! - Permission has been granted

use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::room_data::CurrentRoom;
use crate::util::ecies::decrypt_with_symmetric_key;
use dioxus::logger::tracing::{debug, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::MemberInfoV1;
use river_core::room_state::message::{AuthorizedMessageV1, RoomMessageBody};
use std::collections::HashSet;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Notification, NotificationOptions, NotificationPermission, VisibilityState};

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

/// Request notification permission on first user message.
///
/// This follows best practices:
/// - Only prompts once per browser (persisted in localStorage)
/// - Respects if user already granted or denied permission
/// - Triggered by user action (sending a message) for better UX
pub fn request_permission_on_first_message() {
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

    wasm_bindgen_futures::spawn_local(async {
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

/// Process new messages and show notifications if appropriate
/// Called from apply_delta when new messages are received
///
/// # Arguments
/// * `room_key` - The room's owner verifying key
/// * `new_messages` - New messages from the delta
/// * `self_member_id` - The current user's member ID (to filter out own messages)
/// * `member_info` - Member info for looking up sender names
/// * `room_secret` - Optional room secret for decrypting private messages
/// * `room_secret_version` - Version of the room secret
pub fn notify_new_messages(
    room_key: &VerifyingKey,
    new_messages: &[AuthorizedMessageV1],
    self_member_id: MemberId,
    member_info: &MemberInfoV1,
    room_secret: Option<&[u8; 32]>,
    room_secret_version: Option<u32>,
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

    // Get room name
    let room_name = ROOMS
        .read()
        .map
        .get(room_key)
        .map(|rd| {
            rd.room_state
                .configuration
                .configuration
                .display
                .name
                .to_string_lossy()
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
        .map(|ami| ami.member_info.preferred_nickname.to_string_lossy())
        .unwrap_or_else(|| "Someone".to_string());

    // Get message preview (decrypt if needed)
    let preview = get_message_preview(&msg.message.content, room_secret, room_secret_version);

    show_notification(*room_key, &room_name, &sender_name, &preview);
}

/// Extract a preview from message content, decrypting if necessary
fn get_message_preview(
    content: &RoomMessageBody,
    room_secret: Option<&[u8; 32]>,
    room_secret_version: Option<u32>,
) -> String {
    let text = match content {
        RoomMessageBody::Public { plaintext } => plaintext.clone(),
        RoomMessageBody::Private {
            ciphertext,
            nonce,
            secret_version,
        } => {
            if let (Some(secret), Some(current_version)) = (room_secret, room_secret_version) {
                if current_version == *secret_version {
                    decrypt_with_symmetric_key(secret, ciphertext.as_slice(), nonce)
                        .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
                        .unwrap_or_else(|_| "[Encrypted message]".to_string())
                } else {
                    "[Encrypted message]".to_string()
                }
            } else {
                "[Encrypted message]".to_string()
            }
        }
    };

    // Truncate to ~50 chars for notification
    if text.len() > 50 {
        format!("{}...", &text[..47])
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
