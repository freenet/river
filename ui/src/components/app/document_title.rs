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
use river_core::room_state::message::MessageId;
use std::cell::RefCell;
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

thread_local! {
    /// Cache the last title to avoid redundant postMessage calls
    static LAST_TITLE: RefCell<String> = RefCell::new(String::new());
}

/// Get the current document visibility state
fn get_visibility_state() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .map(|d| d.visibility_state() == VisibilityState::Visible)
        .unwrap_or(true)
}

/// Set the document title, notifying the parent shell via postMessage.
/// Skips the postMessage if the title hasn't changed since the last call.
fn set_document_title(title: &str) {
    if let Some(window) = web_sys::window() {
        if let Some(document) = window.document() {
            document.set_title(title);
        }
        // Only postMessage to parent if the title actually changed
        let changed = LAST_TITLE.with(|last| {
            let mut last = last.borrow_mut();
            if *last == title {
                return false;
            }
            last.clear();
            last.push_str(title);
            true
        });
        if changed {
            // Build the message object via js_sys instead of eval()
            let msg = js_sys::Object::new();
            let _ = js_sys::Reflect::set(
                &msg,
                &JsValue::from_str("__freenet_shell__"),
                &JsValue::TRUE,
            );
            let _ = js_sys::Reflect::set(
                &msg,
                &JsValue::from_str("type"),
                &JsValue::from_str("title"),
            );
            let _ =
                js_sys::Reflect::set(&msg, &JsValue::from_str("title"), &JsValue::from_str(title));
            // Post to parent window (River runs inside an iframe)
            let target = window.parent().ok().flatten().unwrap_or(window);
            let _ = target.post_message(&msg, "*");
        }
    }
}

