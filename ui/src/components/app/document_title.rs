//! Document title management for River chat application.
//!
//! Handles:
//! - Setting document.title to room name when a room is selected
//! - Setting document.title to "River" when no room is selected
//! - Showing unread message count in title when tab is hidden
//! - Tracking document visibility state
//! - Marking messages as read when tab becomes visible

use crate::components::app::chat_delegate::save_rooms_to_delegate;
use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::logger::tracing::{debug, info, warn};
use dioxus::prelude::*;
use river_core::room_state::member::MemberId;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::VisibilityState;

const APP_NAME: &str = "River";

/// Global signal tracking whether the document is currently visible
pub static DOCUMENT_VISIBLE: GlobalSignal<bool> = Global::new(|| true);

/// Tracks whether the document title manager has been initialized
static TITLE_MANAGER_INITIALIZED: GlobalSignal<bool> = Global::new(|| false);

/// Global signal tracking total unread messages across all rooms
pub static TOTAL_UNREAD_COUNT: GlobalSignal<usize> = Global::new(|| 0);

/// Get the current document visibility state
fn get_visibility_state() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .map(|d| d.visibility_state() == VisibilityState::Visible)
        .unwrap_or(true)
}

/// Set the document title
fn set_document_title(title: &str) {
    if let Some(window) = web_sys::window() {
        if let Some(document) = window.document() {
            document.set_title(title);
        }
        // Notify parent shell page to update browser tab title
        let escaped = title.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = js_sys::eval(&format!(
            r#"try {{ window.parent.postMessage({{"__freenet_shell__":true,"type":"title","title":"{}"}}, "*") }} catch(e) {{}}"#,
            escaped
        ));
    }
}

/// Get the current room name (decrypted if private)
fn get_current_room_name() -> Option<String> {
    let current_room = CURRENT_ROOM.read();
    let owner_key = current_room.owner_key?;

    let rooms = ROOMS.read();
    let room_data = rooms.map.get(&owner_key)?;

    let sealed_name = &room_data
        .room_state
        .configuration
        .configuration
        .display
        .name;
    match unseal_bytes_with_secrets(sealed_name, &room_data.secrets) {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).to_string()),
        Err(_) => Some(sealed_name.to_string_lossy()),
    }
}

/// Count unread messages in a room.
/// Only counts display messages (non-action, non-deleted) from other users.
pub fn count_unread_messages_in_room(room_owner: &ed25519_dalek::VerifyingKey) -> usize {
    let rooms = ROOMS.read();
    let Some(room_data) = rooms.map.get(room_owner) else {
        return 0;
    };

    let self_member_id: MemberId = room_data.self_sk.verifying_key().into();
    let last_read_id = room_data.last_read_message_id.as_ref();

    // If no messages have been read, count all messages from others
    // If we have a last read ID, count messages after it from others
    let mut found_last_read = last_read_id.is_none();
    let mut unread_count = 0;

    for msg in room_data.room_state.recent_messages.display_messages() {
        let msg_id = msg.id();

        // Check if this is the last read message
        if let Some(last_id) = last_read_id {
            if &msg_id == last_id {
                found_last_read = true;
                continue; // Don't count the last read message itself
            }
        }

        // Count messages after the last read message from other users
        if found_last_read && msg.message.author != self_member_id {
            unread_count += 1;
        }
    }

    unread_count
}

/// Count total unread messages across all rooms
pub fn count_total_unread_messages() -> usize {
    let rooms = ROOMS.read();
    rooms.map.keys().map(count_unread_messages_in_room).sum()
}

/// Update the document title based on current state
pub fn update_document_title() {
    let is_visible = *DOCUMENT_VISIBLE.read();
    let room_name = get_current_room_name();
    let unread_count = count_total_unread_messages();

    // Update the global unread count signal
    *TOTAL_UNREAD_COUNT.write() = unread_count;

    let title = match (room_name, is_visible, unread_count) {
        // Room selected, tab visible (or no unread) - just show room name
        (Some(name), true, _) => name,
        (Some(name), false, 0) => name,

        // Room selected, tab hidden with unread messages - show count
        (Some(name), false, count) => format!("({}) {}", count, name),

        // No room selected, tab visible (or no unread) - show app name
        (None, true, _) => APP_NAME.to_string(),
        (None, false, 0) => APP_NAME.to_string(),

        // No room selected, tab hidden with unread messages - show count
        (None, false, count) => format!("({}) {}", count, APP_NAME),
    };

    set_document_title(&title);
}

