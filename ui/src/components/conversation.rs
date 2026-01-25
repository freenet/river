use crate::components::app::notifications::request_permission_on_first_message;
use crate::components::app::{CURRENT_ROOM, EDIT_ROOM_MODAL, MEMBER_INFO_MODAL, NEEDS_SYNC, ROOMS};
use crate::room_data::SendMessageError;
use crate::util::ecies::encrypt_with_symmetric_key;
use crate::util::{format_utc_as_full_datetime, format_utc_as_local_time, get_current_system_time};
mod message_actions;
mod message_input;
mod not_member_notification;
use self::not_member_notification::NotMemberNotification;
use crate::components::conversation::message_input::MessageInput;
use chrono::{DateTime, Utc};
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{FaPencil, FaRotate};
use dioxus_free_icons::Icon;
use freenet_scaffold::ComposableState;
use river_core::room_state::member::MemberId;
use river_core::room_state::member_info::MemberInfoV1;
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageId, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
use wasm_bindgen_futures::spawn_local;

/// A group of consecutive messages from the same sender within a time window
#[derive(Clone, PartialEq)]
struct MessageGroup {
    author_id: MemberId,
    author_name: String,
    is_self: bool,
    first_time: DateTime<Utc>,
    messages: Vec<GroupedMessage>,
}

#[derive(Clone, PartialEq)]
struct GroupedMessage {
    content_html: String,
    #[allow(dead_code)]
    time: DateTime<Utc>,
    id: String,
    message_id: MessageId,
    edited: bool,
    reactions: HashMap<String, Vec<MemberId>>,
}

/// Group consecutive messages from the same sender within 5 minutes
fn group_messages(
    messages_state: &MessagesV1,
    member_info: &MemberInfoV1,
    self_member_id: MemberId,
    room_secret: Option<[u8; 32]>,
    room_secret_version: Option<u32>,
) -> Vec<MessageGroup> {
    let mut groups: Vec<MessageGroup> = Vec::new();
    let group_threshold = Duration::from_secs(5 * 60); // 5 minutes

    // Only iterate over displayable messages (non-deleted, non-action)
    for message in messages_state.display_messages() {
        let author_id = message.message.author;
        let message_time = DateTime::<Utc>::from(message.message.time);
        let message_id = message.id();

        let author_name = member_info
            .member_info
            .iter()
            .find(|ami| ami.member_info.member_id == author_id)
            .map(|ami| ami.member_info.preferred_nickname.to_string_lossy())
            .unwrap_or_else(|| "Unknown".to_string());

        // Get effective content (may be edited)
        // effective_text returns edited content if available, or decoded public text
        // For encrypted messages, it returns None and we need to decrypt
        let content_text = messages_state
            .effective_text(message)
            .unwrap_or_else(|| {
                decrypt_message_content(&message.message.content, room_secret, room_secret_version)
            });
        let content_html = message_to_html(&content_text);
        let is_self = author_id == self_member_id;

        // Get edited status and reactions
        let edited = messages_state.is_edited(&message_id);
        let reactions = messages_state
            .reactions(&message_id)
            .cloned()
            .unwrap_or_default();

        let grouped_message = GroupedMessage {
            content_html,
            time: message_time,
            id: format!("{:?}", message_id.0),
            message_id,
            edited,
            reactions,
        };

        // Check if we should add to the last group
        let should_group = groups.last().map_or(false, |last_group| {
            last_group.author_id == author_id
                && (message_time - last_group.messages.last().unwrap().time)
                    .to_std()
                    .unwrap_or(Duration::MAX)
                    < group_threshold
        });

        if should_group {
            groups.last_mut().unwrap().messages.push(grouped_message);
        } else {
            groups.push(MessageGroup {
                author_id,
                author_name,
                is_self,
                first_time: message_time,
                messages: vec![grouped_message],
            });
        }
    }

    groups
}

