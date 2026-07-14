#[cfg(target_arch = "wasm32")]
use crate::components::app::notifications::request_permission_on_first_message;
use crate::components::app::receive_times::{format_delay, get_delay_secs};
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{
    MobileView, CURRENT_ROOM, EDIT_ROOM_MODAL, MEMBER_INFO_MODAL, MOBILE_VIEW, ROOMS,
};
use crate::room_data::SendMessageError;
use crate::util::ecies::{encrypt_with_symmetric_key, unseal_bytes_with_secrets};
use crate::util::{
    date_separator_labels, format_utc_as_full_datetime, format_utc_as_local_time,
    get_current_system_time, local_message_date, local_today,
};
mod emoji_picker;
mod mention;
mod message_actions;
mod message_input;
mod not_member_notification;
use self::emoji_picker::FREQUENT_EMOJIS;
use self::not_member_notification::NotMemberNotification;
use crate::components::conversation::message_input::MessageInput;
use chrono::{DateTime, Utc};
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{
    FaBars, FaChevronDown, FaCircleInfo, FaEllipsisVertical, FaFaceSmile, FaPenToSquare, FaReply,
    FaTrashCan, FaTriangleExclamation, FaUsers,
};
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

/// The representative timestamp (ms since epoch) for a display item, used to
/// decide which local calendar day it belongs to for the date separators.
///
/// A single item is attributed to one day, so a group whose messages straddle
/// local midnight (same author within the 5-minute group window, or events
/// within the 1-hour merge window) is placed entirely under its first/last
/// message's day — no divider is rendered mid-group. This is an accepted,
/// self-correcting limitation: the next group on the new day still gets its
/// own divider and each message keeps its own `HH:MM`. Splitting groups at the
/// local-day boundary would push timezone logic into the otherwise
/// timezone-independent `group_messages`, so it is intentionally not done here.
fn display_item_time_ms(item: &DisplayItem) -> i64 {
    match item {
        DisplayItem::Messages(group) => group.first_time.timestamp_millis(),
        DisplayItem::Event(summary) => summary.last_time.timestamp_millis(),
    }
}

/// The stable per-item key used on the item's rendered root, reused to derive
/// a unique key for the date separator that precedes it.
fn display_item_key(item: &DisplayItem) -> String {
    match item {
        DisplayItem::Messages(group) => group.messages[0].id.clone(),
        DisplayItem::Event(summary) => summary.id.clone(),
    }
}

/// A rendered conversation row: either a day-change date separator or a
/// message/event display item. Separators are flattened into their own rows
/// (rather than emitted as a second root alongside the item) so every row
/// renders as a SINGLE keyed node — otherwise Dioxus takes the list key from
/// the fragment's first root, which would be the keyless separator expression,
/// silently dropping the whole group list to positional diffing and leaking
/// per-group component state on mid-list edits (freenet/river#326 review).
enum DisplayRow {
    DateSeparator { key: String, label: String },
    Item(DisplayItem),
}

/// Group consecutive messages from the same sender within 5 minutes,
/// and summarize consecutive event messages (e.g. joins).
fn group_messages(
    messages_state: &MessagesV1,
    member_info: &MemberInfoV1,
    self_member_id: MemberId,
    secrets: &HashMap<u32, [u8; 32]>,
    member_names: &HashMap<MemberId, String>,
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
        let content_html =
            message_to_html_with_mentions(&content_text, member_names, self_member_id);
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
                // Keep the full current text; mention/markdown cleaning and
                // truncation happen once, below.
                reply_to_preview = Some(current_text);
            }
        }

        // Clean the reply preview for display: resolve @mention tokens to plain
        // `@name` (current nickname) and strip markdown, so the quoted snapshot
        // reads as plain text rather than showing raw `@[name](rv:id)` / `**` /
        // `[text](url)` syntax. Truncate after cleaning. Applies to both the
        // refreshed-current-text and the stored-snapshot fallback paths.
        let reply_to_preview = reply_to_preview.map(|p| {
            clean_reply_preview(&p, member_names)
                .chars()
                .take(100)
                .collect::<String>()
        });

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

pub(crate) fn decrypt_message_content(
    content: &RoomMessageBody,
    secrets: &HashMap<u32, [u8; 32]>,
) -> String {
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
            } else if secrets.is_empty() {
                // Issue freenet/river#284: when the in-memory `secrets`
                // map is empty for a private room, the diagnostic
                // placeholder ("[Encrypted message - secret vN not
                // available (have: [])]") is alarming and looked like
                // data loss, even though the only fix is "wait a few
                // seconds for sync." Render a calm muted-text message
                // instead. Once any secret arrives the branch above
                // (or the rotation fallback) will decrypt the actual
                // content.
                //
                // Wording note (skeptical review M1 on PR #286): the
                // `secrets` map is `#[serde(skip)]`, so EVERY cold-start
                // of an established private room transiently lands
                // here until `repopulate_secrets_from_state` rehydrates
                // it from the encrypted blobs. So the wording must
                // work for BOTH first-time joiners (who really are
                // waiting on a delegate back-fill) AND established
                // members reloading the tab. "Decrypting messages" is
                // accurate for both; an earlier version of this branch
                // said "Decrypting your invitation" which was wrong for
                // the reload case.
                "Decrypting messages — this should only take a moment...".to_string()
            } else {
                // We have SOME secrets but not the one this message
                // was encrypted under. This is the older-message case
                // (rotated past) rather than the sync-window case.
                // Keep the placeholder neutral and informative without
                // dumping the full version list — the diagnostic detail
                // belongs in a debug-only path, not user-facing copy.
                format!(
                    "[Encrypted message - secret v{} unavailable]",
                    secret_version
                )
            }
        }
    }
}

/// Clean a quoted reply-preview snapshot for display: resolve `@[name](rv:id)`
/// mention tokens to plain `@name` (using each member's *current* nickname, with
/// the token snapshot as fallback) and strip markdown formatting, so the preview
/// reads as plain text. Caller truncates the result.
fn clean_reply_preview(text: &str, member_names: &HashMap<MemberId, String>) -> String {
    let with_mentions = river_core::mention::render_plaintext(text, |r| {
        member_names
            .iter()
            .find(|(id, _)| r.matches(**id))
            .map(|(_, name)| name.clone())
    });
    strip_markdown(&with_mentions)
}

/// Reduce markdown to its plain-text content (emphasis/headings/code-fences
/// removed, links rendered as their visible text). Used for the single-line
/// reply-preview snapshot, never for the message body (which renders full
/// markdown). Falls back to the input unchanged if parsing fails.
fn strip_markdown(text: &str) -> String {
    match markdown::to_mdast(text, &markdown::ParseOptions::gfm()) {
        Ok(node) => {
            let mut out = String::with_capacity(text.len());
            collect_mdast_text(&node, &mut out);
            out
        }
        Err(_) => text.to_string(),
    }
}

/// Depth-first collection of the visible text from a markdown AST node.
fn collect_mdast_text(node: &markdown::mdast::Node, out: &mut String) {
    use markdown::mdast::Node;
    match node {
        Node::Text(t) => out.push_str(&t.value),
        Node::InlineCode(c) => out.push_str(&c.value),
        Node::Code(c) => out.push_str(&c.value),
        // A hard/soft break or thematic break becomes a space so words on
        // separate lines don't run together in the single-line preview.
        Node::Break(_) | Node::ThematicBreak(_) => out.push(' '),
        _ => {}
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_mdast_text(child, out);
        }
    }
}

