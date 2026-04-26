#[cfg(target_arch = "wasm32")]
use crate::components::app::notifications::request_permission_on_first_message;
use crate::components::app::receive_times::{format_delay, get_delay_secs};
use crate::components::app::{
    MobileView, CURRENT_ROOM, EDIT_ROOM_MODAL, MEMBER_INFO_MODAL, MOBILE_VIEW, ROOMS,
};
use crate::room_data::SendMessageError;
use crate::util::ecies::{encrypt_with_symmetric_key, unseal_bytes_with_secrets};
use crate::util::{format_utc_as_full_datetime, format_utc_as_local_time, get_current_system_time};
mod emoji_picker;
mod message_actions;
mod message_input;
mod not_member_notification;
use self::emoji_picker::FREQUENT_EMOJIS;
use self::not_member_notification::NotMemberNotification;
use crate::components::conversation::message_input::MessageInput;
use chrono::{DateTime, Utc};
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{FaBars, FaCircleInfo, FaUsers};
use dioxus_free_icons::Icon;
use freenet_scaffold::ComposableState;
use river_core::room_state::member::{MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfoV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageId, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys;

/// Try to build a rejoin delta for the current user in the given room.
/// Returns (None, None) if the user is already a member or ROOMS is busy.
fn try_rejoin_delta(
    room_key: &ed25519_dalek::VerifyingKey,
    action: &str,
) -> (Option<MembersDelta>, Option<Vec<AuthorizedMemberInfo>>) {
    let rooms_guard = ROOMS.try_read();
    if let Ok(rooms_read) = rooms_guard {
        if let Some(room_data) = rooms_read.map.get(room_key) {
            room_data.build_rejoin_delta()
        } else {
            (None, None)
        }
    } else {
        warn!("ROOMS signal busy during {action}, skipping re-add check");
        (None, None)
    }
}

/// Context for a reply-in-progress (held in a signal)
#[derive(Clone, PartialEq, Debug)]
struct ReplyContext {
    message_id: MessageId,
    author_name: String,
    content_preview: String,
}

/// A group of consecutive messages from the same sender within a time window
#[derive(Clone, PartialEq)]
struct MessageGroup {
    author_id: MemberId,
    author_name: String,
    is_self: bool,
    first_time: DateTime<Utc>,
    /// True if any message in this group had a future timestamp that was clamped
    time_clamped: bool,
    /// Propagation delay for the first message in the group (shown in header)
    first_delay_secs: Option<i64>,
    messages: Vec<GroupedMessage>,
}

#[derive(Clone, PartialEq)]
struct GroupedMessage {
    content_text: String,
    content_html: String,
    #[allow(dead_code)]
    time: DateTime<Utc>,
    /// True if the original timestamp was in the future and was clamped to now
    #[allow(dead_code)]
    time_clamped: bool,
    id: String,
    message_id: MessageId,
    edited: bool,
    reactions: HashMap<String, Vec<MemberId>>,
    reply_to_author: Option<String>,
    reply_to_preview: Option<String>,
    reply_to_message_id: Option<MessageId>,
    /// Propagation delay in seconds (send → receive), if known and significant
    #[allow(dead_code)]
    receive_delay_secs: Option<i64>,
}

/// An item in the conversation display — either a message group or an event summary
#[derive(Clone, PartialEq)]
enum DisplayItem {
    Messages(MessageGroup),
    Event(EventSummary),
}

/// Summary of consecutive room events (e.g. joins)
#[derive(Clone, PartialEq)]
struct EventSummary {
    names: Vec<String>,
    id: String,
    last_time: DateTime<Utc>,
}

/// Group consecutive messages from the same sender within 5 minutes,
/// and summarize consecutive event messages (e.g. joins).
fn group_messages(
    messages_state: &MessagesV1,
    member_info: &MemberInfoV1,
    self_member_id: MemberId,
    secrets: &HashMap<u32, [u8; 32]>,
) -> Vec<DisplayItem> {
    let mut items: Vec<DisplayItem> = Vec::new();
    let group_threshold = Duration::from_secs(5 * 60); // 5 minutes

    // Only iterate over displayable messages (non-deleted, non-action)
    for message in messages_state.display_messages() {
        let author_id = message.message.author;
        let now = Utc::now();
        let raw_time = DateTime::<Utc>::from(message.message.time);
        let time_clamped = raw_time > now;
        let message_time = if time_clamped { now } else { raw_time };
        let message_id = message.id();

        let author_name = member_info
            .member_info
            .iter()
            .find(|ami| ami.member_info.member_id == author_id)
            .map(|ami| {
                // Decrypt nickname using version-aware decryption
                match unseal_bytes_with_secrets(&ami.member_info.preferred_nickname, secrets) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                    Err(_) => ami.member_info.preferred_nickname.to_string_lossy(),
                }
            })
            .unwrap_or_else(|| "Unknown".to_string());

        // Handle event messages (join, etc.) — summarize consecutive events within 1 hour
        if message.message.content.is_event() {
            let msg_id_str = format!("{:?}", message_id.0);
            let event_group_threshold = Duration::from_secs(60 * 60);
            let should_merge = matches!(items.last(), Some(DisplayItem::Event(ref s))
                if (message_time - s.last_time).to_std().unwrap_or(Duration::MAX) < event_group_threshold);
            if should_merge {
                if let Some(DisplayItem::Event(ref mut summary)) = items.last_mut() {
                    summary.names.push(author_name);
                    summary.last_time = message_time;
                }
            } else {
                items.push(DisplayItem::Event(EventSummary {
                    names: vec![author_name],
                    id: msg_id_str,
                    last_time: message_time,
                }));
            }
            continue;
        }

        // Get effective content (may be edited)
        // effective_text returns edited content if available, or decoded public text
        // For encrypted messages, it returns None and we need to decrypt
        let content_text = messages_state
            .effective_text(message)
            .unwrap_or_else(|| decrypt_message_content(&message.message.content, secrets));
        let content_html = message_to_html(&content_text);
        let is_self = author_id == self_member_id;

        // Get edited status and reactions
        let edited = messages_state.is_edited(&message_id);
        let reactions = messages_state
            .reactions(&message_id)
            .cloned()
            .unwrap_or_default();

        // Extract reply context if this is a reply message
        let (reply_to_author, mut reply_to_preview, reply_to_message_id) =
            extract_reply_context(&message.message.content, secrets);

        // If the replied-to message has been edited, use its current content
        if let (Some(ref target_id), Some(_)) = (&reply_to_message_id, &reply_to_preview) {
            if let Some(target_msg) = messages_state
                .messages
                .iter()
                .find(|m| &m.id() == target_id)
            {
                let current_text = messages_state
                    .effective_text(target_msg)
                    .unwrap_or_else(|| {
                        decrypt_message_content(&target_msg.message.content, secrets)
                    });
                let preview: String = current_text.chars().take(100).collect();
                reply_to_preview = Some(preview);
            }
        }

        // Look up propagation delay (send time → receive time)
        let send_time_ms = raw_time.timestamp_millis();
        let receive_delay_secs = get_delay_secs(&message_id, send_time_ms);

        let grouped_message = GroupedMessage {
            content_text: content_text.clone(),
            content_html,
            time: message_time,
            time_clamped,
            id: format!("{:?}", message_id.0),
            message_id,
            edited,
            reactions,
            reply_to_author,
            reply_to_preview,
            reply_to_message_id,
            receive_delay_secs,
        };

        // Check if we should add to the last message group
        let should_group = match items.last() {
            Some(DisplayItem::Messages(last_group)) => {
                last_group.author_id == author_id
                    && (message_time - last_group.messages.last().unwrap().time)
                        .to_std()
                        .unwrap_or(Duration::MAX)
                        < group_threshold
            }
            _ => false,
        };

        if should_group {
            if let Some(DisplayItem::Messages(ref mut group)) = items.last_mut() {
                if time_clamped {
                    group.time_clamped = true;
                }
                group.messages.push(grouped_message);
            }
        } else {
            items.push(DisplayItem::Messages(MessageGroup {
                author_id,
                author_name,
                is_self,
                first_time: message_time,
                time_clamped,
                first_delay_secs: receive_delay_secs,
                messages: vec![grouped_message],
            }));
        }
    }

    items
}

/// Format an event summary like "Alice joined the room" or "3 people joined the room"
fn format_event_summary(names: &[String]) -> String {
    match names.len() {
        1 => format!("{} joined the room", names[0]),
        2 => format!("{} and {} joined the room", names[0], names[1]),
        n => format!("{} people joined the room", n),
    }
}