/// Get the current room name (decrypted if private)
fn get_current_room_name() -> Option<String> {
    let current_room = CURRENT_ROOM.read();
    let owner_key = current_room.owner_key?;

    let rooms = ROOMS.try_read().ok()?;
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

/// Count unread messages in a single room's [`RoomData`].
///
/// Counts display messages (non-action, non-deleted) authored by other
/// users that fall after `last_read_message_id`. Pure — takes a borrowed
/// `RoomData` so callers that already hold a `ROOMS` read guard (the
/// room-list badge memo, the title's cross-room sum) don't re-lock.
///
/// The marker is located in the full ordered message list, not the
/// display-filtered view: a last-read message that was later *deleted* is
/// still a valid position marker, so messages read before it stay read.
/// Only a marker entirely absent from the buffer (evicted by the bounded
/// ring buffer) triggers the "treat everything as unread" fallback —
/// otherwise a stale marker would silently report zero unread.
///
/// Assumes `recent.messages` is in chronological `(time, id)` order — the
/// invariant `MessagesV1::apply_delta` maintains — so the slice after the
/// marker's index is exactly the set of messages newer than the marker.
pub fn count_unread_in_room_data(room_data: &crate::room_data::RoomData) -> usize {
    let self_member_id: MemberId = room_data.self_sk.verifying_key().into();
    let recent = &room_data.room_state.recent_messages;

    // Index just past the last-read marker. No marker — or a marker that has
    // been evicted from the buffer entirely — starts at 0 (everything counts).
    let start = match room_data.last_read_message_id.as_ref() {
        None => 0,
        Some(id) => match recent.messages.iter().position(|m| &m.id() == id) {
            Some(idx) => idx + 1,
            None => 0,
        },
    };

    recent.messages[start..]
        .iter()
        // Mirror `MessagesV1::display_messages`: skip action and deleted msgs.
        .filter(|m| {
            !m.message.content.is_action() && !recent.actions_state.deleted.contains(&m.id())
        })
        .filter(|m| m.message.author != self_member_id)
        .count()
}

/// Count total unread messages across all rooms — room messages plus
/// inbound direct messages whose timestamp is newer than the per-pair
/// last-seen value the user advanced by opening the corresponding DM
/// thread (see [`crate::components::direct_messages::DM_LAST_SEEN`]).
///
/// DM unread is tab-title-relevant because the issue lists "incoming DM
/// notifications + unread tracking" as a single line item — without this
/// the inbox badge would update but the document title wouldn't.
pub fn count_total_unread_messages() -> usize {
    let Ok(rooms) = ROOMS.try_read() else {
        return 0;
    };
    let room_unread: usize = rooms.map.values().map(count_unread_in_room_data).sum();
    let dm_unread: usize = count_unread_dms(&rooms);
    room_unread + dm_unread
}

fn count_unread_dms(rooms: &crate::room_data::Rooms) -> usize {
    // `try_read` — never `.read()` — on a global signal that is mutated
    // from `defer()` callbacks. See AGENTS.md "Dioxus WASM Signal Safety
    // Rules"; getting this wrong is a latent re-entrant-borrow panic on
    // Firefox.
    let last_seen = match crate::components::direct_messages::DM_LAST_SEEN.try_read() {
        Ok(g) => g.clone(),
        Err(_) => return 0,
    };
    let mut total = 0usize;
    for (owner_key, room_data) in &rooms.map {
        let self_id: MemberId = room_data.self_sk.verifying_key().into();
        for msg in &room_data.room_state.direct_messages.messages {
            if msg.message.recipient != self_id {
                continue;
            }
            let cutoff = last_seen
                .get(&(*owner_key, msg.message.sender))
                .copied()
                .unwrap_or(0);
            if msg.message.timestamp > cutoff {
                total += 1;
            }
        }
    }
    total
}

/// Update the document title based on current state
pub fn update_document_title() {
    let is_visible = *DOCUMENT_VISIBLE.read();
    let room_name = get_current_room_name();
    let unread_count = count_total_unread_messages();

    // Update the global unread count signal
    *TOTAL_UNREAD_COUNT.write() = unread_count;

    let title = match (room_name, is_visible, unread_count) {
        // Room selected, tab visible (or no unread)
        (Some(name), true, _) | (Some(name), false, 0) => {
            format!("{} - {}", APP_NAME, name)
        }

        // Room selected, tab hidden with unread messages
        (Some(name), false, count) => format!("({}) {} - {}", count, APP_NAME, name),

        // No room selected, tab visible (or no unread)
        (None, true, _) | (None, false, 0) => APP_NAME.to_string(),

        // No room selected, tab hidden with unread messages
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
        let Ok(rooms) = ROOMS.try_read() else {
            return;
        };
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
        let Ok(rooms) = ROOMS.try_read() else {
            return;
        };
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

    // Use safe_spawn_local to avoid re-entrant borrow of wasm-bindgen-futures
    crate::util::safe_spawn_local(async {
        if let Err(e) = save_rooms_to_delegate().await {
            warn!("Failed to save rooms after marking as read: {}", e);
        }
    });

    // Update title
    update_document_title();
}

/// Mark every room as read up to its latest currently-known message.
///
/// Called when the tab transitions from visible to hidden: the user had the
/// chance to see anything already in state, so only messages arriving *after*
/// this point should count as unread in the title badge.
pub fn mark_all_rooms_as_read() {
    let updates: Vec<(ed25519_dalek::VerifyingKey, MessageId)> = {
        let Ok(rooms) = ROOMS.try_read() else {
            return;
        };
        rooms
            .map
            .iter()
            .filter_map(|(owner_key, room_data)| {
                let latest = room_data
                    .room_state
                    .recent_messages
                    .display_messages()
                    .last()
                    .map(|msg| msg.id())?;
                if room_data.last_read_message_id.as_ref() == Some(&latest) {
                    None
                } else {
                    Some((*owner_key, latest))
                }
            })
            .collect()
    };

    if updates.is_empty() {
        return;
    }

    // Defer the signal mutation: this function fires from the raw
    // `visibilitychange` JS event callback, which has no Dioxus scope on the
    // stack. Going through `defer()` pushes the runtime + root scope so signal
    // subscriber notifications can find a current scope, and breaks the call
    // stack so no other RefCell borrows are active when subscribers re-read.
    crate::util::defer(move || {
        ROOMS.with_mut(|rooms| {
            for (owner_key, latest) in &updates {
                if let Some(room_data) = rooms.map.get_mut(owner_key) {
                    room_data.last_read_message_id = Some(latest.clone());
                }
            }
        });

        info!("Marked {} room(s) as read on tab hide", updates.len());

        crate::util::safe_spawn_local(async {
            if let Err(e) = save_rooms_to_delegate().await {
                warn!("Failed to save rooms after marking all as read: {}", e);
            }
        });
    });
}

/// Handle visibility change event
fn on_visibility_change() {
    let is_visible = get_visibility_state();
    let was_visible = *DOCUMENT_VISIBLE.read();
    debug!("Visibility changed: {} -> {}", was_visible, is_visible);

    *DOCUMENT_VISIBLE.write() = is_visible;

    if is_visible {
        // Tab became visible - mark current room as read
        mark_current_room_as_read();
    } else if was_visible {
        // Tab is going from visible to hidden. The user just had the page
        // active, so anything currently in state should be considered seen.
        // Only messages that arrive *after* this point should drive the
        // unread badge in the title.
        mark_all_rooms_as_read();
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
    let rooms_len = ROOMS.try_read().map(|r| r.map.len()).unwrap_or(0);
    let _rooms_version = rooms_len; // Simple trigger for reactivity

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::ROOM_CONTRACT_WASM;
    use crate::room_data::RoomData;
    use crate::util::to_cbor_vec;
    use ed25519_dalek::{SigningKey, VerifyingKey};
    use freenet_stdlib::prelude::{ContractCode, ContractKey, Parameters};
    use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
    use river_core::room_state::ChatRoomParametersV1;
    use river_core::ChatRoomStateV1;
    use std::collections::HashMap;
    use std::time::{Duration, UNIX_EPOCH};

    /// Build a signed display message from `author_sk`, distinct per `n`.
    fn msg(author_sk: &SigningKey, owner_vk: &VerifyingKey, n: u64) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: MemberId::from(owner_vk),
                author: MemberId::from(&author_sk.verifying_key()),
                content: RoomMessageBody::public(format!("message {n}")),
                time: UNIX_EPOCH + Duration::from_secs(n),
            },
            author_sk,
        )
    }

    /// Minimal `RoomData` carrying just the fields `count_unread_in_room_data`
    /// reads: `self_sk`, `recent_messages`, and `last_read_message_id`.
    fn room(
        self_sk: SigningKey,
        owner_vk: VerifyingKey,
        messages: Vec<AuthorizedMessageV1>,
        last_read_message_id: Option<MessageId>,
    ) -> RoomData {
        let mut room_state = ChatRoomStateV1::default();
        room_state.recent_messages.messages = messages;
        let contract_key = ContractKey::from_params_and_code(
            Parameters::from(to_cbor_vec(&ChatRoomParametersV1 { owner: owner_vk })),
            ContractCode::from(ROOM_CONTRACT_WASM),
        );
        RoomData {
            owner_vk,
            room_state,
            self_sk,
            contract_key,
            last_read_message_id,
            secrets: HashMap::new(),
            current_secret_version: None,
            last_secret_rotation: None,
            key_migrated_to_delegate: false,
            self_authorized_member: None,
            invite_chain: vec![],
            self_member_info: None,
            previous_contract_key: None,
        }
    }

    fn keypair() -> (SigningKey, VerifyingKey) {
        let sk = SigningKey::generate(&mut rand::thread_rng());
        let vk = sk.verifying_key();
        (sk, vk)
    }

    #[test]
    fn no_marker_counts_all_other_authored_messages() {
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let messages = vec![
            msg(&owner_sk, &owner_vk, 1),
            msg(&owner_sk, &owner_vk, 2),
            msg(&owner_sk, &owner_vk, 3),
        ];
        let rd = room(self_sk, owner_vk, messages, None);
        assert_eq!(count_unread_in_room_data(&rd), 3);
    }

    #[test]
    fn excludes_messages_authored_by_self() {
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        // 2 from the owner, 2 from self → only the owner's count as unread.
        let messages = vec![
            msg(&owner_sk, &owner_vk, 1),
            msg(&self_sk, &owner_vk, 2),
            msg(&owner_sk, &owner_vk, 3),
            msg(&self_sk, &owner_vk, 4),
        ];
        let rd = room(self_sk, owner_vk, messages, None);
        assert_eq!(count_unread_in_room_data(&rd), 2);
    }

    #[test]
    fn counts_only_messages_after_the_last_read_marker() {
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let messages = vec![
            msg(&owner_sk, &owner_vk, 1),
            msg(&owner_sk, &owner_vk, 2),
            msg(&owner_sk, &owner_vk, 3),
            msg(&owner_sk, &owner_vk, 4),
        ];
        // Mark the 2nd message read → messages 3 and 4 remain unread.
        let marker = messages[1].id();
        let rd = room(self_sk, owner_vk, messages, Some(marker));
        assert_eq!(count_unread_in_room_data(&rd), 2);
    }

    #[test]
    fn last_read_marker_itself_is_not_counted() {
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let messages = vec![msg(&owner_sk, &owner_vk, 1), msg(&owner_sk, &owner_vk, 2)];
        // Marker is the latest message → nothing after it → 0 unread.
        let marker = messages[1].id();
        let rd = room(self_sk, owner_vk, messages, Some(marker));
        assert_eq!(count_unread_in_room_data(&rd), 0);
    }

    #[test]
    fn pruned_marker_falls_back_to_all_other_authored() {
        // Regression: if last_read_message_id points at a message that has
        // been evicted from the recent-messages ring buffer, the room must
        // still surface its unread messages instead of silently showing 0.
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        // Marker derived from a message that is NOT placed into the room.
        let evicted = msg(&owner_sk, &owner_vk, 99);
        let messages = vec![msg(&owner_sk, &owner_vk, 1), msg(&owner_sk, &owner_vk, 2)];
        let rd = room(self_sk, owner_vk, messages, Some(evicted.id()));
        assert_eq!(count_unread_in_room_data(&rd), 2);
    }

    #[test]
    fn deleted_messages_are_not_counted() {
        // `display_messages` filters deleted messages; the helper must too.
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let messages = vec![
            msg(&owner_sk, &owner_vk, 1),
            msg(&owner_sk, &owner_vk, 2),
            msg(&owner_sk, &owner_vk, 3),
        ];
        let deleted = messages[1].id();
        let mut rd = room(self_sk, owner_vk, messages, None);
        rd.room_state
            .recent_messages
            .actions_state
            .deleted
            .insert(deleted);
        // 3 messages, 1 deleted → 2 displayable, all from others.
        assert_eq!(count_unread_in_room_data(&rd), 2);
    }

    #[test]
    fn deleted_last_read_marker_still_anchors_the_count() {
        // Regression for the Codex re-review finding: a last-read message
        // that is later deleted must still anchor the count. Messages read
        // before it must NOT re-surface as unread just because the marker
        // is no longer in the display-filtered view.
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let messages = vec![
            msg(&owner_sk, &owner_vk, 1),
            msg(&owner_sk, &owner_vk, 2), // last read, then deleted
            msg(&owner_sk, &owner_vk, 3),
            msg(&owner_sk, &owner_vk, 4),
        ];
        let marker = messages[1].id();
        let mut rd = room(self_sk, owner_vk, messages, Some(marker.clone()));
        rd.room_state
            .recent_messages
            .actions_state
            .deleted
            .insert(marker);
        // Only messages 3 and 4 follow the marker → 2 unread (not 3 — the
        // already-read message 1 must stay read despite the deletion).
        assert_eq!(count_unread_in_room_data(&rd), 2);
    }

    #[test]
    fn empty_room_with_marker_counts_zero() {
        // A marker over an empty buffer: `position` is `None` → `start` 0 →
        // empty slice → 0, with no panic on the empty-slice index.
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let orphan = msg(&owner_sk, &owner_vk, 1).id();
        let rd = room(self_sk, owner_vk, vec![], Some(orphan));
        assert_eq!(count_unread_in_room_data(&rd), 0);
    }

    #[test]
    fn helper_agrees_with_display_messages_filter() {
        // Drift guard: the helper hand-mirrors `MessagesV1::display_messages`'s
        // action/deleted filter. With no marker, the count must equal
        // `display_messages()` filtered to other authors — if the two
        // predicates ever diverge, this fails.
        let (self_sk, self_vk) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let self_id: MemberId = (&self_vk).into();
        let messages = vec![
            msg(&owner_sk, &owner_vk, 1),
            msg(&owner_sk, &owner_vk, 2),
            msg(&self_sk, &owner_vk, 3),
        ];
        let deleted = messages[1].id();
        let mut rd = room(self_sk, owner_vk, messages, None);
        rd.room_state
            .recent_messages
            .actions_state
            .deleted
            .insert(deleted);
        let expected = rd
            .room_state
            .recent_messages
            .display_messages()
            .filter(|m| m.message.author != self_id)
            .count();
        assert_eq!(count_unread_in_room_data(&rd), expected);
    }
}