/// Extract reply context from a message body, if it is a reply.
/// Returns (author_name, content_preview, target_message_id) or (None, None, None).
pub(crate) fn extract_reply_context(
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
/// Re-exported as `pub(crate)` so the DM thread renderer can share the
/// same linkify + Freenet-URL-rewrite path as room messages.
pub(crate) fn message_to_html(text: &str) -> String {
    message_to_html_inner(text, running_behind_freenet_gateway())
}

fn message_to_html_inner(text: &str, behind_gateway: bool) -> String {
    // Convert single newlines to hard breaks (two spaces + newline)
    // This preserves line breaks in chat messages as users expect
    let with_hard_breaks = text.replace("\n", "  \n");

    markdown_to_html(&with_hard_breaks, behind_gateway)
}

/// Convert markdown text to HTML with clickable links that open in new tabs.
fn markdown_to_html(text: &str, behind_gateway: bool) -> String {
    // Convert markdown to HTML using GFM mode, which includes autolink
    // literals that correctly handle code spans, existing links, etc.
    let html = markdown::to_html_with_options(text, &markdown::Options::gfm())
        .unwrap_or_else(|_| markdown::to_html(text));

    finalize_anchors(&html, behind_gateway)
}

/// Render message text to HTML, turning `@[name](rv:id)` mention tokens into
/// styled, clickable chips that show each member's *current* nickname.
///
/// Mentions are extracted *before* markdown runs — each is replaced by an inert
/// private-use sentinel, markdown + anchor finalization run over the sentinel'd
/// text, then the sentinels are swapped for chip HTML. This keeps mention
/// rendering independent of markdown's link grammar (so a token can never be
/// mangled by adjacent markdown, and a malicious `[..](javascript:..)` payload
/// can't masquerade as a mention).
///
/// `member_names` maps each member id to their decrypted current nickname (the
/// `[name]` snapshot in the token is only used as a fallback when the id is not
/// in the map). `self_member_id` gets a distinct highlight (a mention of you).
pub(crate) fn message_to_html_with_mentions(
    text: &str,
    member_names: &HashMap<MemberId, String>,
    self_member_id: MemberId,
) -> String {
    use river_core::mention::{parse_segments, MentionSegment};

    let segments = parse_segments(text);
    // Fast path: no mentions -> byte-identical to the plain renderer.
    if !segments
        .iter()
        .any(|s| matches!(s, MentionSegment::Mention(_)))
    {
        return message_to_html(text);
    }

    // Private-use sentinels that markdown passes through verbatim and that
    // never legitimately appear in chat text. Strip any pre-existing
    // occurrences from plain-text runs so a crafted message can't smuggle a
    // sentinel and hijack the post-markdown substitution.
    const OPEN: char = '\u{E000}';
    const CLOSE: char = '\u{E001}';

    let mut working = String::with_capacity(text.len());
    let mut chips: Vec<String> = Vec::new();
    for seg in segments {
        match seg {
            MentionSegment::Text(t) => {
                working.extend(t.chars().filter(|c| *c != OPEN && *c != CLOSE));
            }
            MentionSegment::Mention(m) => {
                let idx = chips.len();
                // The token's reference is the member's short (truncated-base32)
                // label; recover the full id by matching it against the room's
                // known members so the chip stays clickable and self-highlighted.
                // An unknown member yields `None` -> non-clickable snapshot chip.
                let resolved = m.member_ref.resolve(member_names.keys().copied());
                let name = resolved
                    .and_then(|id| member_names.get(&id).cloned())
                    .unwrap_or(m.display_name);
                chips.push(render_mention_chip_html(
                    resolved,
                    &name,
                    resolved == Some(self_member_id),
                ));
                working.push(OPEN);
                working.push_str(&idx.to_string());
                working.push(CLOSE);
            }
        }
    }

    let mut html = message_to_html(&working);
    // The CLOSE delimiter bounds each index, so `…0␁` never matches inside
    // `…10␁` — replacement is unambiguous regardless of order.
    for (idx, chip) in chips.iter().enumerate() {
        html = html.replace(&format!("{OPEN}{idx}{CLOSE}"), chip);
    }
    html
}

/// Build the inline chip markup for one mention. `name` is the resolved current
/// nickname (or snapshot fallback) and is HTML-escaped — nicknames are
/// attacker-controlled and this string goes through `dangerous_inner_html`
/// (freenet/river#227).
///
/// `id` is the resolved member id, or `None` when the token's short reference
/// names a member this client doesn't know. When present, `data-member-id`
/// carries the lossless hex id (an in-session, full-precision handoff — NOT the
/// wire token) so the document-level click interceptor can open the member-info
/// modal. When absent the chip still renders the `@name` but is inert (nothing
/// to open), which is the correct degradation for an unknown member.
fn render_mention_chip_html(id: Option<MemberId>, name: &str, is_self: bool) -> String {
    let class = if is_self {
        "river-mention river-mention-self"
    } else {
        "river-mention"
    };
    let data_member_id = match id {
        Some(id) => format!(
            " data-member-id=\"{}\"",
            river_core::mention::member_id_to_hex(id)
        ),
        None => String::new(),
    };
    format!(
        "<span class=\"{class}\" data-river-mention=\"1\"{data_member_id} \
         role=\"button\" tabindex=\"0\" title=\"@{title}\">@{label}</span>",
        title = escape_html_attr(name),
        label = escape_html(name),
    )
}

/// Escape `&<>` for HTML text content.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape `&<>"'` for an HTML attribute value (double-quoted).
fn escape_html_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// True when River is currently being served from a path under
/// `/v1/contract/web/`, which is what gateway hosting looks like to the
/// browser. Returning false here suppresses the host-stripping href rewrite
/// for `dx serve`, `cargo make dev-example`, and the static-server flows
/// documented in AGENTS.md, where rewriting `https://gw.example/v1/...` to
/// `/v1/...` would only break the link (the dev server has no gateway
/// behind it). Label beautification is unconditional — that's purely
/// cosmetic and doesn't depend on the hosting environment.
#[cfg(target_arch = "wasm32")]
fn running_behind_freenet_gateway() -> bool {
    web_sys::window()
        .and_then(|w| w.location().pathname().ok())
        .map(|p| p.starts_with("/v1/contract/web/"))
        .unwrap_or(false)
}

/// Native test builds: default to true so existing tests verify the
/// production (gateway-hosted) behavior. Tests covering the dev-mode path
/// call `message_to_html_inner` with an explicit `false` flag.
#[cfg(not(target_arch = "wasm32"))]
fn running_behind_freenet_gateway() -> bool {
    true
}

/// Walk anchor tags in HTML once and:
///
/// - Add `target="_blank" rel="noopener noreferrer"` to every anchor.
/// - When `rewrite_freenet_hrefs` is true, rewrite Freenet web-contract URLs
///   to a host/port-agnostic same-origin absolute path so the link works for
///   any reader regardless of which gateway they're connected to. The flag
///   is only true when River itself is hosted under `/v1/contract/web/...`
///   (i.e. behind a gateway). In `dx serve` / dev-example / static-server
///   modes there is no gateway to redirect to, so the original absolute URL
///   is left in place — letting the user reach the embedded gateway directly.
/// - For bare Freenet web URLs (where the visible text equals the original
///   href), shorten the label to `freenet:<id-prefix>[/<path>]` regardless of
///   hosting. User-supplied link text from `[label](url)` is left alone.
///
/// Assumes the markdown crate emits anchors as `<a href="...">...</a>` with
/// `href` as the first attribute. If that ever changes, target/rel injection
/// silently no-ops and href rewrite + label beautification are skipped.
fn finalize_anchors(html: &str, rewrite_freenet_hrefs: bool) -> String {
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
        let original_href = extract_href(&opening);
        let opening = if rewrite_freenet_hrefs {
            match original_href.as_deref().and_then(rewrite_freenet_href) {
                Some(new_href) => {
                    let orig = original_href.as_deref().unwrap();
                    opening.replacen(
                        &format!("href=\"{orig}\""),
                        &format!("href=\"{new_href}\""),
                        1,
                    )
                }
                None => opening,
            }
        } else {
            opening
        };
        let new_inner = match original_href.as_deref() {
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

/// Parsed shape of a Freenet web-contract URL.
struct FreenetWebUrl<'a> {
    /// The contract ID — base58-encoded 32-byte BLAKE3 hash (43 or 44 chars).
    contract_id: &'a str,
    /// Anything after the contract ID: leading slash + path, query, fragment.
    /// `""` for `/v1/contract/web/<id>` with nothing after.
    suffix: &'a str,
    /// Same-origin absolute path including the marker: `/v1/contract/web/<id><suffix>`.
    /// Used as a host/port-agnostic href.
    absolute_path: &'a str,
}

/// Parse a Freenet web-contract URL, validating the contract ID looks like a
/// real base58-encoded 32-byte BLAKE3 hash. The hash shape is the reliable
/// indicator: it rejects same-prefix paths whose ID segment is too short or
/// uses characters outside the base58 alphabet (e.g. visual-confusion chars
/// `0OIl`, which a real contract ID can never contain).
///
/// The URL must use `http` or `https` (defense in depth — `[label](url)`
/// markdown can in theory carry other schemes; we don't want to rewrite a
/// `javascript:`-flavored input even though the rewrite would defang it).
///
/// The suffix must not contain `..` path segments. Without this guard, a
/// pasted `http://attacker/v1/contract/web/<valid-shape-id>/../../foo`
/// would be rewritten to a same-origin path that the browser normalizes
/// into `/foo` on the reader's local gateway — sending the click to a
/// path the attacker chose on the *victim's* gateway, instead of to the
/// attacker's host where it would have gone before the rewrite.
fn parse_freenet_web_url(url: &str) -> Option<FreenetWebUrl<'_>> {
    let scheme_end = url.find("://")?;
    let scheme = &url[..scheme_end];
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let after_scheme = &url[scheme_end + 3..];
    let path_offset = after_scheme.find('/')?;
    let path = &after_scheme[path_offset..];
    let after_marker = path.strip_prefix("/v1/contract/web/")?;

    let id_end = after_marker
        .find(|c: char| !is_base58_char(c))
        .unwrap_or(after_marker.len());
    if !matches!(id_end, 43 | 44) {
        return None;
    }
    let suffix = &after_marker[id_end..];
    if suffix_has_dotdot_segment(suffix) {
        return None;
    }
    Some(FreenetWebUrl {
        contract_id: &after_marker[..id_end],
        suffix,
        absolute_path: path,
    })
}

/// True if any path segment in `suffix` is exactly `..`. Path segments are
/// the `/`-separated components before any `?` query or `#` fragment.
fn suffix_has_dotdot_segment(suffix: &str) -> bool {
    let path_only = suffix
        .split_once(['?', '#'])
        .map(|(p, _)| p)
        .unwrap_or(suffix);
    path_only.split('/').any(|seg| seg == "..")
}

/// Bitcoin-style base58 alphabet: digits and letters minus the visually
/// ambiguous `0`, `O`, `I`, `l`. A base58 string never contains these four.
fn is_base58_char(c: char) -> bool {
    matches!(c,
        '1'..='9'
        | 'A'..='H' | 'J'..='N' | 'P'..='Z'
        | 'a'..='k' | 'm'..='z'
    )
}

/// Rewrite a Freenet web-contract URL's href to a same-origin absolute path,
/// stripping the scheme + host + port. Returns None for non-Freenet URLs.
///
/// `http://127.0.0.1:7509/v1/contract/web/<id>/foo` → `/v1/contract/web/<id>/foo`
/// `https://gw.example/v1/contract/web/<id>/#hash`  → `/v1/contract/web/<id>/#hash`
///
/// The browser resolves the absolute path against the current page's origin,
/// so the rewritten link points at whichever gateway River is loaded from —
/// fixing pasted links that hard-code the sender's local gateway address.
fn rewrite_freenet_href(url: &str) -> Option<String> {
    Some(parse_freenet_web_url(url)?.absolute_path.to_string())
}

/// If `url` is a Freenet web-contract URL, return a beautified label like
/// `freenet:UDzGbcWr` or `freenet:UDzGbcWr/index.html`. Returns None for any
/// other URL so the caller falls back to the original link text.
fn beautify_freenet_label(url: &str) -> Option<String> {
    let parsed = parse_freenet_web_url(url)?;
    // Defense in depth: refuse to beautify if the suffix carries raw HTML
    // metacharacters. The markdown crate URL-encodes these today, but the
    // label is rendered via dangerous_inner_html with no further escaping,
    // so we'd rather skip the rewrite than risk smuggling markup.
    if parsed.suffix.contains(['<', '>', '"']) {
        return None;
    }
    // A bare trailing slash adds no information; drop it.
    let suffix = if parsed.suffix == "/" {
        ""
    } else {
        parsed.suffix
    };
    let id_prefix = &parsed.contract_id[..8];
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
    // Which message's touch action menu (kebab) is open, by message ID string.
    // Owned by Conversation (not per message group) so only ONE menu is open at
    // a time across the whole history — opening one closes any other (#402).
    let open_action_menu: Signal<Option<String>> = use_signal(|| None);
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
                    return Some(markdown_to_html(&text, running_behind_freenet_gateway()));
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
                    // Build member name lookup (reaction tooltips, @mention chips).
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
                    let groups = group_messages(
                        &room_state.recent_messages,
                        &room_state.member_info,
                        self_member_id,
                        &room_data.secrets,
                        &member_names,
                    );
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
                // Defer the signal write: this raw JS callback runs with no
                // Dioxus runtime/scope on the stack, and `is_at_bottom` is now
                // subscribed in render (the scroll-to-latest button), so a
                // direct `.set()` would fire a subscriber notification from an
                // empty scope and panic on Firefox mobile. See
                // .claude/rules/dioxus-signal-safety.md. (#402)
                let intersecting = entry.is_intersecting();
                crate::util::defer(move || is_at_bottom.set(intersecting));
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

    // Trigger scroll to bottom when recent messages change (only if user is near bottom).
    // Scroll the chat-scroll-container itself, not the last bubble: scrollIntoView aligns
    // the bubble's top to the container's top, which can leave the actual bottom (reactions,
    // sentinel, padding) off-screen. On page refresh this surfaced as scrolling only ~70%
    // of the way down.
    //
    // First scroll uses Instant so a page refresh snaps to the bottom rather than animating
    // a ~500ms scroll from top. Subsequent scrolls (new messages while at bottom) use Smooth.
    let first_scroll = use_hook(|| Rc::new(std::cell::Cell::new(true)));
    // Set by the room-change effect below to force the next mount-triggered
    // scroll regardless of `is_at_bottom` (#402). This decouples the room-switch
    // snap from `is_at_bottom`, so the IntersectionObserver flipping the signal
    // to `false` (new room's persisted scroll position) between the room-change
    // effect and this one can't suppress the snap. A plain `Cell`, not a signal,
    // so reading it here does not subscribe.
    let force_scroll = use_hook(|| Rc::new(std::cell::Cell::new(false)));
    use_effect({
        let first_scroll = first_scroll.clone();
        let force_scroll = force_scroll.clone();
        move || {
            // Re-run when the last bubble mounts (new messages or initial load).
            let trigger = last_chat_element();
            let forced = force_scroll.get();
            let should_scroll = forced || *is_at_bottom.peek();
            if should_scroll && trigger.is_some() {
                force_scroll.set(false);
                let is_first = first_scroll.replace(false);
                // `behavior` is only used inside the wasm32 block below; on
                // native it would warn as unused. Gate the binding too so
                // clippy stays clean across both targets.
                #[cfg(target_arch = "wasm32")]
                let behavior = if is_first {
                    web_sys::ScrollBehavior::Instant
                } else {
                    web_sys::ScrollBehavior::Smooth
                };
                #[cfg(not(target_arch = "wasm32"))]
                let _ = is_first;
                #[cfg(target_arch = "wasm32")]
                crate::util::safe_spawn_local(async move {
                    let Some(window) = web_sys::window() else {
                        return;
                    };
                    let Some(document) = window.document() else {
                        return;
                    };
                    let Some(container) = document.get_element_by_id("chat-scroll-container")
                    else {
                        warn!("chat-scroll-container missing; skipping scroll-to-bottom");
                        return;
                    };
                    let opts = web_sys::ScrollToOptions::new();
                    opts.set_top(container.scroll_height() as f64);
                    opts.set_behavior(behavior);
                    container.scroll_to_with_scroll_to_options(&opts);
                });
            }
        }
    });

    // Snap to the newest message whenever the selected room changes (#402). The
    // Conversation component is mounted once and reused across rooms (hidden or
    // shown via CSS), so `#chat-scroll-container` and `is_at_bottom` otherwise
    // persist from the previous room, frequently leaving the user far up the
    // new room's history. Reading `CURRENT_ROOM` (which holds only `owner_key`)
    // makes this effect re-run on every room change and nothing else.
    //
    // This effect only raises `force_scroll`; the actual scroll runs in the
    // mount-triggered effect above once the new room's last bubble mounts (its
    // trigger, `last_chat_element`, necessarily changes AFTER this room change,
    // so the ordering is causal — not dependent on effect scheduling). Using a
    // persistent `force_scroll` flag instead of `is_at_bottom` means the
    // observer can't cancel the snap in the gap between the two effects.
    // `first_scroll` = true makes that snap instant rather than animated from an
    // arbitrary position. `is_at_bottom` = true hides the scroll-to-latest
    // button immediately on switch (the observer reconfirms after the snap).
    //
    // Guarded on an ACTUAL key change: Dioxus re-runs the effect on any write
    // to `CURRENT_ROOM`, and re-selecting the already-open room in the sidebar
    // rewrites it with the same key. Without the guard that would arm
    // `force_scroll` with no new bubble to consume it, so a later message would
    // snap the reader to the bottom (#402 review).
    {
        let first_scroll = first_scroll.clone();
        let force_scroll = force_scroll.clone();
        let prev_room =
            use_hook(|| Rc::new(std::cell::Cell::new(None::<ed25519_dalek::VerifyingKey>)));
        use_effect(move || {
            let room = CURRENT_ROOM.read().owner_key;
            if prev_room.get() != room {
                prev_room.set(room);
                force_scroll.set(true);
                first_scroll.set(true);
                is_at_bottom.set(true);
            }
        });
    }

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
                                        // See #310 — keep private actions_state intact
                                        // across the optimistic apply_delta.
                                        room_data.rebuild_private_actions_state();
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
                                    // See #310 — keep private actions_state intact
                                    // across the optimistic apply_delta.
                                    room_data.rebuild_private_actions_state();
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
                                    // See #310 — re-derive private actions_state so the
                                    // just-made edit shows immediately instead of waiting
                                    // for the network echo's decrypt-aware rebuild.
                                    room_data.rebuild_private_actions_state();
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
        let force_scroll = force_scroll.clone();
        move |(message_text, reply_ctx): (String, Option<ReplyContext>)| {
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

                // Cloned into the async send so `force_scroll` (consumed by the
                // mount effect when the sent message appears) is raised ONLY
                // after the delta applies locally — a rejected send (empty,
                // over-size, serialize/sign/delta failure) then leaves the
                // scroll position and the scroll-to-latest button untouched
                // rather than snapping a later unrelated message to the bottom
                // (#402 review).
                let force_scroll = force_scroll.clone();
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
                                    // For private rooms, re-derive actions_state with
                                    // decrypted payloads. apply_delta's built-in rebuild
                                    // only handles public actions, so without this the
                                    // optimistic update wipes private edits/deletes/reactions
                                    // until the network echo re-applies them (#310).
                                    room_data.rebuild_private_actions_state();
                                    true
                                }
                            } else {
                                crate::util::debug_log("[send] room not found in ROOMS!");
                                false
                            }
                        });
                        if delta_applied {
                            // Local apply succeeded and a message will mount:
                            // scroll it into view — but only if the user is still
                            // viewing the room this send targeted. Signing is
                            // async, so they may have switched rooms; arming the
                            // conversation-wide flag then would snap the NEW room
                            // to the bottom on its next message (#402 review).
                            if CURRENT_ROOM.peek().owner_key == Some(current_room) {
                                force_scroll.set(true);
                            }
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
                            div { class: "flex items-center justify-between gap-2 md:gap-3 max-w-4xl mx-auto",
                                // Mobile: hamburger to open rooms panel. `mr-1` plus the row
                                // `gap-2` keep this switch-rooms button clear of the room-name
                                // tap target so a touch user does not open the room-details
                                // modal by mistake when reaching for the room list (#402).
                                button {
                                    class: "md:hidden flex-shrink-0 mr-1 p-2 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
                                    onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Rooms),
                                    Icon { icon: FaBars, width: 18, height: 18 }
                                }
                                // Description is a sibling of the title button, not a child:
                                // `<a>` is interactive content and cannot be nested inside
                                // `<button>` per the HTML spec. Nesting also bubbles link
                                // clicks to the modal-opening onclick handler.
                                div { class: "min-w-0 flex-1",
                                    button {
                                        // `md:-mx-3` only pulls the hover target outward on
                                        // desktop, where there is no adjacent hamburger. On
                                        // mobile the negative margin is dropped so this
                                        // room-details target stays clear of the hamburger (#402).
                                        class: "flex items-center gap-2 px-3 py-1.5 md:-mx-3 rounded-lg bg-transparent hover:bg-surface transition-colors cursor-pointer min-w-0 w-full",
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
                class: "flex-1 min-h-0 relative",
                div {
                    // `overflow-x-hidden` is a backstop: a kebab action menu on
                    // a very short self message can extend a few px past the
                    // viewport edge; clip it (trailing whitespace only — the
                    // menu content is left-aligned and stays visible) rather
                    // than show a horizontal scrollbar in the history. #402.
                    class: "h-full overflow-y-auto overflow-x-hidden",
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
                                    // Flatten the message/event groups into render rows,
                                    // inserting a day-change separator row above the first
                                    // item of each local calendar day (a run of messages on
                                    // the same day shows a single "Today" / "Monday, June 3"
                                    // divider). Each row renders as a single keyed root so the
                                    // list keeps diffing by key — see DisplayRow. The separator
                                    // labels are computed at render time so relative
                                    // "Today"/"Yesterday" labels stay fresh across re-renders.
                                    let rows: Vec<DisplayRow> = {
                                        let item_dates: Vec<chrono::NaiveDate> = groups
                                            .iter()
                                            .map(|item| {
                                                local_message_date(display_item_time_ms(item))
                                            })
                                            .collect();
                                        let labels =
                                            date_separator_labels(&item_dates, local_today());
                                        let mut rows = Vec::with_capacity(groups.len());
                                        for (item, label) in groups.into_iter().zip(labels) {
                                            if let Some(label) = label {
                                                rows.push(DisplayRow::DateSeparator {
                                                    key: format!("date-sep-{}", display_item_key(&item)),
                                                    label,
                                                });
                                            }
                                            rows.push(DisplayRow::Item(item));
                                        }
                                        rows
                                    };
                                    let rows_len = rows.len();
                                    Some(rsx! {
                                        div { class: "space-y-4",
                                            {rows.into_iter().enumerate().map({
                                                let handle_toggle_reaction = handle_toggle_reaction.clone();
                                                let member_names = member_names.clone();
                                                move |(row_idx, row)| {
                                                let is_last_group = row_idx == rows_len - 1;
                                                let handle_toggle_reaction = handle_toggle_reaction.clone();
                                                let handle_edit_message = handle_edit_message.clone();
                                                let member_names = member_names.clone();
                                                match row {
                                                    DisplayRow::DateSeparator { key, label } => rsx! {
                                                        div {
                                                            key: "{key}",
                                                            class: "flex justify-center py-2",
                                                            span {
                                                                class: "text-xs font-medium text-text-muted bg-surface px-3 py-1 rounded-full",
                                                                "{label}"
                                                            }
                                                        }
                                                    },
                                                    DisplayRow::Item(DisplayItem::Event(summary)) => {
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
                                                    DisplayRow::Item(DisplayItem::Messages(group)) => {
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
                                                                open_action_menu: open_action_menu,
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
                // Scroll-to-latest button (#402): shown whenever the user is not
                // pinned to the bottom of the history. Reuses the `is_at_bottom`
                // IntersectionObserver state, so it appears after scrolling up
                // (e.g. reading back through a long, multi-room history) and
                // hides once the newest message is in view. Handy on every
                // device but especially on touch, where there is no scrollbar
                // to drag.
                if !is_at_bottom() {
                    button {
                        class: "absolute bottom-4 right-4 z-30 flex items-center justify-center w-10 h-10 rounded-full bg-panel shadow-lg border border-border text-text-muted hover:text-accent transition-colors",
                        "aria-label": "Scroll to latest messages",
                        "data-testid": "scroll-to-bottom",
                        // Do NOT optimistically set `is_at_bottom` here: the
                        // IntersectionObserver flips it (hiding the button) once
                        // the sentinel actually reaches view. Setting it eagerly
                        // would leave the button hidden if the user interrupts
                        // the smooth scroll before reaching the bottom (the
                        // observer emits no new change and stays quiet). #402.
                        onclick: move |_| {
                            #[cfg(target_arch = "wasm32")]
                            crate::util::safe_spawn_local(async move {
                                let Some(container) = web_sys::window()
                                    .and_then(|w| w.document())
                                    .and_then(|d| d.get_element_by_id("chat-scroll-container"))
                                else {
                                    return;
                                };
                                let opts = web_sys::ScrollToOptions::new();
                                opts.set_top(container.scroll_height() as f64);
                                opts.set_behavior(web_sys::ScrollBehavior::Smooth);
                                container.scroll_to_with_scroll_to_options(&opts);
                            });
                        },
                        Icon { icon: FaChevronDown, width: 18, height: 18 }
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

                // A room still awaiting its initial sync can reach a terminal
                // `RoomSyncStatus::Error` — most importantly the bounded
                // contract-absent case (freenet/river#290), but also a failed
                // GET/PUT send (WebSocket/API error). The spinner below is gated
                // on `is_awaiting_initial_sync()`, which stays true while the
                // room holds placeholder state — so without this check it would
                // spin forever. Surface the STORED error message (not a
                // hardcoded "not found") so WebSocket/API failures are not
                // misreported as the room being removed.
                //
                // Scoped to rooms that are STILL awaiting initial sync: a room
                // that already synced real state and later hit some other
                // `Error` (e.g. a transient PUT failure) is handled by the
                // normal `Some(room_data)` arm below.
                let initial_sync_error_msg: Option<String> =
                    current_room_data.as_ref().and_then(|room_data| {
                        if !room_data.is_awaiting_initial_sync() {
                            return None;
                        }
                        match SYNC_INFO
                            .try_read()
                            .ok()
                            .and_then(|si| si.get_sync_status(&room_data.owner_vk).cloned())
                        {
                            Some(RoomSyncStatus::Error(msg)) => Some(msg),
                            _ => None,
                        }
                    });

                match current_room_data.as_ref() {
                    Some(_) if initial_sync_error_msg.is_some() => {
                        let msg = initial_sync_error_msg.unwrap_or_default();
                        rsx! {
                            div { class: "px-4 py-3 mx-4 mb-4 bg-error-bg rounded-lg text-sm text-red-700 dark:text-red-400 flex items-center gap-3",
                                Icon { width: 16, height: 16, icon: FaTriangleExclamation }
                                span { "{msg}" }
                            }
                        }
                    },
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
                                // Mentionable members for the @ autocomplete: every member
                                // with a (decrypted) nickname except self, sorted by name.
                                let self_id = MemberId::from(&room_data.self_sk.verifying_key());
                                let mut mention_members: Vec<(MemberId, String)> = room_data
                                    .room_state
                                    .member_info
                                    .member_info
                                    .iter()
                                    .filter(|ami| ami.member_info.member_id != self_id)
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
                                    .filter(|(_, name)| !name.trim().is_empty())
                                    .collect();
                                mention_members
                                    .sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
                                rsx! {
                                    MessageInput {
                                        handle_send_message: move |msg: (String, Option<ReplyContext>)| {
                                            let mut handle = handle_send_message.clone();
                                            handle(msg)
                                        },
                                        replying_to: replying_to,
                                        on_request_edit_last: request_edit_last,
                                        max_message_size: max_msg_size,
                                        members: mention_members,
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
                            // Issue #159: new users (especially on mobile) need a
                            // concrete next step. Link to the Freenet quickstart
                            // invite form so they can get into the "Freenet
                            // Official" room.
                            p { class: "text-text-muted mt-3",
                                a {
                                    class: "text-accent hover:underline",
                                    href: "https://freenet.org/quickstart#invite-form",
                                    target: "_blank",
                                    rel: "noopener noreferrer",
                                    "Click here to get an invitation to channel \"Freenet Official\""
                                }
                            }
                            // Bug #5 (Ivvor, Matrix 2026-05-17): on mobile,
                            // the default MOBILE_VIEW is Chat, so a brand-new
                            // user with no rooms lands here without ever
                            // seeing the left-rail indicator. Render the
                            // pill inline so they get the same WebSocket
                            // signal regardless of viewport.
                            div { class: "mt-8 md:hidden",
                                crate::components::members::ConnectionStatusIndicator {}
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
    // Shared across all groups so only one action menu is open at a time (#402).
    open_action_menu: Signal<Option<String>>,
) -> Element {
    let mut open_action_menu = open_action_menu;
    // Per-group: at most one picker per group, and while a picker is open its
    // raised (z-[60]) backdrop covers every other group's kebabs and "+"
    // buttons, so tapping one dismisses the picker rather than stacking a
    // second popover — the single-popover guarantee comes from the z-order, not
    // a shared signal (#402).
    let mut open_emoji_picker: Signal<Option<String>> = use_signal(|| None);
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

    // Whether the kebab action menu should open above (true) or below (false)
    // the kebab. Set from the tap position so the menu for a message near the
    // bottom of the viewport flips upward instead of being clipped by the
    // composer — mirrors `picker_show_above` for the emoji picker (#402).
    let mut menu_show_above: Signal<bool> = use_signal(|| false);

    // Whether the kebab action menu should be left-anchored (open rightward) or
    // right-anchored (open leftward). Chosen from the tap's horizontal position
    // so the menu always opens toward the viewport centre regardless of
    // self/other side or bubble width, and never clips its content off a narrow
    // screen edge (#402 review).
    let mut menu_align_left: Signal<bool> = use_signal(|| false);

    // Max height (px) for the kebab action menu, measured at tap time as the
    // actual space available on the chosen side within the chat scrollport. The
    // menu is `overflow-y-auto`, so on a very short/landscape viewport where it
    // fits neither side fully it scrolls internally instead of being clipped by
    // the scroll container with Edit/Delete unreachable (#402 review).
    let mut menu_max_h: Signal<f64> = use_signal(|| 0.0);

    // Track if emoji picker should appear above (true) or below (false) the button
    let mut picker_show_above: Signal<bool> = use_signal(|| false);

    // Track which message is being edited and its current text
    let mut editing_message: Signal<Option<String>> = use_signal(|| None);
    let mut edit_text: Signal<String> = use_signal(String::new);
    // @mention autocomplete state for the inline edit form (mirrors the
    // composer in message_input.rs). One signal suffices: at most one message
    // in this group is edited at a time.
    let mut edit_mention = use_signal(|| None as Option<mention::MentionAutocomplete>);

    // Mentionable members for the edit form's @ autocomplete: every member with
    // a (decrypted) nickname except self, sorted by name — the same shape the
    // composer receives, derived from `member_names` so no extra prop plumbing.
    let edit_mention_members: Vec<(MemberId, String)> = {
        let mut v: Vec<(MemberId, String)> = member_names
            .iter()
            .filter(|(id, _)| **id != self_member_id)
            .filter(|(_, name)| !name.trim().is_empty())
            .map(|(id, name)| (*id, name.clone()))
            .collect();
        v.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        v
    };

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
                            // `min-w-0 max-w-full` clamps this per-message wrapper to
                            // its column (`max-w-[75%]`) width. For a SELF message the
                            // enclosing bubbles wrapper is `flex flex-col items-end`, so
                            // without this the wrapper is a non-stretched flex item that
                            // sizes to the bubble's `max-w-prose` (65ch) content width and
                            // escapes the column. A self reply whose nowrap reply-strip
                            // preview holds a long URL then overflows both edges of a
                            // narrow mobile viewport (clipped, text cut off). `min-w-0`
                            // lets the flex item shrink below its content's min-size so
                            // `max-w-full` can actually take effect.
                            div {
                                key: "{msg.id}",
                                id: "msg-{msg.id}",
                                class: "flex flex-col group min-w-0 max-w-full",
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
                                            // Unique DOM id so the @mention caret math targets THIS
                                            // edit textarea (multiple groups can theoretically edit).
                                            let edit_id = format!("edit-msg-{}", msg.id);
                                            let pick_id = edit_id.clone();
                                            let kd_id = edit_id.clone();
                                            let input_id = edit_id.clone();
                                            let input_members = edit_mention_members.clone();
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
                                                    // Global key bindings on the container (#94): Esc cancels,
                                                    // Enter saves. Kept here (not solely on the textarea) so the
                                                    // "(Esc)"/"(Enter)" button shortcuts work when keyboard focus
                                                    // is on a button. The textarea's @mention handler calls
                                                    // stop_propagation when it consumes a key, so these never
                                                    // double-fire with mention navigation.
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
                                                    // `relative` anchors the @mention autocomplete
                                                    // dropdown to the textarea.
                                                    div { class: "relative",
                                                        // @mention autocomplete dropdown (floats above the textarea)
                                                        mention::MentionDropdown {
                                                            mention: edit_mention,
                                                            on_pick: move |i| mention::apply_mention_selection(
                                                                pick_id.clone(),
                                                                edit_text,
                                                                edit_mention,
                                                                i,
                                                                || {},
                                                            ),
                                                        }
                                                        textarea {
                                                            id: "{edit_id}",
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
                                                            oninput: move |e| {
                                                                let value = e.value().to_string();
                                                                edit_text.set(value.clone());
                                                                // Detect / update the @mention autocomplete.
                                                                mention::update_mention_from_input(
                                                                    &input_id, &value, &input_members, edit_mention,
                                                                );
                                                            },
                                                            // @mention navigation (Arrow/Enter/Tab/Esc) takes
                                                            // precedence while the dropdown is open. When it
                                                            // consumes the key, stop_propagation keeps the
                                                            // container's Esc-cancel / Enter-save (above) from
                                                            // also firing for that same key. Non-mention keys
                                                            // bubble up to the container handler unchanged.
                                                            onkeydown: move |e: KeyboardEvent| {
                                                                if mention::handle_mention_keydown(
                                                                    &kd_id, &e, edit_text, edit_mention, || {},
                                                                ) {
                                                                    e.stop_propagation();
                                                                }
                                                            },
                                                            // Dismiss the dropdown when focus leaves the textarea
                                                            // (click elsewhere). Dropdown rows use mousedown +
                                                            // preventDefault, so picking one does not blur first.
                                                            onfocusout: move |_| {
                                                                crate::util::defer(move || edit_mention.set(None));
                                                            },
                                                        }
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
                                        // Clean the snapshot (mentions -> @name, markdown stripped)
                                        // BEFORE truncating, so the stored preview is plain text and
                                        // no consumer (UI, CLI, old client) ever sees a raw token —
                                        // even one that would have crossed the truncation boundary.
                                        let reply_text_preview = clean_reply_preview(&msg.content_text, &member_names)
                                            .chars()
                                            .take(100)
                                            .collect::<String>();
                                        let reply_author_name = group.author_name.clone();
                                        rsx! {
                                            div {
                                                // `.hover-actions` (main.css) makes this invisible
                                                // (opacity-0) bar `pointer-events:none` ONLY on touch
                                                // devices (@media hover:none), so it can't intercept a
                                                // gutter tap there — while leaving it fully hit-testable on
                                                // desktop, where the pointer must cross an empty gap to
                                                // reach it (a Tailwind `group-hover:pointer-events` gate
                                                // would drop hover mid-gap and make it unreachable). #402.
                                                class: format!(
                                                    "hover-actions absolute top-1/2 -translate-y-1/2 transition-opacity z-50 flex flex-col items-start bg-panel rounded-lg shadow-md border border-border px-2 py-1.5 opacity-0 group-hover:opacity-100 {} {}",
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
                                    // Touch-only kebab action menu (#402). The hover
                                    // action bar above is wrapped by Tailwind in
                                    // `@media (hover:hover)`, so it can never appear on a
                                    // touch device. `.touch-actions` (main.css) reveals
                                    // this kebab only where there is no hover pointer;
                                    // tapping it opens a menu with the same Reply / React /
                                    // Edit / Delete actions.
                                    {
                                        let msg_id_kebab = msg.id.clone();
                                        let msg_id_kebab_toggle = msg.id.clone();
                                        let msg_id_menu_reply = msg.message_id.clone();
                                        let msg_id_menu_delete = msg.message_id.clone();
                                        let msg_id_menu_edit = msg.id.clone();
                                        let msg_id_menu_react = msg.id.clone();
                                        let edit_text_kebab = msg.content_text.clone();
                                        let reply_author_kebab = group.author_name.clone();
                                        let reply_preview_kebab = clean_reply_preview(&msg.content_text, &member_names)
                                            .chars()
                                            .take(100)
                                            .collect::<String>();
                                        let menu_open = open_action_menu.read().as_deref()
                                            == Some(msg_id_kebab.as_str());
                                        rsx! {
                                            div {
                                                // Positioned in the gutter beside the bubble with
                                                // `right-full`/`left-full` (NOT `translate`): a transform
                                                // would become the containing block for the `fixed`
                                                // dismiss backdrop below, shrinking it to this element
                                                // instead of the viewport (#402 review).
                                                // Raise the OPEN wrapper above sibling kebabs: every
                                                // `.touch-actions` is z-50, and later ones paint above an
                                                // open popover, so without this a nearby message's kebab
                                                // could sit over the menu rows and steal the tap. `z-[60]`
                                                // lifts the whole open popover+backdrop above them (and its
                                                // backdrop then covers those kebabs, so a tap on one just
                                                // dismisses). (#402 review)
                                                class: format!(
                                                    "touch-actions absolute top-1 {} {}",
                                                    if menu_open { "z-[60]" } else { "z-50" },
                                                    if is_self { "right-full mr-1" } else { "left-full ml-1" }
                                                ),
                                                // Kebab toggle button
                                                button {
                                                    class: "flex items-center justify-center w-8 h-8 rounded-full bg-panel shadow-md border border-border text-text-muted",
                                                    "aria-label": "Message actions",
                                                    "aria-haspopup": "menu",
                                                    "aria-expanded": "{menu_open}",
                                                    "data-testid": "message-kebab",
                                                    onclick: move |e: MouseEvent| {
                                                        e.stop_propagation();
                                                        let is_open = open_action_menu.peek().as_deref()
                                                            == Some(msg_id_kebab_toggle.as_str());
                                                        if is_open {
                                                            crate::util::defer(move || open_action_menu.set(None));
                                                        } else {
                                                            // Position the menu from the tap coordinates: flip it
                                                            // above the kebab when the tap is in the bottom ~40% of
                                                            // the viewport (so the composer doesn't clip it), and
                                                            // open it toward the viewport centre (left-anchored when
                                                            // the kebab is on the left half, right-anchored on the
                                                            // right half) so its content never runs off a screen edge.
                                                            let coords = e.client_coordinates();
                                                            let win_w = web_sys::window()
                                                                .and_then(|w| w.inner_width().ok())
                                                                .and_then(|v| v.as_f64())
                                                                .unwrap_or(400.0);
                                                            // Choose the flip direction from the space available in
                                                            // BOTH directions within the chat scrollport (which lives
                                                            // inside an overflow-y-auto container whose bounds sit
                                                            // above the composer and below the header). Open downward
                                                            // when the menu fits below; only flip up when it doesn't
                                                            // fit below AND there's more room above. A received menu
                                                            // (2 rows) is shorter than an own menu (4 rows), so it
                                                            // stays down in cases where an own menu would flip.
                                                            // (#402 review)
                                                            let (sp_top, sp_bottom) = web_sys::window()
                                                                .and_then(|w| w.document())
                                                                .and_then(|d| {
                                                                    d.get_element_by_id("chat-scroll-container")
                                                                })
                                                                .map(|el| {
                                                                    let r = el.get_bounding_client_rect();
                                                                    (r.top(), r.bottom())
                                                                })
                                                                .unwrap_or((60.0, 600.0));
                                                            let menu_height = if is_self { 200.0 } else { 110.0 };
                                                            let space_below = sp_bottom - coords.y;
                                                            let space_above = coords.y - sp_top;
                                                            let above =
                                                                space_below < menu_height && space_above > space_below;
                                                            let align_left = coords.x < win_w * 0.5;
                                                            // Cap the menu to the actual space on the chosen side
                                                            // (minus a small gap) so it scrolls internally rather
                                                            // than being clipped by the scroll container when it
                                                            // fits neither side. Floor so it never collapses.
                                                            // Exactly the space on the chosen side (minus the
                                                            // mt-1/mb-1 gap): never larger, so the overflow-y-auto
                                                            // menu can't exceed the scrollport and clip its own
                                                            // rows. `above` already selects the roomier side, so
                                                            // this is realistically ample; the 1px floor only
                                                            // guards a degenerate near-zero measurement.
                                                            let max_h = ((if above { space_above } else { space_below })
                                                                - 16.0)
                                                                .max(1.0);
                                                            let id = msg_id_kebab_toggle.clone();
                                                            // Defer signal writes out of the event handler per
                                                            // .claude/rules/dioxus-signal-safety.md (Firefox-mobile
                                                            // re-entrant borrow crashes).
                                                            crate::util::defer(move || {
                                                                menu_show_above.set(above);
                                                                menu_align_left.set(align_left);
                                                                menu_max_h.set(max_h);
                                                                // Dismiss any open reaction picker so the two
                                                                // popovers can't stack (#402 review).
                                                                open_emoji_picker.set(None);
                                                                open_action_menu.set(Some(id));
                                                            });
                                                        }
                                                    },
                                                    Icon { icon: FaEllipsisVertical, width: 16, height: 16 }
                                                }
                                                // Action menu popover + dismiss backdrop. The backdrop is
                                                // `fixed inset-0` (covers the viewport now that no transformed
                                                // ancestor clips it) so a tap anywhere else dismisses.
                                                if menu_open {
                                                    div {
                                                        class: "fixed inset-0 z-40",
                                                        onclick: move |_| crate::util::defer(move || open_action_menu.set(None)),
                                                    }
                                                    div {
                                                        // Opens toward the bubble/centre (self: right of the
                                                        // left-gutter kebab; other: left of the right-gutter
                                                        // kebab); `max-w` clamps it to the viewport as a
                                                        // backstop against a narrow-screen overflow.
                                                        class: format!(
                                                            "absolute z-50 min-w-[8rem] max-w-[calc(100vw-1rem)] overflow-y-auto bg-panel rounded-lg shadow-lg border border-border py-1 flex flex-col {} {}",
                                                            if *menu_show_above.read() { "bottom-full mb-1" } else { "top-full mt-1" },
                                                            if *menu_align_left.read() { "left-0" } else { "right-0" }
                                                        ),
                                                        style: format!("max-height: {}px", *menu_max_h.read()),
                                                        "data-testid": "message-action-menu",
                                                        button {
                                                            class: "flex items-center gap-2 px-3 py-2 text-sm text-text hover:bg-surface text-left",
                                                            onclick: move |_| {
                                                                let id = msg_id_menu_reply.clone();
                                                                let author = reply_author_kebab.clone();
                                                                let preview = reply_preview_kebab.clone();
                                                                crate::util::defer(move || {
                                                                    on_reply.call(ReplyContext {
                                                                        message_id: id,
                                                                        author_name: author,
                                                                        content_preview: preview,
                                                                    });
                                                                    open_action_menu.set(None);
                                                                });
                                                            },
                                                            Icon { icon: FaReply, width: 14, height: 14 }
                                                            "Reply"
                                                        }
                                                        button {
                                                            class: "flex items-center gap-2 px-3 py-2 text-sm text-text hover:bg-surface text-left",
                                                            onclick: move |_| {
                                                                let picker_id = format!("inline-{}", msg_id_menu_react);
                                                                // Inherit the kebab's flip direction so the picker
                                                                // for a bottom message also opens upward, not
                                                                // clipped by the composer (#402 review).
                                                                let above = *menu_show_above.peek();
                                                                crate::util::defer(move || {
                                                                    picker_show_above.set(above);
                                                                    open_emoji_picker.set(Some(picker_id));
                                                                    open_action_menu.set(None);
                                                                });
                                                            },
                                                            Icon { icon: FaFaceSmile, width: 14, height: 14 }
                                                            "React"
                                                        }
                                                        if is_self {
                                                            button {
                                                                class: "flex items-center gap-2 px-3 py-2 text-sm text-text hover:bg-surface text-left",
                                                                onclick: move |_| {
                                                                    let t = edit_text_kebab.clone();
                                                                    let id = msg_id_menu_edit.clone();
                                                                    crate::util::defer(move || {
                                                                        edit_text.set(t);
                                                                        editing_message.set(Some(id));
                                                                        open_action_menu.set(None);
                                                                    });
                                                                },
                                                                Icon { icon: FaPenToSquare, width: 14, height: 14 }
                                                                "Edit"
                                                            }
                                                            button {
                                                                class: "flex items-center gap-2 px-3 py-2 text-sm text-red-500 hover:bg-error-bg text-left",
                                                                onclick: move |_| {
                                                                    let id = msg_id_menu_delete.clone();
                                                                    crate::util::defer(move || {
                                                                        on_request_delete.call(id);
                                                                        open_action_menu.set(None);
                                                                    });
                                                                },
                                                                Icon { icon: FaTrashCan, width: 14, height: 14 }
                                                                "Delete"
                                                            }
                                                        }
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
                                                // Raise the whole picker (grid + z-40 backdrop) above the
                                                // z-50 message kebabs while it's open, so a nearby closed
                                                // kebab can't paint over the emoji grid and steal a tap
                                                // (mirrors the action menu's z-[60] behaviour). (#402 review)
                                                class: format!(
                                                    "relative group/react inline-flex items-center {}",
                                                    if is_inline_picker_open { "z-[60]" } else { "" }
                                                ),
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

    /// Issue #315 — pin that the markdown renderer never passes raw HTML
    /// through to the DOM. `markdown_to_html` feeds attacker-controlled
    /// message text into `markdown::to_html_with_options(_, Options::gfm())`,
    /// whose output is then injected via `dangerous_inner_html` at three
    /// sites (room description, message body, DM body). GFM mode defaults
    /// `allow_dangerous_html = false`, so raw HTML is escaped rather than
    /// emitted as live markup. This test fails loudly if a future `markdown`
    /// upgrade or an `Options` change ever flips that on — which would
    /// re-open the stored-XSS hole closed alongside #227 / #314.
    #[test]
    fn raw_html_is_escaped_not_executed() {
        // Each payload is a classic stored-XSS vector. With
        // allow_dangerous_html=false the leading `<` must be escaped to
        // `&lt;`, so the markup survives as inert text instead of a live
        // element.
        //
        // `<img>` and `<svg>` are deliberately chosen: they are NOT on the
        // GFM tagfilter's neutralization list, so if `allow_dangerous_html`
        // were ever flipped on they would pass through as live `<img …>` /
        // `<svg …>` tags — making these the payloads that actually trip the
        // tripwire. (`<script>`/`<iframe>` are masked by the tagfilter even
        // with dangerous HTML enabled, so they can't distinguish the flip;
        // they're covered below only for the escaping guarantee.)
        let live_tag_vectors = ["<img src=x onerror=alert(1)>", "<svg onload=alert(1)>"];
        for payload in live_tag_vectors {
            let html = message_to_html(payload);
            assert!(
                html.contains("&lt;"),
                "raw HTML payload should be HTML-escaped (expected `&lt;`): \
                 input={payload:?} output={html:?}"
            );
            assert!(
                !html.contains("<img")
                    && !html.contains("<svg")
                    && !html.contains("<iframe")
                    && !html.contains("<script"),
                "raw HTML payload must not survive as an executable tag: \
                 input={payload:?} output={html:?}"
            );
        }

        // `<script>`/`<iframe>` must still be escaped on the safe path.
        for payload in ["<script>alert(1)</script>", "<iframe></iframe>"] {
            let html = message_to_html(payload);
            assert!(
                html.contains("&lt;"),
                "raw HTML should be HTML-escaped (expected `&lt;`): \
                 input={payload:?} output={html:?}"
            );
        }
    }

    /// Issue #315 — pin that the markdown renderer never emits a dangerous
    /// URL scheme in an `href`. GFM mode defaults
    /// `allow_dangerous_protocol = false`, so `javascript:` / `vbscript:` /
    /// `data:` links (whether autolinked `<scheme:...>` or `[text](scheme:...)`)
    /// have their `href` neutralized to empty rather than carrying the
    /// executable scheme into the DOM. This fails loudly if that protection
    /// is ever switched off.
    #[test]
    fn dangerous_url_schemes_are_neutralized() {
        let cases = [
            "<javascript:alert(1)>",
            "[click](javascript:alert(1))",
            "[x](vbscript:msgbox(1))",
            "[d](data:text/html,<script>alert(1)</script>)",
        ];
        for payload in cases {
            let html = message_to_html(payload);
            assert!(
                !html.contains("href=\"javascript:")
                    && !html.contains("href=\"vbscript:")
                    && !html.contains("href=\"data:"),
                "dangerous URL scheme must not reach an href: \
                 input={payload:?} output={html:?}"
            );
        }
    }

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

    /// Issue #158: a blank line between two blocks of text is a paragraph
    /// break, and the Markdown renderer must emit a separate `<p>` for each
    /// so the stylesheet can space them apart. This is the HTML-structure
    /// half of the fix; the CSS half is pinned by
    /// `prose_paragraph_spacing_css_present` below.
    #[test]
    fn blank_line_produces_separate_paragraphs() {
        let html = message_to_html("First paragraph.\n\nSecond paragraph.");
        let paragraphs = html.matches("<p>").count();
        assert_eq!(
            paragraphs, 2,
            "blank-line-separated text should render as two <p> blocks: {html}"
        );
    }

    /// Issue #158 root cause: Tailwind v4's Preflight reset zeroes the
    /// margin on every element, so the `<p>` blocks above collapse into a
    /// single wall of text unless the stylesheet re-adds paragraph spacing.
    /// The message body is rendered inside a `.prose` container, so the
    /// `.prose p` margin rule is what actually makes paragraph breaks
    /// visible. Pin its presence in the source stylesheet so a future
    /// Tailwind bump or CSS refactor that silently drops it fails CI rather
    /// than regressing the rendering. `styles.css` (the compiled output) is
    /// a gitignored build artifact, so we assert against the tracked source.
    #[test]
    fn prose_paragraph_spacing_css_present() {
        const TAILWIND_CSS: &str = include_str!("../../assets/tailwind.css");
        assert!(
            TAILWIND_CSS.contains(".prose p {"),
            "tailwind.css must keep a `.prose p` rule so Markdown paragraph \
             breaks are visible (issue #158); the Tailwind reset zeroes <p> \
             margins otherwise"
        );
    }

    const SAMPLE_ID: &str = "UDzGbcWrKN748tYbhvbPCCCQrZc9r9xkN3tUuun5Rts";
    /// Real-shape 44-char base58 ID for tests that need a second distinct ID.
    const SAMPLE_ID_2: &str = "EqJ5YpEEV3XLqEvKWLQHFhGAac2qXzSUoE6k2zbdnXBr";

    #[test]
    fn freenet_web_url_label_shortened() {
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&url);
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/\"")),
            "href should be rewritten to absolute path: {html}"
        );
        assert!(
            !html.contains("href=\"http://127.0.0.1:7509/"),
            "host/port must be stripped from href: {html}"
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
            "should not rewrite label when link text is custom: {html}"
        );
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/\"")),
            "href should still be rewritten so the link works for any reader: {html}"
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
        let url_b = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID_2}/foo.html");
        let html = message_to_html(&format!("see {url_a} and also {url_b} thanks"));
        assert!(
            html.contains(">freenet:UDzGbcWr</a>"),
            "first link should be shortened: {html}"
        );
        assert!(
            html.contains(">freenet:EqJ5YpEE/foo.html</a>"),
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

    #[test]
    fn href_rewritten_strips_host_and_port() {
        // The whole point of this fix: a link pasted with `127.0.0.1:7509`
        // by user A must still resolve for user B, who is connected to
        // their own gateway on a different host/port.
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/index.html");
        let html = message_to_html(&url);
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/index.html\"")),
            "href should be rewritten to a same-origin absolute path: {html}"
        );
        assert!(
            !html.contains("127.0.0.1:7509"),
            "the original host:port must not survive anywhere in the output: {html}"
        );
    }

    #[test]
    fn href_rewrite_preserves_fragment() {
        // Lukas's report (matrix, 2026-04-27): pasted River room URLs include a
        // fragment like `#AWPjDQdKey/1/home`. Stripping the host but losing the
        // fragment would still break navigation, so this guards the suffix path.
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID_2}/#AWPjDQdKey/1/home");
        let html = message_to_html(&url);
        assert!(
            html.contains(&format!(
                "href=\"/v1/contract/web/{SAMPLE_ID_2}/#AWPjDQdKey/1/home\""
            )),
            "fragment should be carried through the rewrite: {html}"
        );
    }

    #[test]
    fn href_rewrite_handles_https() {
        let url = format!("https://gw.example.com/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html(&url);
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/\"")),
            "https URLs should also have host stripped: {html}"
        );
    }

    #[test]
    fn invalid_base58_mid_id_not_rewritten() {
        // The id_end scan stops at the first non-base58 char. Construct a
        // string where an `O` sits 20 chars in: id_end == 20 falls outside
        // the 43|44 length window, so the URL must not be rewritten. This
        // exercises the "valid base58 chars surround a forbidden char"
        // path, not the "first char is forbidden" path that returns id_end=0.
        let mid_bogus = format!(
            "{}O{}",
            &SAMPLE_ID[..20],
            &SAMPLE_ID[21..] // total = 20 + 1 + 22 = 43 chars, but with `O` at position 20
        );
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{mid_bogus}/index.html");
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "bogus ID with mid-string non-base58 char must not be beautified: {html}"
        );
        assert!(
            html.contains(&format!("href=\"{url}\"")),
            "bogus ID must leave href alone (host/port preserved): {html}"
        );
    }

    #[test]
    fn short_id_segment_not_rewritten() {
        // ID segments shorter than 43 chars cannot be BLAKE3 hashes.
        let url = "http://127.0.0.1:7509/v1/contract/web/tooshort/page.html".to_string();
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "short ID must not be beautified: {html}"
        );
        assert!(
            html.contains(&format!("href=\"{url}\"")),
            "short ID must leave href alone: {html}"
        );
    }

    #[test]
    fn overlong_id_segment_not_rewritten() {
        // ID segments longer than 44 chars also cannot be BLAKE3 hashes.
        let too_long = "a".repeat(45);
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{too_long}/");
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "overlong ID must not be beautified: {html}"
        );
        assert!(
            html.contains(&format!("href=\"{url}\"")),
            "overlong ID must leave href alone: {html}"
        );
    }

    #[test]
    fn id_segment_42_chars_not_rewritten() {
        // 42-char base58 segment: one char short of the lower bound.
        // Pins the lower edge of the `matches!(id_end, 43 | 44)` predicate.
        let too_short = "a".repeat(42);
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{too_short}/");
        let html = message_to_html(&url);
        assert!(
            !html.contains("freenet:"),
            "42-char ID is one short of lower bound, must not be beautified: {html}"
        );
        assert!(
            html.contains(&format!("href=\"{url}\"")),
            "42-char ID must leave href alone: {html}"
        );
    }

    #[test]
    fn dev_mode_does_not_rewrite_href_but_still_beautifies_label() {
        // When River is served outside a gateway (e.g. `dx serve`,
        // `cargo make dev-example`, or `python -m http.server`), there is no
        // gateway behind the dev server to redirect to. Stripping the host
        // would turn a working `https://gw.example/v1/contract/web/<id>/`
        // link into a 404 against the dev server. Leave the href intact;
        // the label can still be shortened (purely cosmetic).
        let url = format!("https://gw.example.com/v1/contract/web/{SAMPLE_ID}/");
        let html = message_to_html_inner(&url, /* behind_gateway = */ false);
        assert!(
            html.contains(&format!("href=\"{url}\"")),
            "dev-mode must preserve the original gateway-qualified href: {html}"
        );
        assert!(
            !html.contains("href=\"/v1/contract/web/"),
            "dev-mode must not produce a same-origin absolute path: {html}"
        );
        assert!(
            html.contains(">freenet:UDzGbcWr</a>"),
            "label beautification still applies in dev mode (cosmetic only): {html}"
        );
    }

    #[test]
    fn path_traversal_in_suffix_not_rewritten() {
        // Defense against an attacker pasting a URL whose suffix contains
        // `..` segments. Without this guard, the same-origin rewrite would
        // hand the browser a path that normalizes onto unrelated endpoints
        // on the reader's local gateway — turning a paste into a CSRF-style
        // redirect. (Caught in skeptical review of #224.)
        let url = format!(
            "http://attacker.example/v1/contract/web/{SAMPLE_ID}/../../v1/peer/diagnostics"
        );
        let html = message_to_html(&url);
        assert!(
            !html.contains("href=\"/v1/contract/web/"),
            "URL with `..` segments in suffix must NOT be rewritten to same-origin: {html}"
        );
        assert!(
            !html.contains("freenet:"),
            "URL with `..` segments in suffix must not be beautified either: {html}"
        );
        assert!(
            html.contains(&format!("href=\"{url}\"")),
            "original URL must be left intact so the click goes to attacker.example, \
             not to the reader's own gateway: {html}"
        );
    }

    #[test]
    fn dotdot_in_query_or_fragment_is_fine() {
        // `..` is only dangerous as a path segment — the browser normalizes
        // path segments before the `?` or `#`. A literal `..` inside a query
        // value or fragment is just data and should not block the rewrite.
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/?next=../foo");
        let html = message_to_html(&url);
        assert!(
            html.contains(&format!(
                "href=\"/v1/contract/web/{SAMPLE_ID}/?next=../foo\""
            )),
            "`..` in query string should not block the rewrite: {html}"
        );
    }

    #[test]
    fn non_http_scheme_not_rewritten() {
        // `parse_freenet_web_url` must restrict to http/https. Defense in
        // depth: if the markdown crate (or a future change) ever surfaces a
        // `[label](javascript:...)` link, we want the parser to refuse to
        // touch it rather than trust the upstream sanitizer alone.
        // Markdown autolinks won't normally produce non-http(s) URLs from
        // bare text, so we exercise this via the explicit-link form.
        let html = message_to_html(&format!(
            "[click](ftp://x.example/v1/contract/web/{SAMPLE_ID}/)"
        ));
        assert!(
            !html.contains("href=\"/v1/contract/web/"),
            "non-http(s) scheme must not be rewritten to same-origin: {html}"
        );
    }

    // -----------------------------------------------------------------
    // Issue freenet/river#284 regression coverage:
    //
    // A newly-invited member of a private room briefly has NO local
    // secrets — the chat-delegate hasn't published the owner's back-
    // fill ciphertext yet (or it's mid-flight). The previous
    // diagnostic placeholder "[Encrypted message - secret vN not
    // available (have: [])]" was alarming and looked like data loss.
    // Replace it with a calm, plain-language explanation that this is
    // expected and will resolve in a few seconds. Once any secret
    // arrives the decryption branch fires and the placeholder
    // disappears.
    //
    // The "we have SOME secrets but not THIS version" case is left as
    // a less-alarming neutral diagnostic — that's the rotated-past
    // case rather than the sync-window case, and it deserves a
    // different message than the joiner's UX.
    // -----------------------------------------------------------------

    fn private_msg_body(secret_version: u32) -> river_core::room_state::message::RoomMessageBody {
        // Hand-construct a Private body — we don't actually decrypt in
        // these tests, just exercise the placeholder-selection branch.
        river_core::room_state::message::RoomMessageBody::Private {
            content_type: river_core::room_state::content::CONTENT_TYPE_TEXT,
            content_version: river_core::room_state::content::TEXT_CONTENT_VERSION,
            ciphertext: vec![0u8; 32],
            nonce: [0u8; 12],
            secret_version,
        }
    }

    /// Issue #284: empty-secrets-map case (joiner sync window) renders
    /// the friendly "Decrypting messages" message, not the
    /// diagnostic placeholder that exposes raw `(have: [])` internals.
    #[test]
    fn decrypt_placeholder_for_empty_secrets_is_friendly() {
        let body = private_msg_body(1);
        let secrets: HashMap<u32, [u8; 32]> = HashMap::new();
        let rendered = decrypt_message_content(&body, &secrets);
        assert!(
            rendered.contains("Decrypting messages"),
            "empty-secrets-map (sync window) must render the friendly \
             explanation, got: {rendered}"
        );
        assert!(
            !rendered.contains("(have: ["),
            "the alarming diagnostic dump must NOT appear in the sync-window \
             placeholder, got: {rendered}"
        );
    }

    /// Issue #284: when we have SOME secrets but not the one this
    /// message was encrypted under, render a neutral placeholder.
    /// This is the rotated-past case, not the sync-window case — the
    /// user has decrypted other messages successfully, so the friendly
    /// "your invitation is still arriving" copy would be wrong here.
    #[test]
    fn decrypt_placeholder_for_missing_version_is_neutral_not_alarming() {
        let body = private_msg_body(5);
        // We have version 1 and 2 but not 5 (the one needed).
        let mut secrets: HashMap<u32, [u8; 32]> = HashMap::new();
        secrets.insert(1, [0u8; 32]);
        secrets.insert(2, [0u8; 32]);
        let rendered = decrypt_message_content(&body, &secrets);
        assert!(
            !rendered.contains("Decrypting messages"),
            "joiner-friendly copy must not surface when secrets ARE \
             populated (this is the rotated-past case, not sync-window), \
             got: {rendered}"
        );
        assert!(
            !rendered.contains("(have: ["),
            "the alarming diagnostic dump must not appear in any \
             placeholder branch, got: {rendered}"
        );
        // We DO want to surface the version number for diagnostics — it
        // helps both the user and the developer triage rotation issues.
        assert!(
            rendered.contains("v5"),
            "the rotated-past placeholder should surface the missing \
             version number, got: {rendered}"
        );
    }

    // --- @mention rendering ---------------------------------------------

    fn mid_from(hex: &str) -> MemberId {
        river_core::mention::member_id_from_hex(hex).unwrap()
    }

    #[test]
    fn strip_markdown_removes_formatting_and_keeps_link_text() {
        let s = strip_markdown("**bold** and `code` and [a link](http://x.example) end");
        assert!(!s.contains('*'), "emphasis markers removed: {s}");
        assert!(!s.contains('`'), "code fences removed: {s}");
        assert!(!s.contains("http://x.example"), "link url dropped: {s}");
        assert!(s.contains("bold") && s.contains("code") && s.contains("end"));
        assert!(s.contains("a link"), "link visible text kept: {s}");
    }

    #[test]
    fn clean_reply_preview_resolves_mention_to_current_name_and_strips_markdown() {
        let id = mid_from("00000000000000aa");
        let mut names = HashMap::new();
        names.insert(id, "Alice".to_string());
        // Token snapshot is "OldAlice"; the live map says "Alice".
        let token = river_core::mention::encode_mention(id, "OldAlice");
        let cleaned = clean_reply_preview(&format!("hey {token}, **see** this"), &names);
        assert!(
            cleaned.contains("@Alice"),
            "current nickname used: {cleaned}"
        );
        assert!(
            !cleaned.contains("OldAlice"),
            "snapshot overridden: {cleaned}"
        );
        assert!(
            !cleaned.contains("rv:"),
            "no raw mention token syntax: {cleaned}"
        );
        assert!(
            !cleaned.contains("**") && !cleaned.contains("]("),
            "markdown stripped: {cleaned}"
        );
        assert!(cleaned.contains("see"));
    }

    #[test]
    fn mention_chip_uses_current_name_not_snapshot() {
        let id = mid_from("00000000000000aa");
        let mut names = HashMap::new();
        names.insert(id, "CurrentName".to_string());
        let token = river_core::mention::encode_mention(id, "OldName");
        let html = message_to_html_with_mentions(
            &format!("hi {token}!"),
            &names,
            mid_from("00000000000000ff"),
        );
        assert!(
            html.contains("data-member-id=\"00000000000000aa\""),
            "chip must carry the lossless member id: {html}"
        );
        assert!(
            html.contains(">@CurrentName</span>"),
            "chip must show the CURRENT name, following renames: {html}"
        );
        assert!(
            !html.contains("OldName"),
            "stale snapshot name must be overridden: {html}"
        );
    }

    #[test]
    fn mention_chip_falls_back_to_snapshot_for_unknown_member() {
        let id = mid_from("0000000000000abc");
        let names = HashMap::new(); // member not resolvable
        let token = river_core::mention::encode_mention(id, "Ghost");
        let html = message_to_html_with_mentions(&token, &names, mid_from("0000000000000001"));
        assert!(
            html.contains(">@Ghost</span>"),
            "unknown member falls back to the token's snapshot name: {html}"
        );
    }

    #[test]
    fn mention_chip_escapes_attacker_controlled_nickname() {
        // Nicknames are attacker-controlled and the chip enters the DOM via
        // dangerous_inner_html (freenet/river#227) — must be escaped.
        let id = mid_from("0000000000000001");
        let mut names = HashMap::new();
        names.insert(id, "<img src=x onerror=alert(1)>".to_string());
        let token = river_core::mention::encode_mention(id, "snap");
        let html = message_to_html_with_mentions(&token, &names, mid_from("0000000000000002"));
        assert!(
            !html.contains("<img"),
            "raw markup must not survive: {html}"
        );
        assert!(
            html.contains("&lt;img"),
            "nickname must be HTML-escaped: {html}"
        );
    }

    #[test]
    fn mention_of_self_gets_distinct_highlight_class() {
        let me = mid_from("0000000000000007");
        let mut names = HashMap::new();
        names.insert(me, "Me".to_string());
        let token = river_core::mention::encode_mention(me, "Me");
        let html = message_to_html_with_mentions(&token, &names, me);
        assert!(
            html.contains("river-mention-self"),
            "a mention of the local user must get the self class: {html}"
        );
    }

    #[test]
    fn text_without_mentions_renders_identically_to_plain() {
        let names = HashMap::new();
        let text = "a normal *markdown* msg with a https://example.com link";
        let any = mid_from("0000000000000001");
        assert_eq!(
            message_to_html_with_mentions(text, &names, any),
            message_to_html(text),
            "the no-mention fast path must match the plain renderer byte-for-byte"
        );
    }

    #[test]
    fn multiple_mentions_each_resolve_independently() {
        let a = mid_from("000000000000000a");
        let b = mid_from("000000000000000b");
        let mut names = HashMap::new();
        names.insert(a, "Ann".to_string());
        names.insert(b, "Bob".to_string());
        let text = format!(
            "{} and {}",
            river_core::mention::encode_mention(a, "x"),
            river_core::mention::encode_mention(b, "y")
        );
        let html = message_to_html_with_mentions(&text, &names, mid_from("00000000000000ff"));
        assert!(html.contains(">@Ann</span>"), "{html}");
        assert!(html.contains(">@Bob</span>"), "{html}");
        assert!(
            html.contains("data-member-id=\"000000000000000a\""),
            "{html}"
        );
        assert!(
            html.contains("data-member-id=\"000000000000000b\""),
            "{html}"
        );
    }

    #[test]
    fn current_token_chip_carries_full_id_resolved_from_short_ref() {
        // The wire token now carries only the 8-char short ref; the chip must
        // still recover the FULL id (for the click interceptor) by matching the
        // short ref against the known members.
        let id = mid_from("00000000000000aa");
        let mut names = HashMap::new();
        names.insert(id, "Alice".to_string());
        let token = river_core::mention::encode_mention(id, "Alice");
        assert!(
            token.contains(&format!(
                "rv:{}",
                river_core::mention::member_id_to_short(id)
            )),
            "token uses the short base32 ref: {token}"
        );
        let html = message_to_html_with_mentions(&token, &names, mid_from("00000000000000ff"));
        assert!(
            html.contains("data-member-id=\"00000000000000aa\""),
            "chip recovers the lossless id from the short ref: {html}"
        );
    }

    #[test]
    fn legacy_hex_mention_chip_is_clickable_even_when_member_unknown() {
        // A legacy `rv:<hex>` token carries the full id, so its chip stays
        // clickable (data-member-id present) even for a member we can't name.
        let id = mid_from("0000000000000abc");
        let names = HashMap::new(); // member not resolvable by name
        let legacy = format!(
            "hi @[Bob]({}{})!",
            river_core::mention::REF_SCHEME,
            river_core::mention::member_id_to_hex(id)
        );
        let html = message_to_html_with_mentions(&legacy, &names, mid_from("0000000000000001"));
        assert!(
            html.contains("data-member-id=\"0000000000000abc\""),
            "legacy chip keeps the full id: {html}"
        );
        assert!(html.contains(">@Bob</span>"), "snapshot name used: {html}");
    }

    #[test]
    fn unknown_short_mention_renders_inert_chip_without_member_id() {
        // A current (short) token naming a member this client doesn't know
        // cannot recover a full id, so the chip renders the snapshot name but
        // carries no data-member-id (nothing for the interceptor to open).
        let id = mid_from("0000000000000abc");
        let names = HashMap::new();
        let token = river_core::mention::encode_mention(id, "Ghost");
        let html = message_to_html_with_mentions(&token, &names, mid_from("0000000000000001"));
        assert!(
            html.contains(">@Ghost</span>"),
            "snapshot name shown: {html}"
        );
        assert!(
            !html.contains("data-member-id"),
            "unknown short ref must not fabricate a member id: {html}"
        );
    }

    #[test]
    fn legacy_hex_self_mention_gets_highlight() {
        // A self-mention in an OLD (hex) message must still resolve to self and
        // get the self-highlight class, just like the current short form.
        let me = mid_from("0000000000000007");
        let mut names = HashMap::new();
        names.insert(me, "Me".to_string());
        let legacy = format!(
            "@[Me]({}{})",
            river_core::mention::REF_SCHEME,
            river_core::mention::member_id_to_hex(me)
        );
        let html = message_to_html_with_mentions(&legacy, &names, me);
        assert!(
            html.contains("river-mention-self"),
            "legacy self-mention must get the self class: {html}"
        );
    }
}