fn decrypt_message_content(
    content: &RoomMessageBody,
    room_secret: Option<[u8; 32]>,
    room_secret_version: Option<u32>,
) -> String {
    use river_core::room_state::content::{TextContentV1, CONTENT_TYPE_ACTION, CONTENT_TYPE_TEXT};

    match content {
        RoomMessageBody::Public {
            content_type, data, ..
        } => {
            // Action messages - display as action description
            if *content_type == CONTENT_TYPE_ACTION {
                return content.to_string_lossy();
            }
            // Text messages - decode and return text
            if *content_type == CONTENT_TYPE_TEXT {
                if let Ok(text_content) = TextContentV1::decode(data) {
                    return text_content.text;
                }
            }
            // Unknown content type
            content.to_string_lossy()
        }
        RoomMessageBody::Private {
            content_type,
            ciphertext,
            nonce,
            secret_version,
            ..
        } => {
            if let (Some(secret), Some(current_version)) =
                (room_secret.as_ref(), room_secret_version)
            {
                if current_version == *secret_version {
                    use crate::util::ecies::decrypt_with_symmetric_key;
                    // Decrypt the ciphertext
                    if let Ok(decrypted_bytes) =
                        decrypt_with_symmetric_key(secret, ciphertext.as_slice(), nonce)
                    {
                        // For text messages, decode the content
                        if *content_type == CONTENT_TYPE_TEXT {
                            if let Ok(text_content) = TextContentV1::decode(&decrypted_bytes) {
                                return text_content.text;
                            }
                        }
                        // Fallback to UTF-8 string
                        return String::from_utf8_lossy(&decrypted_bytes).to_string();
                    }
                    content.to_string_lossy()
                } else {
                    format!(
                        "[Encrypted message with different secret version: v{} (current: v{})]",
                        secret_version, current_version
                    )
                }
            } else {
                content.to_string_lossy()
            }
        }
    }
}

/// Convert message text to HTML with clickable links that open in new tabs.
///
/// This function:
/// 1. Auto-linkifies plain URLs (http/https) that aren't already in markdown link syntax
/// 2. Converts markdown to HTML
/// 3. Adds target="_blank" rel="noopener noreferrer" to all links for security
fn message_to_html(text: &str) -> String {
    // First, auto-linkify plain URLs that aren't already markdown links
    let linkified = auto_linkify_urls(text);

    // Convert markdown to HTML
    let html = markdown::to_html(&linkified);

    // Add target="_blank" and rel="noopener noreferrer" to all links
    make_links_open_in_new_tab(&html)
}