/// Mark all messages in the current room as read
pub fn mark_current_room_as_read() {
    let current_room = CURRENT_ROOM.read();
    let Some(owner_key) = current_room.owner_key else {
        return;
    };

    // Get the latest message ID
    let latest_message_id = {
        let rooms = ROOMS.read();
        let Some(room_data) = rooms.map.get(&owner_key) else {
            return;
        };

        // Get the last display message ID
        room_data
            .room_state
            .recent_messages
            .display_messages()
            .last()
            .map(|msg| msg.id())
    };

    let Some(new_last_read_id) = latest_message_id else {
        return; // No messages to mark as read
    };

    // Check if we need to update
    {
        let rooms = ROOMS.read();
        if let Some(room_data) = rooms.map.get(&owner_key) {
            if room_data.last_read_message_id.as_ref() == Some(&new_last_read_id) {
                return; // Already marked as read
            }
        }
    }

    // Update the last read message ID
    ROOMS.with_mut(|rooms| {
        if let Some(room_data) = rooms.map.get_mut(&owner_key) {
            info!("Marking room as read up to message {:?}", new_last_read_id);
            room_data.last_read_message_id = Some(new_last_read_id);
        }
    });

    // Save to delegate
    wasm_bindgen_futures::spawn_local(async {
        if let Err(e) = save_rooms_to_delegate().await {
            warn!("Failed to save rooms after marking as read: {}", e);
        }
    });

    // Update title
    update_document_title();
}

/// Handle visibility change event
fn on_visibility_change() {
    let is_visible = get_visibility_state();
    debug!("Visibility changed: {}", is_visible);

    *DOCUMENT_VISIBLE.write() = is_visible;

    if is_visible {
        // Tab became visible - mark current room as read
        mark_current_room_as_read();
    }

    update_document_title();
}

/// Initialize the document title management system.
/// Should be called once when the app starts.
pub fn init_document_title_manager() {
    // Only initialize once
    if *TITLE_MANAGER_INITIALIZED.read() {
        return;
    }
    *TITLE_MANAGER_INITIALIZED.write() = true;

    // Set initial visibility state
    *DOCUMENT_VISIBLE.write() = get_visibility_state();

    // Set initial title
    update_document_title();

    // Add visibility change listener
    if let Some(document) = web_sys::window().and_then(|w| w.document()) {
        let callback = Closure::wrap(Box::new(move || {
            on_visibility_change();
        }) as Box<dyn Fn()>);

        document
            .add_event_listener_with_callback("visibilitychange", callback.as_ref().unchecked_ref())
            .expect("Failed to add visibilitychange listener");

        // Leak the closure to keep it alive for the lifetime of the app
        callback.forget();

        info!("Document title manager initialized");
    }
}

/// Hook to use in components that need to track document title updates.
/// Call this in the App component to ensure title updates when room changes.
#[component]
pub fn DocumentTitleUpdater() -> Element {
    // Track current room changes
    let current_room = CURRENT_ROOM.read();
    let _current_room_key = current_room.owner_key;

    // Track room data changes (for message count updates)
    let rooms = ROOMS.read();
    let _rooms_version = rooms.map.len(); // Simple trigger for reactivity

    // Update title on changes
    use_effect(move || {
        update_document_title();

        // If visible and a room is selected, mark as read
        if *DOCUMENT_VISIBLE.read() && CURRENT_ROOM.read().owner_key.is_some() {
            mark_current_room_as_read();
        }
    });

    // Initialize on first render
    use_effect(|| {
        init_document_title_manager();
    });

    rsx! {}
}