fn decrypt_message_content(content: &RoomMessageBody, secrets: &HashMap<u32, [u8; 32]>) -> String {
    use river_core::room_state::content::{
        ReplyContentV1, TextContentV1, CONTENT_TYPE_ACTION, CONTENT_TYPE_REPLY, CONTENT_TYPE_TEXT,
    };

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
            // Reply messages - decode and return reply text
            if *content_type == CONTENT_TYPE_REPLY {
                if let Ok(reply) = ReplyContentV1::decode(data) {
                    return reply.text;
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
            // Look up the secret for this message's version
            if let Some(secret) = secrets.get(secret_version) {
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
                    // For reply messages, decode and return reply text
                    if *content_type == CONTENT_TYPE_REPLY {
                        if let Ok(reply) = ReplyContentV1::decode(&decrypted_bytes) {
                            return reply.text;
                        }
                    }
                    // Fallback to UTF-8 string
                    return String::from_utf8_lossy(&decrypted_bytes).to_string();
                }
                content.to_string_lossy()
            } else {
                format!(
                    "[Encrypted message - secret v{} not available (have: {:?})]",
                    secret_version,
                    secrets.keys().collect::<Vec<_>>()
                )
            }
        }
    }
}

/// Extract reply context from a message body, if it is a reply.
/// Returns (author_name, content_preview, target_message_id) or (None, None, None).
fn extract_reply_context(
    content: &RoomMessageBody,
    secrets: &HashMap<u32, [u8; 32]>,
) -> (Option<String>, Option<String>, Option<MessageId>) {
    use river_core::room_state::content::{ReplyContentV1, CONTENT_TYPE_REPLY};

    match content {
        RoomMessageBody::Public {
            content_type, data, ..
        } if *content_type == CONTENT_TYPE_REPLY => {
            if let Ok(reply) = ReplyContentV1::decode(data) {
                return (
                    Some(reply.target_author_name),
                    Some(reply.target_content_preview),
                    Some(reply.target_message_id),
                );
            }
        }
        RoomMessageBody::Private {
            content_type,
            ciphertext,
            nonce,
            secret_version,
            ..
        } if *content_type == CONTENT_TYPE_REPLY => {
            if let Some(secret) = secrets.get(secret_version) {
                use crate::util::ecies::decrypt_with_symmetric_key;
                if let Ok(decrypted_bytes) =
                    decrypt_with_symmetric_key(secret, ciphertext.as_slice(), nonce)
                {
                    if let Ok(reply) = ReplyContentV1::decode(&decrypted_bytes) {
                        return (
                            Some(reply.target_author_name),
                            Some(reply.target_content_preview),
                            Some(reply.target_message_id),
                        );
                    }
                }
            }
        }
        _ => {}
    }
    (None, None, None)
}

/// Convert message text to HTML with clickable links that open in new tabs.
///
/// Uses GFM autolink literals to linkify plain URLs while correctly
/// skipping URLs inside code spans and other non-text contexts.
fn message_to_html(text: &str) -> String {
    // Convert single newlines to hard breaks (two spaces + newline)
    // This preserves line breaks in chat messages as users expect
    let with_hard_breaks = text.replace("\n", "  \n");

    markdown_to_html(&with_hard_breaks)
}

/// Convert markdown text to HTML with clickable links that open in new tabs.
fn markdown_to_html(text: &str) -> String {
    // Convert markdown to HTML using GFM mode, which includes autolink
    // literals that correctly handle code spans, existing links, etc.
    let html = markdown::to_html_with_options(text, &markdown::Options::gfm())
        .unwrap_or_else(|_| markdown::to_html(text));

    finalize_anchors(&html)
}

/// Walk anchor tags in HTML once: add target="_blank" rel="noopener noreferrer"
/// to all of them, and shorten the visible text of bare Freenet web-contract
/// URLs to a `freenet:<id-prefix>[/<path>]` label (only when the visible text
/// equals the href, so user-customized link text is left alone).
///
/// Assumes the markdown crate emits anchors as `<a href="...">...</a>` with
/// `href` as the first attribute. Both `extract_href` and the `replacen` here
/// rely on that shape; if it ever changes, target/rel injection silently
/// no-ops and beautification is skipped.
fn finalize_anchors(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + 32);
    let mut rest = html;
    while let Some(pos) = rest.find("<a ") {
        out.push_str(&rest[..pos]);
        let tag = &rest[pos..];
        let Some(open_end) = tag.find('>') else {
            out.push_str(tag);
            return out;
        };
        let opening = &tag[..=open_end];
        let after_open = &tag[open_end + 1..];
        let Some(close_pos) = after_open.find("</a>") else {
            out.push_str(tag);
            return out;
        };
        let inner = &after_open[..close_pos];
        let tail = &after_open[close_pos + 4..];

        let opening = opening.replacen(
            "<a href=\"",
            "<a target=\"_blank\" rel=\"noopener noreferrer\" href=\"",
            1,
        );
        let href = extract_href(&opening);
        let new_inner = match href.as_deref() {
            Some(h) if h == inner => beautify_freenet_label(h).unwrap_or_else(|| inner.to_string()),
            _ => inner.to_string(),
        };

        out.push_str(&opening);
        out.push_str(&new_inner);
        out.push_str("</a>");
        rest = tail;
    }
    out.push_str(rest);
    out
}

fn extract_href(opening_tag: &str) -> Option<String> {
    let start = opening_tag.find("href=\"")? + "href=\"".len();
    let end = opening_tag[start..].find('"')?;
    Some(opening_tag[start..start + end].to_string())
}

/// If `url` is a Freenet web-contract URL, return a beautified label like
/// `freenet:UDzGbcWr` or `freenet:UDzGbcWr/index.html`. Returns None for any
/// other URL so the caller falls back to the original link text.
///
/// The marker must appear at the start of the URL path, not just anywhere
/// in the URL — otherwise links like `https://x/redirect?next=/v1/contract/web/<id>/`
/// would be mis-presented as Freenet links.
fn beautify_freenet_label(url: &str) -> Option<String> {
    let scheme_end = url.find("://")?;
    let after_scheme = &url[scheme_end + 3..];
    // Locate the path: skip authority (host[:port]) up to the first '/'.
    let path_start = after_scheme.find('/')?;
    let path = &after_scheme[path_start..];
    let after_marker = path.strip_prefix("/v1/contract/web/")?;

    let id_end = after_marker
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(after_marker.len());
    if id_end == 0 {
        return None;
    }
    let id = &after_marker[..id_end];
    let suffix = &after_marker[id_end..];
    // Defense in depth: refuse to beautify if the path/query carries raw HTML
    // metacharacters. The markdown crate URL-encodes these today, but this
    // value is rendered via dangerous_inner_html with no further escaping,
    // so we'd rather skip the rewrite than risk smuggling markup.
    if suffix.contains(['<', '>', '"']) {
        return None;
    }
    // A bare trailing slash adds no information; drop it.
    let suffix = if suffix == "/" { "" } else { suffix };

    let id_prefix = &id[..id.len().min(8)];
    Some(format!("freenet:{id_prefix}{suffix}"))
}