/// Auto-linkify plain URLs that aren't already in markdown link syntax.
/// Matches http:// and https:// URLs.
fn auto_linkify_urls(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        // Check if we're inside a markdown link [...](url) - skip the URL part
        if c == ']' {
            result.push(c);
            if let Some(&(_, '(')) = chars.peek() {
                // This is a markdown link, copy until closing paren
                result.push(chars.next().unwrap().1); // '('
                while let Some((_, ch)) = chars.next() {
                    result.push(ch);
                    if ch == ')' {
                        break;
                    }
                }
            }
            continue;
        }

        // Check for URL start
        let remaining = &text[i..];
        if remaining.starts_with("http://") || remaining.starts_with("https://") {
            // Check if this URL is already inside a markdown link by looking back
            // for an unclosed '(' that follows ']'
            let before = &text[..i];
            let is_in_markdown_link = {
                let mut depth = 0i32;
                let mut in_link_url = false;
                for ch in before.chars().rev() {
                    if ch == ')' {
                        depth += 1;
                    } else if ch == '(' {
                        if depth > 0 {
                            depth -= 1;
                        } else {
                            in_link_url = true;
                            break;
                        }
                    } else if ch == ']' && depth == 0 {
                        // Found ']' before '(' - not in a link URL
                        break;
                    }
                }
                in_link_url
            };

            if is_in_markdown_link {
                result.push(c);
                continue;
            }

            // Extract the URL (until whitespace or certain punctuation at end)
            let url_end = remaining
                .find(|ch: char| ch.is_whitespace() || ch == '<' || ch == '>' || ch == '"')
                .unwrap_or(remaining.len());

            let mut url = &remaining[..url_end];

            // Trim trailing punctuation that's likely not part of the URL
            while url
                .ends_with(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']'))
            {
                url = &url[..url.len() - 1];
            }

            // Create markdown link
            result.push('[');
            result.push_str(url);
            result.push_str("](");
            result.push_str(url);
            result.push(')');

            // Skip the URL characters we just processed
            for _ in 0..url.len() - 1 {
                chars.next();
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Add target="_blank" and rel="noopener noreferrer" to all anchor tags in HTML.
fn make_links_open_in_new_tab(html: &str) -> String {
    // Replace <a href=" with <a target="_blank" rel="noopener noreferrer" href="
    html.replace(
        "<a href=\"",
        "<a target=\"_blank\" rel=\"noopener noreferrer\" href=\"",
    )
}

#[component]
pub fn Conversation() -> Element {
    let current_room_data = {
        let current_room = CURRENT_ROOM.read();
        if let Some(key) = current_room.owner_key {
            let rooms = ROOMS.read();
            rooms.map.get(&key).cloned()
        } else {
            None
        }
    };
    let last_chat_element = use_signal(|| None as Option<Rc<MountedData>>);

    // State for delete confirmation modal
    let mut pending_delete: Signal<Option<MessageId>> = use_signal(|| None);

    let current_room_label = use_memo({
        move || {
            let current_room = CURRENT_ROOM.read();
            if let Some(key) = current_room.owner_key {
                let rooms = ROOMS.read();
                if let Some(room_data) = rooms.map.get(&key) {
                    return room_data
                        .room_state
                        .configuration
                        .configuration
                        .display
                        .name
                        .to_string_lossy();
                }
            }
            "No Room Selected".to_string()
        }
    });

    // Memoize expensive message grouping (decryption + markdown parsing)
    // This prevents re-computing on every render/keystroke
    let message_groups = use_memo(move || {
        let current_room = CURRENT_ROOM.read();
        if let Some(key) = current_room.owner_key {
            let rooms = ROOMS.read();
            if let Some(room_data) = rooms.map.get(&key) {
                let room_state = &room_data.room_state;
                // Check if there are any displayable messages
                if room_state
                    .recent_messages
                    .display_messages()
                    .next()
                    .is_some()
                {
                    let self_member_id = MemberId::from(&room_data.self_sk.verifying_key());
                    return Some(group_messages(
                        &room_state.recent_messages,
                        &room_state.member_info,
                        self_member_id,
                        room_data.current_secret,
                        room_data.current_secret_version,
                    ));
                }
            }
        }
        None
    });

    // Trigger scroll to bottom when recent messages change
    use_effect(move || {
        let container = last_chat_element();
        if let Some(container) = container {
            wasm_bindgen_futures::spawn_local(async move {
                let _ = container.scroll_to(ScrollBehavior::Smooth).await;
            });
        }
    });

    // Handler for adding a reaction to a message
    let handle_add_reaction = {
        let current_room_data = current_room_data.clone();
        move |target_message_id: MessageId, emoji: String| {
            if let (Some(current_room), Some(current_room_data)) =
                (CURRENT_ROOM.read().owner_key, current_room_data.clone())
            {
                let room_key = current_room_data.room_key();
                let self_sk = current_room_data.self_sk.clone();
                let room_state_clone = current_room_data.room_state.clone();

                spawn_local(async move {
                    let content = RoomMessageBody::reaction(target_message_id, emoji);

                    let message = MessageV1 {
                        room_owner: MemberId::from(current_room),
                        author: MemberId::from(&self_sk.verifying_key()),
                        content,
                        time: get_current_system_time(),
                    };

                    let mut message_bytes = Vec::new();
                    if let Err(e) = ciborium::ser::into_writer(&message, &mut message_bytes) {
                        error!("Failed to serialize reaction message: {:?}", e);
                        return;
                    }

                    let signature = crate::signing::sign_message_with_fallback(
                        room_key,
                        message_bytes,
                        &self_sk,
                    )
                    .await;

                    let auth_message = AuthorizedMessageV1::with_signature(message, signature);
                    let delta = ChatRoomStateV1Delta {
                        recent_messages: Some(vec![auth_message]),
                        ..Default::default()
                    };
                    info!("Sending reaction");
                    ROOMS.with_mut(|rooms| {
                        if let Some(room_data) = rooms.map.get_mut(&current_room) {
                            if let Err(e) = room_data.room_state.apply_delta(
                                &room_state_clone,
                                &ChatRoomParametersV1 {
                                    owner: current_room,
                                },
                                &Some(delta),
                            ) {
                                error!("Failed to apply reaction delta: {:?}", e);
                            } else {
                                NEEDS_SYNC.write().insert(current_room);
                            }
                        }
                    });
                });
            }
        }
    };

    // Handler for deleting a message
    let handle_delete_message = {
        let current_room_data = current_room_data.clone();
        move |target_message_id: MessageId| {
            if let (Some(current_room), Some(current_room_data)) =
                (CURRENT_ROOM.read().owner_key, current_room_data.clone())
            {
                let room_key = current_room_data.room_key();
                let self_sk = current_room_data.self_sk.clone();
                let room_state_clone = current_room_data.room_state.clone();

                spawn_local(async move {
                    let content = RoomMessageBody::delete(target_message_id);

                    let message = MessageV1 {
                        room_owner: MemberId::from(current_room),
                        author: MemberId::from(&self_sk.verifying_key()),
                        content,
                        time: get_current_system_time(),
                    };

                    let mut message_bytes = Vec::new();
                    if let Err(e) = ciborium::ser::into_writer(&message, &mut message_bytes) {
                        error!("Failed to serialize delete message: {:?}", e);
                        return;
                    }

                    let signature = crate::signing::sign_message_with_fallback(
                        room_key,
                        message_bytes,
                        &self_sk,
                    )
                    .await;

                    let auth_message = AuthorizedMessageV1::with_signature(message, signature);
                    let delta = ChatRoomStateV1Delta {
                        recent_messages: Some(vec![auth_message]),
                        ..Default::default()
                    };
                    info!("Sending delete action");
                    ROOMS.with_mut(|rooms| {
                        if let Some(room_data) = rooms.map.get_mut(&current_room) {
                            if let Err(e) = room_data.room_state.apply_delta(
                                &room_state_clone,
                                &ChatRoomParametersV1 {
                                    owner: current_room,
                                },
                                &Some(delta),
                            ) {
                                error!("Failed to apply delete delta: {:?}", e);
                            } else {
                                NEEDS_SYNC.write().insert(current_room);
                            }
                        }
                    });
                });
            }
        }
    };

    // Message sending handler - receives message text from MessageInput component
    let handle_send_message = {
        let current_room_data = current_room_data.clone();
        move |message_text: String| {
            if message_text.is_empty() {
                warn!("Message is empty");
                return;
            }
            if let (Some(current_room), Some(current_room_data)) =
                (CURRENT_ROOM.read().owner_key, current_room_data.clone())
            {
                // Clone what we need for the async block
                let room_key = current_room_data.room_key();
                let self_sk = current_room_data.self_sk.clone();
                let room_state_clone = current_room_data.room_state.clone();
                let is_private = current_room_data.is_private();
                // Copy the secret data (get_secret returns Option<(&[u8; 32], u32)>)
                let secret_opt: Option<([u8; 32], u32)> = current_room_data
                    .get_secret()
                    .map(|(secret, version)| (*secret, version));

                spawn_local(async move {
                    use river_core::room_state::content::{
                        TextContentV1, CONTENT_TYPE_TEXT, TEXT_CONTENT_VERSION,
                    };

                    // Encrypt message if room is private and we have the secret
                    let content = if is_private {
                        if let Some((secret, version)) = secret_opt {
                            // Encode the text content first, then encrypt
                            let text_content = TextContentV1::new(message_text.clone());
                            let content_bytes = text_content.encode();
                            let (ciphertext, nonce) =
                                encrypt_with_symmetric_key(&secret, &content_bytes);
                            RoomMessageBody::private(
                                CONTENT_TYPE_TEXT,
                                TEXT_CONTENT_VERSION,
                                ciphertext,
                                nonce,
                                version,
                            )
                        } else {
                            warn!("Room is private but no secret available, sending as public");
                            RoomMessageBody::public(message_text.clone())
                        }
                    } else {
                        RoomMessageBody::public(message_text.clone())
                    };

                    let message = MessageV1 {
                        room_owner: MemberId::from(current_room),
                        author: MemberId::from(&self_sk.verifying_key()),
                        content,
                        time: get_current_system_time(),
                    };

                    // Serialize message to CBOR for signing
                    let mut message_bytes = Vec::new();
                    if let Err(e) = ciborium::ser::into_writer(&message, &mut message_bytes) {
                        error!("Failed to serialize message for signing: {:?}", e);
                        return;
                    }

                    // Sign using delegate with fallback to local signing
                    let signature = crate::signing::sign_message_with_fallback(
                        room_key,
                        message_bytes,
                        &self_sk,
                    )
                    .await;

                    let auth_message = AuthorizedMessageV1::with_signature(message, signature);
                    let delta = ChatRoomStateV1Delta {
                        recent_messages: Some(vec![auth_message.clone()]),
                        ..Default::default()
                    };
                    info!("Sending message: {:?}", auth_message);
                    ROOMS.with_mut(|rooms| {
                        if let Some(room_data) = rooms.map.get_mut(&current_room) {
                            if let Err(e) = room_data.room_state.apply_delta(
                                &room_state_clone,
                                &ChatRoomParametersV1 {
                                    owner: current_room,
                                },
                                &Some(delta),
                            ) {
                                error!("Failed to apply message delta: {:?}", e);
                            } else {
                                // Mark room as needing sync after message added
                                NEEDS_SYNC.write().insert(current_room);

                                // Request notification permission on first message
                                request_permission_on_first_message();
                            }
                        }
                    });
                });
            }
        }
    };

    rsx! {
        div { class: "flex-1 flex flex-col min-w-0 bg-bg",
            // Room header
            {
                current_room_data.as_ref().map(|room_data| {
                    let is_owner = room_data.owner_vk == room_data.self_sk.verifying_key();
                    let is_private = room_data.is_private();

                    rsx! {
                        div { class: "flex-shrink-0 px-6 py-3 border-b border-border bg-panel",
                            div { class: "flex items-center justify-between max-w-4xl mx-auto",
                                div { class: "flex items-center gap-2",
                                    h2 { class: "text-lg font-semibold text-text",
                                        "{current_room_label}"
                                    }
                                    button {
                                        class: "p-1.5 rounded text-text-muted hover:text-text hover:bg-surface transition-colors",
                                        title: "Edit room",
                                        onclick: move |_| {
                                            if let Some(current_room) = CURRENT_ROOM.read().owner_key {
                                                EDIT_ROOM_MODAL.with_mut(|modal| {
                                                    modal.room = Some(current_room);
                                                });
                                            }
                                        },
                                        Icon { icon: FaPencil, width: 12, height: 12 }
                                    }
                                    {
                                        if is_owner && is_private {
                                            Some(rsx! {
                                                button {
                                                    class: "p-1.5 rounded text-text-muted hover:text-text hover:bg-surface transition-colors",
                                                    title: "Rotate room secret",
                                                    onclick: move |_| {
                                                        if let Some(current_room) = CURRENT_ROOM.read().owner_key {
                                                            info!("Rotating secret for room {:?}", MemberId::from(current_room));
                                                            ROOMS.with_mut(|rooms| {
                                                                if let Some(room_data) = rooms.map.get_mut(&current_room) {
                                                                    match room_data.rotate_secret() {
                                                                        Ok(secrets_delta) => {
                                                                            info!("Secret rotated successfully");
                                                                            let current_state = room_data.room_state.clone();
                                                                            let delta = ChatRoomStateV1Delta {
                                                                                secrets: Some(secrets_delta),
                                                                                ..Default::default()
                                                                            };
                                                                            if let Err(e) = room_data.room_state.apply_delta(
                                                                                &current_state,
                                                                                &ChatRoomParametersV1 { owner: current_room },
                                                                                &Some(delta),
                                                                            ) {
                                                                                error!("Failed to apply rotation delta: {}", e);
                                                                            } else {
                                                                                NEEDS_SYNC.write().insert(current_room);
                                                                            }
                                                                        }
                                                                        Err(e) => error!("Failed to rotate secret: {}", e),
                                                                    }
                                                                }
                                                            });
                                                        }
                                                    },
                                                    Icon { icon: FaRotate, width: 12, height: 12 }
                                                }
                                            })
                                        } else {
                                            None
                                        }
                                    }
                                }
                            }
                        }
                    }
                })
            }

            // Message area with constrained width
            div { class: "flex-1 overflow-y-auto",
                div { class: "max-w-4xl mx-auto px-4 py-4",
                    {
                        // Use memoized message groups to avoid expensive re-computation on keystrokes
                        if current_room_data.is_some() {
                            match message_groups.read().as_ref() {
                                Some(groups) => {
                                    let groups = groups.clone();
                                    let groups_len = groups.len();
                                    Some(rsx! {
                                        div { class: "space-y-4",
                                            {groups.into_iter().enumerate().map({
                                                let handle_add_reaction = handle_add_reaction.clone();
                                                move |(group_idx, group)| {
                                                let is_last_group = group_idx == groups_len - 1;
                                                let key = group.messages[0].id.clone();
                                                let handle_add_reaction = handle_add_reaction.clone();
                                                rsx! {
                                                    MessageGroupComponent {
                                                        key: "{key}",
                                                        group: group,
                                                        last_chat_element: if is_last_group { Some(last_chat_element) } else { None },
                                                        on_react: move |(msg_id, emoji)| {
                                                            handle_add_reaction(msg_id, emoji);
                                                        },
                                                        on_request_delete: move |msg_id| {
                                                            pending_delete.set(Some(msg_id));
                                                        },
                                                    }
                                                }
                                            }})}
                                        }
                                    })
                                }
                                None => Some(rsx! {
                                    div { class: "flex flex-col items-center justify-center h-64 text-text-muted",
                                        p { "No messages yet. Start the conversation!" }
                                    }
                                })
                            }
                        } else {
                            None
                        }
                    }
                }
            }

            // Message input or status
            {
                match current_room_data.as_ref() {
                    Some(room_data) => {
                        match room_data.can_send_message() {
                            Ok(()) => rsx! {
                                MessageInput {
                                    handle_send_message: move |text| {
                                        let handle = handle_send_message.clone();
                                        handle(text)
                                    },
                                }
                            },
                            Err(SendMessageError::UserNotMember) => {
                                let user_vk = room_data.self_sk.verifying_key();
                                let user_id = MemberId::from(&user_vk);
                                if !room_data.room_state.members.members.iter().any(|m| MemberId::from(&m.member.member_vk) == user_id) {
                                    rsx! {
                                        NotMemberNotification {
                                            user_verifying_key: user_vk
                                        }
                                    }
                                } else {
                                    rsx! {
                                        MessageInput {
                                            handle_send_message: move |text| {
                                                let handle = handle_send_message.clone();
                                                handle(text)
                                            },
                                        }
                                    }
                                }
                            },
                            Err(SendMessageError::UserBanned) => rsx! {
                                div { class: "px-4 py-3 mx-4 mb-4 bg-error-bg text-red-700 dark:text-red-400 rounded-lg text-sm",
                                    "You have been banned from sending messages in this room."
                                }
                            },
                        }
                    },
                    None => rsx! {
                        div { class: "flex-1 flex flex-col items-center justify-center text-center p-8",
                            img {
                                class: "w-24 h-24 mb-6 opacity-50",
                                src: asset!("/assets/river_logo.svg"),
                                alt: "River Logo"
                            }
                            h1 { class: "text-2xl font-semibold text-text mb-2",
                                "Welcome to River"
                            }
                            p { class: "text-text-muted",
                                "Create a new room, or get invited to an existing one."
                            }
                        }
                    },
                }
            }

            // Delete confirmation modal
            if pending_delete.read().is_some() {
                div {
                    class: "fixed inset-0 bg-black/50 flex items-center justify-center z-50",
                    onclick: move |_| pending_delete.set(None),
                    div {
                        class: "bg-panel rounded-lg shadow-xl p-6 max-w-sm mx-4",
                        onclick: move |e| e.stop_propagation(),
                        h3 { class: "text-lg font-semibold text-text mb-2",
                            "Delete Message?"
                        }
                        p { class: "text-text-muted text-sm mb-4",
                            "This action cannot be undone. The message will be permanently deleted."
                        }
                        div { class: "flex gap-3 justify-end",
                            button {
                                class: "px-4 py-2 rounded-lg bg-surface hover:bg-surface/80 text-text transition-colors",
                                onclick: move |_| pending_delete.set(None),
                                "Cancel"
                            }
                            button {
                                class: "px-4 py-2 rounded-lg bg-red-500 hover:bg-red-600 text-white transition-colors",
                                onclick: move |_| {
                                    let msg_id_opt = pending_delete.read().clone();
                                    if let Some(msg_id) = msg_id_opt {
                                        handle_delete_message(msg_id);
                                    }
                                    pending_delete.set(None);
                                },
                                "Delete"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Curated emoji set for reactions - covers most common emotional responses
const REACTION_EMOJIS: &[&str] = &["👍", "❤️", "😂", "😮", "😢", "😡", "🎉", "🤔"];

#[component]
fn MessageGroupComponent(
    group: MessageGroup,
    last_chat_element: Option<Signal<Option<Rc<MountedData>>>>,
    on_react: EventHandler<(MessageId, String)>,
    on_request_delete: EventHandler<MessageId>,
) -> Element {
    let timestamp_ms = group.first_time.timestamp_millis();
    let time_str = format_utc_as_local_time(timestamp_ms);
    let full_time_str = format_utc_as_full_datetime(timestamp_ms);
    let is_self = group.is_self;

    // Track which message's emoji picker is open (by message ID string)
    let mut open_emoji_picker: Signal<Option<String>> = use_signal(|| None);

    rsx! {
        div {
            class: format!(
                "flex {}",
                if is_self { "justify-end" } else { "justify-start" }
            ),
            div {
                class: format!(
                    "max-w-[75%] {}",
                    if is_self { "items-end" } else { "items-start" }
                ),
                // Header with name and time (only for others)
                if !is_self {
                    div { class: "flex items-baseline gap-2 mb-1 px-1",
                        span {
                            class: "text-sm font-medium text-text cursor-pointer hover:text-accent transition-colors",
                            title: "Member ID: {group.author_id}",
                            onclick: move |_| {
                                MEMBER_INFO_MODAL.with_mut(|signal| {
                                    signal.member = Some(group.author_id);
                                });
                            },
                            "{group.author_name}"
                        }
                        span {
                            class: "text-xs text-text-muted cursor-default",
                            title: "{full_time_str}",
                            "{time_str}"
                        }
                    }
                }

                // Message bubbles
                div {
                    class: format!(
                        "space-y-1 {}",
                        if is_self { "flex flex-col items-end" } else { "" }
                    ),
                    {
                        let messages_len = group.messages.len();
                        group.messages.into_iter().enumerate().map(move |(idx, msg)| {
                        let is_last = idx == messages_len - 1;
                        let is_first = idx == 0;
                        let has_reactions = !msg.reactions.is_empty();

                        rsx! {
                            div {
                                key: "{msg.id}",
                                class: "flex flex-col",
                                // Container for message bubble + hover actions
                                div {
                                    class: "relative group",
                                    // Message bubble
                                    div {
                                        class: format!(
                                            "px-3 py-2 text-sm {} {} {}",
                                            if is_self {
                                                "bg-accent text-white"
                                            } else {
                                                "bg-surface text-text"
                                            },
                                            // Rounded corners based on position
                                            if is_self {
                                                if is_first && is_last && !has_reactions {
                                                    "rounded-2xl"
                                                } else if is_first {
                                                    "rounded-t-2xl rounded-bl-2xl rounded-br-md"
                                                } else if is_last && !has_reactions {
                                                    "rounded-b-2xl rounded-tl-2xl rounded-tr-md"
                                                } else {
                                                    "rounded-l-2xl rounded-r-md"
                                                }
                                            } else {
                                                if is_first && is_last && !has_reactions {
                                                    "rounded-2xl"
                                                } else if is_first {
                                                    "rounded-t-2xl rounded-br-2xl rounded-bl-md"
                                                } else if is_last && !has_reactions {
                                                    "rounded-b-2xl rounded-tr-2xl rounded-tl-md"
                                                } else {
                                                    "rounded-r-2xl rounded-l-md"
                                                }
                                            },
                                            // Max width for readability
                                            "max-w-prose"
                                        ),
                                        onmounted: move |cx| {
                                            if is_last {
                                                if let Some(mut last_el) = last_chat_element {
                                                    last_el.set(Some(cx.data()));
                                                }
                                            }
                                        },
                                        span {
                                            class: "prose prose-sm dark:prose-invert max-w-none",
                                            dangerous_inner_html: "{msg.content_html}"
                                        }
                                        // Edited indicator
                                        if msg.edited {
                                            span {
                                                class: format!(
                                                    "text-xs ml-2 {}",
                                                    if is_self { "text-white/70" } else { "text-text-muted" }
                                                ),
                                                "(edited)"
                                            }
                                        }
                                    }
                                    // Hover action bar with emoji picker
                                    {
                                        let msg_id_str = msg.id.clone();
                                        let msg_id_for_delete = msg.message_id.clone();
                                        let is_picker_open = open_emoji_picker.read().as_ref() == Some(&msg_id_str);
                                        rsx! {
                                            // Invisible backdrop to catch outside clicks when picker is open
                                            if is_picker_open {
                                                div {
                                                    class: "fixed inset-0 z-40",
                                                    onclick: move |_| open_emoji_picker.set(None),
                                                }
                                            }
                                            div {
                                                class: format!(
                                                    "absolute top-0 -translate-y-1/2 transition-opacity z-50 flex items-center gap-0.5 bg-panel rounded-lg shadow-md border border-border p-1 {} {}",
                                                    if is_self { "left-0 -translate-x-full -ml-2" } else { "right-0 translate-x-full ml-2" },
                                                    // Keep visible when picker is open, otherwise use hover
                                                    if is_picker_open { "opacity-100" } else { "opacity-0 group-hover:opacity-100" }
                                                ),
                                                // Reaction trigger with expandable picker
                                                div { class: "relative",
                                                    button {
                                                        class: "p-1.5 rounded-full hover:bg-amber-100 dark:hover:bg-amber-900/30 transition-colors text-sm",
                                                        title: "Add reaction",
                                                        onclick: {
                                                            let msg_id_str = msg_id_str.clone();
                                                            move |e: MouseEvent| {
                                                                e.stop_propagation();
                                                                let current = open_emoji_picker.read().clone();
                                                                if current.as_ref() == Some(&msg_id_str) {
                                                                    open_emoji_picker.set(None);
                                                                } else {
                                                                    open_emoji_picker.set(Some(msg_id_str.clone()));
                                                                }
                                                            }
                                                        },
                                                        "😊"
                                                    }
                                                    // Emoji picker dropdown - vertical layout
                                                    if is_picker_open {
                                                        div {
                                                            class: format!(
                                                                "absolute top-full mt-1 p-1 bg-panel rounded-xl shadow-xl border border-border grid grid-cols-2 gap-0.5 z-50 {}",
                                                                if is_self { "right-0" } else { "left-0" }
                                                            ),
                                                            onclick: move |e: MouseEvent| e.stop_propagation(),
                                                            {REACTION_EMOJIS.iter().map(|emoji| {
                                                                let emoji_str = emoji.to_string();
                                                                let msg_id = msg.message_id.clone();
                                                                rsx! {
                                                                    button {
                                                                        key: "{emoji}",
                                                                        class: "p-2 rounded-lg hover:bg-surface hover:scale-110 transition-all text-xl",
                                                                        title: "React with {emoji}",
                                                                        onclick: move |_| {
                                                                            on_react.call((msg_id.clone(), emoji_str.clone()));
                                                                            open_emoji_picker.set(None);
                                                                        },
                                                                        "{emoji}"
                                                                    }
                                                                }
                                                            })}
                                                        }
                                                    }
                                                }
                                                // Divider
                                                if is_self {
                                                    div { class: "w-px h-5 bg-border mx-0.5" }
                                                }
                                                // Edit button (only for own messages)
                                                if is_self {
                                                    button {
                                                        class: "p-1.5 rounded hover:bg-surface transition-colors text-sm opacity-50 hover:opacity-100 cursor-not-allowed",
                                                        title: "Edit message (coming soon)",
                                                        "✏️"
                                                    }
                                                }
                                                // Delete button (only for own messages)
                                                if is_self {
                                                    button {
                                                        class: "p-1.5 rounded hover:bg-red-100 dark:hover:bg-red-900/30 hover:text-red-500 transition-colors text-sm opacity-50 hover:opacity-100",
                                                        title: "Delete message",
                                                        onclick: move |_| {
                                                            on_request_delete.call(msg_id_for_delete.clone());
                                                        },
                                                        "🗑️"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                // Reactions display
                                if has_reactions {
                                    div {
                                        class: format!(
                                            "flex flex-wrap gap-1 mt-0.5 {}",
                                            if is_self { "justify-end" } else { "justify-start" }
                                        ),
                                        {
                                            let mut sorted_reactions: Vec<_> = msg.reactions.iter().collect();
                                            sorted_reactions.sort_by_key(|(emoji, _)| emoji.as_str());
                                            sorted_reactions.into_iter().map(|(emoji, reactors)| {
                                                let count = reactors.len();
                                                rsx! {
                                                    span {
                                                        key: "{emoji}",
                                                        class: "inline-flex items-center gap-1 px-1.5 py-0.5 rounded-full bg-surface text-xs border border-border hover:border-accent transition-colors cursor-default",
                                                        title: "{count} reaction(s)",
                                                        "{emoji}"
                                                        if count > 1 {
                                                            span { class: "text-text-muted", "{count}" }
                                                        }
                                                    }
                                                }
                                            })
                                        }
                                    }
                                }
                            }
                        }
                    })
                    }
                }

                // Time for self messages (shown at the end)
                if is_self {
                    div {
                        class: "text-xs text-text-muted mt-1 px-1 cursor-default",
                        title: "{full_time_str}",
                        "{time_str}"
                    }
                }
            }
        }
    }
}