#[component]
pub fn Conversation() -> Element {
    let current_room_data = {
        let current_room = CURRENT_ROOM.read();
        if let Some(key) = current_room.owner_key {
            // Use try_read() to avoid panic when ROOMS is mutably borrowed.
            // Dioxus write guard Drop notifies subscribers synchronously on Firefox,
            // which can re-enter this component while ROOMS is still borrowed.
            ROOMS
                .try_read()
                .ok()
                .and_then(|rooms| rooms.map.get(&key).cloned())
        } else {
            None
        }
    };
    let last_chat_element = use_signal(|| None as Option<Rc<MountedData>>);
    let mut is_at_bottom = use_signal(|| true);
    let mut replying_to: Signal<Option<ReplyContext>> = use_signal(|| None);

    // State for delete confirmation modal
    let mut pending_delete: Signal<Option<MessageId>> = use_signal(|| None);

    // Trigger for editing a message from outside MessageGroupComponent (e.g. up-arrow in input)
    // Value is (message_id_str, message_text)
    let mut edit_trigger: Signal<Option<(String, String)>> = use_signal(|| None);

    let current_room_label = use_memo({
        move || {
            let current_room = CURRENT_ROOM.read();
            if let Some(key) = current_room.owner_key {
                let Ok(rooms) = ROOMS.try_read() else {
                    return "No Room Selected".to_string();
                };
                if let Some(room_data) = rooms.map.get(&key) {
                    let sealed_name = &room_data
                        .room_state
                        .configuration
                        .configuration
                        .display
                        .name;
                    return match unseal_bytes_with_secrets(sealed_name, &room_data.secrets) {
                        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                        Err(_) => sealed_name.to_string_lossy(),
                    };
                }
            }
            "No Room Selected".to_string()
        }
    });

    // Memoize room description as rendered HTML (markdown)
    let current_room_description_html = use_memo({
        move || {
            let current_room = CURRENT_ROOM.read();
            if let Some(key) = current_room.owner_key {
                let Ok(rooms) = ROOMS.try_read() else {
                    return None;
                };
                if let Some(room_data) = rooms.map.get(&key) {
                    let sealed_desc = room_data
                        .room_state
                        .configuration
                        .configuration
                        .display
                        .description
                        .as_ref()?;
                    let text = match unseal_bytes_with_secrets(sealed_desc, &room_data.secrets) {
                        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                        Err(_) => sealed_desc.to_string_lossy(),
                    };
                    if text.is_empty() {
                        return None;
                    }
                    return Some(markdown_to_html(&text));
                }
            }
            None
        }
    });

    // Memoize expensive message grouping (decryption + markdown parsing)
    // This prevents re-computing on every render/keystroke
    // Returns (groups, self_member_id, member_names) so we can highlight user's reactions and show names in tooltips
    let message_groups = use_memo(move || {
        let current_room = CURRENT_ROOM.read();
        if let Some(key) = current_room.owner_key {
            let Ok(rooms) = ROOMS.try_read() else {
                return None;
            };
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
                    let groups = group_messages(
                        &room_state.recent_messages,
                        &room_state.member_info,
                        self_member_id,
                        &room_data.secrets,
                    );
                    // Build member name lookup for reaction tooltips
                    let member_names: HashMap<MemberId, String> = room_state
                        .member_info
                        .member_info
                        .iter()
                        .map(|ami| {
                            let name = match unseal_bytes_with_secrets(
                                &ami.member_info.preferred_nickname,
                                &room_data.secrets,
                            ) {
                                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                                Err(_) => ami.member_info.preferred_nickname.to_string_lossy(),
                            };
                            (ami.member_info.member_id, name)
                        })
                        .collect();
                    return Some((groups, self_member_id, member_names));
                }
            }
        }
        None
    });

    // Use IntersectionObserver to track whether the user is near the bottom of the
    // chat scroll container.  This replaces the old `onscroll` handler that performed
    // DOM queries (scrollTop / clientHeight / scrollHeight) on every scroll event,
    // causing visible scroll-bar jank on mobile (issue #151).
    //
    // A 1px invisible sentinel div sits at the bottom of the scroll content.
    // A rootMargin of "0px 0px 100px 0px" expands the detection zone 100px above
    // the sentinel, so the user is considered "at the bottom" when within 100px of
    // the end.  The observer fires only on intersection changes, so there is zero
    // work during normal scrolling.
    #[cfg(target_arch = "wasm32")]
    use_effect(move || {
        use wasm_bindgen::prelude::*;

        let Some(window) = web_sys::window() else {
            return;
        };
        let Some(document) = window.document() else {
            return;
        };
        let Some(sentinel) = document.get_element_by_id("bottom-sentinel") else {
            return;
        };
        let Some(root) = document.get_element_by_id("chat-scroll-container") else {
            return;
        };

        let cb = Closure::wrap(Box::new(move |entries: js_sys::Array| {
            if let Some(entry) = entries
                .get(0)
                .dyn_ref::<web_sys::IntersectionObserverEntry>()
            {
                is_at_bottom.set(entry.is_intersecting());
            }
        }) as Box<dyn FnMut(js_sys::Array)>);

        let options = web_sys::IntersectionObserverInit::new();
        options.set_root(Some(&root));
        // Expand detection zone 100px below the viewport edge so the user is
        // considered "at bottom" when within 100px of the sentinel.
        options.set_root_margin("0px 0px 100px 0px");
        options.set_threshold(&JsValue::from_f64(0.0));

        if let Ok(observer) =
            web_sys::IntersectionObserver::new_with_options(cb.as_ref().unchecked_ref(), &options)
        {
            observer.observe(&sentinel);
            // Leak the closure so it lives as long as the observer.  The Conversation
            // component is mounted once and never unmounted (hidden/shown via CSS),
            // so this leak is bounded.  Dioxus use_effect has no cleanup return, so
            // explicit disconnect is not possible here.
            cb.forget();
        }
    });

    // Trigger scroll to bottom when recent messages change (only if user is near bottom)
    use_effect(move || {
        let container = last_chat_element();
        let should_scroll = *is_at_bottom.peek();
        if should_scroll {
            if let Some(container) = container {
                crate::util::safe_spawn_local(async move {
                    let _ = container.scroll_to(ScrollBehavior::Smooth).await;
                });
            }
        }
    });

    // Handler for toggling a reaction on a message (add or remove)
    let handle_toggle_reaction = {
        let current_room_data = current_room_data.clone();
        move |target_message_id: MessageId, emoji: String| {
            if let (Some(current_room), Some(current_room_data)) =
                (CURRENT_ROOM.read().owner_key, current_room_data.clone())
            {
                let room_key = current_room_data.room_key();
                let self_sk = current_room_data.self_sk.clone();
                let room_state_clone = current_room_data.room_state.clone();
                let is_private = current_room_data
                    .room_state
                    .configuration
                    .configuration
                    .privacy_mode
                    == river_core::room_state::privacy::PrivacyMode::Private;
                let secret_opt = current_room_data
                    .get_secret()
                    .map(|(secret, version)| (*secret, version));

                // Check user's existing reaction on this message (if any)
                // Rule: one reaction per user per message
                let self_member_id = MemberId::from(&self_sk.verifying_key());
                let existing_reaction: Option<String> = current_room_data
                    .room_state
                    .recent_messages
                    .reactions(&target_message_id)
                    .and_then(|reactions| {
                        reactions.iter().find_map(|(e, reactors)| {
                            if reactors.contains(&self_member_id) {
                                Some(e.clone())
                            } else {
                                None
                            }
                        })
                    });

                let clicked_same = existing_reaction.as_ref() == Some(&emoji);
                let has_existing = existing_reaction.is_some();

                spawn_local(async move {
                    use crate::util::ecies::encrypt_with_symmetric_key;
                    use river_core::room_state::content::ActionContentV1;

                    // Build list of actions:
                    // - If clicking same emoji: just remove it
                    // - If clicking different emoji: remove old (if any) + add new
                    let mut messages_to_send = Vec::new();

                    if clicked_same {
                        // Remove the existing reaction
                        let content = if is_private {
                            if let Some((secret, version)) = &secret_opt {
                                let action = ActionContentV1::remove_reaction(
                                    target_message_id.clone(),
                                    emoji.clone(),
                                );
                                let action_bytes = action.encode();
                                let (ciphertext, nonce) =
                                    encrypt_with_symmetric_key(secret, &action_bytes);
                                RoomMessageBody::private_action(ciphertext, nonce, *version)
                            } else {
                                warn!("Room is private but no secret available");
                                return;
                            }
                        } else {
                            RoomMessageBody::remove_reaction(
                                target_message_id.clone(),
                                emoji.clone(),
                            )
                        };
                        messages_to_send.push(content);
                    } else {
                        // Remove old reaction if exists, then add new one
                        if let Some(old_emoji) = existing_reaction {
                            let content = if is_private {
                                if let Some((secret, version)) = &secret_opt {
                                    let action = ActionContentV1::remove_reaction(
                                        target_message_id.clone(),
                                        old_emoji,
                                    );
                                    let action_bytes = action.encode();
                                    let (ciphertext, nonce) =
                                        encrypt_with_symmetric_key(secret, &action_bytes);
                                    RoomMessageBody::private_action(ciphertext, nonce, *version)
                                } else {
                                    warn!("Room is private but no secret available");
                                    return;
                                }
                            } else {
                                RoomMessageBody::remove_reaction(
                                    target_message_id.clone(),
                                    old_emoji,
                                )
                            };
                            messages_to_send.push(content);
                        }

                        // Add new reaction
                        let content = if is_private {
                            if let Some((secret, version)) = &secret_opt {
                                let action = ActionContentV1::reaction(
                                    target_message_id.clone(),
                                    emoji.clone(),
                                );
                                let action_bytes = action.encode();
                                let (ciphertext, nonce) =
                                    encrypt_with_symmetric_key(secret, &action_bytes);
                                RoomMessageBody::private_action(ciphertext, nonce, *version)
                            } else {
                                warn!("Room is private but no secret available");
                                return;
                            }
                        } else {
                            RoomMessageBody::reaction(target_message_id.clone(), emoji.clone())
                        };
                        messages_to_send.push(content);
                    }

                    // Sign and collect all messages
                    let mut auth_messages = Vec::new();
                    for content in messages_to_send {
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
                            message_bytes.clone(),
                            &self_sk,
                        )
                        .await;

                        auth_messages.push(AuthorizedMessageV1::with_signature(message, signature));
                    }

                    // Apply all messages in one delta
                    if !auth_messages.is_empty() {
                        let (members_delta, member_info_delta) =
                            try_rejoin_delta(&current_room, "reaction");
                        let delta = ChatRoomStateV1Delta {
                            recent_messages: Some(auth_messages),
                            members: members_delta,
                            member_info: member_info_delta,
                            ..Default::default()
                        };
                        info!(
                            "Toggling reaction (clicked_same={}, had_existing={})",
                            clicked_same, has_existing
                        );
                        // Defer ROOMS mutation to a clean execution context to
                        // prevent RefCell re-entrant borrow panics (see #send handler).
                        crate::util::defer(move || {
                            let reaction_applied = ROOMS.with_mut(|rooms| {
                                if let Some(room_data) = rooms.map.get_mut(&current_room) {
                                    if let Err(e) = room_data.room_state.apply_delta(
                                        &room_state_clone,
                                        &ChatRoomParametersV1 {
                                            owner: current_room,
                                        },
                                        &Some(delta),
                                    ) {
                                        error!("Failed to apply reaction delta: {:?}", e);
                                        false
                                    } else {
                                        true
                                    }
                                } else {
                                    false
                                }
                            });
                            if reaction_applied {
                                crate::components::app::mark_needs_sync(current_room);
                            }
                        });
                    }
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
                let is_private = current_room_data
                    .room_state
                    .configuration
                    .configuration
                    .privacy_mode
                    == river_core::room_state::privacy::PrivacyMode::Private;
                let secret_opt = current_room_data
                    .get_secret()
                    .map(|(secret, version)| (*secret, version));

                spawn_local(async move {
                    use crate::util::ecies::encrypt_with_symmetric_key;
                    use river_core::room_state::content::ActionContentV1;

                    // Create the action content - encrypt if private room
                    let content = if is_private {
                        if let Some((secret, version)) = secret_opt {
                            let action = ActionContentV1::delete(target_message_id.clone());
                            let action_bytes = action.encode();
                            let (ciphertext, nonce) =
                                encrypt_with_symmetric_key(&secret, &action_bytes);
                            RoomMessageBody::private_action(ciphertext, nonce, version)
                        } else {
                            warn!("Room is private but no secret available, cannot send delete");
                            return;
                        }
                    } else {
                        RoomMessageBody::delete(target_message_id)
                    };

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
                    let (members_delta, member_info_delta) =
                        try_rejoin_delta(&current_room, "delete");
                    let delta = ChatRoomStateV1Delta {
                        recent_messages: Some(vec![auth_message]),
                        members: members_delta,
                        member_info: member_info_delta,
                        ..Default::default()
                    };
                    info!("Sending delete action");
                    // Defer ROOMS mutation to a clean execution context to
                    // prevent RefCell re-entrant borrow panics.
                    crate::util::defer(move || {
                        let delete_applied = ROOMS.with_mut(|rooms| {
                            if let Some(room_data) = rooms.map.get_mut(&current_room) {
                                if let Err(e) = room_data.room_state.apply_delta(
                                    &room_state_clone,
                                    &ChatRoomParametersV1 {
                                        owner: current_room,
                                    },
                                    &Some(delta),
                                ) {
                                    error!("Failed to apply delete delta: {:?}", e);
                                    false
                                } else {
                                    true
                                }
                            } else {
                                false
                            }
                        });
                        if delete_applied {
                            crate::components::app::mark_needs_sync(current_room);
                        }
                    });
                });
            }
        }
    };

    // Handler for editing a message
    let handle_edit_message = {
        let current_room_data = current_room_data.clone();
        move |target_message_id: MessageId, new_text: String| {
            if new_text.is_empty() {
                warn!("Edit text is empty");
                return;
            }
            if let (Some(current_room), Some(current_room_data)) =
                (CURRENT_ROOM.read().owner_key, current_room_data.clone())
            {
                let room_key = current_room_data.room_key();
                let self_sk = current_room_data.self_sk.clone();
                let room_state_clone = current_room_data.room_state.clone();
                let is_private = current_room_data
                    .room_state
                    .configuration
                    .configuration
                    .privacy_mode
                    == river_core::room_state::privacy::PrivacyMode::Private;
                let secret_opt = current_room_data
                    .get_secret()
                    .map(|(secret, version)| (*secret, version));

                spawn_local(async move {
                    use crate::util::ecies::encrypt_with_symmetric_key;
                    use river_core::room_state::content::ActionContentV1;

                    // Create the edit action content
                    let content = if is_private {
                        if let Some((secret, version)) = secret_opt {
                            // For private rooms, encrypt the action
                            let action = ActionContentV1::edit(target_message_id.clone(), new_text);
                            let action_bytes = action.encode();
                            let (ciphertext, nonce) =
                                encrypt_with_symmetric_key(&secret, &action_bytes);
                            RoomMessageBody::private_action(ciphertext, nonce, version)
                        } else {
                            warn!("Room is private but no secret available, cannot send edit");
                            return;
                        }
                    } else {
                        // For public rooms, use the public edit constructor
                        RoomMessageBody::edit(target_message_id, new_text)
                    };

                    let message = MessageV1 {
                        room_owner: MemberId::from(current_room),
                        author: MemberId::from(&self_sk.verifying_key()),
                        content,
                        time: get_current_system_time(),
                    };

                    let mut message_bytes = Vec::new();
                    if let Err(e) = ciborium::ser::into_writer(&message, &mut message_bytes) {
                        error!("Failed to serialize edit message: {:?}", e);
                        return;
                    }

                    let signature = crate::signing::sign_message_with_fallback(
                        room_key,
                        message_bytes,
                        &self_sk,
                    )
                    .await;

                    let auth_message = AuthorizedMessageV1::with_signature(message, signature);
                    let (members_delta, member_info_delta) =
                        try_rejoin_delta(&current_room, "edit");
                    let delta = ChatRoomStateV1Delta {
                        recent_messages: Some(vec![auth_message]),
                        members: members_delta,
                        member_info: member_info_delta,
                        ..Default::default()
                    };
                    info!("Sending edit action");
                    // Defer ROOMS mutation to a clean execution context to
                    // prevent RefCell re-entrant borrow panics.
                    crate::util::defer(move || {
                        let edit_applied = ROOMS.with_mut(|rooms| {
                            if let Some(room_data) = rooms.map.get_mut(&current_room) {
                                if let Err(e) = room_data.room_state.apply_delta(
                                    &room_state_clone,
                                    &ChatRoomParametersV1 {
                                        owner: current_room,
                                    },
                                    &Some(delta),
                                ) {
                                    error!("Failed to apply edit delta: {:?}", e);
                                    false
                                } else {
                                    true
                                }
                            } else {
                                false
                            }
                        });
                        if edit_applied {
                            crate::components::app::mark_needs_sync(current_room);
                        }
                    });
                });
            }
        }
    };

    // Message sending handler - receives message text from MessageInput component
    let handle_send_message = {
        move |(message_text, reply_ctx): (String, Option<ReplyContext>)| {
            // Always scroll to bottom when user sends their own message
            is_at_bottom.set(true);

            if message_text.is_empty() {
                warn!("Message is empty");
                return;
            }
            crate::util::debug_log(&format!(
                "[send] start: {}...",
                crate::util::truncate_str(&message_text, 30)
            ));
            let current_room_opt = CURRENT_ROOM.read().owner_key;
            if current_room_opt.is_none() {
                error!("Cannot send message: no room selected (CURRENT_ROOM is None)");
                return;
            }
            // Re-read room data from ROOMS signal (don't rely on stale closure capture)
            let fresh_room_data =
                current_room_opt.and_then(|key| ROOMS.try_read().ok()?.map.get(&key).cloned());
            if fresh_room_data.is_none() {
                error!("Cannot send message: room data not loaded (ROOMS has no entry for current room)");
                return;
            }
            if let (Some(current_room), Some(current_room_data)) =
                (current_room_opt, fresh_room_data)
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
                        ReplyContentV1, TextContentV1, CONTENT_TYPE_REPLY, CONTENT_TYPE_TEXT,
                        REPLY_CONTENT_VERSION, TEXT_CONTENT_VERSION,
                    };

                    // Build content based on whether this is a reply or regular message
                    let content = if let Some(reply) = reply_ctx {
                        // Reply message
                        if is_private {
                            if let Some((secret, version)) = secret_opt {
                                let reply_content = ReplyContentV1::new(
                                    message_text.clone(),
                                    reply.message_id,
                                    reply.author_name,
                                    reply.content_preview,
                                );
                                let content_bytes = reply_content.encode();
                                let (ciphertext, nonce) =
                                    encrypt_with_symmetric_key(&secret, &content_bytes);
                                RoomMessageBody::private(
                                    CONTENT_TYPE_REPLY,
                                    REPLY_CONTENT_VERSION,
                                    ciphertext,
                                    nonce,
                                    version,
                                )
                            } else {
                                warn!("Room is private but no secret available, sending reply as public");
                                RoomMessageBody::reply(
                                    message_text.clone(),
                                    reply.message_id,
                                    reply.author_name,
                                    reply.content_preview,
                                )
                            }
                        } else {
                            RoomMessageBody::reply(
                                message_text.clone(),
                                reply.message_id,
                                reply.author_name,
                                reply.content_preview,
                            )
                        }
                    } else {
                        // Regular text message
                        if is_private {
                            if let Some((secret, version)) = secret_opt {
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
                        }
                    };

                    // Safety net: check encoded content size before signing.
                    // The input UI blocks sending when text is over limit, but
                    // encoded size can differ slightly from raw text length.
                    let content_size = content.content_len();
                    let max_size = room_state_clone
                        .configuration
                        .configuration
                        .max_message_size;
                    if content_size > max_size {
                        warn!(
                            "Message too long: {} encoded bytes, max {} bytes",
                            content_size, max_size
                        );
                        return;
                    }

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
                    crate::util::debug_log("[send] signing message...");
                    let signature = crate::signing::sign_message_with_fallback(
                        room_key,
                        message_bytes,
                        &self_sk,
                    )
                    .await;
                    crate::util::debug_log("[send] signed OK");

                    let auth_message = AuthorizedMessageV1::with_signature(message, signature);

                    // Re-add ourselves if pruned for inactivity.
                    // Uses try_read() to avoid RefCell re-entrant borrow panics
                    // inside spawn_local (see AGENTS.md "Dioxus WASM Signal Safety Rules").
                    let (members_delta, member_info_delta) =
                        try_rejoin_delta(&current_room, "send");

                    // Build message list. No join event here — join events are
                    // published at invitation acceptance time (in get_response.rs).
                    // This path only fires when re-adding after inactivity pruning.
                    let messages = vec![auth_message.clone()];

                    let delta = ChatRoomStateV1Delta {
                        recent_messages: Some(messages),
                        members: members_delta,
                        member_info: member_info_delta,
                        ..Default::default()
                    };
                    info!("Sending message: {:?}", auth_message);

                    crate::util::debug_log("[send] applying delta to local state...");
                    // Defer ROOMS mutation to a clean execution context to
                    // prevent RefCell re-entrant borrow panics.
                    crate::util::defer(move || {
                        let delta_applied = ROOMS.with_mut(|rooms| {
                            if let Some(room_data) = rooms.map.get_mut(&current_room) {
                                if let Err(e) = room_data.room_state.apply_delta(
                                    &room_state_clone,
                                    &ChatRoomParametersV1 {
                                        owner: current_room,
                                    },
                                    &Some(delta),
                                ) {
                                    crate::util::debug_log(&format!(
                                        "[send] delta FAILED: {:?}",
                                        e
                                    ));
                                    error!("Failed to apply message delta: {:?}", e);
                                    false
                                } else {
                                    crate::util::debug_log("[send] delta applied OK");
                                    true
                                }
                            } else {
                                crate::util::debug_log("[send] room not found in ROOMS!");
                                false
                            }
                        });
                        if delta_applied {
                            crate::util::debug_log("[send] marking NEEDS_SYNC");
                            crate::components::app::mark_needs_sync(current_room);
                            #[cfg(target_arch = "wasm32")]
                            request_permission_on_first_message();
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
                current_room_data.as_ref().map(|_room_data| {
                    rsx! {
                        div { class: "flex-shrink-0 px-3 md:px-6 py-3 border-b border-border bg-panel",
                            div { class: "flex items-center justify-between max-w-4xl mx-auto",
                                // Mobile: hamburger to open rooms panel
                                button {
                                    class: "md:hidden p-2 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
                                    onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Rooms),
                                    Icon { icon: FaBars, width: 18, height: 18 }
                                }
                                button {
                                    class: "flex items-center gap-2 px-3 py-1.5 -mx-3 rounded-lg bg-transparent hover:bg-surface transition-colors cursor-pointer min-w-0 flex-1",
                                    title: "Room details",
                                    onclick: move |_| {
                                        crate::util::defer(move || {
                                            if let Some(current_room) = CURRENT_ROOM.read().owner_key {
                                                EDIT_ROOM_MODAL.with_mut(|modal| {
                                                    modal.room = Some(current_room);
                                                });
                                            }
                                        });
                                    },
                                    div { class: "min-w-0",
                                        div { class: "flex items-center gap-2",
                                            h2 { class: "text-lg font-semibold text-text truncate",
                                                "{current_room_label}"
                                            }
                                            span {
                                                class: "text-text-muted flex-shrink-0",
                                                Icon { icon: FaCircleInfo, width: 16, height: 16 }
                                            }
                                        }
                                        if let Some(desc_html) = current_room_description_html.read().as_ref() {
                                            div {
                                                class: "prose prose-sm dark:prose-invert max-w-none text-xs text-text-muted truncate [&>p]:m-0 [&>p]:inline",
                                                dangerous_inner_html: "{desc_html}"
                                            }
                                        }
                                    }
                                }
                                // Mobile: button to open members panel
                                button {
                                    class: "md:hidden p-2 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors flex-shrink-0",
                                    onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Members),
                                    Icon { icon: FaUsers, width: 18, height: 18 }
                                }
                            }
                        }
                    }
                })
            }

            // Message area with constrained width
            // Outer div handles flex sizing; inner div handles scrolling.
            // Combining flex-1 with overflow on the same element causes the
            // scroll container to shift behind the sidebar during re-renders.
            div {
                class: "flex-1 min-h-0",
                div {
                    class: "h-full overflow-y-auto",
                    id: "chat-scroll-container",
                    div { class: "max-w-4xl mx-auto px-4 py-4",
                    {
                        // Use memoized message groups to avoid expensive re-computation on keystrokes
                        if current_room_data.is_some() {
                            match message_groups.read().as_ref() {
                                Some((groups, self_member_id, member_names)) => {
                                    let groups = groups.clone();
                                    let self_member_id = *self_member_id;
                                    let member_names = member_names.clone();
                                    let groups_len = groups.len();
                                    Some(rsx! {
                                        div { class: "space-y-4",
                                            {groups.into_iter().enumerate().map({
                                                let handle_toggle_reaction = handle_toggle_reaction.clone();
                                                let member_names = member_names.clone();
                                                move |(group_idx, item)| {
                                                let is_last_group = group_idx == groups_len - 1;
                                                let handle_toggle_reaction = handle_toggle_reaction.clone();
                                                let handle_edit_message = handle_edit_message.clone();
                                                let member_names = member_names.clone();
                                                match item {
                                                    DisplayItem::Event(summary) => {
                                                        let text = format_event_summary(&summary.names);
                                                        let key = summary.id.clone();
                                                        let mut last_el = last_chat_element;
                                                        rsx! {
                                                            div {
                                                                key: "{key}",
                                                                class: "flex justify-center py-1",
                                                                span {
                                                                    class: "text-xs text-text-muted italic",
                                                                    "{text}"
                                                                }
                                                                if is_last_group {
                                                                    div {
                                                                        onmounted: move |data| {
                                                                            last_el.set(Some(data.data()));
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                    DisplayItem::Messages(group) => {
                                                        let key = group.messages[0].id.clone();
                                                        rsx! {
                                                            MessageGroupComponent {
                                                                key: "{key}",
                                                                group: group,
                                                                self_member_id: self_member_id,
                                                                member_names: member_names,
                                                                last_chat_element: if is_last_group { Some(last_chat_element) } else { None },
                                                                edit_trigger: edit_trigger,
                                                                on_react: move |(msg_id, emoji)| {
                                                                    handle_toggle_reaction(msg_id, emoji);
                                                                },
                                                                on_request_delete: move |msg_id| {
                                                                    pending_delete.set(Some(msg_id));
                                                                },
                                                                on_edit: move |(msg_id, new_text)| {
                                                                    handle_edit_message(msg_id, new_text);
                                                                },
                                                                on_reply: move |ctx: ReplyContext| {
                                                                    replying_to.set(Some(ctx));
                                                                    if let Some(window) = web_sys::window() {
                                                                        if let Some(doc) = window.document() {
                                                                            if let Some(el) = doc.get_element_by_id("message-input") {
                                                                                if let Some(el) = el.dyn_ref::<web_sys::HtmlElement>() {
                                                                                    let _ = el.focus();
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                },
                                                            }
                                                        }
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
                    // Invisible sentinel near the bottom of the scroll container.
                    // An IntersectionObserver watches this element instead of using
                    // onscroll, which avoids per-scroll-event DOM queries that cause
                    // scroll jank on mobile (see issue #151).
                    div {
                        id: "bottom-sentinel",
                        class: "h-px pointer-events-none",
                    }
            }
            }

            // Message input or status
            {
                // Find user's most recent message for up-arrow-to-edit
                let request_edit_last = move |_| {
                    if let Some((groups, _, _)) = message_groups.read().as_ref() {
                        for item in groups.iter().rev() {
                            if let DisplayItem::Messages(group) = item {
                                if group.is_self {
                                    if let Some(msg) = group.messages.last() {
                                        edit_trigger.set(Some((msg.id.clone(), msg.content_text.clone())));
                                        return;
                                    }
                                }
                            }
                        }
                    }
                };

                match current_room_data.as_ref() {
                    Some(room_data) if room_data.is_awaiting_initial_sync() => {
                        rsx! {
                            div { class: "px-4 py-3 mx-4 mb-4 bg-surface rounded-lg text-sm text-text-muted flex items-center gap-3",
                                div { class: "animate-spin w-4 h-4 border-2 border-accent border-t-transparent rounded-full" }
                                span { "Syncing room state from the network... You'll be able to send messages once sync completes." }
                            }
                        }
                    },
                    Some(room_data) => {
                        match room_data.can_participate() {
                            Ok(()) => {
                                let max_msg_size = room_data.room_state.configuration.configuration.max_message_size;
                                rsx! {
                                    MessageInput {
                                        handle_send_message: move |msg: (String, Option<ReplyContext>)| {
                                            let mut handle = handle_send_message.clone();
                                            handle(msg)
                                        },
                                        replying_to: replying_to,
                                        on_request_edit_last: request_edit_last,
                                        max_message_size: max_msg_size,
                                    }
                                }
                            },
                            Err(SendMessageError::UserNotMember) => {
                                let user_vk = room_data.self_sk.verifying_key();
                                rsx! {
                                    NotMemberNotification {
                                        user_verifying_key: user_vk
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
                        // Mobile: show hamburger to access room list even with no room selected
                        div { class: "md:hidden flex-shrink-0 px-3 py-3 border-b border-border bg-panel",
                            button {
                                class: "p-2 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
                                onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Rooms),
                                Icon { icon: FaBars, width: 18, height: 18 }
                            }
                        }
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

#[component]
fn MessageGroupComponent(
    group: MessageGroup,
    self_member_id: MemberId,
    member_names: HashMap<MemberId, String>,
    last_chat_element: Option<Signal<Option<Rc<MountedData>>>>,
    edit_trigger: Signal<Option<(String, String)>>,
    on_react: EventHandler<(MessageId, String)>,
    on_request_delete: EventHandler<MessageId>,
    on_edit: EventHandler<(MessageId, String)>,
    on_reply: EventHandler<ReplyContext>,
) -> Element {
    let timestamp_ms = group.first_time.timestamp_millis();
    let time_str = format_utc_as_local_time(timestamp_ms);
    let delay_suffix = group
        .first_delay_secs
        .map(|s| format!(" (received after {} delay)", format_delay(s)));
    let full_time_str = if group.time_clamped {
        format!(
            "{} (sender's clock may be incorrect — original timestamp was in the future)",
            format_utc_as_full_datetime(timestamp_ms)
        )
    } else if let Some(ref suffix) = delay_suffix {
        format!("{}{}", format_utc_as_full_datetime(timestamp_ms), suffix)
    } else {
        format_utc_as_full_datetime(timestamp_ms)
    };
    let time_clamped = group.time_clamped;
    let is_self = group.is_self;

    // Track which message's emoji picker is open (by message ID string)
    let mut open_emoji_picker: Signal<Option<String>> = use_signal(|| None);

    // Track if emoji picker should appear above (true) or below (false) the button
    let mut picker_show_above: Signal<bool> = use_signal(|| false);

    // Track which message is being edited and its current text
    let mut editing_message: Signal<Option<String>> = use_signal(|| None);
    let mut edit_text: Signal<String> = use_signal(String::new);

    // Watch for external edit requests (e.g. up-arrow in empty input)
    let message_ids: Vec<String> = group.messages.iter().map(|m| m.id.clone()).collect();
    use_effect(move || {
        let trigger = edit_trigger.read().clone();
        if let Some((trigger_id, trigger_text)) = trigger {
            if message_ids.contains(&trigger_id) {
                edit_text.set(trigger_text);
                editing_message.set(Some(trigger_id));
                edit_trigger.set(None);
            }
        }
    });

    rsx! {
        div {
            class: format!(
                "flex min-w-0 {}",
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
                                crate::util::defer(move || {
                                    MEMBER_INFO_MODAL.with_mut(|signal| {
                                        signal.member = Some(group.author_id);
                                    });
                                });
                            },
                            "{group.author_name}"
                        }
                        span {
                            class: if time_clamped {
                                "text-xs text-text-muted cursor-default italic opacity-70"
                            } else {
                                "text-xs text-text-muted cursor-default"
                            },
                            title: "{full_time_str}",
                            if time_clamped { "~{time_str}" } else { "{time_str}" }
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
                        let reply_author_val = msg.reply_to_author.clone();
                        let reply_preview_val = msg.reply_to_preview.clone();
                        let reply_target_id_val = msg.reply_to_message_id.clone();

                        rsx! {
                            div {
                                key: "{msg.id}",
                                id: "msg-{msg.id}",
                                class: "flex flex-col group",
                                // Container for message bubble + hover actions
                                div {
                                    class: "relative",
                                    // Message bubble (or edit form if editing)
                                    {
                                        let is_editing = editing_message.read().as_ref() == Some(&msg.id);
                                        let msg_id_for_save = msg.message_id.clone();
                                        let original_text = msg.content_text.clone();
                                        if is_editing {
                                            let save_msg_id = msg_id_for_save.clone();
                                            let save_original = original_text.clone();
                                            rsx! {
                                                div {
                                                    class: format!(
                                                        "p-3 rounded-2xl {}",
                                                        if is_self { "bg-accent" } else { "bg-surface" }
                                                    ),
                                                    style: "width: 100%; max-width: 550px; overflow: visible;",
                                                    tabindex: "0",
                                                    // Scroll into view when edit dialog appears (#93)
                                                    onmounted: move |cx| {
                                                        let el = cx.data();
                                                        wasm_bindgen_futures::spawn_local(async move {
                                                            let _ = el.scroll_to(ScrollBehavior::Smooth).await;
                                                        });
                                                    },
                                                    // Global key bindings on the container (#94)
                                                    onkeydown: {
                                                        let msg_id = msg_id_for_save.clone();
                                                        let original = original_text.clone();
                                                        move |e: KeyboardEvent| {
                                                            if e.key() == Key::Escape {
                                                                editing_message.set(None);
                                                            } else if e.key() == Key::Enter && !e.modifiers().shift() {
                                                                e.prevent_default();
                                                                let new_text = edit_text.read().clone();
                                                                if !new_text.is_empty() && new_text != original {
                                                                    on_edit.call((msg_id.clone(), new_text));
                                                                }
                                                                editing_message.set(None);
                                                            }
                                                        }
                                                    },
                                                    textarea {
                                                        class: format!(
                                                            "w-full min-h-[240px] p-2 rounded-lg text-sm resize-y focus:outline-none {}",
                                                            if is_self { "bg-white/10 text-white placeholder-white/50 border border-white/20" } else { "bg-bg text-text border border-border" }
                                                        ),
                                                        value: "{edit_text}",
                                                        onmounted: move |cx| {
                                                            let element = cx.data();
                                                            wasm_bindgen_futures::spawn_local(async move {
                                                                let _ = element.set_focus(true).await;
                                                            });
                                                        },
                                                        oninput: move |e| edit_text.set(e.value().clone()),
                                                    }
                                                    div { class: "flex justify-end gap-3 mt-3",
                                                        style: "overflow: visible;",
                                                        button {
                                                            class: if is_self {
                                                                "flex-shrink-0 px-3 py-1.5 text-xs rounded-lg bg-white/20 text-white hover:bg-white/30"
                                                            } else {
                                                                "flex-shrink-0 px-3 py-1.5 text-xs rounded-lg bg-surface text-text hover:bg-border"
                                                            },
                                                            onclick: move |_| editing_message.set(None),
                                                            "Cancel (Esc)"
                                                        }
                                                        button {
                                                            class: "flex-shrink-0 px-3 py-1.5 text-xs rounded-lg font-medium hover:opacity-90",
                                                            style: "background-color: #2563eb; color: white;",
                                                            onclick: move |_| {
                                                                let new_text = edit_text.read().clone();
                                                                if !new_text.is_empty() && new_text != save_original {
                                                                    on_edit.call((save_msg_id.clone(), new_text));
                                                                }
                                                                editing_message.set(None);
                                                            },
                                                            "Save (Enter)"
                                                        }
                                                    }
                                                }
                                            }
                                        } else {
                                            let reply_author_inner = reply_author_val.clone();
                                            let reply_preview_inner = reply_preview_val.clone();
                                            let reply_target_inner = reply_target_id_val.clone();
                                            rsx! {
                                                // Message bubble. The reply strip (if any) is rendered as
                                                // the first child INSIDE the bubble so it shares the
                                                // bubble's width and its intrinsic size cannot reflow
                                                // the parent (fixes #206 and #207).
                                                div {
                                                    class: format!(
                                                        "flex flex-col text-sm overflow-hidden {} {} {}",
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
                                                        } else if is_first && is_last && !has_reactions {
                                                            "rounded-2xl"
                                                        } else if is_first {
                                                            "rounded-t-2xl rounded-br-2xl rounded-bl-md"
                                                        } else if is_last && !has_reactions {
                                                            "rounded-b-2xl rounded-tr-2xl rounded-tl-md"
                                                        } else {
                                                            "rounded-r-2xl rounded-l-md"
                                                        },
                                                        // Max width for readability; overflow-hidden on
                                                        // parent + min-w-0 on the reply strip prevents
                                                        // the nowrap strip from widening the bubble.
                                                        "max-w-prose"
                                                    ),
                                                    onmounted: move |cx| {
                                                        if is_last {
                                                            if let Some(mut last_el) = last_chat_element {
                                                                last_el.set(Some(cx.data()));
                                                            }
                                                        }
                                                    },
                                                    // Reply context strip (inside bubble, first child).
                                                    // Self bubbles use a white-tinted overlay so the strip
                                                    // stays legible against the accent background; other
                                                    // bubbles use a dark-tinted overlay against the surface
                                                    // background. The previous `bg-accent/40 text-accent`
                                                    // was invisible on self bubbles because the strip
                                                    // composited to the same colour as the bubble.
                                                    if let (Some(author), Some(preview)) = (reply_author_inner, reply_preview_inner) {
                                                        {
                                                            let target_id_str = reply_target_inner.map(|id| format!("{:?}", id.0)).unwrap_or_default();
                                                            // Clone the target id so we can own one copy in the
                                                            // onclick handler and one in the onkeydown handler.
                                                            let target_id_for_key = target_id_str.clone();
                                                            rsx! {
                                                                div {
                                                                    "data-testid": "reply-strip",
                                                                    class: format!(
                                                                        "reply-strip min-w-0 w-full text-[11px] leading-normal px-3 pt-1.5 pb-1.5 cursor-pointer {}",
                                                                        if is_self { "bg-white/25 text-white/90" } else { "bg-black/[0.12] text-text-muted" }
                                                                    ),
                                                                    title: "Scroll to original message (Enter or Space to activate)",
                                                                    role: "button",
                                                                    tabindex: "0",
                                                                    "aria-label": "Scroll to the message this is a reply to",
                                                                    onclick: move |_| {
                                                                        if let Some(window) = web_sys::window() {
                                                                            if let Some(doc) = window.document() {
                                                                                if let Some(el) = doc.get_element_by_id(&format!("msg-{}", target_id_str)) {
                                                                                    el.scroll_into_view();
                                                                                    let _ = el.class_list().add_1("reply-highlight");
                                                                                }
                                                                            }
                                                                        }
                                                                    },
                                                                    onkeydown: move |e: KeyboardEvent| {
                                                                        // Activate the same scroll-to-original
                                                                        // behaviour via Enter or Space so keyboard
                                                                        // users can reach it without a mouse.
                                                                        if e.key() == Key::Enter || e.key() == Key::Character(" ".to_string()) {
                                                                            e.prevent_default();
                                                                            if let Some(window) = web_sys::window() {
                                                                                if let Some(doc) = window.document() {
                                                                                    if let Some(el) = doc.get_element_by_id(&format!("msg-{}", target_id_for_key)) {
                                                                                        el.scroll_into_view();
                                                                                        let _ = el.class_list().add_1("reply-highlight");
                                                                                    }
                                                                                }
                                                                            }
                                                                        }
                                                                    },
                                                                    span { class: "font-medium", "\u{21a9} @{author}: " }
                                                                    span { "{preview}" }
                                                                }
                                                            }
                                                        }
                                                    }
                                                    // Message body, wrapped in a padding container so the
                                                    // "(edited)" indicator can sit inline at the trailing
                                                    // edge of the body text rather than as a separate
                                                    // flex-column row. `[overflow-wrap:anywhere]` ensures
                                                    // long URLs and unbreakable tokens wrap instead of
                                                    // forcing the bubble past `max-w-prose`. `anywhere` is
                                                    // stricter than `break-word`: it also lowers the
                                                    // element's min-content so flex/grid parents can shrink
                                                    // the bubble to fit.
                                                    div {
                                                        class: "px-3 py-2 min-w-0",
                                                        div {
                                                            class: "prose prose-sm dark:prose-invert max-w-none [overflow-wrap:anywhere]",
                                                            dangerous_inner_html: "{msg.content_html}"
                                                        }
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
                                                }
                                            }
                                        }
                                    }
                                    // Hover action bar (reply for all, edit/delete for own)
                                    {
                                        let msg_id_str_for_edit = msg.id.clone();
                                        let msg_id_for_delete = msg.message_id.clone();
                                        let msg_id_for_reply = msg.message_id.clone();
                                        let current_text = msg.content_text.clone();
                                        let reply_text_preview = msg.content_text.chars().take(100).collect::<String>();
                                        let reply_author_name = group.author_name.clone();
                                        rsx! {
                                            div {
                                                class: format!(
                                                    "absolute top-1/2 -translate-y-1/2 transition-opacity z-50 flex flex-col items-start bg-panel rounded-lg shadow-md border border-border px-2 py-1.5 opacity-0 group-hover:opacity-100 {} {}",
                                                    if is_self { "left-0 -translate-x-full -ml-2" } else { "right-0 translate-x-full ml-2" },
                                                    ""
                                                ),
                                                // Reply button - available for all messages
                                                button {
                                                    class: "text-xs text-text-muted hover:text-accent transition-colors",
                                                    title: "Reply",
                                                    onclick: move |_| {
                                                        on_reply.call(ReplyContext {
                                                            message_id: msg_id_for_reply.clone(),
                                                            author_name: reply_author_name.clone(),
                                                            content_preview: reply_text_preview.clone(),
                                                        });
                                                    },
                                                    "reply"
                                                }
                                                // Edit/Delete buttons - only for own messages
                                                if is_self {
                                                    button {
                                                        class: "text-xs text-text-muted hover:text-text transition-colors",
                                                        title: "Edit message",
                                                        onclick: move |_| {
                                                            edit_text.set(current_text.clone());
                                                            editing_message.set(Some(msg_id_str_for_edit.clone()));
                                                        },
                                                        "edit"
                                                    }
                                                    button {
                                                        class: "text-xs text-text-muted hover:text-red-500 transition-colors",
                                                        title: "Delete message",
                                                        onclick: move |_| {
                                                            on_request_delete.call(msg_id_for_delete.clone());
                                                        },
                                                        "delete"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                // Reactions display with inline add button
                                {
                                    let msg_id_for_inline = msg.id.clone();
                                    let msg_id_react = msg.message_id.clone();
                                    let is_inline_picker_open = open_emoji_picker.read().as_ref() == Some(&format!("inline-{}", msg_id_for_inline));

                                    // Find user's current reaction on this message (if any)
                                    let user_reaction: Option<String> = msg.reactions.iter().find_map(|(emoji, reactors)| {
                                        if reactors.contains(&self_member_id) {
                                            Some(emoji.clone())
                                        } else {
                                            None
                                        }
                                    });
                                    let user_reaction_for_picker = user_reaction.clone();

                                    rsx! {
                                        div {
                                            class: format!(
                                                "flex flex-wrap items-center gap-1 mt-0.5 {}",
                                                if is_self { "justify-end" } else { "justify-start" }
                                            ),
                                            // Existing reactions (clickable to toggle if user has reacted)
                                            {
                                                let mut sorted_reactions: Vec<_> = msg.reactions.iter().collect();
                                                sorted_reactions.sort_by_key(|(emoji, _)| emoji.as_str());
                                                sorted_reactions.into_iter().map(|(emoji, reactors)| {
                                                    let count = reactors.len();
                                                    let is_user_reaction = reactors.contains(&self_member_id);
                                                    let emoji_for_click = emoji.clone();
                                                    let msg_id_for_click = msg_id_react.clone();

                                                    // Build list of reactor names for tooltip
                                                    let reactor_names: Vec<String> = reactors.iter().map(|reactor_id| {
                                                        if *reactor_id == self_member_id {
                                                            "You".to_string()
                                                        } else {
                                                            member_names.get(reactor_id)
                                                                .cloned()
                                                                .unwrap_or_else(|| "Unknown".to_string())
                                                        }
                                                    }).collect();
                                                    let names_str = reactor_names.join(", ");

                                                    let tooltip = if is_user_reaction {
                                                        format!("{} (click to remove)", names_str)
                                                    } else {
                                                        names_str
                                                    };

                                                    rsx! {
                                                        span {
                                                            key: "{emoji}",
                                                            class: format!(
                                                                "inline-flex items-center gap-0.5 text-base transition-transform {}",
                                                                if is_user_reaction {
                                                                    // Subtle indicator: underline for user's reaction
                                                                    "cursor-pointer hover:scale-110 underline decoration-accent decoration-2 underline-offset-4"
                                                                } else {
                                                                    "cursor-default hover:scale-110"
                                                                }
                                                            ),
                                                            title: "{tooltip}",
                                                            onclick: move |_| {
                                                                if is_user_reaction {
                                                                    on_react.call((msg_id_for_click.clone(), emoji_for_click.clone()));
                                                                }
                                                            },
                                                            "{emoji}"
                                                            if count > 1 {
                                                                span { class: "text-xs text-text-muted", "{count}" }
                                                            }
                                                        }
                                                    }
                                                })
                                            }
                                            // Inline add reaction button (same line height as reactions)
                                            div {
                                                class: "relative group/react inline-flex items-center",
                                                // Invisible backdrop when picker is open
                                                if is_inline_picker_open {
                                                    div {
                                                        class: "fixed inset-0 z-40",
                                                        onclick: move |_| open_emoji_picker.set(None),
                                                    }
                                                }
                                                button {
                                                    class: format!(
                                                        "add-reaction-btn inline-flex items-center justify-center text-xl leading-none hover:scale-110 {}",
                                                        if has_reactions || is_inline_picker_open { "has-reactions" } else { "" }
                                                    ),
                                                    title: "Add reaction",
                                                    onclick: {
                                                        let picker_id = format!("inline-{}", msg_id_for_inline);
                                                        move |e: MouseEvent| {
                                                            e.stop_propagation();
                                                            let current = open_emoji_picker.read().clone();
                                                            if current.as_ref() == Some(&picker_id) {
                                                                open_emoji_picker.set(None);
                                                            } else {
                                                                // Determine if picker should appear above or below based on click position
                                                                // If click is in bottom 40% of viewport, show picker above
                                                                let click_y = e.client_coordinates().y;
                                                                let viewport_height = web_sys::window()
                                                                    .and_then(|w| w.inner_height().ok())
                                                                    .and_then(|h| h.as_f64())
                                                                    .unwrap_or(800.0);
                                                                picker_show_above.set(click_y > viewport_height * 0.6);
                                                                open_emoji_picker.set(Some(picker_id.clone()));
                                                            }
                                                        }
                                                    },
                                                    "+"
                                                }
                                                // Emoji picker for inline button (flips based on viewport position)
                                                if is_inline_picker_open {
                                                    div {
                                                        class: format!(
                                                            "absolute p-1.5 bg-panel rounded-xl shadow-xl border border-border z-50 grid {} {}",
                                                            if *picker_show_above.read() { "bottom-full mb-1" } else { "top-full mt-1" },
                                                            if is_self { "right-0" } else { "left-0" }
                                                        ),
                                                        style: "grid-template-columns: repeat(4, 1fr); gap: 2px;",
                                                        onclick: move |e: MouseEvent| e.stop_propagation(),
                                                        {FREQUENT_EMOJIS.iter().map(|emoji| {
                                                            let emoji_str = emoji.to_string();
                                                            let msg_id = msg_id_react.clone();
                                                            let is_current = user_reaction_for_picker.as_ref() == Some(&emoji_str);
                                                            rsx! {
                                                                button {
                                                                    key: "{emoji}",
                                                                    class: format!(
                                                                        "p-1 rounded hover:bg-surface transition-colors text-xl leading-none {}",
                                                                        if is_current { "bg-accent/20 ring-2 ring-accent" } else { "" }
                                                                    ),
                                                                    title: if is_current {
                                                                        format!("Remove {} reaction", emoji)
                                                                    } else {
                                                                        format!("React with {}", emoji)
                                                                    },
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
                        class: if time_clamped {
                            "text-xs text-text-muted mt-1 px-1 cursor-default italic opacity-70"
                        } else {
                            "text-xs text-text-muted mt-1 px-1 cursor-default"
                        },
                        title: "{full_time_str}",
                        if time_clamped { "~{time_str}" } else { "{time_str}" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_url_is_linkified() {
        let html = message_to_html("check out https://freenet.org for more info");
        assert!(
            html.contains(
                r#"<a target="_blank" rel="noopener noreferrer" href="https://freenet.org">"#
            ),
            "bare URL should be linkified with target=_blank: {html}"
        );
    }

    #[test]
    fn url_in_code_span_not_linkified() {
        let html = message_to_html("did you do `curl -fsSL https://freenet.org/install.sh | sh`?");
        assert!(
            !html.contains("<a "),
            "URL inside backticks should NOT be linkified: {html}"
        );
    }

    #[test]
    fn url_in_fenced_code_block_not_linkified() {
        let html = message_to_html("```\ncurl https://freenet.org/install.sh\n```");
        assert!(
            !html.contains("<a "),
            "URL in code block should NOT be linkified: {html}"
        );
    }

    #[test]
    fn markdown_link_preserved() {
        let html = message_to_html("see [Freenet](https://freenet.org)");
        assert!(
            html.contains(r#"href="https://freenet.org">"#),
            "markdown link should be preserved: {html}"
        );
        assert!(
            html.contains(">Freenet</a>"),
            "markdown link text should be preserved: {html}"
        );
    }

    #[test]
    fn newlines_become_hard_breaks() {
        let html = message_to_html("line one\nline two");
        assert!(
            html.contains("<br"),
            "newlines should become hard breaks: {html}"
        );
    }

    const SAMPLE_ID: &str = "UDzGbcWrKN748tYbhvbPCCCQrZc9r9xkN3tUuun5Rts";

    #[test]
    fn freenet_web_url_label_shortened() {
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&url);
        assert!(
            html.contains(&format!("href=\"{url}\"")),
            "href should be preserved: {html}"
        );
        assert!(
            html.contains(">freenet:UDzGbcWr</a>"),
            "label should be shortened to 8-char prefix: {html}"
        );
        assert!(
            !html.contains(&format!(">{url}</a>")),
            "raw URL should not appear as link text: {html}"
        );
    }

    #[test]
    fn freenet_web_url_with_path_keeps_path() {
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/index.html");
        let html = message_to_html(&url);
        assert!(
            html.contains(">freenet:UDzGbcWr/index.html</a>"),
            "label should include path: {html}"
        );
    }

    #[test]
    fn freenet_web_url_https_handled() {
        let url = format!("https://nova.locut.us/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&url);
        assert!(
            html.contains(">freenet:UDzGbcWr</a>"),
            "https URL should also be beautified: {html}"
        );
    }

    #[test]
    fn freenet_web_url_with_query_kept() {
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/?invite=abc");
        let html = message_to_html(&url);
        assert!(
            html.contains(">freenet:UDzGbcWr/?invite=abc</a>"),
            "query string should be kept: {html}"
        );
    }

    #[test]
    fn custom_markdown_link_text_preserved() {
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&format!("see [my room]({url}) here"));
        assert!(
            html.contains(">my room</a>"),
            "user-supplied link text should be preserved: {html}"
        );
        assert!(
            !html.contains("freenet:UDzGbcWr"),
            "should not rewrite when link text is custom: {html}"
        );
    }

    #[test]
    fn non_freenet_url_unchanged() {
        let html = message_to_html("https://example.com/foo/bar");
        assert!(
            html.contains(">https://example.com/foo/bar</a>"),
            "unrelated URLs should keep their full text: {html}"
        );
    }

    #[test]
    fn freenet_url_in_code_span_unchanged() {
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&format!("run `curl {url}`"));
        assert!(
            !html.contains("<a "),
            "URL inside backticks should not be linkified: {html}"
        );
        assert!(
            !html.contains("freenet:UDzGbcWr"),
            "URL inside backticks should not be beautified: {html}"
        );
    }

    #[test]
    fn non_web_contract_path_left_alone() {
        // /v1/contract/<id>/ (no /web/) is not a real browsable route; leave the
        // link text as-is rather than pretend we beautified it.
        let url = format!("http://127.0.0.1:7509/v1/contract/{SAMPLE_ID}/");
        let html = message_to_html(&url);
        assert!(
            html.contains(&format!(">{url}</a>")),
            "non-web contract URL should keep its full text: {html}"
        );
    }

    #[test]
    fn marker_in_query_string_not_beautified() {
        // External redirect URLs that happen to embed the marker in a query
        // parameter must NOT be presented as Freenet links — that would be a
        // phishing vector (caught by Codex review of #223).
        let url = format!("https://evil.example/redirect?next=/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "marker buried in query must not produce a freenet: label: {html}"
        );
    }

    #[test]
    fn marker_after_userinfo_or_path_segment_not_beautified() {
        // Marker buried deeper in the path must not match either.
        let url = format!("https://evil.example/foo/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "marker as a deeper path segment must not match: {html}"
        );
    }

    #[test]
    fn multiple_freenet_links_in_one_message() {
        let url_a = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        let other = "AAAAAAAAbbbbCCCCddddEEEEffffGGGGhhhhIIIIjjj";
        let url_b = format!("http://127.0.0.1:7509/v1/contract/web/{other}/foo.html");
        let html = message_to_html(&format!("see {url_a} and also {url_b} thanks"));
        assert!(
            html.contains(">freenet:UDzGbcWr</a>"),
            "first link should be shortened: {html}"
        );
        assert!(
            html.contains(">freenet:AAAAAAAA/foo.html</a>"),
            "second link should be shortened: {html}"
        );
        assert!(
            html.matches("<a ").count() == 2,
            "both anchors should be present: {html}"
        );
    }

    #[test]
    fn empty_contract_id_not_beautified() {
        // /v1/contract/web// has no id — bail out and leave the original.
        let url = "http://127.0.0.1:7509/v1/contract/web//".to_string();
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "empty contract id must not produce a label: {html}"
        );
    }

    #[test]
    fn ampersand_in_query_keeps_full_url() {
        // GFM HTML-escapes `&` to `&amp;` in BOTH href and text content, so the
        // h == inner equality still holds and beautification proceeds with the
        // entity-encoded suffix.
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/?a=1&b=2");
        let html = message_to_html(&url);
        assert!(
            html.contains(">freenet:UDzGbcWr/?a=1&amp;b=2</a>"),
            "ampersand-bearing query should be kept (entity-encoded): {html}"
        );
    }
}
