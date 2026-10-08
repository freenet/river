use crate::components::app::document_title::count_unread_behind_rooms_panel;
#[cfg(target_arch = "wasm32")]
use crate::components::app::notifications::request_permission_on_first_message;
use crate::components::app::receive_times::{
    first_seen_ms, format_delay, get_delay_secs_from, ReceiveTimes,
};
use crate::components::app::sync_info::{RoomSyncStatus, SYNC_INFO};
use crate::components::app::{
    MobileView, CURRENT_ROOM, EDIT_ROOM_MODAL, MEMBER_INFO_MODAL, MOBILE_VIEW, NOTIFICATION_MODAL,
    ROOMS,
};
use crate::components::members::{
    deputy_badges_for_viewer, impersonation_checker_for_viewer, impersonation_warning_for_display,
    privilege_in_view, DeputyBadge,
};
use crate::components::scroll_to_latest::LatestButton;
#[cfg(target_arch = "wasm32")]
use crate::components::scroll_to_latest::{
    scroll_to_end, sentinel_in_view, NEWEST_IN_VIEW_SLACK_PX,
};
use crate::room_data::{NotificationMode, SendMessageError};
use crate::util::confusable::{ImpersonationChecker, ImpersonationWarning};
use crate::util::display_name::{display_nickname, sanitize_display_name};
use crate::util::ecies::{encrypt_with_symmetric_key, unseal_text_or_placeholder};
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
    FaBars, FaBell, FaBellSlash, FaCircleInfo, FaEllipsisVertical, FaFaceSmile, FaPenToSquare,
    FaReply, FaTrashCan, FaTriangleExclamation, FaUsers,
};
use dioxus_free_icons::Icon;
use freenet_scaffold::ComposableState;
use river_core::room_state::member::{MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfoV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageId, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
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

/// Snapshot the currently-open room's data, cloning it out of [`ROOMS`].
///
/// Called at INTERACTION time by the message-action handlers rather than
/// captured into them. A captured `RoomData` is a DEEP clone of the whole room
/// state — every retained message, member and signature — and Dioxus clones an
/// event-handler closure once per rendered row, so capturing made the
/// conversation's resident memory O(messages × state_size): measured at ~343 KB
/// per clone × 2 handlers × 1133 rows ≈ 0.8 GB in the "Off Topic" room, on top
/// of a WASM heap that never returns linear memory to the OS. Looking the room
/// up when the user actually clicks costs one clone per interaction instead of
/// two per row, and reads fresher state than a render-time snapshot would.
///
/// Keep this cheap to call and keep callers from capturing its result: the
/// per-row cost is the whole point. Pinned by
/// `message_action_handlers_do_not_capture_room_data`.
fn current_room_data_snapshot() -> Option<crate::room_data::RoomData> {
    // Scoped so the CURRENT_ROOM guard is dropped before ROOMS is read — the
    // same re-entrancy hazard the render-side lookup documents.
    let key = { CURRENT_ROOM.read().owner_key }?;
    // `try_read`, not `read`, for the reason the render-side lookup documents:
    // a Dioxus write guard's Drop notifies subscribers synchronously on Firefox
    // and can re-enter while ROOMS is still borrowed.
    //
    // A miss here means the action the reader just took is dropped, which the
    // old render-time capture could not do — so say so rather than failing
    // silently. It should not happen from a click handler (the event starts a
    // fresh task, with no ROOMS write guard on the stack); a warning in the
    // wild means that assumption is wrong somewhere.
    match ROOMS.try_read() {
        Ok(rooms) => rooms.map.get(&key).cloned(),
        Err(_) => {
            warn!("ROOMS signal busy while resolving the open room; action dropped");
            None
        }
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
    /// The 🛡 shield to show next to this author's name, or `None` for none.
    /// See [`DeputyBadge`] for the predicate and why visibility and the
    /// "can ban you" wording are separate questions.
    author_badge: Option<DeputyBadge>,
    /// The ⚠ impersonation warning for this author, or `None`. Same value, and
    /// therefore the same tier rule and tooltip, the member-list row shows —
    /// both go through
    /// [`crate::components::members::impersonation_warning_for_display`].
    ///
    /// **NOT mutually exclusive with `author_badge`.** A deputy IS warned about
    /// when their name collides with another protected name — two deputies
    /// sharing a name each render 🛡 *and* ⚠ — so both slots can be occupied at
    /// once and the layout must handle it. The warning renders FIRST, and its
    /// tooltip switches wording when the flagged member holds privilege (see
    /// [`ImpersonationWarning::flagged_privilege`]) rather than being
    /// suppressed: suppression is what a deputised sockpuppet wants.
    /// `a_shield_and_a_warning_can_both_render` pins the co-occurrence.
    author_impersonation: Option<ImpersonationWarning>,
    is_self: bool,
    first_time: DateTime<Utc>,
    /// True if any message in this group carried a sender timestamp later than
    /// when we received it, by more than [`CLOCK_SKEW_TOLERANCE_SECS`], and was
    /// therefore clamped back to its arrival time.
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
    /// True if the sender's timestamp ran ahead of when we received this
    /// message and was clamped back to that arrival time. (It is clamped to
    /// ARRIVAL, not to "now": see [`MessageClock`] for why the render's wall
    /// clock cannot be the target.)
    #[allow(dead_code)]
    time_clamped: bool,
    id: String,
    message_id: MessageId,
    edited: bool,
    reactions: HashMap<String, Vec<MemberId>>,
    /// What the reply-quote strip should render, already resolved against live
    /// room state — see [`ReplyStrip`] and [`resolve_reply_strip`].
    reply_strip: ReplyStrip,
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
    /// The newest event folded in, for the read rule (see
    /// [`display_item_last_message_id`]).
    last_message_id: MessageId,
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

/// The newest display message `item` renders: what the read rule marks when
/// the item is the newest one on screen.
fn display_item_last_message_id(item: &DisplayItem) -> Option<MessageId> {
    match item {
        DisplayItem::Messages(group) => group.messages.last().map(|m| m.message_id.clone()),
        DisplayItem::Event(summary) => Some(summary.last_message_id.clone()),
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

/// What the reply-quote strip should render, resolved against live room state
/// by [`resolve_reply_strip`].
///
/// An enum rather than co-varying `Option` fields plus a flag: the renderer has
/// two mutually exclusive arms, and the freenet "paired `Option` fields that
/// must co-occur" bug pattern is exactly the shape where a later edit sets one
/// half and the strip silently renders wrong (or not at all). Matching on this
/// makes the inconsistent state unrepresentable.
///
/// Distinct from [`ReplyContext`], which is the reply-in-progress the composer
/// holds while the user is writing a reply.
#[derive(Clone, Debug, Default, PartialEq)]
enum ReplyStrip {
    /// Not a reply — no strip at all. Also the "still decrypting" state, which
    /// must look like nothing rather than like a failure.
    #[default]
    NotAReply,
    /// A reply whose quoted message could not be read back from room state.
    Unavailable,
    /// A quote verified against the message it quotes. Every field is re-read
    /// from that message; nothing here comes from the replier's snapshot.
    Quote {
        /// Current nickname of the quoted message's ACTUAL author.
        author: String,
        /// Current text of the quoted message, mention-resolved and truncated.
        preview: String,
        /// Quoted message id, used to scroll to the original.
        target_id: MessageId,
    },
}

/// Resolve a message's reply quote against LIVE room state, showing it only
/// when the quoted message is actually still there.
///
/// `ReplyContentV1.target_author_name` / `.target_content_preview` are a
/// snapshot written and signed by the REPLIER; the contract validates neither,
/// so the snapshot is trustworthy only while the quoted message can be re-read
/// from room state. Rendering it unconditionally lets the quoted text outlive
/// the message it quotes:
///
///   * BANNED author — the motivating case. An ENFORCED ban removes the member,
///     which makes `post_apply_cleanup` purge their messages (step 4b) AND
///     their `member_info` record, so their abusive text would otherwise
///     survive verbatim in every reply that quoted it. This CANNOT be keyed on
///     looking the quoted author up in `bans`: the snapshot carries no
///     `MemberId` (only a name string), `MessageId` is `fast_hash(signature)`
///     and so is not invertible, and a banned member has no nickname left
///     anywhere in state to match the name against. Absence of the quoted
///     message is the only observable, so that is what this keys on. (The same
///     purge applies to any member removed from the room, e.g. by
///     inactivity-prune — the ban is just the case that matters.)
///   * DELETED target — deleting a message should remove its text from the
///     room, quotes of it included.
///   * FORGED snapshot — any member can post a reply naming an arbitrary author
///     and arbitrary "quoted" text with a `target_message_id` that points at
///     nothing, or at a message somebody else wrote.
///
/// The cost is that a target which merely aged out of the bounded
/// `recent_messages` window (`max_recent_messages`, default 100 and
/// owner-configurable) also loses its preview. That is deliberate and is why
/// the placeholder wording is neutral: absence cannot distinguish a ban from an
/// ordinary aged-out message, so the UI must not claim "banned".
///
/// A tempting discriminator is the timestamp — if the oldest retained message
/// is NEWER than the reply, the target provably fell out of the window rather
/// than being purged. Do NOT use it: `MessageV1::time` is self-signed and
/// unvalidated, so an ally of a banned member could backdate a reply to force
/// that branch and resurrect the text.
///
/// The strip therefore renders ENTIRELY from live state or not at all — when
/// the target is readable, both its text and its attribution are re-read from
/// it, so edits and renames propagate and neither half of the replier-supplied
/// snapshot is ever displayed.
fn resolve_reply_strip(
    content: &RoomMessageBody,
    messages_state: &MessagesV1,
    member_info: &MemberInfoV1,
    secrets: &HashMap<u32, [u8; 32]>,
    member_names: &HashMap<MemberId, String>,
) -> ReplyStrip {
    use river_core::room_state::content::CONTENT_TYPE_REPLY;

    let Some(target_id) = extract_reply_target_id(content, secrets) else {
        // We could not decode the reply at all.
        //
        // "Still decrypting" is NOT "unavailable" — see `pending_decryption`.
        // Otherwise this is a REPLY we cannot read (`content_type` is cleartext
        // even on the `Private` variant, and a public body can carry undecodable
        // `data`), so say so rather than silently dropping the strip. riverctl
        // reports the same for the same inputs.
        return if pending_decryption(content, secrets)
            || content.content_type() != CONTENT_TYPE_REPLY
        {
            ReplyStrip::NotAReply
        } else {
            ReplyStrip::Unavailable
        };
    };

    let target = messages_state
        .messages
        .iter()
        .find(|m| m.id() == target_id)
        // Mirror `display_messages()` and then some: `messages` retains
        // soft-deleted messages and action messages, and events render as a
        // merged summary rather than a `msg-{id}` row. None of those is a
        // quotable message.
        //
        // The event filter is load-bearing, not tidiness: an EVENT is editable
        // by its author (`rebuild_actions_state_with_decrypted` excludes only
        // `is_action()` from `message_authors`), and `effective_text` consults
        // `edited_content` BEFORE any content-type decode. So without this a
        // member could post a join event, edit it to arbitrary text, and quote
        // it — surfacing text that is rendered nowhere else in the app, since
        // events render through `format_event_summary`. Do NOT drop this filter.
        // Quoting an action or event would also aim scroll-to-original at a
        // `msg-{id}` element that never exists.
        .filter(|target_msg| {
            !messages_state.is_deleted(&target_id)
                && !target_msg.message.content.is_action()
                && !target_msg.message.content.is_event()
        });
    let Some(target_msg) = target else {
        return ReplyStrip::Unavailable;
    };

    let Some(text) = target_plaintext(messages_state, target_msg, secrets) else {
        return if pending_decryption(&target_msg.message.content, secrets) {
            ReplyStrip::NotAReply
        } else {
            ReplyStrip::Unavailable
        };
    };

    // Attribute to the CURRENT nickname of the message's ACTUAL author rather
    // than the snapshot's `target_author_name`, which can name anyone and does
    // not track renames. Routed through the same `canonical()` lookup the
    // message header uses, so a member with duplicate `member_info` records
    // cannot be labelled one way in the quote and another way on their own
    // message.
    let author = resolve_member_nickname(member_info, target_msg.message.author, secrets);

    ReplyStrip::Quote {
        author,
        // Clean the preview for display: resolve @mention tokens to plain
        // `@name` (current nickname) and strip markdown, so the quote reads as
        // plain text rather than showing raw `@[name](rv:id)` / `**` /
        // `[text](url)` syntax. Truncate after cleaning.
        preview: clean_reply_preview(&text, member_names)
            .chars()
            .take(100)
            .collect::<String>(),
        target_id,
    }
}

/// Whether an unreadable private body is merely WAITING on the room secrets
/// rather than genuinely unreadable.
///
/// `RoomData.secrets` is `#[serde(skip)]`, so every cold start of an
/// established private room transiently holds NO secrets until
/// `repopulate_secrets_from_state` rehydrates them from the encrypted blobs.
/// (Transient is the motivating case, not the only one: a public room always
/// has no secrets, and any member of one can post a `Private` body — nothing
/// rejects it — flipping this to true for everyone. The failure mode is benign,
/// since both branches render zero attacker-supplied text; all it can suppress
/// is the "unavailable" marker.)
/// In that window nothing is verifiable and nothing is being hidden (the
/// snapshot is unreadable too), so the strip must render as if the message
/// simply were not a reply — flashing "Original message unavailable" over every
/// reply on every page load is the alarming-placeholder failure that
/// freenet/river#284 was filed to remove. The message body itself takes the
/// same view, rendering a calm "Decrypting messages…".
///
/// The discriminator is deliberately "no secrets AT ALL", not "this version is
/// missing": a rotated-past version is genuinely unavailable and must keep
/// showing the placeholder.
fn pending_decryption(content: &RoomMessageBody, secrets: &HashMap<u32, [u8; 32]>) -> bool {
    matches!(content, RoomMessageBody::Private { .. }) && secrets.is_empty()
}

/// The quoted message's plaintext, or `None` when it genuinely cannot be read.
///
/// Deliberately NOT `decrypt_message_content`: that returns user-facing
/// diagnostics on a missing secret or a failed decrypt ("[Encrypted message -
/// secret v2 unavailable]", "Decrypting messages — this should only take a
/// moment..."), and quoting one of those would render machine copy as if it
/// were the quoted author's words — while reporting the quote as verified. A
/// private target we cannot decrypt has not been re-read, so it must fall
/// through to the neutral placeholder like any other unreadable target.
fn target_plaintext(
    messages_state: &MessagesV1,
    target: &AuthorizedMessageV1,
    secrets: &HashMap<u32, [u8; 32]>,
) -> Option<String> {
    use river_core::room_state::content::{
        ReplyContentV1, TextContentV1, CONTENT_TYPE_REPLY, CONTENT_TYPE_TEXT,
    };

    // An edit supersedes the body, and reaches `actions_state` already
    // decrypted (`RoomData::rebuild_private_actions_state`). Falls back to the
    // public body text, which is `None` for a private message.
    if let Some(text) = messages_state.effective_text(target) {
        return Some(text);
    }

    let RoomMessageBody::Private {
        content_type,
        ciphertext,
        nonce,
        secret_version,
        ..
    } = &target.message.content
    else {
        return None;
    };
    let secret = secrets.get(secret_version)?;
    let plaintext =
        crate::util::ecies::decrypt_with_symmetric_key(secret, ciphertext.as_slice(), nonce)
            .ok()?;
    // Adding a content type? Add it here, to riverctl's mirror
    // `decrypt_private_quote_text`, and (for a public body) to
    // `DecodedContent::as_text` — otherwise replies quoting it render the
    // "unavailable" placeholder even on a client that supports it.
    match *content_type {
        CONTENT_TYPE_TEXT => TextContentV1::decode(&plaintext).ok().map(|c| c.text),
        CONTENT_TYPE_REPLY => ReplyContentV1::decode(&plaintext).ok().map(|r| r.text),
        _ => None,
    }
}

/// A member's current nickname, decrypted when the room is private and
/// sanitised for display.
///
/// Routes through [`MemberInfoV1::canonical`] rather than a pre-collected
/// `member_id -> name` map: River accepts duplicate `member_info` records until
/// `post_apply_cleanup` dedups them, and a raw `.collect()` keeps whichever
/// record happens to come last. Every by-id read must agree on the canonical
/// (highest-rank) record.
///
/// The sanitisation is [`crate::util::display_name::display_nickname`], which
/// strips emoji so a nickname cannot forge the 🛡 deputy badge rendered beside
/// it. Every nickname that reaches the screen goes through it.
fn resolve_member_nickname(
    member_info: &MemberInfoV1,
    member_id: MemberId,
    secrets: &HashMap<u32, [u8; 32]>,
) -> String {
    member_info
        .canonical(member_id)
        .map(|ami| display_nickname(&ami.member_info.preferred_nickname, secrets))
        .unwrap_or_else(|| crate::util::display_name::UNKNOWN_MEMBER.to_string())
}

/// How far a sender's timestamp may run ahead of when we received the message
/// before we stop believing it.
///
/// Not zero. The clamp target used to be `Utc::now()` at render time, which
/// carried the whole propagation delay as implicit slack; the target is now
/// arrival time, which has none, so a bare `>` would flag EVERY message from
/// any peer whose clock is a few seconds fast — and a few seconds of skew
/// between two ordinary machines is normal. A minute is roughly where the
/// rendered `HH:MM` would start disagreeing with reality, which is the point at
/// which telling the reader the timestamp is untrustworthy earns its keep.
const CLOCK_SKEW_TOLERANCE_SECS: i64 = 60;

/// What a grouping pass knows about time.
///
/// Both fields are read once for the whole pass. That is the point: reading
/// the clock per message made the grouping of a clock-skewed message depend on
/// exactly when the render happened, so it changed under the reader.
#[derive(Clone, Copy)]
struct MessageClock<'a> {
    /// When this client first saw each message — see [`ReceiveTimes`].
    receive_times: &'a ReceiveTimes,
    /// Clamp target for a message with no recorded arrival time.
    fallback_now: DateTime<Utc>,
}

impl MessageClock<'_> {
    /// The latest time `message_id` is allowed to claim.
    fn clamp_target(&self, message_id: &MessageId) -> DateTime<Utc> {
        first_seen_ms(self.receive_times, message_id)
            .and_then(|ms| DateTime::<Utc>::from_timestamp_millis(ms as i64))
            .unwrap_or(self.fallback_now)
    }
}

/// Group consecutive messages from the same sender within 5 minutes,
/// and summarize consecutive event messages (e.g. joins).
///
/// A message whose sender-supplied timestamp runs ahead of when we received it
/// (a skewed remote clock) is clamped, because an hour-ahead timestamp would
/// otherwise group and date-separate as if it were an hour in the future — and
/// would then move again on every single render, since the old clamp target was
/// `Utc::now()` read fresh inside this loop. (Nothing here sorts: the display
/// order is the stored order, via `display_messages`.) Clamping to when THIS
/// client first saw the message
/// (`RECEIVE_TIMES`, first-seen-wins and persisted in localStorage) makes the
/// result idempotent: the same input produces the same grouping on every
/// render, which is what `2cb49ec11` set out to do. `fallback_now` covers a
/// message with no recorded arrival — the same behaviour as before, but read
/// once for the whole pass rather than once per message.
fn group_messages(
    messages_state: &MessagesV1,
    member_info: &MemberInfoV1,
    // `None` when this build holds no locally-known identity for the room.
    // Every message still renders; the identity-relative cosmetics (the
    // `is_self` side of the bubble, the "mention of you" chip highlight)
    // degrade to "not me". Never a mis-attribution: an unknown identity
    // matches nobody.
    self_member_id: Option<MemberId>,
    secrets: &HashMap<u32, [u8; 32]>,
    member_names: &HashMap<MemberId, String>,
    // Which members show a 🛡 shield in THIS viewer's conversation, built once
    // per render by `deputy_badges_for_viewer`. Absent ⇒ no shield.
    deputy_badges: &HashMap<MemberId, DeputyBadge>,
    // The ⚠ impersonation checker for this viewer, built once per render by
    // `impersonation_checker_for_viewer` — passed in rather than built here so
    // the protected set is folded once, not once per call.
    impersonation: &ImpersonationChecker,
    // The room owner, so an author line can tell whether the member it is
    // flagging holds privilege themselves — see `privilege_in_view`.
    owner_id: MemberId,
    clock: MessageClock<'_>,
) -> Vec<DisplayItem> {
    let mut items: Vec<DisplayItem> = Vec::new();
    let group_threshold = Duration::from_secs(5 * 60); // 5 minutes

    // Inputs to the per-message HTML cache (see MESSAGE_HTML_CACHE). The member
    // fingerprint is computed once per render, not per message.
    let members_fp = member_names_fingerprint(member_names);
    let mut seen_message_ids: std::collections::HashSet<MessageId> =
        std::collections::HashSet::new();

    // Only iterate over displayable messages (non-deleted, non-action)
    for message in messages_state.display_messages() {
        let author_id = message.message.author;
        let message_id = message.id();
        let raw_time = DateTime::<Utc>::from(message.message.time);
        // Clamp target: when we first saw it, else the pass-wide "now".
        let clamp_to = clock.clamp_target(&message_id);
        // `checked_add_signed` rather than `+`: `clamp_to` can come from
        // persisted storage, and chrono's `Add` panics at the end of its range.
        let time_clamped = clamp_to
            .checked_add_signed(chrono::Duration::seconds(CLOCK_SKEW_TOLERANCE_SECS))
            .is_some_and(|limit| raw_time > limit);
        let message_time = if time_clamped { clamp_to } else { raw_time };

        let author_name = resolve_member_nickname(member_info, author_id, secrets);

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
                    summary.last_message_id = message_id;
                }
            } else {
                items.push(DisplayItem::Event(EventSummary {
                    names: vec![author_name],
                    id: msg_id_str,
                    last_time: message_time,
                    last_message_id: message_id,
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
        seen_message_ids.insert(message_id.clone());
        let content_html = render_message_html_cached(
            &message_id,
            &content_text,
            member_names,
            members_fp,
            self_member_id,
        );
        // With no locally-known identity nothing is "self", so every group
        // renders on the other-party side rather than claiming an author.
        let is_self = Some(author_id) == self_member_id;

        // Get edited status and reactions
        let edited = messages_state.is_edited(&message_id);
        let reactions = messages_state
            .reactions(&message_id)
            .cloned()
            .unwrap_or_default();

        // Resolve the reply quote (if any) against live room state.
        let reply_strip = resolve_reply_strip(
            &message.message.content,
            messages_state,
            member_info,
            secrets,
            member_names,
        );

        // Look up propagation delay (send time → receive time)
        let send_time_ms = raw_time.timestamp_millis();
        let receive_delay_secs =
            get_delay_secs_from(clock.receive_times, &message_id, send_time_ms);

        let grouped_message = GroupedMessage {
            content_text: content_text.clone(),
            content_html,
            time: message_time,
            time_clamped,
            id: format!("{:?}", message_id.0),
            message_id,
            edited,
            reactions,
            reply_strip,
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
            // Once per GROUP, not once per message: consecutive messages from
            // one author share a header, so the warning is a property of the
            // group.
            //
            // `author_id` is the id of the member whose NAME is on this line.
            // Passing `self_member_id` compiles and passes every behavioural
            // test while putting ⚠ on the genuine owner and every genuine
            // moderator; the argument is pinned by
            // `impersonation_warning_is_wired_into_every_render_surface`.
            let author_impersonation = impersonation_warning_for_display(
                impersonation,
                author_id,
                &author_name,
                privilege_in_view(author_id, owner_id, deputy_badges),
            );
            items.push(DisplayItem::Messages(MessageGroup {
                author_id,
                author_name,
                author_badge: deputy_badges.get(&author_id).cloned(),
                author_impersonation,
                is_self,
                first_time: message_time,
                time_clamped,
                first_delay_secs: receive_delay_secs,
                messages: vec![grouped_message],
            }));
        }
    }

    // Evict cached HTML for messages that are no longer visible so the cache
    // stays bounded to the current room's message set.
    prune_message_html_cache(&seen_message_ids);

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

/// The message body's readable text, or `None` when the body is private and
/// cannot be read at all — either its secret version is missing, or the
/// secret we hold for that version does not decrypt it.
///
/// [`decrypt_message_content`] answers the same question but substitutes a
/// human-readable PLACEHOLDER in those cases, which is right for rendering
/// and wrong for anything that then INSPECTS the text: a mention scan over
/// `"[Encrypted message: 42 bytes, v2]"` finds no mention and reads as a
/// confident "not a mention". Callers that need to tell "no mention" from
/// "could not look" must use this (freenet/river#500).
pub(crate) fn try_decrypt_message_content(
    content: &RoomMessageBody,
    secrets: &HashMap<u32, [u8; 32]>,
) -> Option<String> {
    use river_core::room_state::content::{
        ReplyContentV1, TextContentV1, CONTENT_TYPE_REPLY, CONTENT_TYPE_TEXT,
    };

    match content {
        // A public body has no secret to be missing, so it is never
        // "unreadable" in the sense this function is about. (A corrupt or
        // forward-incompatible public payload still yields
        // `to_string_lossy()`'s placeholder rather than `None` — nothing can
        // make that body readable later, so there is no transient state to
        // distinguish.)
        RoomMessageBody::Public { .. } => Some(decrypt_message_content(content, secrets)),
        RoomMessageBody::Private {
            content_type,
            ciphertext,
            nonce,
            secret_version,
            ..
        } => {
            use crate::util::ecies::decrypt_with_symmetric_key;
            let secret = secrets.get(secret_version)?;
            let plaintext =
                decrypt_with_symmetric_key(secret, ciphertext.as_slice(), nonce).ok()?;
            if *content_type == CONTENT_TYPE_TEXT {
                if let Ok(text_content) = TextContentV1::decode(&plaintext) {
                    return Some(text_content.text);
                }
            }
            if *content_type == CONTENT_TYPE_REPLY {
                if let Ok(reply) = ReplyContentV1::decode(&plaintext) {
                    return Some(reply.text);
                }
            }
            Some(String::from_utf8_lossy(&plaintext).to_string())
        }
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
        RoomMessageBody::Private { secret_version, .. } => {
            // The read itself lives in `try_decrypt_message_content`, so there
            // is ONE definition of "what does this body say". Everything below
            // is the placeholder to render when it says nothing.
            if let Some(text) = try_decrypt_message_content(content, secrets) {
                return text;
            }
            if secrets.contains_key(secret_version) {
                // We hold a secret at this version and it did not decrypt the
                // body — a wrong key, which `repopulate_secrets_from_state`
                // overwrites from the owner-signed blob when it arrives.
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
    use river_core::mention::{parse_segments, MentionSegment};

    // Mirrors `river_core::mention::render_plaintext`, except the fallback
    // display name — the `[name]` snapshot the SENDER embedded in the message
    // — is sanitised. That snapshot is arbitrary attacker text, so an
    // unsanitised fallback would let `@[Alice 🛡](rv:…)` paint a shield into a
    // reply preview for a member id nobody in the room resolves. Names that DO
    // resolve come from `member_names`, which is already sanitised at source.
    let mut with_mentions = String::with_capacity(text.len());
    for seg in parse_segments(text) {
        match seg {
            MentionSegment::Text(t) => with_mentions.push_str(&t),
            MentionSegment::Mention(m) => {
                let name = member_names
                    .iter()
                    .find(|(id, _)| m.member_ref.matches(**id))
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| sanitize_display_name(&m.display_name));
                with_mentions.push('@');
                with_mentions.push_str(&name);
            }
        }
    }
    strip_markdown(&with_mentions)
}

/// Reduce markdown to its plain-text content (emphasis/headings/code-fences
/// removed, links rendered as their visible text). Used for the single-line
/// reply-preview snapshot, never for the message body (which renders full
/// markdown). Falls back to the input unchanged if parsing fails.
fn strip_markdown(text: &str) -> String {
    // The preview is truncated anyway, so parse at most a bounded prefix.
    let text = truncate_to_char_boundary(text, MARKDOWN_MAX_SOURCE_BYTES);
    if !markdown_cost_is_bounded(text) {
        return text.to_string();
    }
    match markdown::to_mdast(text, &markdown::ParseOptions::gfm()) {
        Ok(node) => {
            let mut out = String::with_capacity(text.len());
            collect_mdast_text(&node, &mut out);
            drop_mdast(node);
            out
        }
        Err(_) => text.to_string(),
    }
}

/// Depth-first collection of the visible text from a markdown AST node.
///
/// Iterative, not recursive: nesting depth is chosen by whoever wrote the
/// text, and a recursive walk of a deep tree overflows the stack.
fn collect_mdast_text(root: &markdown::mdast::Node, out: &mut String) {
    use markdown::mdast::Node;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
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
            stack.extend(children.iter().rev());
        }
    }
}

/// Drop a markdown AST without recursing: the tree's own `Drop` recurses once
/// per nesting level, which overflows the stack on deeply nested text.
fn drop_mdast(root: markdown::mdast::Node) {
    let mut stack = vec![root];
    while let Some(mut node) = stack.pop() {
        if let Some(children) = node.children_mut() {
            stack.append(children);
        }
    }
}

/// The longest prefix of `text` that is at most `max` bytes and ends on a
/// char boundary.
fn truncate_to_char_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The id of the message this reply quotes, or `None` if it is not a reply (or
/// cannot be decoded).
///
/// Deliberately returns ONLY the id. `ReplyContentV1` also carries
/// `target_author_name` and `target_content_preview` — a snapshot written and
/// signed by the REPLIER and validated by nothing — and the whole point of
/// `resolve_reply_strip` is that neither is ever rendered. Not returning them
/// makes that structural instead of a convention every caller has to honour.
pub(crate) fn extract_reply_target_id(
    content: &RoomMessageBody,
    secrets: &HashMap<u32, [u8; 32]>,
) -> Option<MessageId> {
    use river_core::room_state::content::{ReplyContentV1, CONTENT_TYPE_REPLY};

    match content {
        RoomMessageBody::Public {
            content_type, data, ..
        } if *content_type == CONTENT_TYPE_REPLY => ReplyContentV1::decode(data)
            .ok()
            .map(|r| r.target_message_id),
        RoomMessageBody::Private {
            content_type,
            ciphertext,
            nonce,
            secret_version,
            ..
        } if *content_type == CONTENT_TYPE_REPLY => {
            use crate::util::ecies::decrypt_with_symmetric_key;
            secrets
                .get(secret_version)
                .and_then(|secret| {
                    decrypt_with_symmetric_key(secret, ciphertext.as_slice(), nonce).ok()
                })
                .and_then(|plaintext| ReplyContentV1::decode(&plaintext).ok())
                .map(|r| r.target_message_id)
        }
        _ => None,
    }
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
    if !markdown_cost_is_bounded(text) {
        return plain_text_to_html(text);
    }

    // Convert single newlines to hard breaks (two spaces + newline)
    // This preserves line breaks in chat messages as users expect
    let with_hard_breaks = text.replace("\n", "  \n");

    markdown_to_html(&with_hard_breaks, text, behind_gateway)
}

/// Longest text rendered as markdown.
const MARKDOWN_MAX_SOURCE_BYTES: usize = 4096;

/// Most block containers (`>`, `-`, `1.` ...) opened at the start of one line.
const MARKDOWN_MAX_LINE_NESTING: usize = 16;

/// Most `|` on one line, which bounds the columns of a table.
const MARKDOWN_MAX_LINE_PIPES: usize = 32;

/// Longest HTML kept from rendering markdown (see `markdown_to_html`).
const MARKDOWN_MAX_HTML_BYTES: usize = 64 * 1024;

/// Whether `text` is cheap enough to parse as markdown. The parser's time
/// grows faster than linearly with nesting depth, and a table's output with
/// columns times rows, so text past these limits is shown as plain text.
/// Depth is counted per line. Nesting built up across lines needs growing
/// indentation, which the size limit bounds to a few hundred levels (a few
/// ms to parse at the default 1000-byte message size, tens of ms at the size
/// limit). Only DMs and rooms that raised `max_message_size` above the
/// default can carry text past the size limit, which is large enough for the
/// longest valid share link (see `longest_valid_bare_link_still_converts`).
fn markdown_cost_is_bounded(text: &str) -> bool {
    if text.len() > MARKDOWN_MAX_SOURCE_BYTES {
        return false;
    }
    // Characters that can hide a marker from the count below but not from
    // the parser: a leading byte order mark, which the parser skips, and the
    // sentinels `extract_bare_freenet_links` may drop before parsing.
    let ignored = ['\u{feff}', BARE_LINK_OPEN, BARE_LINK_CLOSE];
    let text: std::borrow::Cow<str> = if text.contains(ignored) {
        text.replace(ignored, "").into()
    } else {
        text.into()
    };
    // `\r` alone also ends a line in markdown.
    text.split(['\n', '\r']).all(|line| {
        line_container_depth(line) <= MARKDOWN_MAX_LINE_NESTING
            && line.bytes().filter(|&b| b == b'|').count() <= MARKDOWN_MAX_LINE_PIPES
    })
}

/// How many block quote, list item or footnote definition markers open at
/// the start of `line` (an over-count is fine: it only makes plain text more
/// likely).
fn line_container_depth(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut depth = 0;
    loop {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        let marker_end = match bytes.get(i) {
            Some(b'>') => i + 1,
            // A GFM footnote definition, `[^label]: `.
            Some(b'[') if bytes.get(i + 1) == Some(&b'^') => match line[i..].find("]:") {
                Some(end) => {
                    depth += 1;
                    i += end + 2;
                    continue;
                }
                None => return depth,
            },
            Some(b'-' | b'*' | b'+') => i + 1,
            Some(b'0'..=b'9') => {
                let mut j = i;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                match bytes.get(j) {
                    Some(b'.' | b')') => j + 1,
                    _ => return depth,
                }
            }
            _ => return depth,
        };
        // A list marker must be followed by whitespace or the line's end.
        let is_quote = bytes[i] == b'>';
        if !is_quote && !matches!(bytes.get(marker_end), None | Some(b' ' | b'\t')) {
            return depth;
        }
        depth += 1;
        i = marker_end;
    }
}

/// Render a room description: markdown, or plain text when
/// [`markdown_cost_is_bounded`] says no.
fn description_to_html(text: &str, behind_gateway: bool) -> String {
    if !markdown_cost_is_bounded(text) {
        plain_text_to_html(text)
    } else {
        markdown_to_html(text, text, behind_gateway)
    }
}

/// Text not rendered as markdown: escaped, with each line break kept.
fn plain_text_to_html(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    format!("<p>{}</p>", escape_html(&text).replace('\n', "<br />\n"))
}

/// Convert markdown text to HTML with clickable links that open in new tabs.
///
/// `plain` is what to show as plain text if the HTML comes out too long: the
/// text as the user wrote it, before any markdown-specific rewriting.
fn markdown_to_html(original_text: &str, plain: &str, behind_gateway: bool) -> String {
    // GFM only autolinks http(s)/www text, so a bare `freenet:<id>` share link
    // would stay plain text. Pull valid ones out BEFORE markdown runs, so
    // emphasis/strikethrough syntax inside a link (`…/a*b*c`) cannot split it,
    // and put them back as anchors afterwards; turning an anchor into a link to
    // the reader's node is `finalize_anchors`' job, the same as for every other
    // share link. Only behind a gateway, because without one there is no node
    // to link to (see `finalize_anchors`).
    let (text, bare_links) = if behind_gateway {
        extract_bare_freenet_links(original_text)
    } else {
        (std::borrow::Cow::Borrowed(original_text), Vec::new())
    };

    // Some shapes (many references to a definition with a long title, a
    // table with many rows) expand a short message into megabytes, so HTML
    // past `MARKDOWN_MAX_HTML_BYTES` is dropped for plain text before any
    // further work is done on it.
    let too_long = |html: &String| html.len() > MARKDOWN_MAX_HTML_BYTES;
    let rendered = render_gfm(&text);
    if too_long(&rendered) {
        return plain_text_to_html(plain);
    }
    let html = if bare_links.is_empty() {
        rendered
    } else {
        // The source scan and the render parse slightly different text (a
        // sentinel in place of each link), so in rare shapes they disagree
        // about what is prose, e.g. a link's own `(` inside a link
        // destination. If any link did not come back exactly once, in prose,
        // or a sentinel survived anywhere, render the original text with no
        // bare-link pass rather than show a mangled message.
        match restore_bare_freenet_links(&rendered, &bare_links) {
            Some(html) => html,
            None => render_gfm(original_text),
        }
    };
    if too_long(&html) {
        return plain_text_to_html(plain);
    }

    let html = finalize_anchors(&html, behind_gateway);
    if too_long(&html) {
        return plain_text_to_html(plain);
    }
    html
}

/// Convert markdown to HTML using GFM mode, which includes autolink literals
/// that correctly handle code spans, existing links, etc.
fn render_gfm(text: &str) -> String {
    markdown::to_html_with_options(text, &markdown::Options::gfm())
        .unwrap_or_else(|_| markdown::to_html(text))
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
/// in the map). `self_member_id` gets a distinct highlight (a mention of you);
/// `None` (no locally-known identity) simply means no chip is self-highlighted.
pub(crate) fn message_to_html_with_mentions(
    text: &str,
    member_names: &HashMap<MemberId, String>,
    self_member_id: Option<MemberId>,
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
                // The `[name]` snapshot is sender-controlled text, so an
                // unresolved mention must be sanitised before it becomes a
                // chip — otherwise `@[Admin 🛡](rv:…)` paints a shield inside
                // a message. Resolved names come from `member_names`, which is
                // already sanitised at source.
                let name = resolved
                    .and_then(|id| member_names.get(&id).cloned())
                    .unwrap_or_else(|| sanitize_display_name(&m.display_name));
                chips.push(render_mention_chip_html(
                    resolved,
                    &name,
                    // `self_member_id.is_some_and(..)` rather than
                    // `resolved == self_member_id`: the latter would call an
                    // UNRESOLVED mention a mention of you when the local
                    // identity is also unknown (`None == None`).
                    self_member_id.is_some_and(|me| resolved == Some(me)),
                ));
                working.push(OPEN);
                working.push_str(&idx.to_string());
                working.push(CLOSE);
            }
        }
    }

    let mut html = message_to_html(&working);
    // Markdown can move a placeholder into a tag (a link title) or copy it
    // (a reference definition's title, used many times). Substituting there
    // would put chip markup inside an attribute or multiply it, so unless
    // every placeholder comes back exactly once, in text, show the message
    // as plain text, where each one does.
    if !mention_placeholders_are_sound(&html, chips.len(), OPEN, CLOSE) {
        html = plain_text_to_html(&working);
    }
    // The CLOSE delimiter bounds each index, so `…0␁` never matches inside
    // `…10␁` — replacement is unambiguous regardless of order.
    for (idx, chip) in chips.iter().enumerate() {
        html = html.replace(&format!("{OPEN}{idx}{CLOSE}"), chip);
    }
    html
}

/// Whether each of the `count` mention placeholders (`{open}{index}{close}`)
/// appears exactly once in `html`, and only in text, never inside a tag. The
/// markdown crate encodes `>` inside attributes, so the first `>` always ends
/// a tag.
fn mention_placeholders_are_sound(html: &str, count: usize, open: char, close: char) -> bool {
    let mut seen = vec![0usize; count];
    let mut in_tag = false;
    for (i, c) in html.char_indices() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if c == open => {
                if in_tag {
                    return false;
                }
                let rest = &html[i + open.len_utf8()..];
                let Some(end) = rest.find(close) else {
                    return false;
                };
                match rest[..end].parse::<usize>() {
                    Ok(idx) if idx < count => seen[idx] += 1,
                    _ => return false,
                }
            }
            _ => {}
        }
    }
    seen.iter().all(|&n| n == 1)
}

thread_local! {
    /// Per-message rendered-HTML cache for message bodies.
    ///
    /// The `message_groups` memo rebuilds the *entire* visible message list
    /// whenever `ROOMS` changes — which is on every sync tick, incoming
    /// message, reaction, and DM. Rendering one body runs a full markdown
    /// parse + HTML serialize + mention/anchor rewrite (tens of µs natively,
    /// several-fold more in WASM on a mobile CPU). Re-parsing all
    /// `max_recent_messages` (default 100) bodies on every update was a major
    /// source of the mobile jank users reported ("really slows down the mobile
    /// browser").
    ///
    /// This cache keys each body's HTML by `MessageId` plus a fingerprint of
    /// every input that affects the output, so steady-state message flow
    /// re-parses only the one new/changed message instead of the whole
    /// history. It is single-threaded (WASM) / per-thread (native tests) and
    /// is pruned each render (`prune_message_html_cache`) to the currently
    /// visible set, so it stays bounded to the current room's messages.
    static MESSAGE_HTML_CACHE: RefCell<HashMap<MessageId, (u64, String)>> =
        RefCell::new(HashMap::new());
}

/// Order-independent fingerprint of the member-name map (mention chips resolve
/// to members' *current* nicknames, so a rename must invalidate any cached body
/// that renders a chip). Combining per-entry hashes with `wrapping_add` makes
/// the result independent of `HashMap` iteration order, so identical logical
/// content always fingerprints identically. `len()` is folded in to
/// distinguish an empty map from an all-zero-hash one.
fn member_names_fingerprint(member_names: &HashMap<MemberId, String>) -> u64 {
    let mut acc: u64 = member_names.len() as u64;
    for (id, name) in member_names {
        let mut h = DefaultHasher::new();
        id.hash(&mut h);
        name.hash(&mut h);
        acc = acc.wrapping_add(h.finish());
    }
    acc
}

/// Fingerprint of every input that determines a message body's rendered HTML.
///
/// The body depends on the effective `text` always, and — only when the text
/// can carry a mention (`rv:` reference scheme present) — on the member-name
/// map (chip nicknames) and the local member id (self-mention highlight).
/// `running_behind_freenet_gateway()` is constant per session, so it is not
/// part of the key. Gating the member inputs on the presence of a mention
/// means an ordinary (mention-free) message survives member joins/renames in
/// the cache, while a message that renders a chip is invalidated by them.
fn message_html_fingerprint(text: &str, members_fp: u64, self_member_id: Option<MemberId>) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    if text.contains(river_core::mention::REF_SCHEME) {
        members_fp.hash(&mut h);
        self_member_id.hash(&mut h);
    }
    h.finish()
}

/// Cached wrapper around [`message_to_html_with_mentions`]. Returns the cached
/// HTML on a fingerprint match, otherwise renders it and stores it. The output
/// is always byte-identical to calling `message_to_html_with_mentions` directly
/// with the current inputs — the cache only skips redundant re-rendering.
fn render_message_html_cached(
    message_id: &MessageId,
    text: &str,
    member_names: &HashMap<MemberId, String>,
    members_fp: u64,
    self_member_id: Option<MemberId>,
) -> String {
    let fp = message_html_fingerprint(text, members_fp, self_member_id);
    MESSAGE_HTML_CACHE.with(|cache| {
        if let Some((cached_fp, html)) = cache.borrow().get(message_id) {
            if *cached_fp == fp {
                return html.clone();
            }
        }
        let html = message_to_html_with_mentions(text, member_names, self_member_id);
        cache
            .borrow_mut()
            .insert(message_id.clone(), (fp, html.clone()));
        html
    })
}

/// Drop cached entries for messages no longer visible (e.g. after a room
/// switch or history trim), keeping the cache bounded to the current room's
/// message set.
fn prune_message_html_cache(seen: &std::collections::HashSet<MessageId>) {
    MESSAGE_HTML_CACHE.with(|cache| {
        cache.borrow_mut().retain(|id, _| seen.contains(id));
    });
}

/// Test-only: reset the cache so each test observes a clean cache regardless of
/// thread reuse across the test binary.
#[cfg(test)]
fn clear_message_html_cache() {
    MESSAGE_HTML_CACHE.with(|cache| cache.borrow_mut().clear());
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
/// `/v1/contract/web/` (or `/v2/…`), which is what gateway hosting looks like to the
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
        .map(|p| p.starts_with("/v1/contract/web/") || p.starts_with("/v2/contract/web/"))
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
        let Some(close_pos) = matching_anchor_close(after_open) else {
            out.push_str(tag);
            return out;
        };
        // The markdown crate still emits an anchor for an angle-bracket
        // autolink inside a link label (`[<x:y>](url)`), i.e. a nested `<a>`.
        // Browsers split those into two anchors, so the inner one would escape
        // this function (no target/rel, and an empty href that reloads River
        // in place). Flatten it to its text: the outer link is what is shown.
        let flattened;
        let inner = if after_open[..close_pos].contains("<a ") {
            flattened = strip_anchor_tags(&after_open[..close_pos]);
            flattened.as_str()
        } else {
            &after_open[..close_pos]
        };
        let tail = &after_open[close_pos + 4..];

        let opening = opening.replacen(
            "<a href=\"",
            "<a target=\"_blank\" rel=\"noopener noreferrer\" href=\"",
            1,
        );
        let original_href = extract_href(&opening);

        // A share link (freenet.org/open or `freenet:`) opens the named webapp
        // on the reader's own node. The destination is derived ONLY from the
        // visible text, never from the hidden href, and the text is kept as
        // it is (full contract id included): the link goes exactly where it
        // says, so `[freenet:<A>](https://freenet.org/open#<B>)` opens A, and
        // a share link hidden behind a label is left as a plain link to the
        // freenet.org page, which shows the id before anything opens. (Pasted
        // gateway URLs, `http://…/v1/contract/web/<id>/…`, are a separate,
        // older rewrite below, which does apply to labelled links.)
        if rewrite_freenet_hrefs {
            if let Some(target) = share_link_in_anchor_text(inner) {
                // Rebuilt from scratch rather than edited, so nothing else
                // from the message survives on the anchor: a `title` naming
                // some other link would otherwise show as its tooltip.
                out.push_str(&format!(
                    "<a target=\"_blank\" rel=\"noopener noreferrer\" href=\"{}\">",
                    escape_html_attr(&target.local_path())
                ));
                out.push_str(inner);
                out.push_str("</a>");
                rest = tail;
                continue;
            }
        }

        // A link whose href opens a contract on the reader's node (a gateway
        // URL, which the rewrite below makes same-origin, or a relative href,
        // which already is) must not carry a label that visibly names a
        // DIFFERENT contract: show such a label as plain text rather than as
        // a link that is not what it says. A bare pasted link (its text is
        // the URL) names its own destination and is exempt.
        if rewrite_freenet_hrefs {
            if let Some(dest_id) = node_destination(original_href.as_deref()) {
                // A pasted absolute URL (its text IS the href) shows where it
                // goes, host and contract id first, so its label is not
                // checked (a River invite's long `?invitation=` code, or an
                // app path holding other ids, is not a claim). Unless its
                // destination is unknown AND something in it moves where it
                // lands unreadably (a dot segment, a percent-escape).
                let bare =
                    original_href.as_deref() == Some(inner) && has_scheme_and_authority(inner);
                // A tooltip is label text too.
                let title = extract_attr(&opening, "title");
                let unlink = if bare {
                    (dest_id.is_none() && !plain_absolute_url(inner))
                        || pasted_url_poses_before_its_path(inner)
                        || title.as_deref().is_some_and(|t| {
                            label_contradicts_destination(t, None, dest_id.clone())
                        })
                } else {
                    label_contradicts_destination(inner, title.as_deref(), dest_id)
                };
                if unlink {
                    out.push_str(inner);
                    rest = tail;
                    continue;
                }
            }
        }
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

/// If an anchor's visible text is exactly a valid share link, return it.
///
/// `inner` is the anchor's inner HTML as the markdown crate emits it: text is
/// entity-encoded (`&amp;`, `&lt;`, `&gt;`, `&quot;`), and any nested markup
/// (emphasis, a mention sentinel's chip, ...) means it is not a bare link.
fn share_link_in_anchor_text(inner: &str) -> Option<crate::util::share_link::ShareTarget> {
    // Defense in depth: the validator refuses `<` and `>` anyway, but nested
    // markup is never a bare link, so do not even decode it.
    if inner.contains('<') {
        return None;
    }
    crate::util::share_link::parse_share_link(&decode_html_text(inner))
}

/// The contract a link opens on the reader's node, if it opens one there:
/// `Some(Some(id))` for a gateway URL on any host (the rewrite below makes it
/// same-origin) or a relative `/v1|v2/contract/web/<id>` path; `Some(None)`
/// for any other relative href (it resolves against River's own URL, e.g.
/// `../<id>/`, so where it lands is not worth guessing); `None` for a link
/// that goes elsewhere.
fn node_destination(href: Option<&str>) -> Option<Option<String>> {
    let href = href?;
    // Protocol-relative `//host/…` takes the page's scheme.
    let absolute = match href.strip_prefix("//") {
        Some(rest) => Some(format!("http://{rest}")),
        None => has_scheme_and_authority(href).then(|| href.to_string()),
    };
    if let Some(absolute) = absolute {
        if let Some(parsed) = parse_freenet_web_url(&absolute) {
            return Some(Some(parsed.contract_id.to_string()));
        }
        // A contract path that does not parse (a dot segment, an encoded id
        // character, …) may still resolve to SOME contract on a node; which
        // one is not worth guessing.
        return absolute.contains("/contract/web/").then_some(None);
    }
    // `http:/path` and `http:../x` (a special scheme with no `//`) resolve
    // against the page when it has that scheme, exactly like a relative href.
    let relative = ["http:", "https:"]
        .iter()
        .find_map(|scheme| {
            href.get(..scheme.len())
                .filter(|p| p.eq_ignore_ascii_case(scheme))
                .map(|_| &href[scheme.len()..])
        })
        .or_else(|| is_relative_href(href).then_some(href))?;
    // A dot segment (literal or encoded) moves the path when the browser
    // resolves it, so the first segment would not be the destination.
    let path = relative.split(['?', '#']).next().unwrap_or("");
    if path.split('/').any(crate::util::share_link::is_dot_segment) {
        return Some(None);
    }
    let id = ["/v1/contract/web/", "/v2/contract/web/"]
        .iter()
        .find_map(|m| relative.strip_prefix(m))
        .map(|after| {
            after
                .split(['/', '?', '#'])
                .next()
                .unwrap_or("")
                .to_string()
        })
        .filter(|id| !id.is_empty());
    Some(id)
}

/// True if an anchor's visible text names a contract other than `dest_id`
/// (or names any contract, when the destination is unknown).
///
/// Best effort, as defence in depth for links River did not write: what counts
/// as naming a contract is judged on what the reader sees
/// ([`crate::util::confusable::visual_ascii`] of the decoded text, tags
/// dropped but image `alt` kept), so invisible characters, character
/// references, fancy text and homoglyphs inside an id do not hide it. A claim
/// is a run of 32+ ASCII letters/digits with an uppercase letter (a contract
/// id is 43-44; a hex hash or an ordinary word is not), or an id-looking run
/// of 8+ directly after a `freenet:` marker with no space (a shortened id
/// such as `freenet:UDzGbcWr`). Each claim must be a prefix of the destination id,
/// and the label's tooltip and any image tooltip inside it are read too. An id
/// broken up by a visible ASCII space or punctuation is not recognised
/// (freenet/river#736); a label naming no contract (`[River update](…)`)
/// claims nothing, like any other link text.
fn label_contradicts_destination(
    inner: &str,
    title: Option<&str>,
    dest_id: Option<String>,
) -> bool {
    // The label, the link's tooltip, and any tooltip of an image inside it.
    let mut claims = label_claimed_ids(inner);
    let img_titles = inner
        .split("<img")
        .skip(1)
        .filter_map(|rest| extract_attr(&format!("<img{}", rest.split('>').next()?), "title"));
    for text in title.map(str::to_string).into_iter().chain(img_titles) {
        claims.extend(label_claimed_ids(&text));
    }
    if claims.is_empty() {
        return false;
    }
    let Some(dest_id) = dest_id else { return true };
    claims
        .iter()
        .any(|claim| !dest_id.starts_with(claim.as_str()))
}

/// The contract-id claims in an anchor's visible text. See
/// [`label_contradicts_destination`].
fn label_claimed_ids(inner: &str) -> Vec<String> {
    // Anything that can split an id without the reader seeing a break must
    // not (the whitespace and joining rules below). This is a comparison, not
    // rendering, so joining too much can only unlink more, never link
    // something new.
    let decoded = decode_html_text(&visible_text_with_alt(inner));
    // Visible, full-width spaces separate, like an ASCII space. Every other
    // whitespace character is dropped: line and paragraph breaks (an id
    // wrapping looks the same), narrow and hair spaces, and controls a
    // browser draws with no width.
    let visible_space = |c: char| {
        matches!(
            c,
            ' ' | '\t' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
                ..='\u{2005}' | '\u{2007}' | '\u{2008}' | '\u{3000}'
        )
    };
    let spaced: String = decoded
        .chars()
        .filter(|&c| !c.is_whitespace() || visible_space(c))
        .collect();
    // Colon look-alikes read as the `:` of a `freenet:` marker.
    let folded: Vec<char> = crate::util::confusable::visual_ascii(&spaced)
        .chars()
        .map(|c| match c {
            '\u{02D0}' | '\u{02F8}' | '\u{0589}' | '\u{05C3}' | '\u{0903}' | '\u{0A83}'
            | '\u{1361}' | '\u{1804}' | '\u{205A}' | '\u{2236}' | '\u{A4FD}' | '\u{A789}'
            | '\u{FE13}' | '\u{FE30}' | '\u{FE55}' => ':',
            other => other,
        })
        .collect();
    // A run of non-ASCII characters left between two ASCII letters/digits
    // after the visual fold is kept IN the id run as one placeholder (which
    // can never be a prefix of an ASCII id): an unfolded homoglyph, a lone
    // symbol look-alike, or any number of zero-width marks stacked on a
    // letter. It separates, as it looks, only if it holds a space,
    // punctuation, an ellipsis, a full-width character (CJK, kana, Hangul,
    // Yi, fullwidth forms, emoji), or a real word of a script written without
    // spaces (Thai, Lao, Myanmar, Khmer). This errs towards joining: joining
    // can only unlink more, never link something new.
    const JOINED: char = '\u{FFFD}';
    let separates = |c: char| {
        // Combining marks inside the full-width blocks still stack, and a few
        // characters there read as a Latin letter (`⸦`/`⸧` as C, `〇` and the
        // Hangul compatibility jamo such as `ㅇ` as o; real Korean text uses
        // syllables, U+AC00 on): these join like any look-alike.
        let exempt = matches!(u32::from(c),
            0x302A..=0x302F | 0x3099..=0x309A | 0x2E26 | 0x2E27 | 0x3007 | 0x3130..=0x318F);
        c.is_whitespace()
            || !exempt
                && matches!(u32::from(c),
                0x2000..=0x206F      // general punctuation
                | 0x22EE..=0x22F1    // ellipses
                | 0x2E00..=0x2E7F    // supplemental punctuation
                | 0x2E80..=0x30FF    // CJK radicals, symbols, kana
                | 0x3130..=0x9FFF    // Hangul compatibility, CJK unified
                | 0xA000..=0xA4CF    // Yi
                | 0xAC00..=0xD7FF    // Hangul
                | 0xF900..=0xFAFF    // CJK compatibility
                | 0xFE30..=0xFE4F    // CJK compatibility forms
                | 0xFF00..=0xFFEF    // halfwidth / fullwidth forms left unfolded
                | 0x1F000..=0x1FAFF  // emoji and pictographs
                | 0x20000..=0x3FFFF) // CJK extensions
    };
    // Letters (not marks) of the scripts written without spaces.
    let no_space_letter = |c: char| {
        let cp = u32::from(c);
        let block = matches!(cp, 0x0E00..=0x0EFF | 0x1000..=0x109F | 0x1780..=0x17FF);
        let mark = matches!(cp,
            0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E              // Thai
            | 0x0EB1 | 0x0EB4..=0x0EBC | 0x0EC8..=0x0ECE           // Lao
            | 0x102B..=0x103E | 0x1056..=0x1059 | 0x105E..=0x1060  // Myanmar
            | 0x1062..=0x1064 | 0x1067..=0x106D | 0x1071..=0x1074
            | 0x1082..=0x108D | 0x108F | 0x109A..=0x109D
            | 0x17B4..=0x17D3 | 0x17DD); // Khmer
        block && !mark
    };
    let mut text = String::with_capacity(folded.len());
    let mut k = 0;
    while k < folded.len() {
        let c = folded[k];
        if c.is_ascii() {
            text.push(c);
            k += 1;
            continue;
        }
        let run_end = (k..folded.len())
            .find(|&e| folded[e].is_ascii())
            .unwrap_or(folded.len());
        let run = &folded[k..run_end];
        let next_is_alnum = folded
            .get(run_end)
            .is_some_and(|n| n.is_ascii_alphanumeric());
        // (Bopomofo, U+3100-312F, is deliberately not a separator: a lone
        // `ㄚ` reads as Y.)
        let joins = !run.iter().any(|&c| separates(c))
            && run.iter().filter(|&&c| no_space_letter(c)).count() <= 3;
        let flanked = k > 0 && folded[k - 1].is_ascii_alphanumeric() && next_is_alnum && joins;
        // Marks sitting on a marker's `:` or `/` (`freenet:\u{05BC}<id>`)
        // are dropped, so the marker still sits right before the id.
        let on_marker = k > 0 && matches!(folded[k - 1], ':' | '/') && next_is_alnum && joins;
        if flanked {
            text.push(JOINED);
        } else if !on_marker {
            text.push(' ');
        }
        k = run_end;
    }
    let lower = text.to_ascii_lowercase();
    // Runs are over ASCII letters and digits, not just base58: `O`, `0`, `I`
    // and `l` read as part of an id, and a run containing one can never be a
    // prefix of a real id.
    let in_run = |c: char| c.is_ascii_alphanumeric() || c == JOINED;
    let mut claims = Vec::new();
    let flush = |start: usize, end: usize, claims: &mut Vec<String>| {
        let run = &text[start..end];
        let len = run.chars().count();
        let has_upper = run.chars().any(|c| c.is_ascii_uppercase());
        // A SHORT run counts only right after a `freenet:` marker with no
        // space (`freenet:UDzGbcWr`, `freenet://…`, the shapes River itself
        // displays), 8+ long and looking like an id (a digit, or a capital
        // after the first character), so "Freenet: GitHub mirror" is prose.
        let id_like_short = run.chars().any(|c| c.is_ascii_digit() || c == JOINED)
            || run.chars().skip(1).any(|c| c.is_ascii_uppercase());
        let after_marker = lower[..start].trim_end_matches('/').ends_with("freenet:");
        if (len >= 32 && has_upper) || (len >= 8 && after_marker && id_like_short) {
            claims.push(run.to_string());
        }
    };
    let mut run_start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        match (in_run(c), run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(start)) => {
                flush(start, i, &mut claims);
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run_start {
        flush(start, text.len(), &mut claims);
    }
    claims
}

/// True for `scheme://…` (RFC 3986 scheme syntax), i.e. an absolute URL with
/// an authority. A `://` later in the href (in a query or fragment) does not
/// count, so `../x/#://` stays relative.
fn has_scheme_and_authority(href: &str) -> bool {
    let Some(colon) = href.find("://") else {
        return false;
    };
    let scheme = &href[..colon];
    scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// True if a pasted URL's text before its contract path could be read as a
/// different destination: any userinfo (`http://freenet:<A>@host/…`), or a
/// contract id or `freenet:` claim in the host (`http://<A>.example/…`).
/// What follows the contract path is the app's own business (a River
/// invite's long code, an app path holding other ids) and is not read.
fn pasted_url_poses_before_its_path(url: &str) -> bool {
    let decoded = decode_html_text(url);
    let Some((_, after_scheme)) = decoded.split_once("://") else {
        return false;
    };
    let head_end = ["/v1/contract/web/", "/v2/contract/web/"]
        .iter()
        .filter_map(|m| after_scheme.find(m))
        .min()
        .unwrap_or_else(|| {
            after_scheme
                .find(['/', '?', '#'])
                .unwrap_or(after_scheme.len())
        });
    let head = &after_scheme[..head_end];
    head.contains('@') || !label_claimed_ids(head).is_empty()
}

/// True for an absolute `scheme://host/path` whose text is where it goes:
/// no percent-escapes and no dot segments in the path.
fn plain_absolute_url(url: &str) -> bool {
    if !has_scheme_and_authority(url) || url.contains('%') {
        return false;
    }
    let after_authority = url
        .split_once("://")
        .map(|(_, rest)| rest.find('/').map_or("", |p| &rest[p..]))
        .unwrap_or("");
    let path = after_authority.split(['?', '#']).next().unwrap_or("");
    !path.split('/').any(crate::util::share_link::is_dot_segment)
}

/// The raw (still entity-encoded) value of attribute `name` in a tag emitted
/// by the markdown crate, which always double-quotes values and encodes `"`
/// inside them.
fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let marker = format!(" {name}=\"");
    let start = tag.find(&marker)? + marker.len();
    let end = tag[start..].find('"')?;
    Some(tag[start..start + end].to_string())
}

/// An anchor's inner HTML reduced to what a reader sees: tags dropped, except
/// that an image contributes its `alt` text (which the browser shows when the
/// image does not load).
fn visible_text_with_alt(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find('<') {
        out.push_str(&rest[..pos]);
        let Some(end) = rest[pos..].find('>') else {
            rest = "";
            break;
        };
        let tag = &rest[pos..pos + end + 1];
        // No padding: a broken image shows its alt inline, and an empty one
        // shows nothing, so neither must split what the reader sees as one id.
        if tag.starts_with("<img") {
            if let Some(alt) = extract_attr(tag, "alt") {
                out.push_str(&alt);
            }
        }
        rest = &rest[pos + end + 1..];
    }
    out.push_str(rest);
    out
}

/// True for an href with no scheme and no authority, i.e. one the browser
/// resolves against River's own URL (including the empty href the markdown
/// crate emits for a scheme it does not allow).
fn is_relative_href(href: &str) -> bool {
    if href.starts_with("//") {
        return false;
    }
    let first_delimiter = href.find(['/', '?', '#']).unwrap_or(href.len());
    !href[..first_delimiter].contains(':')
}

/// Undo the markdown crate's text encoding (`&amp;`, `&lt;`, `&gt;`, `&quot;`)
/// in one pass, so `&amp;lt;` decodes to `&lt;`, not `<`. Anything else is
/// left verbatim.
fn decode_html_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let (decoded, len) = [
            ("&amp;", '&'),
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&quot;", '"'),
        ]
        .iter()
        .find(|(entity, _)| tail.starts_with(entity))
        .map(|(entity, c)| (*c, entity.len()))
        .unwrap_or(('&', 1));
        out.push(decoded);
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

/// Encode text content the way the markdown crate does (`&"<>`), so a text run
/// that is split around a new anchor re-encodes to exactly what it was.
fn encode_html_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Private-use sentinels that stand in for a bare `freenet:` link while
/// markdown runs (`{BARE_LINK_OPEN}<index>{BARE_LINK_CLOSE}`). Distinct from the
/// mention sentinels (`U+E000`/`U+E001`), and stripped from the input before
/// any are inserted, so a message cannot forge one.
const BARE_LINK_OPEN: char = '\u{E002}';
const BARE_LINK_CLOSE: char = '\u{E003}';

/// Longest whitespace-free run examined as a bare `freenet:` link candidate:
/// the longest link the validator can accept, plus slack for trailing
/// punctuation that trimming would remove.
const MAX_BARE_LINK_CANDIDATE_LEN: usize = crate::util::share_link::MAX_SHARE_LINK_LEN + 128;

/// Find each bare, valid `freenet:` share link in the markdown SOURCE and
/// replace it with a sentinel, returning the rewritten source and the links in
/// sentinel order. Messages with no `freenet:` at all are returned untouched.
///
/// A candidate starts at `freenet:` (any case) that does not follow a letter
/// or digit, runs to the next whitespace, and then loses trailing punctuation
/// exactly as the markdown crate's GFM autolink does (`!"'),.:;?_~*`, with `)`
/// only when unbalanced), so "see freenet:<id>." links the id without the full
/// stop. A `.` that follows `/` or `.` is kept, so trimming can never turn a
/// refused dot segment (`…/a/..`) into an accepted link (`…/a/`). Only a
/// candidate that passes the shared share-link validation is taken.
///
/// A candidate inside any construct whose text is not rendered as plain prose
/// (code, links and autolinks, images, raw HTML, definitions, footnote
/// references) is left alone. Those are found by parsing the source once more
/// to an mdast, which only happens when a valid candidate exists.
fn extract_bare_freenet_links(text: &str) -> (std::borrow::Cow<'_, str>, Vec<String>) {
    use std::borrow::Cow;
    const SCHEME: &str = "freenet:";
    if !text.to_ascii_lowercase().contains(SCHEME) {
        return (Cow::Borrowed(text), Vec::new());
    }
    let clean: String = text
        .chars()
        .filter(|c| *c != BARE_LINK_OPEN && *c != BARE_LINK_CLOSE)
        .collect();

    // ASCII lowercasing keeps byte offsets identical to `clean`.
    let lower = clean.to_ascii_lowercase();
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    let mut search_from = 0;
    // End of the whitespace-free run the current candidate sits in, cached so
    // `freenet:freenet:…` does not rescan the same run once per occurrence.
    let mut run_end = 0;
    while let Some(rel) = lower[search_from..].find(SCHEME) {
        let start = search_from + rel;
        search_from = start + SCHEME.len();
        if clean[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            continue;
        }
        if run_end <= start {
            // A `|` ends it too: tables split cells on it before inline
            // parsing, which this pass runs ahead of.
            run_end = clean[start..]
                .find(|c: char| c.is_whitespace() || c == '|')
                .map_or(clean.len(), |p| start + p);
        }
        // Message text is attacker-controlled, so bound the work per
        // candidate: anything longer than the longest valid link plus some
        // trailing punctuation cannot validate, so it is not even trimmed.
        if run_end - start > MAX_BARE_LINK_CANDIDATE_LEN {
            continue;
        }
        let mut token = &clean[start..run_end];
        let open_parens = token.matches('(').count();
        let mut close_parens = token.matches(')').count();
        while let Some(last) = token.chars().next_back() {
            let unbalanced_paren = last == ')' && close_parens > open_parens;
            let dot_in_segment = last == '.'
                && matches!(
                    token[..token.len() - 1].chars().next_back(),
                    Some('/' | '.')
                );
            // Non-ASCII punctuation (`。`, `」`, …) can never be part of a
            // valid link, so it is trimmed like ASCII sentence punctuation.
            let foreign_punctuation = !last.is_ascii() && !last.is_alphanumeric();
            if ("!\"',.:;?_~*".contains(last) && !dot_in_segment)
                || unbalanced_paren
                || foreign_punctuation
            {
                token = &token[..token.len() - last.len_utf8()];
                if last == ')' {
                    close_parens -= 1;
                }
            } else {
                break;
            }
        }
        if crate::util::share_link::parse_freenet_link(token).is_some() {
            candidates.push((start, start + token.len()));
            search_from = start + token.len();
        }
    }
    if candidates.is_empty() {
        return (Cow::Borrowed(text), Vec::new());
    }

    let mut excluded = non_prose_ranges(&clean);
    excluded.sort_unstable();
    let mut out = String::with_capacity(clean.len());
    let mut links = Vec::new();
    let mut cursor = 0;
    // Both lists are in source order and the excluded ranges do not nest
    // (the walk does not descend into an excluded node), so one sweep does.
    let mut next_excluded = 0;
    for (start, end) in candidates {
        while next_excluded < excluded.len() && excluded[next_excluded].1 <= start {
            next_excluded += 1;
        }
        if excluded.get(next_excluded).is_some_and(|&(s, _)| s < end) {
            continue;
        }
        out.push_str(&clean[cursor..start]);
        out.push(BARE_LINK_OPEN);
        out.push_str(&links.len().to_string());
        out.push(BARE_LINK_CLOSE);
        links.push(clean[start..end].to_string());
        cursor = end;
    }
    if links.is_empty() {
        return (Cow::Borrowed(text), Vec::new());
    }
    out.push_str(&clean[cursor..]);
    (Cow::Owned(out), links)
}

/// Byte ranges of the markdown source that are NOT rendered as plain prose.
fn non_prose_ranges(source: &str) -> Vec<(usize, usize)> {
    use markdown::mdast::Node;
    // Iterative for the same reason as `collect_mdast_text`. Ranges come out
    // in document order, as a recursive pre-order walk would give them.
    fn walk(root: &Node, out: &mut Vec<(usize, usize)>) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let skip = matches!(
                node,
                Node::InlineCode(_)
                    | Node::Code(_)
                    | Node::Link(_)
                    | Node::LinkReference(_)
                    | Node::Image(_)
                    | Node::ImageReference(_)
                    | Node::Definition(_)
                    | Node::Html(_)
                    | Node::FootnoteReference(_)
                    | Node::FootnoteDefinition(_)
                    | Node::InlineMath(_)
                    | Node::Math(_)
            );
            if skip {
                if let Some(pos) = node.position() {
                    out.push((pos.start.offset, pos.end.offset));
                }
                continue;
            }
            if let Some(children) = node.children() {
                stack.extend(children.iter().rev());
            }
        }
    }
    let mut out = Vec::new();
    match markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        Ok(root) => {
            walk(&root, &mut out);
            drop_mdast(root);
        }
        // Unparseable: treat everything as non-prose, so nothing is linked.
        Err(_) => out.push((0, source.len())),
    }
    out
}

/// Put the links taken out by [`extract_bare_freenet_links`] back into the
/// rendered HTML, each as `<a href="">…</a>`: the shape the markdown crate
/// itself emits for an autolink with a scheme it does not allow
/// (`<freenet:…>`). `finalize_anchors` then gives it its real href from the
/// text.
///
/// Returns `None`, so the caller renders the message without the bare-link
/// pass, unless every link comes back exactly once and in prose: a sentinel
/// inside a tag (an attribute), inside `<a>`, `<code>` or `<pre>`, repeated,
/// missing, or surviving percent-encoded in a URL means the source scan and
/// the render disagreed about the message's structure, and guessing could
/// nest an anchor or break an attribute. The markdown crate encodes `>` inside
/// attributes, so the first `>` always ends a tag.
fn restore_bare_freenet_links(html: &str, links: &[String]) -> Option<String> {
    let has_sentinel = |s: &str| s.contains([BARE_LINK_OPEN, BARE_LINK_CLOSE]);
    let mut restored = vec![0usize; links.len()];
    let mut out = String::with_capacity(html.len() + links.len() * 64);
    let mut skip_depth: usize = 0;
    let mut rest = html;
    while !rest.is_empty() {
        if rest.starts_with('<') {
            let end = rest.find('>')?;
            let tag = &rest[..=end];
            if has_sentinel(tag) {
                return None;
            }
            let (closing, name) = match tag[1..].strip_prefix('/') {
                Some(after) => (true, after),
                None => (false, &tag[1..]),
            };
            let name_end = name
                .find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(name.len());
            if matches!(
                name[..name_end].to_ascii_lowercase().as_str(),
                "a" | "code" | "pre"
            ) {
                if closing {
                    skip_depth = skip_depth.saturating_sub(1);
                } else if !tag.ends_with("/>") {
                    skip_depth += 1;
                }
            }
            out.push_str(tag);
            rest = &rest[end + 1..];
        } else {
            let text_end = rest.find('<').unwrap_or(rest.len());
            let text = &rest[..text_end];
            if skip_depth > 0 {
                if has_sentinel(text) {
                    return None;
                }
                out.push_str(text);
            } else {
                out.push_str(&replace_bare_link_sentinels(text, links, &mut restored)?);
            }
            rest = &rest[text_end..];
        }
    }
    let encoded_sentinel = ["%EE%80%82", "%EE%80%83"]
        .iter()
        .any(|e| out.to_ascii_uppercase().contains(e));
    if restored.iter().any(|&n| n != 1) || has_sentinel(&out) || encoded_sentinel {
        return None;
    }
    Some(out)
}

/// Replace every `{BARE_LINK_OPEN}<index>{BARE_LINK_CLOSE}` in one prose text
/// run with its link's anchor, counting each restore in `restored`. `None` for
/// anything that is not a well-formed sentinel for a known index.
fn replace_bare_link_sentinels(
    s: &str,
    links: &[String],
    restored: &mut [usize],
) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(BARE_LINK_OPEN) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + BARE_LINK_OPEN.len_utf8()..];
        let close = after.find(BARE_LINK_CLOSE)?;
        let idx: usize = after[..close].parse().ok()?;
        let link = links.get(idx)?;
        restored[idx] += 1;
        out.push_str("<a href=\"\">");
        out.push_str(&encode_html_text(link));
        out.push_str("</a>");
        rest = &after[close + BARE_LINK_CLOSE.len_utf8()..];
    }
    out.push_str(rest);
    Some(out)
}

/// Offset of the `</a>` that closes an anchor whose content starts at the
/// beginning of `after_open`, counting any nested `<a …>` opened inside it.
fn matching_anchor_close(after_open: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = 0;
    while i < after_open.len() {
        let rest = &after_open[i..];
        let next_open = rest.find("<a ");
        let next_close = rest.find("</a>")?;
        match next_open {
            Some(o) if o < next_close => {
                depth += 1;
                i += o + 3;
            }
            _ => {
                if depth == 0 {
                    return Some(i + next_close);
                }
                depth -= 1;
                i += next_close + 4;
            }
        }
    }
    None
}

/// Remove every `<a …>` and `</a>` tag from `html`, keeping their content.
fn strip_anchor_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        let open = rest.find("<a ");
        let close = rest.find("</a>");
        let (pos, is_open) = match (open, close) {
            (Some(o), Some(c)) if o < c => (o, true),
            (Some(o), None) => (o, true),
            (_, Some(c)) => (c, false),
            (None, None) => break,
        };
        out.push_str(&rest[..pos]);
        rest = if is_open {
            match rest[pos..].find('>') {
                Some(end) => &rest[pos + end + 1..],
                None => "",
            }
        } else {
            &rest[pos + 4..]
        };
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
/// The suffix must not contain dot path segments, literal or percent-encoded
/// (see `suffix_has_dotdot_segment`). Without this guard, a
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
    let after_marker = path
        .strip_prefix("/v1/contract/web/")
        .or_else(|| path.strip_prefix("/v2/contract/web/"))?;

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

/// True if any path segment in `suffix` is a dot segment, literal or
/// percent-encoded (`..`, `%2e%2e`, `.%2E`, `.`, ...), which a browser resolves
/// the same way. Checking only a literal `..` let `…/<A>/%2e%2e/<B>/` open
/// contract B, or climb out of `/v1/contract/web/` altogether. Path segments
/// are the `/`-separated components before any `?` query or `#` fragment.
fn suffix_has_dotdot_segment(suffix: &str) -> bool {
    let path_only = suffix
        .split_once(['?', '#'])
        .map(|(p, _)| p)
        .unwrap_or(suffix);
    path_only
        .split('/')
        .any(crate::util::share_link::is_dot_segment)
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
    // The label shows only an 8-character id prefix, so the rest of it must
    // not read as a Freenet link of its own: `…/web/<B>/freenet:<A>` would
    // otherwise read as a link to A. Show such a URL in full instead.
    let suffix_seen = crate::util::confusable::visual_ascii(parsed.suffix).to_ascii_lowercase();
    let names_a_contract = parsed
        .suffix
        .split(|c: char| !is_base58_char(c))
        .any(crate::util::share_link::is_valid_contract_id);
    if suffix_seen.contains("freenet") || suffix_seen.contains("contract/web") || names_a_contract {
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

/// How many display items (message groups and event summaries) the conversation
/// renders when a room is opened.
///
/// Rendering an entire room at once is what makes a busy room unusable.
/// Profiling the live "Off Topic" room on 2026-07-26 (1133 messages, 136
/// members) measured 24,305 DOM nodes and a 1.65 GB WASM heap — and WASM never
/// returns linear memory to the OS, so that peak is permanent for the tab.
/// Rendering the tail the reader actually lands on, and backfilling only when
/// they scroll off the top of it, keeps both bounded by what has been looked at.
///
/// Counted in ITEMS, not messages: consecutive messages from one author share a
/// group, so this is a floor on messages shown, never a cap. A room with fewer
/// items than this renders exactly as it always did — no sentinel, no
/// backfill — so the common case carries none of the windowing's behaviour.
const INITIAL_WINDOW_ITEMS: usize = 60;

/// How many more items each backfill reveals when the reader reaches the top of
/// the current window. Matching [`INITIAL_WINDOW_ITEMS`] means a reader paging
/// back through history grows the window at the rate they consume it.
const WINDOW_GROWTH_ITEMS: usize = 60;

/// Hard ceiling on how many display items arrival-growth can accumulate.
///
/// Arrivals GROW the rendered window instead of sliding it — see
/// [`HistoryWindow::resolve_held`] — so a long session in a busy room would
/// otherwise re-accumulate exactly the unbounded render the window exists to
/// prevent (freenet/river#498). Four initial windows is the chosen bound:
/// deep enough that trims are rare (240 items is hours of a busy room, and the
/// bottom-settle trim usually fires long before), shallow enough that the
/// worst-case DOM stays ~4x the room-open cost rather than unbounded.
///
/// At the ceiling the range stops at its end and holds newer items back
/// (`HistoryWindow::has_newer`), even for a reader idle at the bottom: nothing
/// follows arrivals, so they land below the view either way.
///
/// The ceiling caps ARRIVAL growth only. A reader paging back through history
/// raises `window_items` explicitly, and that requested size always wins over
/// the ceiling — capping it would make the backfill sentinel a no-op past 240
/// items and dead-end the history (see the cap in
/// [`HistoryWindow::resolve_held`]).
const WINDOW_ITEMS_CEILING: usize = INITIAL_WINDOW_ITEMS * 4;

/// How far into the rendered history the backfill trigger REACHES, in px.
///
/// The sentinel is a strip spanning `[0, BACKFILL_LEAD_PX]` from the top of the
/// history, not a marker at a point. Both halves of that matter:
///
/// * Not a 1px marker at the very top: it would only intersect at
///   `scrollTop == 0`, so the reader hits the end of the rendered history and
///   *then* watches it grow. Reaching a screenful in means the next page is
///   already there by the time they get to it, as `bottom-newer-sentinel`
///   does at the other end of a held range.
/// * Not a 1px marker at `top: BACKFILL_LEAD_PX` either. That was the first
///   attempt and it is worse than doing nothing: on any viewport SHORTER than
///   the lead, scrolling to the very top puts the marker BELOW the viewport, so
///   the reader who scrolls straight to the top — the exact case this exists
///   for — never triggers a backfill and the history dead-ends. A strip is
///   intersecting for every scroll position in the band, including 0, on every
///   viewport size.
const BACKFILL_LEAD_PX: i32 = 800;

/// Where the history's tail window starts, and whether anything is behind it.
///
/// Split out from the render so the arithmetic is testable: getting it wrong
/// either renders an empty history (start past the end) or silently renders
/// everything (the blow-up the window exists to prevent), and neither is
/// visible in a unit test of the component.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct HistoryWindow {
    /// Index of the first display item to render.
    start: usize,
    /// One past the last display item to render.
    end: usize,
    /// Whether older items are held back — drives the backfill sentinel.
    has_older: bool,
    /// Whether newer items are held back below the reader. Drives the
    /// newer-history sentinel and the catch-up button.
    has_newer: bool,
}

/// Inputs for the range's lower edge. `Default` ends at the newest item.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct RangeHold {
    /// The relocated end of last render's range, if it stopped short of the
    /// newest item.
    end: Option<usize>,
    /// The start never moves past this index, even above the ceiling: the
    /// reading item, less one for a group the view top cuts through.
    keep: Option<usize>,
}

/// A one-shot request to take the view to the end of `room`'s history:
/// opening the room, the reader's own send, or Latest from a held range. A
/// room change replaces it, and the render drops one made for another room.
/// The reading-position corrections stand down while it is pending.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ScrollRequest {
    room: Option<ed25519_dalek::VerifyingKey>,
}

/// The last reading position measured while the history was visible. A hidden
/// panel measures 0 everywhere, so the reveal restores from this instead.
#[derive(Clone, PartialEq, Debug)]
struct ReadingAnchor {
    /// `(data-anchor-key, offset from the visible top)`, reading row first, then
    /// its neighbours nearest first, the one above before the one below. A
    /// neighbour stands in if the reading row is deleted. Keyed by message id
    /// because a group re-keys when its first message is pruned.
    rows: Vec<(String, i32)>,
    /// Display-item index at capture time; a relocation hint.
    item_hint: usize,
    /// The container's `clientHeight` at capture, so a later height change can
    /// hold the bottom edge as it was then, whatever clamped in between.
    view_height: i32,
    /// The container's `clientWidth` at capture. After a width change the rows
    /// have rewrapped and their saved offsets describe another layout.
    view_width: i32,
}

/// Rows a [`ReadingAnchor`] remembers, the reading row included.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const READING_ANCHOR_ROWS: usize = 7;

/// What a render's range holds, enough to tell an arrival from rows changing
/// height (10c decision 13): its first row, its newest message and how many
/// messages and events it renders.
#[derive(Clone, PartialEq, Debug)]
struct RenderedMessages {
    first: String,
    newest: Option<MessageId>,
    count: usize,
}

/// [`RenderedMessages`] for the display items a render puts on screen.
fn rendered_messages(items: &[DisplayItem]) -> Option<RenderedMessages> {
    Some(RenderedMessages {
        first: display_item_key(items.first()?),
        newest: items.last().and_then(display_item_last_message_id),
        count: items
            .iter()
            .map(|item| match item {
                DisplayItem::Messages(group) => group.messages.len(),
                DisplayItem::Event(summary) => summary.names.len(),
            })
            .sum(),
    })
}

/// Does the end hold survive the step from the last render's range to this
/// one? Only when both have rows and nothing arrived in between: no new newest
/// message, no higher count, and no new first row without a lower count (a
/// drain and an out-of-order insert in one patch). A trim or a deletion above
/// only shrinks the range and keeps it; content changes (decryption, an edit)
/// change none of the three.
fn end_hold_survives(before: Option<&RenderedMessages>, now: Option<&RenderedMessages>) -> bool {
    let (Some(before), Some(now)) = (before, now) else {
        return false;
    };
    now.newest == before.newest
        && now.count <= before.count
        && (now.first == before.first || now.count < before.count)
}

/// The value a contended read stands in with: the last good one, but only if
/// it was taken for `room`. Another room's rows never render in this one.
fn last_good_for_room<K: PartialEq, T: Clone>(cache: &Option<(K, T)>, room: &K) -> Option<T> {
    cache
        .as_ref()
        .filter(|(cached, _)| cached == room)
        .map(|(_, value)| value.clone())
}

/// The `message_groups` memo's value: the room's display items, self's member
/// id and the member names. Shared, so the contended-read cache costs a
/// pointer, not a copy of every rendered message.
type MessageGroupsValue = Rc<(
    Vec<DisplayItem>,
    Option<MemberId>,
    HashMap<MemberId, String>,
)>;

/// State shared by the render, its effects and the raw scroll and resize
/// callbacks. Plain cells, so reads never subscribe; re-renders come from
/// `window_items`. The render writes `has_newer` before the catch-up button
/// reads it.
#[derive(Default)]
struct ReaderPosition {
    /// The IDENTITY of the item the last render's window started at, so
    /// arrivals GROW the window instead of sliding it (#501), and so index
    /// shifts from at-cap message pruning cannot move the head in content
    /// space (#505 review, blocker 1). See [`WindowAnchor`].
    window_anchor: std::cell::RefCell<Option<WindowAnchor>>,
    /// Identity of a held range's last items (newest first), set only while
    /// `HistoryWindow::has_newer`. Relocated like `window_anchor`.
    window_tail: std::cell::RefCell<Option<WindowAnchor>>,
    /// How many display items the last render actually put on screen; the
    /// backfill growth step grows from it (see `grown_window`).
    window_rendered: std::cell::Cell<usize>,
    /// Whether the rendered window holds more than a fresh room-open would
    /// render, i.e. whether the bottom-settle trim has anything to do.
    window_overgrown: std::cell::Cell<bool>,
    /// See [`ReadingAnchor`].
    anchor: std::cell::RefCell<Option<ReadingAnchor>>,
    /// Seen without layout since the last restore; the next resize restores.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    hidden: std::cell::Cell<bool>,
    /// First rendered display item, for the capture's index hint.
    range_start: std::cell::Cell<usize>,
    /// The last render's `HistoryWindow::has_newer`.
    has_newer: std::cell::Cell<bool>,
    /// The pending explicit scroll, if any. While newer items are held back
    /// the render first selects the latest range for it.
    request: std::cell::Cell<Option<ScrollRequest>>,
    /// The next render extends a held range by one page of newer items.
    extend_newer: std::cell::Cell<bool>,
    /// Bumped on every room change, so a deferred trim can detect one.
    room_epoch: std::cell::Cell<u64>,
    /// The room and newest display message of the last render, set only when
    /// its range reaches the room's latest message. What `note_newest_seen`
    /// publishes once its bottom is on screen.
    newest_rendered: std::cell::RefCell<Option<(ed25519_dalek::VerifyingKey, MessageId)>>,
    /// The last value `note_newest_seen` published, so a repeat check (every
    /// settle, every render) schedules nothing.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    seen_noted: std::cell::RefCell<Option<(ed25519_dalek::VerifyingKey, MessageId)>>,
    /// The container's last non-zero `clientHeight` the ResizeObserver
    /// handled; 0 while hidden or not yet measured. A settle that measures
    /// another height has beaten the observer to a resize, and must not
    /// capture over the position the observer is about to correct from.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    observed_height: std::cell::Cell<i32>,
    /// The end hold (10c decision 13): `Some(scrollTop)` while the view sits at
    /// the end an explicit request put it at. Rows changing height take it
    /// back there (the ResizeObserver). The reader's first scroll away, an
    /// arrival (the render), a room change and the panel hiding end it.
    end_hold: std::cell::Cell<Option<i32>>,
    /// The last render's range, so the next render can tell an arrival from
    /// rows changing height.
    rendered: std::cell::RefCell<Option<RenderedMessages>>,
    /// The backfill sentinel's capture, consumed by the restore effect. See
    /// `BackfillAnchor`.
    backfill_anchor: std::cell::RefCell<Option<BackfillAnchor>>,
    /// A pending "keep the reader's view still" adjustment: when a render
    /// swaps the window head for a LATER item (the head was pruned out from
    /// under the anchor, or newer paging slid past it), the rows above the
    /// viewport shrink, and with scroll anchoring disabled nothing
    /// compensates. The render captures a [`RepositionAnchor`] here (see
    /// `select_reposition_probe`); the `head_reposition` effect re-measures
    /// the row after the patch and shifts `scrollTop` by the difference.
    ///
    /// The scroll offset is captured too because the browser clamps
    /// `scrollTop` down on its own when a patch shortens the content, BEFORE
    /// the effect runs: computing the target from the post-clamp offset would
    /// apply the shift on top of the clamp (#505 delta review). A trade: a
    /// live read is immune to the reader scrolling between render and effect,
    /// a captured one to the clamp, and the clamp is far more frequent.
    reposition_pending: std::cell::RefCell<Option<RepositionAnchor>>,
}

impl ReaderPosition {
    /// Ask for one move to the end of `room`'s history (see [`ScrollRequest`]).
    fn request_end(&self, room: Option<ed25519_dalek::VerifyingKey>) {
        self.request.set(Some(ScrollRequest { room }));
        self.drop_pending_corrections();
    }

    /// Forget a backfill restore or head reposition still waiting for its
    /// effect. Each was measured against the view a request is replacing, and
    /// an effect that ran after the request completed would undo it.
    fn drop_pending_corrections(&self) {
        *self.backfill_anchor.borrow_mut() = None;
        *self.reposition_pending.borrow_mut() = None;
    }

    /// No range on screen, or a new room's: end the hold and forget the last
    /// range, so the next render's range is never compared against it.
    fn forget_range(&self) {
        *self.rendered.borrow_mut() = None;
        self.end_hold.set(None);
    }
}

/// Does `item` render the row a [`ReadingAnchor`] key names?
fn item_holds_anchor_key(item: &DisplayItem, key: &str) -> bool {
    match item {
        DisplayItem::Messages(group) => group.messages.iter().any(|m| m.id == key),
        DisplayItem::Event(summary) => summary.id == key,
    }
}

/// Index of the first remembered row still in the room.
fn locate_reading_item(items: &[DisplayItem], anchor: &ReadingAnchor) -> Option<usize> {
    anchor.rows.iter().find_map(|(key, _)| {
        relocate_anchor(items.len(), anchor.item_hint, |i| {
            item_holds_anchor_key(&items[i], key)
        })
    })
}

/// The scroll shift that puts the first surviving remembered row back at its
/// offset, moved by `offset_shift` (the height change since capture, to hold
/// the bottom edge instead of the top). `None` when none is rendered.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn reading_anchor_shift(
    anchor: &ReadingAnchor,
    offset_shift: i32,
    top_now: impl Fn(&str) -> Option<i32>,
) -> Option<i32> {
    anchor
        .rows
        .iter()
        .find_map(|(key, saved)| top_now(key).map(|now| now - (saved + offset_shift)))
}

/// The identity of the window's head: which display item the last render
/// started at, remembered by KEY, with the index it held as a relocation hint.
///
/// The anchor is by IDENTITY, not position, because positions are not stable:
/// `MessagesV1::apply_delta` drains the oldest message once a room exceeds
/// `max_recent_messages` (the steady state of every busy room), so an arrival
/// shifts every index down by one. A positional anchor then renders a head one
/// item LATER in identity — the top rendered row is removed in the same patch
/// that appends the arrival, which is the #501 slide reproduced in content
/// space, and with scroll anchoring disabled it crawls a parked reader upward
/// one row per arrival. Anchoring on the head item's key holds the rendered
/// set fixed in identity no matter how the indices shift underneath it.
#[derive(Clone, PartialEq, Debug)]
struct WindowAnchor {
    /// `display_item_key` of the first [`WINDOW_ANCHOR_KEYS`] rendered items,
    /// head first. The spares exist because the head key alone can vanish
    /// while its NEIGHBORS survive: an at-cap drain that consumes the head
    /// (or only its first message — a multi-message head group RE-KEYS, since
    /// a group's key is its first message's id), or a batched delta draining
    /// many items at once. The first spare still present tells us exactly
    /// where the surviving remainder of the old window now sits.
    keys: Vec<String>,
    /// The index the head held last render — a hint so relocation is
    /// O(shift), not O(total), in the common case.
    index: usize,
}

/// How many leading item keys the anchor remembers (head + spares).
///
/// Bounds how large a head-consuming removal can be precisely relocated: a
/// front drain that consumes the head and ALL spares falls back to index 0,
/// which for a front-contiguous drain — the only mechanism that can remove
/// this many contiguous leading items — is exactly the nearest surviving
/// item anyway.
const WINDOW_ANCHOR_KEYS: usize = 8;

/// Where the anchored window starts after the item list changed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct RelocatedWindow {
    /// Start index for the resolve: the head's new position when it survived,
    /// otherwise derived from the first surviving spare (its position minus
    /// its offset in the anchor), otherwise 0.
    start: usize,
    /// Whether the HEAD itself survived under its own key. False means the
    /// pre-patch head row is gone or re-keyed, so rendered content above (or
    /// at) the new head changed and the reader needs the measured
    /// reposition.
    head_survived: bool,
}

/// Search for one anchored key after the item list changed.
///
/// Front prunes shift every surviving index DOWN, so the hint is checked
/// first, then the indices below it, then (for completeness — a merge can
/// insert older history above the head) the indices above. `None` means this
/// key is gone entirely.
fn relocate_anchor(total: usize, hint: usize, is_anchor: impl Fn(usize) -> bool) -> Option<usize> {
    if hint < total && is_anchor(hint) {
        return Some(hint);
    }
    if let Some(i) = (0..hint.min(total)).rev().find(|&i| is_anchor(i)) {
        return Some(i);
    }
    (hint.saturating_add(1)..total).find(|&i| is_anchor(i))
}

/// Re-locate the anchored window: the head by its own key, else by the first
/// surviving spare, else index 0.
///
/// The index-0 fallback is exact for a front-contiguous drain that consumed
/// the head and every spare — the oldest remaining item IS then the nearest
/// survivor. It is a DEGRADED answer for the other way to lose that many
/// contiguous leading items (a ban purge or bulk delete of >=
/// [`WINDOW_ANCHOR_KEYS`] display items above the reader): the window jumps
/// to the front of the room, bounded only by the ceiling, and the reposition
/// probe finds nothing to measure against. Rare, and it fails toward
/// rendering MORE history rather than losing the reader's place entirely.
///
/// The `head_reposition` effect measures and compensates the reader for
/// the TOP-CONTIGUOUS removals this relocation deals in (at-cap drains,
/// batched or not, including a re-keyed multi-message head group, and small
/// leading deletes the spares still cover). It does NOT make every removal
/// invisible: a bulk MID-window removal (a ban purge) or late-loading media
/// above the viewport still shifts the reader — tracked as follow-up
/// work (#507), not covered here.
fn relocate_window(
    total: usize,
    anchor: &WindowAnchor,
    key_at: impl Fn(usize, &str) -> bool,
) -> RelocatedWindow {
    for (spare, key) in anchor.keys.iter().enumerate() {
        let located = relocate_anchor(total, anchor.index + spare, |i| key_at(i, key));
        if let Some(i) = located {
            return RelocatedWindow {
                start: i.saturating_sub(spare),
                head_survived: spare == 0,
            };
        }
    }
    RelocatedWindow {
        start: 0,
        head_survived: false,
    }
}

/// Re-locate a held range's end: one past its last item, or past the first
/// surviving spare before it (keys newest first). A found spare is the last
/// surviving item, so unlike the head it needs no offset. `None` when every
/// key is gone.
fn relocate_tail(
    total: usize,
    anchor: &WindowAnchor,
    key_at: impl Fn(usize, &str) -> bool,
) -> Option<usize> {
    anchor.keys.iter().enumerate().find_map(|(spare, key)| {
        relocate_anchor(total, anchor.index.saturating_sub(spare), |i| {
            key_at(i, key)
        })
        .map(|i| i + 1)
    })
}

/// How many rows past the window head the reposition capture will probe for a
/// row that exists in the PRE-patch DOM.
///
/// The new head itself may have no pre-patch row to measure, for two reasons:
///
/// * a multi-message head group whose first message was drained RE-KEYS (a
///   group's key is its first message's id), so its new key is in no
///   pre-patch row (#505 re-review blocker);
/// * relocation landing via SPARE `k` sets `start = i - k`, widening the
///   window backward by `k` items that were not rendered pre-patch at all, so
///   the first `k` candidates cannot have pre-patch rows either.
///
/// A head-only — or too-short — probe dead-fires, and the parked reader is
/// left uncompensated. Sized to [`WINDOW_ANCHOR_KEYS`] so the walk covers the
/// SPARE term exactly (`k <= WINDOW_ANCHOR_KEYS - 1`, needing `k + 1`
/// candidates). The full count of leading candidates without pre-patch rows is
/// `k + (relocated.start - history_window.start)`, and that second term is
/// non-zero only when `resolve_held` pulls the start back below the relocated
/// head — the bulk mid-window removal `relocate_window` already declares
/// out of scope. For a top-contiguous removal every surviving row at or below
/// the head shifts by the same amount, so whichever candidate lands measures
/// the shift exactly.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const REPOSITION_PROBE_ROWS: usize = WINDOW_ANCHOR_KEYS;

/// Pick the row the reposition will measure: the first of the leading
/// `REPOSITION_PROBE_ROWS` post-patch keys that has a PRE-patch row.
///
/// `pre_patch_offsets` is handed the whole candidate list at once so the DOM
/// is scanned a single time (see `first_history_row_offset`) rather than once
/// per candidate inside the render body.
///
/// Only the wasm render path calls this at runtime; natively it is exercised
/// by the unit tests, hence the targeted allow rather than a cfg that would
/// hide it from them.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn select_reposition_probe(
    keys_from_head: impl Iterator<Item = String>,
    pre_patch_offsets: impl Fn(&[String]) -> Option<(String, i32)>,
) -> Option<(String, i32)> {
    let candidates: Vec<String> = keys_from_head.take(REPOSITION_PROBE_ROWS).collect();
    pre_patch_offsets(&candidates)
}

/// Does `key` identify `item`, without allocating a key `String`?
///
/// The relocation walk compares keys against up to every item on a miss;
/// building a `String` per probe made that O(total) allocations per render at
/// the at-cap steady state (#505 re-review).
fn display_item_key_matches(item: &DisplayItem, key: &str) -> bool {
    match item {
        DisplayItem::Messages(group) => group.messages[0].id == key,
        DisplayItem::Event(summary) => summary.id == key,
    }
}

/// What the backfill sentinel captures just before growing the window, so the
/// restore can put the reader back on the row they were looking at.
///
/// Only ever constructed on wasm (the capture reads the DOM), hence the
/// native allow rather than a cfg that would also hide the type from the
/// component's non-wasm compile.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#[derive(Clone, PartialEq, Debug)]
struct BackfillAnchor {
    /// `data-item-key` of the head row at capture time. It survives the
    /// backfill (revealed rows land ABOVE it), and its `offsetTop` shift
    /// measures EXACTLY the height prepended above the viewport — unlike the
    /// raw `scrollHeight` delta, which also counts content appended BELOW the
    /// viewport when an arrival batches into the same patch, over-shifting
    /// the reader by that content's height (#505 re-review).
    probe_key: String,
    /// The probe row's `offsetTop` at capture time.
    probe_top: i32,
    /// `scrollTop` at capture time.
    scroll_top: i32,
    /// `scrollHeight` at capture time — the fallback delta if the probe row
    /// vanishes in the same patch (an at-cap drain re-keying it; rare).
    scroll_height: i32,
}

/// What the render captures before a patch that swaps the window head for a
/// later item, so the `head_reposition` effect can keep the reader's view
/// still. See `ReaderPosition::reposition_pending`.
///
/// Only ever constructed on wasm (the capture reads the DOM); see
/// `BackfillAnchor` for why the native allow.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#[derive(Clone, PartialEq, Debug)]
struct RepositionAnchor {
    /// `data-item-key` of a row that survives the patch (see
    /// `select_reposition_probe`).
    probe_key: String,
    /// The probe row's `offsetTop` before the patch.
    probe_top: i32,
    /// `scrollTop` before the patch.
    scroll_top: i32,
}

/// Margin over the backfill strip's reach before a trim is allowed. Covers
/// what the measurement leaves out: padding and the date separator the first
/// retained item gains.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const TRIM_HEADROOM_PX: f64 = 200.0;

/// Would a trim leaving `retained_tail_px` of history put the backfill
/// sentinel strip inside the bottom viewport?
///
/// At the bottom, the sentinel strip `[0, BACKFILL_LEAD_PX]` intersects the
/// viewport iff `content_height < client_height + BACKFILL_LEAD_PX`. A tail
/// short enough for that (browser zoom-out, a tall portrait monitor over a
/// modest window) would re-fire the backfill the moment the trim lands:
/// trim → backfill → restore → settle at the bottom → trim, a silent render
/// loop at full speed (#505 re-review). The settle handler then skips the
/// trim and the window stays grown, bounded by the ceiling.
///
/// The tail is measured. An average-row-height estimate looped on rooms whose
/// older rows are much taller than the newest ones.
///
/// Only the wasm settle handler calls this at runtime; natively it is
/// exercised by the unit tests, hence the targeted allow.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn trim_would_rearm_backfill(retained_tail_px: i32, client_height: i32) -> bool {
    (retained_tail_px as f64) < client_height as f64 + BACKFILL_LEAD_PX as f64 + TRIM_HEADROOM_PX
}

/// The requested window size after one backfill growth step.
///
/// Grows from the RENDERED size, not the requested size. Arrivals grow an
/// anchored window past what was requested (`rendered = total - start` exceeds
/// `requested` by the number of arrivals), and a growth step computed from the
/// stale requested size can resolve to a start the anchor already renders —
/// zero new rows, no DOM change, so the sentinel's IntersectionObserver never
/// re-fires and paging dead-ends (#505 review, blocker 2). Growing from the
/// rendered size guarantees every step reveals `WINDOW_GROWTH_ITEMS` more
/// items than are currently on screen.
fn grown_window(requested: usize, rendered: usize) -> usize {
    rendered.max(requested) + WINDOW_GROWTH_ITEMS
}

impl HistoryWindow {
    /// Test shorthand for a range with no held end; see [`Self::resolve_held`].
    #[cfg(test)]
    fn resolve(total_items: usize, window: usize, anchor: Option<usize>) -> Self {
        Self::resolve_held(total_items, window, anchor, RangeHold::default())
    }

    /// Resolve which display items the history renders.
    ///
    /// `anchor` is the start index the PREVIOUS render used (`None` when the
    /// room was just opened, or after a trim). It is what keeps the window
    /// from SLIDING on arrival: without it, every new message advanced `start`
    /// by one, dropping the oldest rendered row in the same patch that
    /// appended the new one at the bottom (freenet/river#501). With scroll
    /// anchoring disabled on the container, removing content above the
    /// viewport visibly shifts the reader. So:
    ///
    /// * `start` NEVER moves forward from the anchor on an ordinary render:
    ///   arrivals grow the rendered count instead (start fixed, new items
    ///   appended below).
    /// * `start` moves BACK (upward, revealing older items) when the reader
    ///   backfills — `window` grew, so the plain tail start is earlier than
    ///   the anchor.
    /// * [`WINDOW_ITEMS_CEILING`] caps arrival growth. Past it the end stops
    ///   instead, wherever the reader is, and newer items are held back.
    ///   `max(window, ...)` keeps the ceiling from ever capping a
    ///   reader-requested backfill.
    ///
    /// `hold` adds the lower edge:
    ///
    /// * A held end moves only when the caller extends it (newer paging); the
    ///   ceiling then slides the start.
    /// * The start never passes `keep`, not even at the ceiling.
    ///
    /// `window` counts back from the end, so backfill works the same either way.
    ///
    /// Trimming back toward [`INITIAL_WINDOW_ITEMS`] is NOT done here — it is
    /// an explicit event (a settle landing at the bottom, or a room switch)
    /// that clears the anchor and resets `window`, because a trim is only
    /// invisible when the view is at the bottom, where the browser's scrollTop
    /// clamp keeps the tail glued in place.
    fn resolve_held(
        total_items: usize,
        window: usize,
        anchor: Option<usize>,
        hold: RangeHold,
    ) -> Self {
        // A held end can never pass the newest item, and never empties the
        // range: if every item it held is gone, fall back to the newest.
        let mut end = match hold.end {
            Some(e) if e > 0 => e.min(total_items),
            _ => total_items,
        };
        // `max(1)` so an item is always on screen: a zero window would
        // render an empty history that the reader has no way to scroll into.
        let base = end.saturating_sub(window.max(1));
        // Grow, never slide: keep the anchored start unless a backfill asked
        // for an even earlier one. `min` also repairs an anchor past the end
        // (messages pruned out from under it).
        let mut start = match anchor {
            None => base,
            Some(prev) => prev.min(base),
        };
        // The ceiling on arrival growth; never on reader-requested backfill.
        // One growth step of grace above the requested size, so a range
        // backfilled to the ceiling still renders the next arrivals before its
        // end holds (#505 review).
        let cap = (window + WINDOW_GROWTH_ITEMS)
            .max(WINDOW_ITEMS_CEILING)
            .max(1);
        if end - start > cap {
            if hold.end.is_none() {
                end = start + cap;
            } else {
                start = end - cap;
            }
        }
        if let Some(keep) = hold.keep {
            start = start.min(keep.min(end.saturating_sub(1)));
        }
        Self {
            start,
            end,
            has_older: start > 0,
            has_newer: end < total_items,
        }
    }
}

/// Trailing delay used to spot a scroll settling on browsers with no
/// `scrollend` event (Safari before 17.4).
#[cfg(target_arch = "wasm32")]
const SCROLL_SETTLE_DEBOUNCE_MS: i32 = 120;

/// Refresh the saved reading position, keeping the old one when nothing can be
/// measured. Called at every settle, before a newer page, once an explicit
/// request or a reveal has placed the view, and when the Rooms or Members
/// button hides the chat panel (afterwards there is no geometry to read). A
/// breakpoint hide relies on the last settle.
fn remember_reading_position(reader: &ReaderPosition) {
    #[cfg(target_arch = "wasm32")]
    if let Some(anchor) = capture_reading_anchor(reader.range_start.get()) {
        *reader.anchor.borrow_mut() = Some(anchor);
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = reader;
}

/// Read the chat history's scroll container, if it is currently in the DOM.
#[cfg(target_arch = "wasm32")]
fn chat_scroll_container() -> Option<web_sys::Element> {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("chat-scroll-container"))
}

/// The furthest down `container` can be scrolled, in px.
#[cfg(target_arch = "wasm32")]
fn max_scroll_top(container: &web_sys::Element) -> i32 {
    (container.scroll_height() - container.client_height()).max(0)
}

/// The `offsetTop` of the history row carrying `data-item-key == key`, if it
/// is currently in the DOM.
///
/// Matched by comparing attributes rather than an attribute SELECTOR, so a key
/// never needs CSS escaping. The scan is bounded by the rendered window (≤ a
/// few hundred rows) and only runs on the rare head-swap paths.
#[cfg(target_arch = "wasm32")]
fn history_row_offset_top(key: &str) -> Option<i32> {
    first_history_row_offset(&[key.to_string()]).map(|(_, top)| top)
}

/// The first of `keys` (in order) that has a row in the CURRENT DOM, as
/// `(key, offsetTop)`.
///
/// One DOM pass for the whole candidate list rather than one pass per
/// candidate: the walk can legitimately need to try [`REPOSITION_PROBE_ROWS`]
/// of them, and the old shape re-ran `querySelectorAll` plus an attribute read
/// per row for each — `candidates x rows` FFI calls. (Not repeated reflows:
/// the DOM does not change between scans, so layout stayed cached after the
/// first `offsetTop`.) Only CANDIDATE rows are measured, so a match on the
/// first row costs a handful of string compares and a single `offsetTop`.
#[cfg(target_arch = "wasm32")]
fn first_history_row_offset(keys: &[String]) -> Option<(String, i32)> {
    use wasm_bindgen::JsCast;
    if keys.is_empty() {
        return None;
    }
    let container = chat_scroll_container()?;
    let rows = container.query_selector_all("[data-item-key]").ok()?;
    // Candidate key -> offsetTop. Bounded by `keys.len()`, not by the rendered
    // window: measuring every row cost an `offsetTop` and a map insert per row
    // (up to the ceiling, 240) on a path that runs per arrival in an at-cap
    // room. The `String` per row is NOT saved — `get_attribute` allocates one
    // either way; what this avoids is the layout read and the insert.
    let mut present: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    for i in 0..rows.length() {
        if present.len() == keys.len() {
            break;
        }
        let Some(node) = rows.item(i) else { continue };
        let Some(el) = node.dyn_ref::<web_sys::HtmlElement>() else {
            continue;
        };
        if let Some(k) = el.get_attribute("data-item-key") {
            if keys.iter().any(|c| c == &k) {
                present.entry(k).or_insert_with(|| el.offset_top());
            }
        }
    }
    // Priority is the CANDIDATE order (nearest the head first), not DOM order.
    keys.iter()
        .find_map(|k| present.get(k).map(|top| (k.clone(), *top)))
}

/// The first history row carrying a `data-item-key` — the current window head
/// item's row — as `(key, offsetTop)`. The backfill capture probes it.
#[cfg(target_arch = "wasm32")]
fn first_history_row_identity() -> Option<(String, i32)> {
    use wasm_bindgen::JsCast;
    let container = chat_scroll_container()?;
    let el = container.query_selector("[data-item-key]").ok()??;
    let key = el.get_attribute("data-item-key")?;
    let html = el.dyn_ref::<web_sys::HtmlElement>()?;
    Some((key, html.offset_top()))
}

/// Does the history have layout? On a phone the chat panel is `display:none`
/// behind Rooms or Members, and every row then measures 0. `MOBILE_VIEW` can't
/// tell: on desktop all panels show whatever it says.
#[cfg(target_arch = "wasm32")]
fn history_has_layout(container: &web_sys::Element) -> bool {
    container.client_height() > 0
}

/// Measure the first message or event row that starts inside the view, with
/// its nearest neighbours. A binary search over item rows keeps this to a
/// handful of layout reads, cheap enough for every settle.
#[cfg(target_arch = "wasm32")]
fn capture_reading_anchor(range_start: usize) -> Option<ReadingAnchor> {
    use wasm_bindgen::JsCast;
    let container = chat_scroll_container()?;
    if !history_has_layout(&container) {
        return None;
    }
    let view_top = container.get_bounding_client_rect().top();
    let rows = container.query_selector_all("[data-item-key]").ok()?;
    let row = |i: u32| {
        rows.item(i)
            .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
    };
    let n = rows.length();
    // The first item row whose bottom is below the visible top.
    let (mut lo, mut hi) = (0u32, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let bottom = row(mid).map_or(f64::MAX, |r| r.get_bounding_client_rect().bottom());
        if bottom <= view_top {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == n {
        return None;
    }
    // The anchor-keyed rows in that item and its neighbours, in page order.
    let mut found: Vec<(String, f64, f64)> = Vec::new();
    for i in lo.saturating_sub(1)..(lo + 3).min(n) {
        let Some(item) = row(i) else { continue };
        let mut push = |el: &web_sys::Element| {
            if let Some(key) = el.get_attribute("data-anchor-key") {
                let rect = el.get_bounding_client_rect();
                found.push((key, rect.top(), rect.bottom()));
            }
        };
        if item.has_attribute("data-anchor-key") {
            push(&item);
        } else if let Ok(inner) = item.query_selector_all("[data-anchor-key]") {
            for j in 0..inner.length() {
                if let Some(el) = inner
                    .item(j)
                    .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
                {
                    push(&el);
                }
            }
        }
    }
    // The row being read: the first one whose top is in view, else the one
    // the visible top cuts through.
    let reading = found
        .iter()
        .position(|(_, top, _)| *top >= view_top - 0.5)
        .or_else(|| found.iter().position(|(_, _, bottom)| *bottom > view_top))?;
    let mut rows = Vec::with_capacity(READING_ANCHOR_ROWS);
    let entry = |i: usize| (found[i].0.clone(), (found[i].1 - view_top).round() as i32);
    rows.push(entry(reading));
    // The row above first: if the reading row is deleted, the row above stays
    // still (10c decision 7), as it does when nothing corrects the removal.
    for d in 1..found.len() {
        if rows.len() >= READING_ANCHOR_ROWS {
            break;
        }
        if d <= reading {
            rows.push(entry(reading - d));
        }
        if reading + d < found.len() && rows.len() < READING_ANCHOR_ROWS {
            rows.push(entry(reading + d));
        }
    }
    Some(ReadingAnchor {
        rows,
        item_hint: range_start + lo as usize,
        view_height: container.client_height(),
        view_width: container.client_width(),
    })
}

/// Scroll so `anchor`'s first surviving row is back at its saved offset, plus
/// `shift_offsets_by`. False when none of its rows is rendered.
#[cfg(target_arch = "wasm32")]
fn restore_reading_anchor(anchor: &ReadingAnchor, shift_offsets_by: i32) -> bool {
    use wasm_bindgen::JsCast;
    let Some(container) = chat_scroll_container() else {
        return false;
    };
    let view_top = container.get_bounding_client_rect().top();
    let Ok(els) = container.query_selector_all("[data-anchor-key]") else {
        return false;
    };
    // One pass, measuring only the remembered keys, and stopping once they are
    // all found. Runs on a reveal and on every chat-area height change.
    let mut tops: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    for i in 0..els.length() {
        if tops.len() == anchor.rows.len() {
            break;
        }
        let Some(el) = els
            .item(i)
            .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        if let Some(key) = el.get_attribute("data-anchor-key") {
            if anchor.rows.iter().any(|(k, _)| *k == key) {
                let top = (el.get_bounding_client_rect().top() - view_top).round() as i32;
                tops.entry(key).or_insert(top);
            }
        }
    }
    let Some(shift) = reading_anchor_shift(anchor, shift_offsets_by, |key| tops.get(key).copied())
    else {
        return false;
    };
    if shift != 0 {
        container.set_scroll_top(container.scroll_top() + shift);
    }
    true
}

/// Height a trim to the newest `retained` items would leave. `None` when no
/// more than that many are rendered.
#[cfg(target_arch = "wasm32")]
fn retained_tail_height(container: &web_sys::Element, retained: usize) -> Option<i32> {
    let rows = container.query_selector_all("[data-item-key]").ok()?;
    let n = rows.length() as usize;
    if n <= retained {
        return None;
    }
    use wasm_bindgen::JsCast;
    let first = rows
        .item((n - retained) as u32)?
        .dyn_into::<web_sys::Element>()
        .ok()?;
    // Content coordinates, from the same rects as the container's own.
    let top_in_content = container.scroll_top() as f64 + first.get_bounding_client_rect().top()
        - container.get_bounding_client_rect().top();
    Some(container.scroll_height() - top_in_content.round() as i32)
}

/// Slack for deciding whether a scroll offset is at the end of the history.
///
/// `scrollTop` is fractional in every engine while `Element::scroll_top`
/// rounds, so an exact comparison would report a 1px difference on a view that
/// never moved. `conversation-autoscroll.spec.ts` is what catches this being
/// too tight.
#[cfg(target_arch = "wasm32")]
const SCROLL_TOP_SLACK_PX: i32 = 2;

/// Land an explicit request (opening, own send, Latest) at the end of the
/// history, and hold it there while rows change height (10c decision 13).
#[cfg(target_arch = "wasm32")]
fn land_at_end(reader: &ReaderPosition, container: &web_sys::Element) {
    reader.drop_pending_corrections();
    scroll_to_end(container);
    reader.end_hold.set(Some(container.scroll_top()));
    // Now, not at the settle: the row read before the request would bound the
    // next render's range (`keep`) and pull a swapped-out range back in.
    remember_reading_position(reader);
    note_newest_seen(reader);
}

/// While the end hold is on, take the view back to the end after a size
/// change: a row above it grew (an image, decryption, a font) or the chat area
/// changed height. Returns whether it held.
#[cfg(target_arch = "wasm32")]
fn keep_end_held(reader: &ReaderPosition, container: &web_sys::Element) -> bool {
    if reader.end_hold.get().is_none() {
        return false;
    }
    scroll_to_end(container);
    reader.end_hold.set(Some(container.scroll_top()));
    remember_reading_position(reader);
    note_newest_seen(reader);
    true
}

/// Finish the pending [`ScrollRequest`] once the history has rows and layout:
/// take the view to the end, then let the paging strips mount. A hidden panel
/// (including one revealed but not yet restored) or an empty room keeps it
/// pending. Returns whether it finished one.
#[cfg(target_arch = "wasm32")]
fn complete_scroll_request(reader: &ReaderPosition, mut opening_snap_done: Signal<bool>) -> bool {
    let Some(request) = reader.request.get() else {
        return false;
    };
    let Some(container) = chat_scroll_container() else {
        return false;
    };
    if !history_has_layout(&container)
        || reader.hidden.get()
        || !container
            .query_selector("[data-item-key]")
            .is_ok_and(|row| row.is_some())
    {
        return false;
    }
    reader.request.set(None);
    land_at_end(reader, &container);
    // The strips wait for the view to reach the end once (#501 H2). Deferred:
    // this can run from a raw observer callback, and the signal is rendered.
    crate::util::defer(move || {
        if CURRENT_ROOM.peek().owner_key == request.room && !*opening_snap_done.peek() {
            opening_snap_done.set(true);
        }
    });
    true
}

/// Publish the newest message as seen if the reader can see it now: the tab is
/// visible, the history has layout and is not awaiting its reveal restore, the
/// rendered range reaches the room's latest message, and that message's bottom
/// is on screen (10c decision 5). Marking a room read never goes past what
/// this publishes; see `document_title::NEWEST_SEEN`.
///
/// Called wherever one of those can change: the Latest observer, each settle,
/// each render, the reveal, a completed request and the tab becoming visible.
/// Touches no signal itself, so raw JS callbacks may call it; the write is
/// deferred.
#[cfg(target_arch = "wasm32")]
fn note_newest_seen(reader: &ReaderPosition) {
    let Some(newest) = reader.newest_rendered.borrow().clone() else {
        return;
    };
    if reader.seen_noted.borrow().as_ref() == Some(&newest) {
        return;
    }
    let Some(container) = chat_scroll_container() else {
        return;
    };
    if !crate::components::app::document_title::get_visibility_state()
        || !history_has_layout(&container)
        || reader.hidden.get()
        || !sentinel_in_view(&container, "bottom-sentinel")
    {
        return;
    }
    *reader.seen_noted.borrow_mut() = Some(newest.clone());
    crate::util::defer(move || {
        *crate::components::app::document_title::NEWEST_SEEN.write() = Some(newest);
    });
}

/// May a bottom-settle trim run now? Requires layout, the exact bottom of a
/// range that reaches the latest message, an overgrown window, nothing
/// pending, and a measured tail that clears the backfill strip (see
/// `trim_would_rearm_backfill`).
///
/// A settle at the bottom is the one moment a trim is invisible: removing rows
/// above the viewport shrinks `scrollHeight`, the browser clamps `scrollTop`
/// with it, and the same tail stays on the bottom edge. Anywhere else, with
/// scroll anchoring off, a trim shifts content under the reader, so growth
/// there is bounded by `WINDOW_ITEMS_CEILING` instead. Gated at
/// `SCROLL_TOP_SLACK_PX`: a trim from even a little above the bottom would
/// clamp the reader to the exact end, a visible yank.
#[cfg(target_arch = "wasm32")]
fn trim_is_due(container: &web_sys::Element, overgrown: bool, reader: &ReaderPosition) -> bool {
    if !overgrown
        || !history_has_layout(container)
        || reader.has_newer.get()
        || reader.hidden.get()
        || reader.request.get().is_some()
    {
        return false;
    }
    let distance = container.scroll_height() - container.scroll_top() - container.client_height();
    if distance > SCROLL_TOP_SLACK_PX {
        return false;
    }
    retained_tail_height(container, INITIAL_WINDOW_ITEMS)
        .is_some_and(|tail| !trim_would_rearm_backfill(tail, container.client_height()))
}

/// Listen for the history's scroll settling. Each settle remembers the reading
/// position, and a settle at the exact bottom trims a grown window.
///
/// Returns whether the listener was installed; `false` means the container was
/// not in the DOM yet and the caller should try again.
#[cfg(target_arch = "wasm32")]
#[must_use]
fn install_scroll_settle_listener(window_items: Signal<usize>, reader: Rc<ReaderPosition>) -> bool {
    use wasm_bindgen::prelude::*;

    let Some(container) = chat_scroll_container() else {
        return false;
    };

    // Passive throughout: none of these handlers call `preventDefault`, and the
    // jank this replaces (#151) came from doing work on the scroll path.
    let passive = web_sys::AddEventListenerOptions::new();
    passive.set_passive(true);

    // One DOM measurement per settle, not per scroll event.
    let settle = {
        let reader = reader.clone();
        Closure::wrap(Box::new(move || {
            let Some(container) = chat_scroll_container() else {
                return;
            };
            // Hidden, everything measures 0 and would read as "at the bottom".
            if !history_has_layout(&container) {
                return;
            }
            // Revealed but not restored yet: not the reader's position.
            if reader.hidden.get() {
                return;
            }
            // See `trim_is_due`. Any settle counts, ours included, so a touch
            // reader who only returns to the bottom through the
            // scroll-to-latest button still trims (#505 review).
            //
            // Deferred: this runs from a raw JS callback with no Dioxus scope,
            // and `window_items` is a signal the render subscribes to. See
            // .claude/rules/dioxus-signal-safety.md.
            if trim_is_due(&container, reader.window_overgrown.get(), &reader) {
                reader.window_overgrown.set(false);
                let reader = reader.clone();
                let epoch = reader.room_epoch.get();
                let mut window_items = window_items;
                crate::util::defer(move || {
                    // Re-check: the reader may have moved, switched rooms or hidden
                    // the panel since the settle.
                    if reader.room_epoch.get() != epoch {
                        return;
                    }
                    if !chat_scroll_container().is_some_and(|c| trim_is_due(&c, true, &reader)) {
                        // Still overgrown; a later settle at the bottom may trim.
                        reader.window_overgrown.set(true);
                        return;
                    }
                    *reader.window_anchor.borrow_mut() = None;
                    window_items.set(INITIAL_WINDOW_ITEMS);
                });
            }
            // A settle in the same frame as a chat-area height change, before
            // the ResizeObserver has handled it, would capture the new height
            // over a view the observer has yet to correct, and the correction
            // would be lost. The observer corrects from the last capture
            // instead, and the next settle captures.
            let observed = reader.observed_height.get();
            if observed == 0 || container.client_height() == observed {
                remember_reading_position(&reader);
            }
            note_newest_seen(&reader);
        }) as Box<dyn FnMut()>)
    };
    let settle_fn: js_sys::Function = settle.as_ref().unchecked_ref::<js_sys::Function>().clone();
    // Leaked deliberately, on the same reasoning as the IntersectionObserver
    // below: `Conversation` mounts once for the app's lifetime (rooms are
    // swapped by CSS, not by unmount) and `use_effect` has no cleanup hook, so
    // there is nothing to disconnect these from.
    settle.forget();

    // `scrollend` fires once, when the position has settled. Where it is
    // missing, a trailing debounce on `scroll` stands in; that listener still
    // measures nothing per event, it only resets a timer.
    let has_scrollend =
        js_sys::Reflect::has(&container, &JsValue::from_str("onscrollend")).unwrap_or(false);
    if has_scrollend {
        let cb = Closure::wrap(Box::new(move |_: web_sys::Event| {
            let _ = settle_fn.call0(&JsValue::NULL);
        }) as Box<dyn FnMut(web_sys::Event)>);
        let _ = container.add_event_listener_with_callback_and_add_event_listener_options(
            "scrollend",
            cb.as_ref().unchecked_ref(),
            &passive,
        );
        cb.forget();
    } else {
        let pending: Rc<std::cell::Cell<Option<i32>>> = Rc::new(std::cell::Cell::new(None));
        let cb = Closure::wrap(Box::new(move |_: web_sys::Event| {
            let Some(window) = web_sys::window() else {
                return;
            };
            if let Some(handle) = pending.take() {
                window.clear_timeout_with_handle(handle);
            }
            if let Ok(handle) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                &settle_fn,
                SCROLL_SETTLE_DEBOUNCE_MS,
            ) {
                pending.set(Some(handle));
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        let _ = container.add_event_listener_with_callback_and_add_event_listener_options(
            "scroll",
            cb.as_ref().unchecked_ref(),
            &passive,
        );
        cb.forget();
    }

    // Ends the end hold (10c decision 13) the first time the reader scrolls
    // the newest message off screen. Returns at once unless a hold is on, and
    // a hold lasts only from an explicit request to the reader's first scroll,
    // so the scroll path stays as light as #151 needs.
    let release_hold = Closure::wrap(Box::new(move |_: web_sys::Event| {
        let Some(held_top) = reader.end_hold.get() else {
            return;
        };
        let Some(container) = chat_scroll_container() else {
            return;
        };
        let top = container.scroll_top();
        let from_end = container.scroll_height() - top - container.client_height();
        // Only a move UP counts. A row growing leaves `scrollTop` alone (the
        // ResizeObserver then takes the view back to the end), and a clamp
        // when the history shrinks lands on the end.
        if top < held_top && from_end as f64 > NEWEST_IN_VIEW_SLACK_PX {
            reader.end_hold.set(None);
        } else {
            reader.end_hold.set(Some(top));
        }
    }) as Box<dyn FnMut(web_sys::Event)>);
    let _ = container.add_event_listener_with_callback_and_add_event_listener_options(
        "scroll",
        release_hold.as_ref().unchecked_ref(),
        &passive,
    );
    release_hold.forget();

    true
}

/// The two affordances the no-room screen must offer in EVERY load state
/// (freenet/river#509).
///
/// * The #159 quickstart invite link. A brand-new user has `room_count == 0`,
///   so they are in an unresolved state for the whole load window; putting the
///   link only on the resolved screen would hide their one concrete next step
///   for exactly as long as they need it.
/// * The connection pill. On mobile the rooms rail is `display:none`, so this
///   is the only copy of it a phone user can see (Bug #5, Ivvor 2026-05-17).
///   It matters most in the unresolved states: a node that never connects
///   leaves `ROOMS_LOAD_STATE` at its `Loading` default forever, because
///   `begin_load_attempt` — which arms the 60s backstop — runs only after a
///   successful connect. Without the pill that user watches a spinner with
///   nothing on screen saying the socket is down. It is also what makes the
///   failed state's "Check your connection and try again" actionable.
///
/// One component so the three unresolved arms and the Welcome arm cannot drift.
#[component]
fn NoRoomFooter() -> Element {
    rsx! {
        p { class: "text-text-muted mt-3",
            a {
                class: "text-accent hover:underline",
                href: "https://freenet.org/quickstart#invite-form",
                target: "_blank",
                rel: "noopener noreferrer",
                "Click here to get an invitation to channel \"Freenet Official\""
            }
        }
        div { class: "mt-8 md:hidden",
            crate::components::members::ConnectionStatusIndicator {}
        }
    }
}

#[component]
pub fn Conversation() -> Element {
    // The open room as the last render whose `ROOMS` read succeeded saw it.
    // See `current_room_data` below.
    let last_good_room = use_hook(|| {
        Rc::new(std::cell::RefCell::new(
            None::<(ed25519_dalek::VerifyingKey, Rc<crate::room_data::RoomData>)>,
        ))
    });
    let current_room_data = {
        let current_room = CURRENT_ROOM.read();
        if let Some(key) = current_room.owner_key {
            // try_read() so a borrowed ROOMS can't panic the render. A
            // contended read renders the open room as last seen, never
            // nothing: with no rows the container collapses, `scrollTop`
            // drops to 0, and nothing puts the reader back when the rows
            // return. Only ever the same room's data (`last_good_for_room`).
            match ROOMS.try_read() {
                Ok(rooms) => {
                    let fresh = rooms.map.get(&key).cloned().map(Rc::new);
                    *last_good_room.borrow_mut() = fresh.clone().map(|room| (key, room));
                    fresh
                }
                Err(_) => {
                    // This render holds no `ROOMS` subscription; the nudge
                    // renders it again (freenet/river#555).
                    crate::util::signal_guard::anchor();
                    crate::util::signal_guard::schedule_nudge();
                    last_good_for_room(&last_good_room.borrow(), &key)
                }
            }
        } else {
            None
        }
    };
    // Drives the scroll-to-latest button: "is the newest message's bottom on
    // screen right now?".
    let mut is_at_bottom = use_signal(|| true);
    // How many trailing display items the history renders. Grows only when the
    // reader reaches the top of what is rendered — see `INITIAL_WINDOW_ITEMS`.
    let mut window_items = use_signal(|| INITIAL_WINDOW_ITEMS);
    // See `ReaderPosition`.
    let reader_position = use_hook(|| Rc::new(ReaderPosition::default()));
    // Whether the opening snap for the CURRENT room has landed. The backfill
    // sentinel only mounts once this is true: a freshly-opened >window room
    // renders at `scrollTop = 0` for a beat before the snap runs, and a
    // sentinel mounted during that beat is intersecting, fires, and cascades
    // the backfill until the whole room is rendered (#501 H2). A signal, not a
    // `Cell`, because the sentinel's `if` in rsx renders from it. Set by
    // `complete_scroll_request`.
    let mut opening_snap_done = use_signal(|| false);

    // Reset the windowing Cells the moment THIS render is for a different
    // room — not only in the effect below, which runs AFTER the first render
    // of the new room has already resolved against the OLD room's anchor and
    // requested size (cloning and patching up to a fully-backfilled room's
    // depth for one wasted frame; #505 review). Cells are safe to write
    // during render; the signals (`window_items`, `opening_snap_done`) still
    // reset in the effect, so this render substitutes `INITIAL_WINDOW_ITEMS`
    // below and keeps the sentinel unmounted until they catch up.
    let room_changed_this_render = {
        // `Option<Option<..>>` so the very first render (no room recorded yet)
        // also counts as a change and starts from a clean window.
        let prev_render_room = use_hook(|| {
            Rc::new(std::cell::Cell::new(
                None::<Option<ed25519_dalek::VerifyingKey>>,
            ))
        });
        let room = CURRENT_ROOM.read().owner_key;
        let changed = prev_render_room.get() != Some(room);
        if changed {
            prev_render_room.set(Some(room));
            *reader_position.window_anchor.borrow_mut() = None;
            *reader_position.window_tail.borrow_mut() = None;
            // Never restore the old room's position. `hidden` stays so the
            // reveal still opens the new room.
            *reader_position.anchor.borrow_mut() = None;
            reader_position.has_newer.set(false);
            // The new room opens at its newest message; this also cancels a
            // request made in the old room, and drops the captures taken from
            // the OLD room's geometry, which must not restore into the new
            // room (#505 re-review).
            reader_position.request_end(room);
            reader_position.extend_newer.set(false);
            reader_position
                .room_epoch
                .set(reader_position.room_epoch.get().wrapping_add(1));
            // The old room's hold and range end with it (10c decision 13).
            reader_position.forget_range();
            reader_position.window_rendered.set(0);
            reader_position.window_overgrown.set(false);
        } else if reader_position
            .request
            .get()
            .is_some_and(|request| request.room != room)
        {
            // Made for a room this history no longer shows.
            reader_position.request.set(None);
        }
        changed
    };

    // Re-window when the reader opens a DIFFERENT room. The window means "how
    // far back have I looked in THIS room", so carrying it across rooms would
    // render a freshly-opened room to the depth of the last one.
    //
    // Guarded on an ACTUAL key change: Dioxus re-runs the effect on any write
    // to `CURRENT_ROOM`, and re-selecting the already-open room in the sidebar
    // rewrites it with the same key. Without the guard, that would collapse a
    // window the reader had scrolled back through — dropping them to the newest
    // 60 items mid-read.
    {
        let prev_windowed_room =
            use_hook(|| Rc::new(std::cell::Cell::new(None::<ed25519_dalek::VerifyingKey>)));
        use_effect(move || {
            let room = CURRENT_ROOM.read().owner_key;
            // The Cells (anchor, rendered, overgrown) were already reset by
            // the render-side check above; only the SIGNALS reset here,
            // because writing them from render would re-enter the render.
            if prev_windowed_room.get() != room {
                prev_windowed_room.set(room);
                window_items.set(INITIAL_WINDOW_ITEMS);
                // Re-gate the backfill sentinel; see `opening_snap_done`.
                opening_snap_done.set(false);
                // Hide the scroll-to-latest button until the observer reports
                // on the new room.
                is_at_bottom.set(true);
            }
        });
    }

    // Put the view back where the reader was after a backfill. The revealed
    // items land ABOVE the current offset, so without this the history jumps by
    // their full height. Restoring the offset is also what stops the backfill
    // cascading: the sentinel ends up above the viewport again, so it stops
    // intersecting until the reader scrolls back up to it.
    #[cfg(target_arch = "wasm32")]
    {
        let reader_position = reader_position.clone();
        use_effect(move || {
            // Subscribe, so this runs after the render that added the items.
            let _ = window_items();
            let Some(anchor) = reader_position.backfill_anchor.borrow_mut().take() else {
                return;
            };
            // An explicit request owns the view until it lands (#501 H3).
            if reader_position.request.get().is_some() {
                return;
            }
            let Some(container) = chat_scroll_container() else {
                return;
            };
            // Synchronous, not deferred: Dioxus has already patched the DOM
            // when effects run, and reading layout here forces the reflow the
            // restore needs. A macrotask hop let the browser paint one frame
            // of the prepended rows at the wrong offset before the
            // reposition landed — a flicker native scroll anchoring used to
            // mask (#505 review).
            //
            // MEASURED, not height-delta'd: the probed head row's `offsetTop`
            // shift is exactly the height prepended ABOVE the viewport. The
            // raw `scrollHeight` delta also counts an arrival batched into
            // the same patch BELOW the viewport, and over-shifts the reader
            // by its height (#505 re-review). The delta method survives only
            // as the fallback for a probe row pruned in the same patch.
            let shift = match history_row_offset_top(&anchor.probe_key) {
                Some(post_top) => post_top - anchor.probe_top,
                None => container.scroll_height() - anchor.scroll_height,
            };
            if shift > 0 {
                container.set_scroll_top(anchor.scroll_top + shift);
            }
        });
    }
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
            // freenet/river#555: anchor before the fallible ROOMS read, so a
            // contended pass cannot strand the room title on a stale value.
            crate::util::signal_guard::anchor();
            let current_room = CURRENT_ROOM.read();
            if let Some(key) = current_room.owner_key {
                let Ok(rooms) = ROOMS.try_read() else {
                    crate::util::signal_guard::schedule_nudge();
                    return "No Room Selected".to_string();
                };
                if let Some(room_data) = rooms.map.get(&key) {
                    return room_data.display_name();
                }
            }
            "No Room Selected".to_string()
        }
    });

    // Unread activity waiting behind the mobile rooms panel: rooms OTHER
    // than the current one, plus inbound DMs (the DM rail lives in that
    // panel). Drives the badge on the mobile hamburger buttons below so a
    // user deep in one room can tell there are new messages elsewhere.
    // Re-runs when ROOMS mutates (message arrival, read-marker advance) or
    // CURRENT_ROOM changes — both read inside the helper.
    let panel_unread = use_memo(count_unread_behind_rooms_panel);

    // The current room's notification mode, for the header bell icon + tooltip.
    // Absent entry means the default (`All`).
    let current_notification_mode = use_memo(move || {
        // freenet/river#555: anchor before the fallible ROOMS read.
        crate::util::signal_guard::anchor();
        let current_room = CURRENT_ROOM.read();
        let Some(key) = current_room.owner_key else {
            return NotificationMode::All;
        };
        match ROOMS.try_read() {
            Ok(rooms) => rooms
                .notification_modes
                .get(&key)
                .copied()
                .unwrap_or_default(),
            Err(_) => {
                crate::util::signal_guard::schedule_nudge();
                NotificationMode::default()
            }
        }
    });

    // Memoize room description as rendered HTML (markdown)
    let current_room_description_html = use_memo({
        move || {
            // freenet/river#555: anchor before the fallible ROOMS read.
            crate::util::signal_guard::anchor();
            let current_room = CURRENT_ROOM.read();
            if let Some(key) = current_room.owner_key {
                let Ok(rooms) = ROOMS.try_read() else {
                    crate::util::signal_guard::schedule_nudge();
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
                    let text = unseal_text_or_placeholder(sealed_desc, &room_data.secrets);
                    if text.is_empty() {
                        return None;
                    }
                    return Some(description_to_html(&text, running_behind_freenet_gateway()));
                }
            }
            None
        }
    });

    // Memoize expensive message grouping (decryption + markdown parsing)
    // This prevents re-computing on every render/keystroke
    // Returns (groups, self_member_id, member_names) so we can highlight user's reactions and show names in tooltips
    //
    // `last_good_groups` is the memo's last value and the room it was computed
    // for. A contended pass returns it rather than `None`, which would empty
    // the history, for the reason `current_room_data` above keeps the room.
    let last_good_groups = use_hook(|| {
        Rc::new(std::cell::RefCell::new(
            None::<(ed25519_dalek::VerifyingKey, Option<MessageGroupsValue>)>,
        ))
    });
    let message_groups = use_memo(move || {
        // Anchor FIRST: a contended `ROOMS.try_read()` below registers no
        // subscription (dioxus-signals `signal.rs:409` returns before
        // `subscribe`), and a memo clears its dependency set on every pass
        // (dioxus-core `reactive_context.rs:196`). Without the anchor this memo
        // came out of a contended pass subscribed only to CURRENT_ROOM, so it
        // stopped reacting to new messages until the reader switched rooms or
        // reloaded -- while rendering "No messages yet. Start the conversation!"
        // into a room full of history. freenet/river#555.
        crate::util::signal_guard::anchor();
        let key = CURRENT_ROOM.read().owner_key?;
        let Ok(rooms) = ROOMS.try_read() else {
            crate::util::signal_guard::schedule_nudge();
            // This room's groups as last computed: another room's never.
            return last_good_for_room(&last_good_groups.borrow(), &key).flatten();
        };
        let groups = rooms.map.get(&key).and_then(|room_data| {
            let room_state = &room_data.room_state;
            // Check if there are any displayable messages
            if room_state
                .recent_messages
                .display_messages()
                .next()
                .is_some()
            {
                // Only the PUBLIC half is needed here, and only for
                // cosmetics: own-reaction highlighting, `is_self` grouping,
                // deputy badges. `None` (no locally-known identity) is
                // carried through rather than returned: bailing here would
                // render "No messages yet. Start the conversation!" over a
                // room full of history, which is the exact wrong-render
                // freenet/river#555 was about. Nothing is mis-attributed —
                // an unknown identity matches no member.
                let self_member_id: Option<MemberId> = room_data.self_member_id();
                // Build member name lookup (reaction tooltips, @mention chips).
                let member_names: HashMap<MemberId, String> = room_state
                    .member_info
                    .member_info
                    .iter()
                    .map(|ami| {
                        (
                            ami.member_info.member_id,
                            display_nickname(
                                &ami.member_info.preferred_nickname,
                                &room_data.secrets,
                            ),
                        )
                    })
                    .collect();
                // Which authors show a 🛡 shield in this viewer's view.
                // Computed once here, not per message: the maps behind it
                // are O(members) to build.
                // The badge map is VIEWER-relative (which deputies could
                // ban *you*), so with no identity there is no viewer to be
                // relative to and the shields degrade to "no badge" — an
                // empty map, exactly what a room with no deputies produces.
                // The helper keeps its `MemberId` parameter: it has ~30
                // call sites, and a fabricated viewer id here would be
                // worse than none.
                let deputy_badges = match self_member_id {
                    Some(self_member_id) => deputy_badges_for_viewer(
                        &room_state.members,
                        &room_state.member_info,
                        &room_data.secrets,
                        MemberId::from(&key),
                        self_member_id,
                    ),
                    None => HashMap::new(),
                };
                // The ⚠ impersonation checker, from the SAME badge map, so
                // "who is a deputy" has one answer across the conversation
                // and the member list. Built once here, like the badges.
                let impersonation = impersonation_checker_for_viewer(
                    &room_state.member_info,
                    &room_data.secrets,
                    MemberId::from(&key),
                    &deputy_badges,
                );
                // Borrowed once per pass, not once per message. The old
                // per-message `get_delay_secs` read the same global from
                // inside the loop, so for any room with messages this is
                // the same subscription taken once instead of N times. (A
                // room with no displayable messages never reached that read
                // and so did not subscribe; now it does. Harmless, and
                // noted so the claim is not overstated.)
                let receive_times = crate::components::app::receive_times::RECEIVE_TIMES.read();
                let groups = group_messages(
                    &room_state.recent_messages,
                    &room_state.member_info,
                    self_member_id,
                    &room_data.secrets,
                    &member_names,
                    &deputy_badges,
                    &impersonation,
                    MemberId::from(&key),
                    MessageClock {
                        receive_times: &receive_times,
                        // One "now" for the whole pass. Only reached by
                        // messages with no recorded arrival time.
                        fallback_now: Utc::now(),
                    },
                );
                return Some(Rc::new((groups, self_member_id, member_names)));
            }
            None
        });
        *last_good_groups.borrow_mut() = Some((key, groups.clone()));
        groups
    });

    // Use IntersectionObserver to track whether the user is near the bottom of the
    // chat scroll container.  This replaces the old `onscroll` handler that performed
    // DOM queries (scrollTop / clientHeight / scrollHeight) on every scroll event,
    // causing visible scroll-bar jank on mobile (issue #151).
    //
    // A 1px invisible sentinel div sits right under the newest rendered row,
    // inside `#chat-content` and above its bottom padding, so it intersects
    // exactly while that row's bottom is on screen, give or take
    // `NEWEST_IN_VIEW_SLACK_PX` of rootMargin. The observer fires only on
    // intersection changes, so there is zero work during normal scrolling.
    #[cfg(target_arch = "wasm32")]
    let observer_reader = reader_position.clone();
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

        let reader_position = observer_reader.clone();
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
                // The reader scrolled to the newest message, or it came back
                // into view: the read rule may mark it.
                if intersecting {
                    note_newest_seen(&reader_position);
                }
            }
        }) as Box<dyn FnMut(js_sys::Array)>);

        let options = web_sys::IntersectionObserverInit::new();
        options.set_root(Some(&root));
        // Only fractional-layout slack below the viewport edge: a newest
        // message whose bottom is any further down offers Latest.
        options.set_root_margin(&format!("0px 0px {NEWEST_IN_VIEW_SLACK_PX}px 0px"));
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

    // The settle listener: reading-position capture and the bottom trim.
    // Installed once, in its own effect, because it must outlive every
    // re-render of the history. Reading `message_groups` only makes the effect
    // re-runnable, so a first attempt that found no container yet (nothing
    // rendered) gets another chance; `installed` is set only on success.
    #[cfg(target_arch = "wasm32")]
    {
        let reader_position = reader_position.clone();
        let installed = use_hook(|| Rc::new(std::cell::Cell::new(false)));
        use_effect(move || {
            let _retry_on_content_change = message_groups.read().is_some();
            if installed.get() {
                return;
            }
            if install_scroll_settle_listener(window_items, reader_position.clone()) {
                installed.set(true);
            }
        });
    }

    // Keep the reader's view still when the window head is swapped for a
    // later item — the head was pruned out from under the anchor (an at-cap
    // room drains its oldest message on every arrival), or newer paging slid
    // the start past it (#505 review, blocker 1). With `overflow-anchor: none`
    // the browser no longer compensates, so this effect is the one piece of
    // scroll anchoring reimplemented under our own control. Re-measuring the
    // row the render captured (see `reposition_pending`) gives exactly how far
    // the content above the viewport shifted, date-separator churn included.
    // Synchronous — post-patch, pre-paint — for the same no-flicker reason as
    // the backfill restore above.
    //
    // Known limitation, deliberately accepted: a mid-window removal (a
    // deletion, a ban purge) or late-loading media above the viewport still
    // shifts the reader — rare events, versus the every-arrival churn this
    // compensates (#507). Full generality is what browser scroll anchoring
    // does; it stays off because Safari before 27 has none.
    #[cfg(target_arch = "wasm32")]
    {
        let reader_position = reader_position.clone();
        use_effect(move || {
            // Subscribe: heads swap on content changes and on newer pages.
            let _ = message_groups.read().is_some();
            let _ = window_items();
            let Some(RepositionAnchor {
                probe_key,
                probe_top: pre_top,
                scroll_top: pre_scroll_top,
            }) = reader_position.reposition_pending.borrow_mut().take()
            else {
                return;
            };
            // An explicit request owns the view until it lands.
            if reader_position.request.get().is_some() {
                return;
            }
            let Some(container) = chat_scroll_container() else {
                return;
            };
            // Hidden rows measure 0; the reveal restores the reader instead.
            if !history_has_layout(&container) {
                return;
            }
            let Some(post_top) = history_row_offset_top(&probe_key) else {
                return;
            };
            let shift = post_top - pre_top;
            if shift != 0 {
                // From the PRE-patch offset; see `reposition_pending`.
                let target = (pre_scroll_top + shift).max(0);
                container.set_scroll_top(target);
            }
        });
    }

    // Finish a pending scroll request after the render it waits for: the new
    // room's rows, the sent message, or the latest range Latest selected. A
    // hidden panel's request is finished by the reveal instead (see the
    // ResizeObserver below). Then check the read rule against what the
    // render left on screen: a deletion can bring the newest message into
    // view, and a short room shows an arrival in full.
    #[cfg(target_arch = "wasm32")]
    {
        let reader_position = reader_position.clone();
        use_effect(move || {
            let _ = message_groups.read().is_some();
            let _ = window_items();
            complete_scroll_request(&reader_position, opening_snap_done);
            note_newest_seen(&reader_position);
        });
    }

    // The tab coming back: whatever is on screen now counts as seen, which
    // nothing else would notice (no scroll, no resize, no observer change).
    #[cfg(target_arch = "wasm32")]
    {
        let reader_position = reader_position.clone();
        use_effect(move || {
            // Anchored before the fallible read, this effect's only
            // subscription (freenet/river#555).
            crate::util::signal_guard::anchor();
            let Ok(visible) = crate::components::app::document_title::DOCUMENT_VISIBLE.try_read()
            else {
                crate::util::signal_guard::schedule_nudge();
                return;
            };
            if *visible {
                note_newest_seen(&reader_position);
            }
        });
    }

    // The container's size is the history's only notice that the chat panel
    // was hidden (it measures 0 behind the mobile Rooms or Members panel) or
    // shown again, and that the chat area changed height (the composer, the
    // phone keyboard, a toolbar). `#chat-content` is observed too, for one job
    // only: while the end hold is on (10c decision 13), a row changing height
    // takes the view back to the end. Observed once, from its own effect;
    // `message_groups` only makes the effect re-runnable until the container
    // exists.
    #[cfg(target_arch = "wasm32")]
    {
        let reader_position = reader_position.clone();
        let observed = use_hook(|| Rc::new(std::cell::Cell::new(false)));
        use_effect(move || {
            use wasm_bindgen::prelude::*;

            let _retry_on_content_change = message_groups.read().is_some();
            if observed.get() {
                return;
            }
            let Some(container) = chat_scroll_container() else {
                return;
            };

            let reader_position = reader_position.clone();
            let cb = Closure::wrap(Box::new(move |_: js_sys::Array| {
                let Some(container) = chat_scroll_container() else {
                    return;
                };
                if !history_has_layout(&container) {
                    reader_position.hidden.set(true);
                    reader_position.observed_height.set(0);
                    // The view the hold kept is gone (decision 13).
                    reader_position.end_hold.set(None);
                    return;
                }
                let height = container.client_height();
                let previous = reader_position.observed_height.replace(height);
                // Revealed: finish a request made while hidden (a room opened
                // behind the panel), else put the reader back on their row.
                if reader_position.hidden.replace(false) {
                    if !complete_scroll_request(&reader_position, opening_snap_done) {
                        let anchor = reader_position.anchor.borrow().clone();
                        if let Some(anchor) = anchor {
                            restore_reading_anchor(&anchor, 0);
                        }
                        // The revealed height need not be the one captured.
                        remember_reading_position(&reader_position);
                        note_newest_seen(&reader_position);
                    }
                    return;
                }
                // Held at the end since an explicit request: back to the end,
                // whatever changed size (decision 13).
                if keep_end_held(&reader_position, &container) {
                    return;
                }
                // The chat area changed height: keep the view's BOTTOM edge,
                // so the top gets covered or uncovered and a reader at the end
                // keeps seeing the newest message (10c decision 10). Width
                // alone, or the history alone, leaves the height alone and
                // gets no correction.
                if previous == 0 || height == previous {
                    return;
                }
                // Exact: the reading rows go back to where they were, relative
                // to the bottom edge, as of the last capture. Computed from
                // that state, not from `scrollTop`, so a clamp or a growth in
                // between (or several resizes before the next settle) cannot
                // be counted twice. Not across a width change: the rewrap
                // moved the rows, and width reflow gets no correction.
                let anchor = reader_position.anchor.borrow().clone();
                if anchor.is_some_and(|anchor| {
                    anchor.view_width == container.client_width()
                        && restore_reading_anchor(&anchor, height - anchor.view_height)
                }) {
                    return;
                }
                // No usable remembered row: keep the edge arithmetically.
                let top = container.scroll_top();
                if height < previous {
                    container.set_scroll_top(top + (previous - height));
                } else if top + SCROLL_TOP_SLACK_PX < max_scroll_top(&container) {
                    // At the new maximum the browser's clamp already holds
                    // the end; a reader who was within the growth of the end
                    // lands on it.
                    container.set_scroll_top(top - (height - previous));
                }
            }) as Box<dyn FnMut(js_sys::Array)>);

            if let Ok(observer) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
                observer.observe(&container);
                if let Some(content) = web_sys::window()
                    .and_then(|w| w.document())
                    .and_then(|d| d.get_element_by_id("chat-content"))
                {
                    observer.observe(&content);
                }
                // Leaked for the same reason as the observers above.
                cb.forget();
                observed.set(true);
            }
        });
    }

    // Handler for toggling a reaction on a message (add or remove)
    let handle_toggle_reaction = {
        // Captures NOTHING: this closure is cloned once per rendered row, so a
        // captured RoomData would be a full room-state copy per message. See
        // `current_room_data_snapshot`.
        move |target_message_id: MessageId, emoji: String| {
            let open_room = { CURRENT_ROOM.read().owner_key };
            if let (Some(current_room), Some(current_room_data)) =
                (open_room, current_room_data_snapshot())
            {
                // Reactions are signed locally. Without the private key we
                // cannot author one, so the toggle degrades to a no-op rather
                // than sending an unsigned (contract-rejected) message.
                let Some(self_sk) = current_room_data.signing_key().cloned() else {
                    warn!("Cannot toggle reaction: local signing key unavailable for this room");
                    return;
                };
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

                // Retained even though this block no longer awaits anything:
                // it keeps the body off the event handler's stack. The guard
                // that actually protects the ROOMS write is the
                // `crate::util::defer` below (a real setTimeout) — inlining
                // this block would put everything up to it back on the
                // handler's stack (freenet/river#512 review).
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

                        let signature =
                            crate::signing::sign_message_locally(&message_bytes, &self_sk);

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
        // Captures NOTHING — see `current_room_data_snapshot`.
        move |target_message_id: MessageId| {
            let open_room = { CURRENT_ROOM.read().owner_key };
            if let (Some(current_room), Some(current_room_data)) =
                (open_room, current_room_data_snapshot())
            {
                // Deletes are signed locally; with no private key there is
                // nothing valid to send, so the action is a no-op.
                let Some(self_sk) = current_room_data.signing_key().cloned() else {
                    warn!("Cannot delete message: local signing key unavailable for this room");
                    return;
                };
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

                // Retained even though this block no longer awaits anything:
                // it keeps the body off the event handler's stack. The guard
                // that actually protects the ROOMS write is the
                // `crate::util::defer` below (a real setTimeout) — inlining
                // this block would put everything up to it back on the
                // handler's stack (freenet/river#512 review).
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

                    let signature = crate::signing::sign_message_locally(&message_bytes, &self_sk);

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
        // Captures NOTHING — see `current_room_data_snapshot`.
        move |target_message_id: MessageId, new_text: String| {
            if new_text.is_empty() {
                warn!("Edit text is empty");
                return;
            }
            let open_room = { CURRENT_ROOM.read().owner_key };
            if let (Some(current_room), Some(current_room_data)) =
                (open_room, current_room_data_snapshot())
            {
                // Edits are signed locally; with no private key there is
                // nothing valid to send, so the action is a no-op.
                let Some(self_sk) = current_room_data.signing_key().cloned() else {
                    warn!("Cannot edit message: local signing key unavailable for this room");
                    return;
                };
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

                // Safety net (see the send-path twin): the edit form disables
                // Save when the encoded action exceeds max_message_size. If
                // this fires, the UI gate drifted from the body construction
                // and the edit is silently dropped — fix the drift.
                let max_size = current_room_data
                    .room_state
                    .configuration
                    .configuration
                    .max_message_size;
                let measured =
                    RoomMessageBody::measure_edit(target_message_id.clone(), &new_text, is_private);
                if measured > max_size {
                    error!(
                        "BUG: over-size edit passed the UI gate ({} encoded bytes, max {}) — edit dropped",
                        measured, max_size
                    );
                    return;
                }

                // Retained even though this block no longer awaits anything:
                // it keeps the body off the event handler's stack. The guard
                // that actually protects the ROOMS write is the
                // `crate::util::defer` below (a real setTimeout) — inlining
                // this block would put everything up to it back on the
                // handler's stack (freenet/river#512 review).
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

                    let signature = crate::signing::sign_message_locally(&message_bytes, &self_sk);

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
        let reader_position = reader_position.clone();
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
                // Clone what we need for the async block.
                // Sending signs locally: with no private key held for this
                // room the send degrades to a logged no-op, the same shape as
                // the "no room selected" / "room not loaded" bails above,
                // rather than emitting an unsignable message.
                let Some(self_sk) = current_room_data.signing_key().cloned() else {
                    error!("Cannot send message: local signing key unavailable for this room");
                    return;
                };
                let room_state_clone = current_room_data.room_state.clone();
                let is_private = current_room_data.is_private();
                // Copy the secret data (get_secret returns Option<(&[u8; 32], u32)>)
                let secret_opt: Option<([u8; 32], u32)> = current_room_data
                    .get_secret()
                    .map(|(secret, version)| (*secret, version));

                // Cloned into the async send so the scroll request is made
                // ONLY after the delta applies locally — a rejected send
                // (empty, over-size, serialize/sign/delta failure) then leaves
                // the scroll position untouched rather than taking a later
                // unrelated message to the end (#402 review).
                let reader_position = reader_position.clone();
                // Retained even though this block no longer awaits anything:
                // it keeps the body off the event handler's stack. The guard
                // that actually protects the ROOMS write is the
                // `crate::util::defer` below (a real setTimeout) — inlining
                // this block would put everything up to it back on the
                // handler's stack (freenet/river#512 review).
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
                    // Should be unreachable — the input gate measures the same
                    // encoded size via RoomMessageBody::measure_* (pinned equal
                    // to content_len() by river-core tests). If this fires, the
                    // measure helpers have drifted from the body construction
                    // above and the draft is already cleared: the message is
                    // LOST, which is the HostFat bug. Fix the drift, don't
                    // relax this check.
                    let content_size = content.content_len();
                    let max_size = room_state_clone
                        .configuration
                        .configuration
                        .max_message_size;
                    if content_size > max_size {
                        error!(
                            "BUG: over-size message passed the input gate ({} encoded bytes, max {}) — measure_* drifted from body construction; message dropped",
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

                    // Sign locally and synchronously. Do NOT reinstate a
                    // delegate round-trip here — see `sign_message_locally`
                    // (freenet/river#512): it cannot change these bytes, and
                    // awaiting it is what made a sent message take seconds to
                    // appear.
                    crate::util::debug_log("[send] signing message...");
                    let signature = crate::signing::sign_message_locally(&message_bytes, &self_sk);
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
                            // take the view to it once, after the render that
                            // adds it — but only if the user is still viewing
                            // the room this send targeted. This runs two task
                            // hops after the keypress (`spawn_local`, then
                            // `defer`'s setTimeout), so they may have switched
                            // rooms (#402 review).
                            if CURRENT_ROOM.peek().owner_key == Some(current_room) {
                                reader_position.request_end(Some(current_room));
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
                    let open_room_details = move || {
                        crate::util::defer(move || {
                            if let Some(current_room) = CURRENT_ROOM.read().owner_key {
                                EDIT_ROOM_MODAL.with_mut(|modal| {
                                    modal.room = Some(current_room);
                                });
                            }
                        });
                    };
                    rsx! {
                        div { class: "flex-shrink-0 px-3 md:px-6 py-3 border-b border-border bg-panel",
                            div {
                                "data-testid": "room-header-row",
                                class: "flex items-center justify-between gap-2 md:gap-3 max-w-4xl mx-auto",
                                // Mobile: hamburger to open rooms panel. `mr-1` plus the row
                                // `gap-2` keep this switch-rooms button clear of the room-name
                                // tap target so a touch user does not open the room-details
                                // modal by mistake when reaching for the room list (#402).
                                button {
                                    // `relative` anchors the unread badge overlay.
                                    class: "relative md:hidden flex-shrink-0 mr-1 p-2 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
                                    "data-testid": "hamburger-rooms-button",
                                    // The button's aria-label overrides descendant text in
                                    // accessible-name computation, so the unread count must
                                    // live HERE — the badge below is visual-only.
                                    "aria-label": if panel_unread() > 0 {
                                        format!("Open room list, {} unread", panel_unread())
                                    } else {
                                        "Open room list".to_string()
                                    },
                                    onclick: {
                                        let reader_position = reader_position.clone();
                                        move |_| {
                                            remember_reading_position(&reader_position);
                                            crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Rooms)
                                        }
                                    },
                                    Icon { icon: FaBars, width: 18, height: 18 }
                                    // Unread-elsewhere badge: new messages in OTHER rooms
                                    // (and DMs) are invisible on mobile while a room fills
                                    // the screen — surface them on the room-list button.
                                    if panel_unread() > 0 {
                                        span {
                                            class: "absolute top-0 right-0 flex items-center justify-center min-w-4 h-4 px-1 rounded-full bg-accent text-white text-[10px] font-semibold leading-none pointer-events-none",
                                            "data-testid": "hamburger-unread-badge",
                                            "aria-hidden": "true",
                                            "{panel_unread}"
                                        }
                                    }
                                }
                                // Description is a sibling of the title button, not a child:
                                // `<a>` is interactive content and cannot be nested inside
                                // `<button>` per the HTML spec. Nesting also bubbles link
                                // clicks to the modal-opening onclick handler.
                                div { class: "min-w-0 flex-1",
                                    // Title on the left; `ml-auto` on the (i) pushes (i)
                                    // and the bell to the right edge.
                                    div { class: "flex items-center gap-1 min-w-0",
                                        button {
                                            // `md:-ml-3` pulls only the LEFT hover edge outward on
                                            // desktop (no adjacent hamburger there). On mobile
                                            // the negative margin is dropped so this room-details
                                            // target stays clear of the hamburger (#402).
                                            class: "flex items-center px-3 py-1.5 md:-ml-3 rounded-lg bg-transparent hover:bg-surface transition-colors cursor-pointer min-w-0",
                                            title: "Room details",
                                            "data-testid": "room-title-button",
                                            onclick: move |_| open_room_details(),
                                            h2 { class: "text-lg font-semibold text-text truncate",
                                                "{current_room_label}"
                                            }
                                        }
                                        button {
                                            "data-testid": "room-info-button",
                                            class: "ml-auto flex-shrink-0 p-1.5 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
                                            title: "Room details",
                                            "aria-label": "Room details",
                                            onclick: move |_| open_room_details(),
                                            Icon { icon: FaCircleInfo, width: 16, height: 16 }
                                        }
                                        // Per-room notification preference. Icon reflects state:
                                        // bell = notifying, bell-slash = muted; the tooltip names
                                        // the exact mode. Opens the compact NotificationModal.
                                        // Sized to 16px to pair with the adjacent (i) icon.
                                        {
                                            let mode = *current_notification_mode.read();
                                            let mode_title = match mode {
                                                NotificationMode::All => "Notifications: All messages",
                                                NotificationMode::MentionsAndReplies => "Notifications: Mentions & replies only",
                                                NotificationMode::Muted => "Notifications: Muted",
                                            };
                                            // The browser may be refusing to deliver, which makes
                                            // every mode above inert (freenet/river#510).
                                            let blocked = crate::components::app::notifications::delivery_is_blocked(
                                                mode,
                                                crate::components::app::notifications::current_notification_status(),
                                            );
                                            rsx! {
                                        button {
                                            "data-testid": "notification-bell-button",
                                            class: "relative flex-shrink-0 p-1.5 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
                                            // `title` stays the MODE alone. The blocked state rides
                                            // on `aria-label` and the dot instead, so the tooltip
                                            // keeps naming exactly one thing.
                                            title: mode_title,
                                            "aria-label": if blocked {
                                                format!("{mode_title} (your browser is not delivering notifications)")
                                            } else {
                                                mode_title.to_string()
                                            },
                                            onclick: move |_| {
                                                crate::util::defer(move || {
                                                    if let Some(current_room) = CURRENT_ROOM.read().owner_key {
                                                        NOTIFICATION_MODAL.with_mut(|modal| {
                                                            modal.room = Some(current_room);
                                                        });
                                                    }
                                                });
                                            },
                                            if mode == NotificationMode::Muted {
                                                Icon { icon: FaBellSlash, width: 16, height: 16 }
                                            } else {
                                                Icon { icon: FaBell, width: 16, height: 16 }
                                            }
                                            if blocked {
                                                // Decorative: `aria-hidden` keeps it out of the
                                                // button's accessible name, which the `aria-label`
                                                // above already carries in full. A badge that
                                                // announced itself separately would append to the
                                                // button's name instead of replacing it.
                                                span {
                                                    "data-testid": "notification-blocked-badge",
                                                    "aria-hidden": "true",
                                                    // `ring-panel` needs the hand-written
                                                    // `@utility` in tailwind.css: `--color-panel` is
                                                    // declared on `:root`, outside `@theme`, so
                                                    // Tailwind v4 generates no colour utility for it
                                                    // and `ring-1` would fall back to `currentColor`
                                                    // — a grey ring that turns blue on hover.
                                                    class: "absolute top-0.5 right-0.5 h-2 w-2 rounded-full bg-red-500 ring-1 ring-panel",
                                                }
                                            }
                                        }
                                            }
                                        }
                                    }
                                    if let Some(desc_html) = current_room_description_html.read().as_ref() {
                                        div {
                                            "data-testid": "room-header-description",
                                            class: "prose prose-sm dark:prose-invert max-w-none text-xs text-text-muted truncate [&>p]:m-0 [&>p]:inline",
                                            dangerous_inner_html: "{desc_html}"
                                        }
                                    }
                                }
                                // Mobile: button to open members panel
                                button {
                                    "data-testid": "header-members-button",
                                    class: "md:hidden p-2 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors flex-shrink-0",
                                    onclick: {
                                        let reader_position = reader_position.clone();
                                        move |_| {
                                            remember_reading_position(&reader_position);
                                            crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Members)
                                        }
                                    },
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
                    // `overflow-anchor: none`: our own corrections (the
                    // backfill restore, the head reposition, the reveal restore)
                    // own this container's `scrollTop`. Safari before 27 has no
                    // scroll anchoring, so relying on the browser would jump
                    // every older-history page there by the height it adds, and
                    // two owners of one job would fight. Inline style, not a
                    // Tailwind arbitrary class, so it cannot depend on the class
                    // scanner seeing it.
                    style: "overflow-anchor:none;",
                    id: "chat-scroll-container",
                    div { class: "max-w-4xl mx-auto px-4 py-4", id: "chat-content",
                    {
                        // Set again below when this render's range reaches the
                        // room's latest message.
                        *reader_position.newest_rendered.borrow_mut() = None;
                        // Use memoized message groups to avoid expensive re-computation on keystrokes
                        if current_room_data.is_some() {
                            match message_groups.read().as_deref() {
                                Some((groups, self_member_id, member_names)) => {
                                    // Render only the tail the reader has asked
                                    // for. Slicing BEFORE the clone is the
                                    // point: `GroupedMessage` carries the
                                    // rendered HTML of every message it holds,
                                    // so cloning the whole history here — on
                                    // every re-render — would pay most of the
                                    // cost the window exists to avoid.
                                    // Subscribe every render; the value is
                                    // substituted on a room switch because the
                                    // signal's own reset only lands in the
                                    // NEXT render's effect pass.
                                    let subscribed_window = window_items();
                                    let requested_window = if room_changed_this_render {
                                        INITIAL_WINDOW_ITEMS
                                    } else {
                                        subscribed_window
                                    };
                                    // Re-locate the anchored window by
                                    // IDENTITY: at-cap pruning shifts every
                                    // index, so the stored index is only a
                                    // hint, and the head key alone can vanish
                                    // while its neighbors survive (#505
                                    // blockers; see `WindowAnchor` and
                                    // `relocate_window`).
                                    //
                                    // A pending request (Latest, an own send)
                                    // swaps a held range for the latest one.
                                    let select_latest = reader_position.request.get().is_some()
                                        && reader_position.has_newer.get();
                                    let key_at =
                                        |i: usize, key: &str| display_item_key_matches(&groups[i], key);
                                    let prev_anchor = if select_latest {
                                        None
                                    } else {
                                        reader_position.window_anchor.borrow().clone()
                                    };
                                    let relocated = prev_anchor.as_ref().map(|a| {
                                        relocate_window(groups.len(), a, key_at)
                                    });
                                    let prev_tail = if select_latest {
                                        None
                                    } else {
                                        reader_position.window_tail.borrow().clone()
                                    };
                                    let extend_newer = reader_position.extend_newer.replace(false);
                                    let held_end = prev_tail.as_ref().map(|t| {
                                        let end = relocate_tail(groups.len(), t, key_at)
                                            // Every held item gone: keep the count.
                                            .unwrap_or_else(|| {
                                                relocated.map_or(0, |r| r.start)
                                                    + reader_position.window_rendered.get()
                                            });
                                        if extend_newer {
                                            end + WINDOW_GROWTH_ITEMS
                                        } else {
                                            end
                                        }
                                    });
                                    // Keep the reading row, plus one item
                                    // for a group the view top cuts through.
                                    let keep = if !select_latest {
                                        reader_position
                                            .anchor
                                            .borrow()
                                            .as_ref()
                                            .and_then(|a| locate_reading_item(groups, a))
                                            .map(|i| i.saturating_sub(1))
                                    } else {
                                        None
                                    };
                                    let history_window = HistoryWindow::resolve_held(
                                        groups.len(),
                                        requested_window,
                                        relocated.as_ref().map(|r| r.start),
                                        RangeHold {
                                            end: held_end,
                                            keep,
                                        },
                                    );
                                    // Was rendered content at or above the new
                                    // head removed in this very patch? True
                                    // when the old head is gone (pruned,
                                    // deleted, or re-keyed by a drained first
                                    // message) or now sits BEFORE the window
                                    // (newer paging). The reader needs their
                                    // offset compensated for it — the
                                    // `head_reposition` effect's job — unless
                                    // a pending request is about to move the
                                    // view anyway.
                                    let head_removed = match &relocated {
                                        None => false,
                                        Some(r) => {
                                            !r.head_survived
                                                || history_window.start > r.start
                                        }
                                    };
                                    if head_removed && reader_position.request.get().is_none() {
                                        // The DOM still shows the previous
                                        // render here. See
                                        // `reposition_pending`.
                                        #[cfg(target_arch = "wasm32")]
                                        if let (Some((probe_key, probe_top)), Some(container)) = (
                                            select_reposition_probe(
                                                groups[history_window.start..history_window.end]
                                                    .iter()
                                                    .map(display_item_key),
                                                first_history_row_offset,
                                            ),
                                            // Hidden rows all measure 0.
                                            chat_scroll_container().filter(history_has_layout),
                                        ) {
                                            *reader_position.reposition_pending.borrow_mut() =
                                                Some(RepositionAnchor {
                                                    probe_key,
                                                    probe_top,
                                                    scroll_top: container.scroll_top(),
                                                });
                                        }
                                    }
                                    // Remember where this render started so the
                                    // NEXT one grows instead of sliding (#501).
                                    // Cell/RefCell writes during render are
                                    // fine: inter-render memory, nothing
                                    // renders from them, and re-resolving with
                                    // the values just written is a fixed point.
                                    *reader_position.window_anchor.borrow_mut() = Some(WindowAnchor {
                                        keys: groups[history_window.start..]
                                            .iter()
                                            .take(WINDOW_ANCHOR_KEYS)
                                            .map(display_item_key)
                                            .collect(),
                                        index: history_window.start,
                                    });
                                    // See `grown_window`.
                                    reader_position
                                        .window_rendered
                                        .set(history_window.end - history_window.start);
                                    // Only a held range remembers its end.
                                    *reader_position.window_tail.borrow_mut() = history_window.has_newer.then(|| {
                                        WindowAnchor {
                                            keys: groups[history_window.start..history_window.end]
                                                .iter()
                                                .rev()
                                                .take(WINDOW_ANCHOR_KEYS)
                                                .map(display_item_key)
                                                .collect(),
                                            index: history_window.end - 1,
                                        }
                                    });
                                    reader_position.range_start.set(history_window.start);
                                    reader_position.has_newer.set(history_window.has_newer);
                                    // An arrival ends the end hold; rows that
                                    // only change height keep it (10c
                                    // decision 13). Against the last render,
                                    // so a request's own render (an own send's
                                    // message) comes before its hold.
                                    let rendered = rendered_messages(
                                        &groups[history_window.start..history_window.end],
                                    );
                                    let before = reader_position.rendered.replace(rendered.clone());
                                    if !end_hold_survives(before.as_ref(), rendered.as_ref()) {
                                        reader_position.end_hold.set(None);
                                    }
                                    // The read rule's candidate: only a range
                                    // that reaches the latest message has it.
                                    if !history_window.has_newer {
                                        *reader_position.newest_rendered.borrow_mut() = CURRENT_ROOM
                                            .peek()
                                            .owner_key
                                            .zip(
                                                groups[..history_window.end]
                                                    .last()
                                                    .and_then(display_item_last_message_id),
                                            );
                                    }
                                    // Tell the settle handler whether a
                                    // bottom-settle trim would shrink anything.
                                    reader_position.window_overgrown.set(
                                        requested_window > INITIAL_WINDOW_ITEMS
                                            || history_window.end - history_window.start
                                                > INITIAL_WINDOW_ITEMS,
                                    );
                                    let withheld_newer = groups.len() - history_window.end;
                                    let groups =
                                        groups[history_window.start..history_window.end].to_vec();
                                    let self_member_id = *self_member_id;
                                    let member_names = member_names.clone();
                                    // Room limits for the in-place edit form's
                                    // encoded-size gate (same measure the
                                    // contract enforces on the edit action).
                                    let (edit_max_size, edit_is_private) = current_room_data
                                        .as_ref()
                                        .map(|rd| {
                                            (
                                                rd.room_state
                                                    .configuration
                                                    .configuration
                                                    .max_message_size,
                                                rd.is_private(),
                                            )
                                        })
                                        .unwrap_or((usize::MAX, false));
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
                                    Some(rsx! {
                                        // Backfill trigger while older items
                                        // are held back; see `BACKFILL_LEAD_PX`.
                                        // Outside the `space-y-4` list, where
                                        // it would add a row gap above the
                                        // history. Waits for the opening snap
                                        // (see `opening_snap_done`) and for
                                        // that signal's room-change reset.
                                        if history_window.has_older
                                            && opening_snap_done()
                                            && !room_changed_this_render
                                        {
                                            // Zero-height wrapper: the strip
                                            // inside adds no layout.
                                            div { style: "position:relative;height:0;",
                                                div {
                                                    id: "top-backfill-sentinel",
                                                    // Inline style: the class
                                                    // scanner does not see class
                                                    // names built inside rsx.
                                                    style: "position:absolute;top:0;height:{BACKFILL_LEAD_PX}px;width:1px;",
                                                    onvisible: {
                                                        let reader_position = reader_position.clone();
                                                        move |evt: dioxus::prelude::Event<VisibleData>| {
                                                            if evt.data().is_intersecting().unwrap_or(false) {
                                                                // Capture BEFORE the re-render; see
                                                                // `BackfillAnchor`.
                                                                #[cfg(target_arch = "wasm32")]
                                                                if let (
                                                                    Some(container),
                                                                    Some((probe_key, probe_top)),
                                                                ) = (
                                                                    chat_scroll_container(),
                                                                    first_history_row_identity(),
                                                                ) {
                                                                    *reader_position.backfill_anchor.borrow_mut() =
                                                                        Some(BackfillAnchor {
                                                                            probe_key,
                                                                            probe_top,
                                                                            scroll_top: container
                                                                                .scroll_top(),
                                                                            scroll_height: container
                                                                                .scroll_height(),
                                                                        });
                                                                }
                                                                // See `grown_window`.
                                                                window_items.with_mut(|n| {
                                                                    *n = grown_window(*n, reader_position.window_rendered.get())
                                                                });
                                                            }
                                                        }
                                                    },
                                                }
                                            }
                                        }
                                        div {
                                            class: "space-y-4",
                                            // Stable automation hook for the
                                            // history rows (AGENTS.md test-id
                                            // rule); additive markup only.
                                            "data-testid": "conversation-history",
                                            // Newer items withheld below the
                                            // reader, for tests (additive markup).
                                            "data-newer-withheld": "{withheld_newer}",
                                            {rows.into_iter().map({
                                                let handle_toggle_reaction = handle_toggle_reaction.clone();
                                                let member_names = member_names.clone();
                                                move |row| {
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
                                                        rsx! {
                                                            div {
                                                                key: "{key}",
                                                                // Identity tag for the
                                                                // head-reposition machinery
                                                                // (`history_row_offset_top`).
                                                                "data-item-key": "{key}",
                                                                // For `capture_reading_anchor`.
                                                                "data-anchor-key": "{key}",
                                                                class: "flex justify-center py-1",
                                                                span {
                                                                    class: "text-xs text-text-muted italic",
                                                                    "{text}"
                                                                }
                                                            }
                                                        }
                                                    }
                                                    DisplayRow::Item(DisplayItem::Messages(group)) => {
                                                        let key = group.messages[0].id.clone();
                                                        rsx! {
                                                            // The wrapper exists to carry
                                                            // `data-item-key` — the identity
                                                            // the head-reposition machinery
                                                            // locates rows by — since a
                                                            // component cannot carry a DOM
                                                            // attribute directly. It is the
                                                            // keyed list child, so diffing
                                                            // and `space-y-4` spacing are
                                                            // unchanged.
                                                            div {
                                                                key: "{key}",
                                                                "data-item-key": "{key}",
                                                                MessageGroupComponent {
                                                                group: group,
                                                                self_member_id: self_member_id,
                                                                member_names: member_names,
                                                                max_message_size: edit_max_size,
                                                                is_private: edit_is_private,
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
                                                }
                                            }})}
                                        }
                                        // Newer-history trigger, mirroring the
                                        // backfill strip at the bottom of a
                                        // held range. Its rows land below the
                                        // view; a start slide past the ceiling
                                        // is compensated by the head reposition.
                                        if history_window.has_newer
                                            && opening_snap_done()
                                            && !room_changed_this_render
                                        {
                                            div { style: "position:relative;height:0;",
                                                div {
                                                    id: "bottom-newer-sentinel",
                                                    style: "position:absolute;bottom:0;height:{BACKFILL_LEAD_PX}px;width:1px;",
                                                    onvisible: {
                                                        let reader_position = reader_position.clone();
                                                        move |evt: dioxus::prelude::Event<VisibleData>| {
                                                            if !evt.data().is_intersecting().unwrap_or(false) {
                                                                return;
                                                            }
                                                            // The reading position bounds the slide.
                                                            remember_reading_position(&reader_position);
                                                            reader_position.extend_newer.set(true);
                                                            // A small `window` lets the ceiling slide
                                                            // the start behind a reader paging down.
                                                            window_items.set(INITIAL_WINDOW_ITEMS);
                                                        }
                                                    },
                                                }
                                            }
                                        }
                                    })
                                }
                                None => {
                                    // No rows: nothing to hold at the end.
                                    reader_position.forget_range();
                                    Some(rsx! {
                                        div { class: "flex flex-col items-center justify-center h-64 text-text-muted",
                                            p { "No messages yet. Start the conversation!" }
                                        }
                                    })
                                }
                            }
                        } else {
                            reader_position.forget_range();
                            None
                        }
                    }
                    // Invisible sentinel right under the newest rendered row,
                    // after the history list and above `#chat-content`'s bottom
                    // padding, so its top edge is that row's bottom (the
                    // newer-history strip before it has no height). An
                    // IntersectionObserver watches it instead of using
                    // onscroll, which avoids per-scroll-event DOM queries that
                    // cause scroll jank on mobile (see issue #151), and the
                    // read rule measures it (`sentinel_in_view`).
                    div {
                        id: "bottom-sentinel",
                        class: "h-px pointer-events-none",
                    }
                }
            }
                // Scroll-to-latest button (#402): shown whenever the newest
                // message's bottom is off screen, past a few px of slack (10c
                // decision 4). Reuses the `is_at_bottom` IntersectionObserver
                // state, so it appears after scrolling up even a little, or
                // when an arrival lands below the view, and hides once the
                // newest message's bottom is in view. Handy on every device but
                // especially on touch, where there is no scrollbar to drag.
                //
                // Also shown while a held range withholds newer items.
                if !is_at_bottom() || reader_position.has_newer.get() {
                    LatestButton {
                        aria_label: "Scroll to latest messages",
                        test_id: "scroll-to-bottom",
                        // Do NOT optimistically set `is_at_bottom` here: the
                        // IntersectionObserver flips it (hiding the button) once
                        // the sentinel actually reaches view. #402.
                        onclick: {
                            let reader_position = reader_position.clone();
                            move |_| {
                            // The newest message isn't rendered: request the
                            // latest range; the request completes after the
                            // render that swaps it in.
                            if reader_position.has_newer.get() {
                                let reader_position = reader_position.clone();
                                let mut window_items = window_items;
                                crate::util::defer(move || {
                                    reader_position.request_end(CURRENT_ROOM.peek().owner_key);
                                    window_items.set(INITIAL_WINDOW_ITEMS);
                                });
                            } else {
                                #[cfg(target_arch = "wasm32")]
                                if let Some(container) = chat_scroll_container() {
                                    land_at_end(&reader_position, &container);
                                }
                            }
                        }},
                    }
                }
            }

            // Message input or status
            {
                // Find user's most recent message for up-arrow-to-edit
                let request_edit_last = move |_| {
                    if let Some((groups, _, _)) = message_groups.read().as_deref() {
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
                                let room_is_private = room_data.is_private();
                                // Mentionable members for the @ autocomplete: every member
                                // with a (decrypted) nickname except self, sorted by name.
                                // Public half only. `can_participate` already
                                // returned Ok, so this is Some here; kept as an
                                // Option so an absent key would merely leave
                                // self in the mention list instead of panicking.
                                let self_id = room_data.self_member_id();
                                let mut mention_members: Vec<(MemberId, String)> = room_data
                                    .room_state
                                    .member_info
                                    .member_info
                                    .iter()
                                    .filter(|ami| Some(ami.member_info.member_id) != self_id)
                                    .map(|ami| {
                                        (
                                            ami.member_info.member_id,
                                            display_nickname(
                                                &ami.member_info.preferred_nickname,
                                                &room_data.secrets,
                                            ),
                                        )
                                    })
                                    // `display_nickname` never returns an empty
                                    // string, so the old is-empty filter became
                                    // dead: the placeholder is what to exclude.
                                    .filter(|(_, name)| name != crate::util::display_name::UNNAMED)
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
                                        is_private: room_is_private,
                                        members: mention_members,
                                    }
                                }
                            },
                            Err(SendMessageError::UserNotMember) => {
                                // `can_participate` reports IdentityUnavailable
                                // (below), never UserNotMember, when the key is
                                // absent — so this is Some. Matched rather than
                                // unwrapped so an absent key renders nothing
                                // instead of panicking the whole conversation.
                                match room_data.self_verifying_key() {
                                    Some(user_vk) => rsx! {
                                        NotMemberNotification {
                                            user_verifying_key: user_vk
                                        }
                                    },
                                    None => rsx! {},
                                }
                            },
                            Err(SendMessageError::UserBanned) => rsx! {
                                div { class: "px-4 py-3 mx-4 mb-4 bg-error-bg text-red-700 dark:text-red-400 rounded-lg text-sm",
                                    "You have been banned from sending messages in this room."
                                }
                            },
                            // Same shape as the banned notice above: the
                            // composer is replaced by an explanation.
                            // `NotMemberNotification` cannot be used here — it
                            // needs the verifying key, which is what is missing.
                            Err(SendMessageError::IdentityUnavailable) => rsx! {
                                div { class: "px-4 py-3 mx-4 mb-4 bg-error-bg text-red-700 dark:text-red-400 rounded-lg text-sm",
                                    "This device doesn't hold your key for this room, so you can't send messages here."
                                }
                            },
                        }
                    },
                    None => rsx! {
                        // Mobile: show hamburger to access room list even with no room selected
                        div { class: "md:hidden flex-shrink-0 px-3 py-3 border-b border-border bg-panel",
                            button {
                                // `relative` anchors the unread badge overlay.
                                class: "relative p-2 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
                                "data-testid": "hamburger-rooms-button",
                                // Count in the button's own accessible name; the badge
                                // below is visual-only (see the room-header hamburger).
                                "aria-label": if panel_unread() > 0 {
                                    format!("Open room list, {} unread", panel_unread())
                                } else {
                                    "Open room list".to_string()
                                },
                                onclick: move |_| crate::util::defer(move || *MOBILE_VIEW.write() = MobileView::Rooms),
                                Icon { icon: FaBars, width: 18, height: 18 }
                                // Same unread-elsewhere badge as the room-header
                                // hamburger; with no room selected every room's
                                // unread (plus DMs) counts.
                                if panel_unread() > 0 {
                                    span {
                                        class: "absolute top-0 right-0 flex items-center justify-center min-w-4 h-4 px-1 rounded-full bg-accent text-white text-[10px] font-semibold leading-none pointer-events-none",
                                        "data-testid": "hamburger-unread-badge",
                                        "aria-hidden": "true",
                                        "{panel_unread}"
                                    }
                                }
                            }
                        }
                        // freenet/river#509: this panel is the ONLY thing a
                        // phone user can see while rooms load — below 768px the
                        // rail that carries #397's loading / migrating / failed
                        // states is `display:none`, not unmounted, and the
                        // default mobile view is Chat. Rendering the Welcome
                        // copy unconditionally therefore told a mid-load user
                        // they had no rooms, and hid a FAILED load (and its
                        // Retry button) behind advice to create one.
                        //
                        // Branches on the SAME call the rail makes, so the two
                        // surfaces cannot disagree. Viewport-independent on
                        // purpose: the desktop centre panel had the same
                        // false-empty text.
                        //
                        // Every arm keeps the two things that are useful in any
                        // state: the #159 quickstart invite link (a brand-new
                        // user has `room_count == 0`, so they are in the
                        // unresolved states for the whole load window and would
                        // otherwise lose their only onboarding affordance) and
                        // the connection pill. The pill matters MOST here: a
                        // node that never connects leaves `ROOMS_LOAD_STATE` at
                        // its `Loading` default indefinitely — `begin_load_attempt`,
                        // which arms the 60s backstop, runs only after a
                        // successful connect — so without the pill that user
                        // would watch a spinner with nothing telling them the
                        // socket is down (#509 review).
                        match crate::components::room_list::current_room_list_display() {
                            crate::components::room_list::RoomListDisplay::Loading => rsx! {
                                div {
                                    class: "flex-1 flex flex-col items-center justify-center gap-3 text-center p-8",
                                    "data-testid": "conversation-rooms-loading",
                                    div { class: "animate-spin w-6 h-6 border-2 border-text-muted border-t-transparent rounded-full" }
                                    span { class: "text-sm text-text-muted", "Loading your rooms…" }
                                    NoRoomFooter {}
                                }
                            },
                            crate::components::room_list::RoomListDisplay::Migrating => rsx! {
                                div {
                                    class: "flex-1 flex flex-col items-center justify-center gap-3 text-center p-8",
                                    "data-testid": "conversation-rooms-migrating",
                                    div { class: "animate-spin w-6 h-6 border-2 border-text-muted border-t-transparent rounded-full" }
                                    span { class: "text-sm text-text-muted", "Migrating your rooms…" }
                                    span { class: "text-xs text-text-muted opacity-70", "(one-time step after an update)" }
                                    NoRoomFooter {}
                                }
                            },
                            crate::components::room_list::RoomListDisplay::LoadFailed => rsx! {
                                div {
                                    class: "flex-1 flex flex-col items-center justify-center gap-3 text-center p-8",
                                    "data-testid": "conversation-rooms-error",
                                    span { class: "text-sm text-text-muted", "Couldn't load your rooms" }
                                    span { class: "text-xs text-text-muted opacity-70", "Check your connection and try again." }
                                    button {
                                        "data-testid": "conversation-rooms-retry-button",
                                        class: "flex items-center justify-center gap-2 px-3 py-2 rounded-lg text-sm text-text-muted bg-surface hover:bg-surface-hover transition-colors",
                                        // Same entry point as the rail's Retry.
                                        // Safe to call directly — `retry_rooms_load`
                                        // defers its signal write and spawns via
                                        // setTimeout(0) (Dioxus signal-safety rules).
                                        onclick: move |_| crate::components::app::chat_delegate::retry_rooms_load(),
                                        span { "Retry" }
                                    }
                                    NoRoomFooter {}
                                }
                            },
                            // The load resolved and there really are no rooms,
                            // or rooms exist and none is selected yet: today's
                            // screen, unchanged. Spelled out rather than `_` so
                            // a future variant is a compile error here instead
                            // of silently falling back to "you have no rooms" —
                            // which is the #397 -> #509 story exactly.
                            crate::components::room_list::RoomListDisplay::Empty
                            | crate::components::room_list::RoomListDisplay::List => rsx! {
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
                                    NoRoomFooter {}
                                }
                            },
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

/// Corner radii for a bubble at this position in its group: every corner is
/// round except the ones on the sender's side that face a neighbouring bubble.
/// Takes position only, so nothing else (reactions, edits) can reshape it.
fn bubble_corner_classes(is_self: bool, is_first: bool, is_last: bool) -> &'static str {
    match (is_self, is_first, is_last) {
        (_, true, true) => "rounded-2xl",
        (true, true, false) => "rounded-t-2xl rounded-bl-2xl rounded-br-md",
        (true, false, true) => "rounded-b-2xl rounded-tl-2xl rounded-tr-md",
        (true, false, false) => "rounded-l-2xl rounded-r-md",
        (false, true, false) => "rounded-t-2xl rounded-br-2xl rounded-bl-md",
        (false, false, true) => "rounded-b-2xl rounded-tr-2xl rounded-tl-md",
        (false, false, false) => "rounded-r-2xl rounded-l-md",
    }
}

#[component]
fn MessageGroupComponent(
    group: MessageGroup,
    /// `None` when this build holds no locally-known identity for the room.
    /// Only cosmetics depend on it here (own-reaction highlight, excluding
    /// yourself from the @mention list), and each degrades to the "not me"
    /// answer.
    self_member_id: Option<MemberId>,
    member_names: HashMap<MemberId, String>,
    /// Room max message size in ENCODED content bytes — bounds the edit
    /// action body (`RoomMessageBody::measure_edit`), not the raw text.
    max_message_size: usize,
    /// Whether the room is private (encrypted edits carry the AES-GCM tag).
    is_private: bool,
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
            "{} (sender's clock may be ahead of yours — their timestamp was later \
             than when this message reached you, so the time shown is when it \
             arrived)",
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
            // With no known identity there is no "yourself" to exclude, so the
            // list keeps every member. Over-inclusive, never wrong: excluding
            // an arbitrary member would silently make them unmentionable.
            .filter(|(id, _)| Some(**id) != self_member_id)
            // `display_nickname` never returns an empty string, so the old
            // is-empty filter became dead: the placeholder is what to exclude.
            .filter(|(_, name)| *name != crate::util::display_name::UNNAMED)
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
                        // Impersonation warning. Sits immediately after the
                        // name, before the shield slot. The two are NOT
                        // mutually exclusive — two deputies whose names collide
                        // each carry a shield AND a warning — so these really
                        // are two badge positions and both can be filled at
                        // once. Do not collapse them into one slot, and do not
                        // suppress the warning when a shield is present: a
                        // deputised sockpuppet carries a genuine shield, and
                        // suppressing on it is the exact immunity
                        // `a_deputised_sockpuppet_cannot_suppress_its_own_warning`
                        // denies.
                        //
                        // The nickname above cannot forge this glyph: U+26A0 is
                        // inside the range `display_name::is_display_hidden`
                        // strips, so it can never survive into a rendered
                        // nickname. `the_warning_glyph_cannot_appear_in_a_nickname`
                        // pins that.
                        if let Some(warning) = group.author_impersonation.as_ref() {
                            {
                                let tooltip = warning.tooltip();
                                rsx! {
                                    span {
                                        "data-testid": "message-author-impersonation-warning",
                                        class: "text-sm cursor-default",
                                        title: "{tooltip}",
                                        "aria-label": "{tooltip}",
                                        {crate::util::confusable::WARNING_GLYPH}
                                    }
                                }
                            }
                        }
                        // Deputy shield. Same glyph, same visibility rule and
                        // the same tooltip as the member-list row and the
                        // member-info modal chip, so the three read as one
                        // badge. The nickname above cannot forge it: nicknames
                        // are stripped of emoji by `crate::util::display_name`.
                        if let Some(badge) = group.author_badge.as_ref() {
                            {
                                let tooltip = badge.tooltip();
                                rsx! {
                                    span {
                                        "data-testid": "message-author-deputy-badge",
                                        class: "text-sm cursor-default",
                                        title: "{tooltip}",
                                        "aria-label": "{tooltip}",
                                        "🛡"
                                    }
                                }
                            }
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
                        let reply_strip_val = msg.reply_strip.clone();

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
                                // For `capture_reading_anchor`; unlike the
                                // group key, a message id never re-keys.
                                "data-anchor-key": "{msg.id}",
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
                                                    // Scroll into view when edit dialog appears (#93),
                                                    // only as far as needed. Instant: the textarea's
                                                    // focus right after cancels a smooth scroll before
                                                    // it moves, leaving Save below the view.
                                                    onmounted: move |cx| {
                                                        let el = cx.data();
                                                        wasm_bindgen_futures::spawn_local(async move {
                                                            let _ = el
                                                                .scroll_to_with_options(dioxus::html::ScrollToOptions {
                                                                    behavior: ScrollBehavior::Instant,
                                                                    vertical: dioxus::html::ScrollLogicalPosition::Nearest,
                                                                    horizontal: dioxus::html::ScrollLogicalPosition::Nearest,
                                                                })
                                                                .await;
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
                                                                // Encoded-size gate: keep the form open so the
                                                                // over-limit edit isn't silently discarded.
                                                                if RoomMessageBody::measure_edit(
                                                                    msg_id.clone(),
                                                                    &new_text,
                                                                    is_private,
                                                                ) > max_message_size
                                                                {
                                                                    return;
                                                                }
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
                                                    // Encoded-size gate for the edit action: same
                                                    // measure the contract enforces. Without it an
                                                    // over-limit edit is signed, sent, and silently
                                                    // pruned by the contract validation.
                                                    {
                                                        let encoded_bytes = RoomMessageBody::measure_edit(
                                                            msg_id_for_save.clone(),
                                                            &edit_text.read(),
                                                            is_private,
                                                        );
                                                        let over_limit = encoded_bytes > max_message_size;
                                                        // `/ 5 * 4` (not `* 4 / 5`): max_message_size is
                                                        // room-config-controlled and falls back to
                                                        // usize::MAX with no room, so multiply-first
                                                        // overflows.
                                                        let near_limit = encoded_bytes > max_message_size / 5 * 4;
                                                        rsx! {
                                                            if near_limit {
                                                                div {
                                                                    class: if over_limit {
                                                                        "text-xs text-right mt-1 pr-1 text-red-600 dark:text-red-400 font-medium"
                                                                    } else if is_self {
                                                                        "text-xs text-right mt-1 pr-1 text-white/70"
                                                                    } else {
                                                                        "text-xs text-right mt-1 pr-1 text-text-muted"
                                                                    },
                                                                    if over_limit {
                                                                        "Message too long \u{2014} {encoded_bytes}/{max_message_size} bytes"
                                                                    } else {
                                                                        "{encoded_bytes}/{max_message_size}"
                                                                    }
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
                                                                    style: if over_limit {
                                                                        "background-color: #9ca3af; color: white; cursor: not-allowed; opacity: 0.6;"
                                                                    } else {
                                                                        "background-color: #2563eb; color: white;"
                                                                    },
                                                                    disabled: over_limit,
                                                                    title: if over_limit {
                                                                        format!("Edited message exceeds the {} byte limit", max_message_size)
                                                                    } else {
                                                                        String::new()
                                                                    },
                                                                    onclick: move |_| {
                                                                        let new_text = edit_text.read().clone();
                                                                        // Same guard as Enter-save: `disabled` should
                                                                        // make this unreachable when over, but a
                                                                        // render-lag click must keep the form open
                                                                        // rather than fall through to the silent
                                                                        // safety-net drop.
                                                                        if RoomMessageBody::measure_edit(
                                                                            save_msg_id.clone(),
                                                                            &new_text,
                                                                            is_private,
                                                                        ) > max_message_size
                                                                        {
                                                                            return;
                                                                        }
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
                                                }
                                            }
                                        } else {
                                            let reply_strip_inner = reply_strip_val.clone();
                                            rsx! {
                                                // Message bubble. The reply strip (if any) is rendered as
                                                // the first child INSIDE the bubble so it shares the
                                                // bubble's width and its intrinsic size cannot reflow
                                                // the parent (fixes #206 and #207).
                                                div {
                                                    "data-testid": "message-bubble",
                                                    class: format!(
                                                        "flex flex-col text-sm overflow-hidden {} {} {}",
                                                        if is_self {
                                                            "bg-accent text-white"
                                                        } else {
                                                            "bg-surface text-text"
                                                        },
                                                        // Grouped bubbles pinch the corner facing a
                                                        // neighbour. Position only: a reaction must
                                                        // not reshape the bubble.
                                                        bubble_corner_classes(is_self, is_first, is_last),
                                                        // Max width for readability; overflow-hidden on
                                                        // parent + min-w-0 on the reply strip prevents
                                                        // the nowrap strip from widening the bubble.
                                                        "max-w-prose"
                                                    ),
                                                    // Reply-quote strip (inside bubble, first child).
                                                    // Exactly one arm renders, enforced by the type: an
                                                    // unverifiable quote is `Unavailable`, which carries no
                                                    // author or preview to render.
                                                    //
                                                    // Self bubbles use a white-tinted overlay so the strip
                                                    // stays legible against the accent background; other
                                                    // bubbles use a dark-tinted overlay against the surface
                                                    // background. The previous `bg-accent/40 text-accent`
                                                    // was invisible on self bubbles because the strip
                                                    // composited to the same colour as the bubble.
                                                    {
                                                        match reply_strip_inner {
                                                            ReplyStrip::NotAReply => rsx! {},
                                                            // Deliberately inert — no `role`/`tabindex`/
                                                            // `onclick` — because there is no original
                                                            // message to scroll to. It therefore carries
                                                            // its own class rather than `reply-strip`,
                                                            // whose hover/focus-expand rules assume a
                                                            // focusable element with ellipsized text.
                                                            //
                                                            // The wording stays neutral: absence cannot
                                                            // distinguish a ban from an ordinary aged-out
                                                            // message, so claiming "banned" here would
                                                            // mislabel the common case.
                                                            ReplyStrip::Unavailable => rsx! {
                                                                div {
                                                                    "data-testid": "reply-strip-unavailable",
                                                                    class: format!(
                                                                        "reply-strip-unavailable min-w-0 w-full text-[11px] leading-normal px-3 pt-1.5 pb-1.5 italic {}",
                                                                        if is_self { "bg-white/25 text-white/90" } else { "bg-black/[0.12] text-text-muted" }
                                                                    ),
                                                                    // The arrow is decorative; the sentence
                                                                    // after it is what a screen reader needs.
                                                                    span { "aria-hidden": "true", "\u{21a9} " }
                                                                    "Original message unavailable"
                                                                }
                                                            },
                                                            ReplyStrip::Quote { author, preview, target_id } => {
                                                                let target_id_str = format!("{:?}", target_id.0);
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
                                    // No known identity ⇒ no reaction is ours,
                                    // so nothing is highlighted and the picker
                                    // offers a fresh reaction rather than a
                                    // toggle of someone else's.
                                    let user_reaction: Option<String> = msg.reactions.iter().find_map(|(emoji, reactors)| {
                                        if self_member_id.is_some_and(|me| reactors.contains(&me)) {
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
                                                    let is_user_reaction = self_member_id.is_some_and(|me| reactors.contains(&me));
                                                    let emoji_for_click = emoji.clone();
                                                    let msg_id_for_click = msg_id_react.clone();

                                                    // Build list of reactor names for tooltip
                                                    let reactor_names: Vec<String> = reactors.iter().map(|reactor_id| {
                                                        // Unknown identity ⇒ nobody is
                                                        // labelled "You"; every reactor
                                                        // falls through to their nickname.
                                                        if Some(*reactor_id) == self_member_id {
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
                                                            "data-testid": "reaction-chip",
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
                                                    "data-testid": "add-reaction-button",
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
                                                        "data-testid": "emoji-picker",
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

    /// A lone bubble is fully round, and a grouped one pinches only the
    /// sender-side corners that face a neighbour.
    #[test]
    fn bubble_corners_pinch_only_toward_a_neighbour() {
        for is_self in [true, false] {
            assert_eq!(bubble_corner_classes(is_self, true, true), "rounded-2xl");
            for (is_first, is_last) in [(true, false), (false, true), (false, false)] {
                let classes = bubble_corner_classes(is_self, is_first, is_last);
                let pinched: Vec<&str> =
                    classes.split(' ').filter(|c| c.ends_with("-md")).collect();
                let side = if is_self { 'r' } else { 'l' };
                let expected = match (is_first, is_last) {
                    (true, false) => format!("rounded-b{side}-md"),
                    (false, true) => format!("rounded-t{side}-md"),
                    _ => format!("rounded-{side}-md"),
                };
                assert_eq!(
                    pinched,
                    vec![expected.as_str()],
                    "is_self={is_self} is_first={is_first} is_last={is_last}"
                );
            }
        }
    }

    /// Source-grep pin: the message-action handlers must LOOK UP the open
    /// room's data when they run, never CAPTURE it.
    ///
    /// Dioxus clones an event-handler closure once per rendered row, and
    /// `RoomData` owns `ChatRoomStateV1` by value — no `Rc` — so a captured
    /// snapshot is a deep copy of every retained message, member and signature.
    /// Capturing it in two handlers made the conversation's resident memory
    /// O(messages × state_size): profiling the live "Off Topic" room
    /// (1133 messages, 136 members) on 2026-07-26 measured 343 KB per room-state
    /// clone and a 1.65 GB WASM heap, which the WASM allocator never returns to
    /// the OS. The capture is a one-line change to re-introduce and costs
    /// nothing observable on a small room, which is exactly why it needs a pin.
    #[test]
    fn message_action_handlers_do_not_capture_room_data() {
        let source = include_str!("conversation.rs");
        // Same cut as the pins below — see `author_deputy_badge_uses_the_shared_helper`
        // for why it must be this needle and not a bare `#[cfg(test)]`.
        let prod = &source[..source
            .find("#[cfg(test)]\nmod tests {")
            .expect("conversation.rs should have a `#[cfg(test)] mod tests` block")];
        // Compare whitespace-stripped so a future rustfmt run that re-wraps the
        // capture across lines cannot silently disarm the pin.
        let squashed: String = prod.chars().filter(|c| !c.is_whitespace()).collect();

        // Split so the needle cannot match its own text via `include_str!`.
        let capture = concat!("letcurrent_room_data=", "current_room_data.clone();");
        assert!(
            !squashed.contains(capture),
            "a handler closure captures `current_room_data` by value. Dioxus \
             clones these closures once per rendered message row, so this deep- \
             copies the entire room state per message (~343 KB × rows). Look the \
             room up at interaction time with `current_room_data_snapshot()` \
             instead."
        );

        // The definition plus one call per handler (react / delete / edit).
        assert!(
            squashed.matches("current_room_data_snapshot()").count() >= 4,
            "the react/delete/edit handlers must each resolve the open room \
             through `current_room_data_snapshot()` at interaction time"
        );
    }

    /// Source-grep pin (freenet/river#512): nothing may be awaited between the
    /// user acting on a message and that message reaching `ROOMS`.
    ///
    /// All four handlers — send, reaction, delete, edit — render optimistically:
    /// the composer (or the reaction pill) updates the instant the delta is
    /// applied to `ROOMS`, and nothing waits for the network echo. So every
    /// suspension point before that write is dead air, with the composer
    /// already cleared and no message in its place.
    ///
    /// The one that shipped was a delegate signing round-trip. On a hosted node
    /// that is a WAN hop queued behind contract merges on a serial WASM
    /// executor, with a 10s timeout before the local fallback. It cost seconds
    /// and could not change the produced bytes — see
    /// `signing::a_delegate_signature_can_never_differ_from_the_local_one`.
    ///
    /// It is also invisible in development: with `--features no-sync` the
    /// delegate request fails instantly, so `dev-example` and every Playwright
    /// spec only ever exercise the fast path. Nothing but this pin stands
    /// between a future `.await` here and another silent multi-second
    /// regression.
    #[test]
    fn nothing_is_awaited_between_acting_on_a_message_and_it_appearing() {
        let source = include_str!("conversation.rs");
        // Same cut as the pins around it — see
        // `author_deputy_badge_uses_the_shared_helper` for why it must be this
        // needle and not a bare `#[cfg(test)]`.
        let prod = &source[..source
            .find("#[cfg(test)]\nmod tests {")
            .expect("conversation.rs should have a `#[cfg(test)] mod tests` block")];
        // Drop whole-line comments, so prose about awaiting (this file has
        // plenty) cannot satisfy the search. Deliberately NOT `split_once`,
        // which would also truncate a line at a `//` inside a string literal
        // (`"https://…"`) and could hide a real trailing `.await`. A trailing
        // comment that happens to contain the needle now fails the test
        // instead, which is the safe direction: loud, not silent.
        let code: String = prod
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // Whitespace-stripped so a rustfmt re-wrap cannot silently disarm it.
        let squashed: String = code.chars().filter(|c| !c.is_whitespace()).collect();

        // Every optimistic-render handler, not just the one #512 was reported
        // against: they are the same shape, and all four carried the same await.
        // Each ends at the `defer` that performs its `ROOMS` write.
        for handler in [
            "lethandle_send_message={",
            "lethandle_toggle_reaction={",
            "lethandle_delete_message={",
            "lethandle_edit_message={",
        ] {
            let start = squashed.find(handler).unwrap_or_else(|| {
                panic!(
                    "conversation.rs no longer defines `{handler}` — re-anchor \
                     this pin on whatever replaced it, do not delete it"
                )
            });
            // Anchored on the ROOMS write itself, not on the `defer` that
            // wraps it: a future edit adding an EARLIER defer to a handler (a
            // scroll defer, a focus defer) would otherwise shrink the region
            // and quietly let an await through behind it.
            let end = squashed[start..]
                .find("ROOMS.with_mut(")
                .map(|i| start + i)
                .unwrap_or_else(|| {
                    panic!(
                        "`{handler}` no longer applies its result to ROOMS \
                         optimistically — re-anchor this pin, do not delete it"
                    )
                });
            let region = &squashed[start..end];

            assert!(
                !region.contains(".await"),
                "`{handler}` awaits something before its optimistic ROOMS \
                 write. The UI is already committed to the action by then, so \
                 the user waits with nothing on screen for as long as that \
                 future takes — which is what freenet/river#512 was. Do the \
                 work after the local apply, or off this path entirely."
            );
            // Per-region, so it also fails if signing moves INTO the deferred
            // closure (outside the scanned region) or switches to another key.
            assert!(
                region.contains("sign_message_locally(&message_bytes,&self_sk)"),
                "`{handler}` must sign with the room's own key, synchronously, \
                 before the message reaches ROOMS"
            );
            // The write must still be deferred to a clean execution context —
            // that `setTimeout`, not `spawn_local`, is what keeps the Dioxus
            // signal write off the event handler's stack.
            assert!(
                region.contains("crate::util::defer(move||{"),
                "`{handler}`'s ROOMS write must stay inside `crate::util::defer`"
            );
        }

        // No path may fall back to asking the delegate. Split so the needle
        // cannot match its own text via `include_str!`.
        assert!(
            !squashed.contains(concat!("sign_message_", "with_fallback")),
            "message signing must not go through the delegate on any path \
             (freenet/river#512)"
        );
    }

    /// Source-grep pin (freenet/river#509): the no-room screen must branch on
    /// the SHARED room-list display state.
    ///
    /// Below 768px the rooms rail is `display:none` rather than unmounted and
    /// the default mobile view is Chat, so for the whole load window this panel
    /// is the only thing a phone user can see. Rendering the Welcome copy
    /// unconditionally told a mid-load user they had no rooms, and hid a FAILED
    /// load — and its Retry button — behind advice to create one.
    ///
    /// The states themselves are covered by `room_list_display_state`'s unit
    /// tests and by `rooms-loading-state.spec.ts`; what this pins is the
    /// WIRING, which is what #397 left undone: it added the states to the rail
    /// and never touched `app.rs` or this file, so `ROOMS_LOAD_STATE` had
    /// exactly one consumer in the whole UI.
    #[test]
    fn the_no_room_screen_branches_on_the_shared_load_state() {
        let source = include_str!("conversation.rs");
        let prod = &source[..source
            .find("#[cfg(test)]\nmod tests {")
            .expect("conversation.rs should have a `#[cfg(test)] mod tests` block")];
        let code: String = prod
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let squashed: String = code.chars().filter(|c| !c.is_whitespace()).collect();

        assert!(
            squashed.contains("current_room_list_display()"),
            "the no-room screen must read the shared room-list display state, \
             not render the Welcome copy unconditionally — that is what mobile \
             showed a user mid-load (freenet/river#509)"
        );
        for (arm, testid) in [
            ("Loading", "conversation-rooms-loading"),
            ("Migrating", "conversation-rooms-migrating"),
            ("LoadFailed", "conversation-rooms-error"),
        ] {
            assert!(
                squashed.contains(&format!("RoomListDisplay::{arm}=>")),
                "the no-room screen must handle `RoomListDisplay::{arm}`"
            );
            assert!(
                squashed.contains(&format!("\"data-testid\":\"{testid}\"")),
                "the {arm} arm must render its `{testid}` element"
            );
        }
        assert!(
            squashed.contains("retry_rooms_load()"),
            "the failed arm must offer Retry — a stalled load is otherwise \
             pixel-identical to an empty account, and the advice on screen is \
             to create a room"
        );
        // The invite link and the connection pill must reach EVERY arm, which
        // is what `NoRoomFooter` is for. The pill in particular is the only
        // thing on screen that can explain an unbounded `Loading`: a node that
        // never connects never runs `begin_load_attempt`, so the 60s backstop
        // is never armed either.
        assert_eq!(
            squashed.matches("NoRoomFooter{}").count(),
            4,
            "every arm of the no-room screen must render `NoRoomFooter` — the \
             invite link and the connection pill are useful in all of them, and \
             the pill is what makes 'Check your connection' actionable"
        );
        assert!(
            squashed.contains("fnNoRoomFooter()->Element{"),
            "the shared footer must exist as one component, so the arms cannot \
             drift on what it contains"
        );
        assert!(
            !squashed.contains("RoomListDisplay::List=>rsx!{}"),
            "the conversation panel must not copy the rail's empty `List` arm; \
             it renders the Welcome screen for List"
        );
    }

    /// The newest item is on screen for ANY window size — including the
    /// degenerate zero, which would otherwise render a history the reader
    /// cannot scroll into (the window only ever grows upward from the bottom).
    #[test]
    fn the_window_always_renders_the_newest_item() {
        for total in 1..200usize {
            for window in [0, 1, 2, 59, 60, 61, 199, 200, 5000] {
                for anchor in [None, Some(0), Some(30), Some(total)] {
                    let w = HistoryWindow::resolve(total, window, anchor);
                    assert!(
                        w.start < total,
                        "total {total}, window {window}, anchor {anchor:?}: \
                         start {} skips every item",
                        w.start
                    );
                }
            }
        }
    }

    /// A room that fits in the window renders exactly as it did before the
    /// window existed: every item, and no backfill sentinel to observe.
    #[test]
    fn a_room_inside_the_window_is_rendered_whole() {
        for total in 0..=INITIAL_WINDOW_ITEMS {
            let w = HistoryWindow::resolve(total, INITIAL_WINDOW_ITEMS, None);
            assert_eq!(
                w.start, 0,
                "total {total} should render from the first item"
            );
            assert!(!w.has_older, "total {total} has nothing to backfill");
        }
    }

    /// Past the window, only the tail renders and the sentinel appears. The
    /// counts here are the live "Off Topic" room's shape.
    #[test]
    fn a_room_past_the_window_renders_only_its_tail() {
        let w = HistoryWindow::resolve(1133, INITIAL_WINDOW_ITEMS, None);
        assert_eq!(w.start, 1133 - INITIAL_WINDOW_ITEMS);
        assert!(w.has_older, "1133 items must offer a backfill");

        // Each backfill reveals exactly one growth step more... (the anchor is
        // carried the way the render carries it, and moving it BACK for a
        // backfill is allowed — only forward movement is the #501 slide)
        let grown = HistoryWindow::resolve(
            1133,
            INITIAL_WINDOW_ITEMS + WINDOW_GROWTH_ITEMS,
            Some(w.start),
        );
        assert_eq!(w.start - grown.start, WINDOW_GROWTH_ITEMS);

        // ...until the window covers the room, at which point the sentinel goes.
        let all = HistoryWindow::resolve(1133, 1133, Some(grown.start));
        assert_eq!(all.start, 0);
        assert!(!all.has_older);
    }

    /// Arrivals GROW an anchored window instead of sliding it (#501): the start
    /// index stays put as the total climbs, so nothing is removed above the
    /// viewport in the same patch that appends below it.
    #[test]
    fn arrivals_grow_an_anchored_window_instead_of_sliding_it() {
        let opened = HistoryWindow::resolve(100, INITIAL_WINDOW_ITEMS, None);
        assert_eq!(opened.start, 40);

        let mut anchor = Some(opened.start);
        for arrivals in 1..=50usize {
            let w = HistoryWindow::resolve(100 + arrivals, INITIAL_WINDOW_ITEMS, anchor);
            assert_eq!(
                w.start, opened.start,
                "arrival {arrivals} slid the window instead of growing it"
            );
            anchor = Some(w.start);
        }
    }

    /// The trim — the settle handler clearing the anchor and resetting the
    /// requested window once the reader's own settle lands at the bottom —
    /// resolves back to the plain tail.
    #[test]
    fn a_trim_resolves_back_to_the_plain_tail() {
        // 40 arrivals grew the anchored window to 100 rendered items...
        let grown = HistoryWindow::resolve(140, INITIAL_WINDOW_ITEMS, Some(40));
        assert_eq!(grown.start, 40);
        // ...and the trim (anchor cleared, window back to INITIAL) collapses
        // it to the newest INITIAL_WINDOW_ITEMS.
        let trimmed = HistoryWindow::resolve(140, INITIAL_WINDOW_ITEMS, None);
        assert_eq!(trimmed.start, 140 - INITIAL_WINDOW_ITEMS);
        assert!(trimmed.has_older);
    }

    /// The ceiling bounds what arrival growth can accumulate — a reader idle
    /// through a very long burst must not re-accumulate the unbounded render
    /// the window exists to prevent — but it NEVER caps a reader-requested
    /// backfill, which would dead-end paging through history.
    #[test]
    fn the_ceiling_caps_arrival_growth_but_never_reader_backfill() {
        // Anchored at 100 while the room ran away to 1000 items: the end stops
        // at the ceiling and the rest is held back.
        let w = HistoryWindow::resolve(1000, INITIAL_WINDOW_ITEMS, Some(100));
        assert_eq!((w.start, w.end), (100, 100 + WINDOW_ITEMS_CEILING));
        assert!(w.has_older && w.has_newer);

        // A reader who backfilled PAST the ceiling keeps everything they asked
        // for: the requested window wins over the ceiling.
        let requested = WINDOW_ITEMS_CEILING + WINDOW_GROWTH_ITEMS;
        let w = HistoryWindow::resolve(1000, requested, Some(1000 - requested));
        assert_eq!(w.start, 1000 - requested);

        // Arrival-grace headroom: a range backfilled TO the ceiling still
        // renders the next few arrivals — the cap sits one growth step above
        // the request (#505 review).
        let at_ceiling = WINDOW_ITEMS_CEILING;
        let anchor = 1000 - at_ceiling;
        let w = HistoryWindow::resolve(1010, at_ceiling, Some(anchor));
        assert_eq!(
            (w.start, w.end),
            (anchor, 1010),
            "10 arrivals within the grace must render without holding the end"
        );
    }

    /// Anchor carrying the head + spare keys the way the render does.
    fn anchor_of(keys: &[String], start: usize) -> WindowAnchor {
        WindowAnchor {
            keys: keys[start..]
                .iter()
                .take(WINDOW_ANCHOR_KEYS)
                .cloned()
                .collect(),
            index: start,
        }
    }

    /// #505 blocker 1: the anchor is re-located by IDENTITY. An at-cap room
    /// prunes its oldest message per arrival, shifting every index down — the
    /// walk below is that steady state, and the head must stay the same ITEM
    /// (its index marching toward 0), not the same index.
    #[test]
    fn the_anchor_follows_the_item_through_at_cap_pruning() {
        // A 100-item room at cap: items are identified by these keys.
        let mut keys: Vec<String> = (0..100).map(|i| format!("m{i}")).collect();
        let opened = HistoryWindow::resolve(keys.len(), INITIAL_WINDOW_ITEMS, None);
        assert_eq!(opened.start, 40);
        let head_key = keys[opened.start].clone();
        let mut anchor = anchor_of(&keys, opened.start);

        for arrival in 0..40usize {
            // apply_delta at cap: drain the oldest, append the new.
            keys.remove(0);
            keys.push(format!("new{arrival}"));
            let relocated = relocate_window(keys.len(), &anchor, |i, key| keys[i] == key);
            assert!(relocated.head_survived, "arrival {arrival}: head survives");
            let start =
                HistoryWindow::resolve(keys.len(), INITIAL_WINDOW_ITEMS, Some(relocated.start))
                    .start;
            assert_eq!(
                keys[start], head_key,
                "arrival {arrival}: the window head changed identity — the \
                 #501 slide reproduced in content space"
            );
            assert_eq!(start, opened.start - (arrival + 1), "start marches down");
            anchor = anchor_of(&keys, start);
        }

        // One more prune consumes the head itself: the head key is gone, but
        // the FIRST SPARE (the item right after the old head) survives at
        // index 0, so the relocation lands on the nearest surviving item and
        // reports the head as removed — which is what arms the measured
        // reposition for a parked reader.
        keys.remove(0);
        keys.push("new40".into());
        let relocated = relocate_window(keys.len(), &anchor, |i, key| keys[i] == key);
        assert!(!relocated.head_survived, "the head itself was pruned");
        assert_eq!(
            relocated.start, 0,
            "the first surviving spare pins the window to the nearest \
             surviving item"
        );
    }

    /// #505 re-review blocker: a MULTI-message head group RE-KEYS when an
    /// at-cap drain consumes its first message (a group's key is its first
    /// message's id). The old key is gone while the group itself survives —
    /// the spares must still pin the window to the survivors, and the
    /// reposition probe must skip the un-measurable new head key and land on
    /// a row that existed pre-patch.
    #[test]
    fn a_rekeyed_multi_message_head_group_is_still_anchored_and_measurable() {
        // Window head at index 0: a group keyed by its first message "m0",
        // followed by neighbors. The at-cap drain consumes "m0"; the group
        // survives RE-KEYED as "m1". (Modelled at the key level — exactly
        // what `display_item_key` exposes to this machinery.)
        let anchor = WindowAnchor {
            keys: vec!["m0".into(), "b".into(), "c".into(), "d".into()],
            index: 0,
        };
        let post_keys = ["m1", "b", "c", "d", "e"];
        let relocated = relocate_window(post_keys.len(), &anchor, |i, key| post_keys[i] == key);
        assert!(
            !relocated.head_survived,
            "the re-keyed head must count as removed — its row is replaced \
             and its height changed"
        );
        assert_eq!(
            relocated.start, 0,
            "the first spare (\"b\", found at index 1, offset 1 in the \
             anchor) pins the window back to the re-keyed group"
        );

        // The measured probe: the new head key "m1" has NO pre-patch row —
        // a head-only probe dead-fires and the parked reader crawls one
        // intra-group line per arrival. The walk lands on "b", which does.
        let probe = select_reposition_probe(
            post_keys.iter().map(|k| k.to_string()),
            pre_patch_dom(&[("m0", 100), ("b", 160), ("c", 220)]),
        );
        assert_eq!(
            probe,
            Some(("b".to_string(), 160)),
            "the probe must walk past the un-measurable re-keyed head to the \
             first row that existed pre-patch"
        );
    }

    /// A pre-patch DOM as a `first_history_row_offset`-shaped lookup: the
    /// first candidate key that has a row, with its offset.
    fn pre_patch_dom(rows: &[(&'static str, i32)]) -> impl Fn(&[String]) -> Option<(String, i32)> {
        let rows = rows.to_vec();
        move |keys: &[String]| {
            keys.iter().find_map(|k| {
                rows.iter()
                    .find(|(rk, _)| rk == k)
                    .map(|(_, top)| (k.clone(), *top))
            })
        }
    }

    /// The probe walk must outlast the DEEPEST spare the anchor can relocate
    /// through. Landing via spare `k` sets `start = i - k`, widening the
    /// window backward by `k` items that were never rendered pre-patch, so
    /// the first `k` candidates cannot have pre-patch rows. A walk shorter
    /// than `WINDOW_ANCHOR_KEYS` dead-fires exactly when the removal is
    /// biggest — a bulk delete of several contiguous leading items above a
    /// parked reader (#505 delta review).
    #[test]
    fn the_probe_walk_outlasts_the_deepest_spare() {
        assert!(
            REPOSITION_PROBE_ROWS >= WINDOW_ANCHOR_KEYS,
            "the probe walk must cover every spare the anchor can land on"
        );

        // Anchor: head + 7 spares. A bulk delete removes the head and the
        // first 6 spares; relocation lands on spare 7 ("s7"), so the window
        // widens back by 7 items that have no pre-patch rows.
        let anchor = WindowAnchor {
            keys: (0..WINDOW_ANCHOR_KEYS)
                .map(|i| {
                    if i == 0 {
                        "head".to_string()
                    } else {
                        format!("s{i}")
                    }
                })
                .collect(),
            index: 20,
        };
        let post_keys: Vec<String> = (0..7)
            .map(|i| format!("older{i}"))
            .chain(std::iter::once("s7".to_string()))
            .chain((0..10).map(|i| format!("rest{i}")))
            .collect();
        let relocated = relocate_window(post_keys.len(), &anchor, |i, key| post_keys[i] == key);
        assert!(!relocated.head_survived);
        assert_eq!(
            relocated.start, 0,
            "spare 7 found at index 7, offset 7 in the anchor → start 0"
        );

        // Only the 8th candidate ("s7") has a pre-patch row; the seven
        // widened-in rows above it do not. A 4-row walk finds nothing.
        let probe = select_reposition_probe(
            post_keys.iter().map(|k| k.to_string()),
            pre_patch_dom(&[("s7", 480), ("rest0", 540)]),
        );
        assert_eq!(
            probe,
            Some(("s7".to_string(), 480)),
            "the walk must reach the deepest spare's row, or the compensation \
             silently dead-fires on the largest removals"
        );
    }

    /// A batched drain that consumes the head AND several spares (the
    /// `appendMessages` burst crossing the anchor) still lands on the first
    /// surviving spare, offset back to where the head would have been.
    #[test]
    fn a_batched_drain_past_the_head_lands_on_the_first_surviving_spare() {
        // 100 items, window head at 25 with spares 25..33.
        let keys: Vec<String> = (0..100).map(|i| format!("m{i}")).collect();
        let anchor = anchor_of(&keys, 25);
        // A 30-item drain + 30 arrivals: items 0..30 gone, head (25) and
        // spares 25..29 with it; spare "m30" (offset 5) survives at index 0.
        let post_keys: Vec<String> = (30..100)
            .map(|i| format!("m{i}"))
            .chain((0..30).map(|i| format!("new{i}")))
            .collect();
        let relocated = relocate_window(post_keys.len(), &anchor, |i, key| post_keys[i] == key);
        assert!(!relocated.head_survived);
        assert_eq!(
            relocated.start, 0,
            "spare m30 found at index 0, offset 5 in the anchor → start 0: \
             the whole surviving remainder of the old window stays rendered"
        );

        // And when the drain consumes every spare too, the only safe answer
        // for a front-contiguous drain is index 0 — the oldest survivor.
        let post_keys: Vec<String> = (40..100)
            .map(|i| format!("m{i}"))
            .chain((0..40).map(|i| format!("new{i}")))
            .collect();
        let relocated = relocate_window(post_keys.len(), &anchor, |i, key| post_keys[i] == key);
        assert!(!relocated.head_survived);
        assert_eq!(relocated.start, 0);
    }

    /// The case where the spares are load-bearing and no fallback can stand
    /// in for them: the head alone vanishes MID-window (its whole group
    /// deleted), the neighbors survive in place. The first spare pins the
    /// window one slot back from where it sits — a head-only relocation
    /// would fall back to index 0 and blow the window open across the whole
    /// room (ceiling-bounded, but a ~180-item over-render and a torn view).
    #[test]
    fn a_mid_window_head_deletion_reanchors_on_the_next_survivor() {
        let keys: Vec<String> = (0..100).map(|i| format!("m{i}")).collect();
        let anchor = anchor_of(&keys, 40);
        // The head item "m40" is deleted outright; everything else survives,
        // shifted down by one from index 41 on.
        let post_keys: Vec<String> = keys
            .iter()
            .filter(|k| k.as_str() != "m40")
            .cloned()
            .collect();
        let relocated = relocate_window(post_keys.len(), &anchor, |i, key| post_keys[i] == key);
        assert!(!relocated.head_survived);
        assert_eq!(
            relocated.start, 39,
            "spare m41 (offset 1) found at index 40 → start 39: the window \
             re-anchors one slot back, not at the front of the room"
        );
    }

    /// Skip the trim when the measured tail would leave the backfill strip in
    /// view; trim and backfill would otherwise loop (#505 re-review).
    #[test]
    fn the_trim_skips_when_the_tail_would_rearm_the_sentinel() {
        let reach = |client: i32| client + BACKFILL_LEAD_PX + TRIM_HEADROOM_PX as i32;
        // Exactly at the reach clears it; one px short does not.
        assert!(!trim_would_rearm_backfill(reach(900), 900));
        assert!(trim_would_rearm_backfill(reach(900) - 1, 900));
    }

    /// Item keys `m0..m{total}` for the range tests.
    fn item_keys(total: usize) -> Vec<String> {
        (0..total).map(|i| format!("m{i}")).collect()
    }

    /// The tail anchor the render stores for a held range.
    fn tail_of(keys: &[String], end: usize) -> WindowAnchor {
        WindowAnchor {
            keys: keys[..end]
                .iter()
                .rev()
                .take(WINDOW_ANCHOR_KEYS)
                .cloned()
                .collect(),
            index: end - 1,
        }
    }

    /// Past the ceiling, the range holds its end instead of sliding its start,
    /// wherever the reader is, and later arrivals don't grow it (#732 review
    /// 42f541b6, 10c).
    #[test]
    fn the_range_holds_its_end_at_the_ceiling() {
        let opened = HistoryWindow::resolve(201, INITIAL_WINDOW_ITEMS, None);
        // 200 arrivals in one patch, start anchored.
        let held = HistoryWindow::resolve(401, INITIAL_WINDOW_ITEMS, Some(opened.start));
        assert_eq!(
            held.start, opened.start,
            "the start must not slide past the reader"
        );
        assert_eq!(held.end - held.start, WINDOW_ITEMS_CEILING);
        assert!(held.has_newer, "the newest items are withheld");

        // More arrivals: the held end stays on its item, so the DOM stops growing.
        let again = HistoryWindow::resolve_held(
            501,
            INITIAL_WINDOW_ITEMS,
            Some(held.start),
            RangeHold {
                end: Some(held.end),
                ..RangeHold::default()
            },
        );
        assert_eq!((again.start, again.end), (held.start, held.end));

        // Below the ceiling nothing is held.
        let small = HistoryWindow::resolve(230, INITIAL_WINDOW_ITEMS, Some(opened.start));
        assert_eq!((small.start, small.end), (opened.start, 230));
        assert!(!small.has_newer);
    }

    /// An at-cap drain shifts every index; a held range keeps both edges on
    /// their items.
    #[test]
    fn a_held_range_follows_its_items_through_pruning() {
        let mut keys = item_keys(400);
        let (start, end) = (100, 100 + WINDOW_ITEMS_CEILING);
        let head = anchor_of(&keys, start);
        let tail = tail_of(&keys, end);
        let (first, last) = (keys[start].clone(), keys[end - 1].clone());
        // A batch of 40 arrivals drains the 40 oldest items.
        keys.drain(..40);
        keys.extend((0..40).map(|i| format!("new{i}")));
        let relocated = relocate_window(keys.len(), &head, |i, k| keys[i] == k);
        let held_end = relocate_tail(keys.len(), &tail, |i, k| keys[i] == k);
        assert_eq!(held_end, Some(end - 40));
        let w = HistoryWindow::resolve_held(
            keys.len(),
            INITIAL_WINDOW_ITEMS,
            Some(relocated.start),
            RangeHold {
                end: held_end,
                keep: None,
            },
        );
        assert_eq!(keys[w.start], first);
        assert_eq!(keys[w.end - 1], last);
        assert!(w.has_newer);

        // The last held item deleted outright: its spare places the end.
        let gone = keys[w.end - 1].clone();
        keys.retain(|k| *k != gone);
        assert_eq!(
            relocate_tail(
                keys.len(),
                &tail_of_keys(&[gone, keys[w.end - 2].clone()]),
                |i, k| keys[i] == k
            ),
            Some(w.end - 1)
        );
        assert_eq!(
            relocate_tail(keys.len(), &tail_of_keys(&["nope".into()]), |i, k| keys[i]
                == k),
            None
        );
    }

    /// A tail anchor from explicit keys (newest first), hint past the end.
    fn tail_of_keys(keys: &[String]) -> WindowAnchor {
        WindowAnchor {
            keys: keys.to_vec(),
            index: 10_000,
        }
    }

    /// A held range pages both ways. A newer page slides the start behind the
    /// reader, but never past `keep`.
    #[test]
    fn a_held_range_pages_older_and_newer() {
        let total = 1000;
        let (start, end) = (300, 300 + WINDOW_ITEMS_CEILING);
        let held = |end, keep| RangeHold {
            end: Some(end),
            keep,
        };
        // Older: one growth step from the rendered size reveals exactly one page.
        let requested = grown_window(INITIAL_WINDOW_ITEMS, end - start);
        let older = HistoryWindow::resolve_held(total, requested, Some(start), held(end, None));
        assert_eq!((older.start, older.end), (start - WINDOW_GROWTH_ITEMS, end));

        // Newer: the end moves one page down and the ceiling slides the start.
        let newer = HistoryWindow::resolve_held(
            total,
            INITIAL_WINDOW_ITEMS,
            Some(start),
            held(end + WINDOW_GROWTH_ITEMS, None),
        );
        assert_eq!(newer.end, end + WINDOW_GROWTH_ITEMS);
        assert_eq!(newer.end - newer.start, WINDOW_ITEMS_CEILING);

        // The reader's row bounds that slide: the cap gives way, not the row.
        let keep = start + 20;
        let kept = HistoryWindow::resolve_held(
            total,
            INITIAL_WINDOW_ITEMS,
            Some(start),
            held(end + WINDOW_GROWTH_ITEMS, Some(keep)),
        );
        assert_eq!(kept.start, keep);
        assert!(kept.end - kept.start > WINDOW_ITEMS_CEILING);

        // Paging newer until the end reaches the newest item ends the hold.
        let last = HistoryWindow::resolve_held(
            total,
            INITIAL_WINDOW_ITEMS,
            Some(900),
            held(total + 30, None),
        );
        assert_eq!(last.end, total);
        assert!(!last.has_newer);

        // A held end with nothing left falls back to the newest, not to empty.
        let empty =
            HistoryWindow::resolve_held(total, INITIAL_WINDOW_ITEMS, Some(900), held(0, None));
        assert_eq!(empty.end, total);
    }

    /// A deleted reading row falls back to its nearest surviving neighbour,
    /// the one above first (the order `capture_reading_anchor` records).
    #[test]
    fn the_reading_anchor_restores_through_its_neighbours() {
        let anchor = ReadingAnchor {
            rows: vec![
                ("read".into(), 40),
                ("above".into(), -30),
                ("below".into(), 120),
            ],
            item_hint: 0,
            view_height: 700,
            view_width: 800,
        };
        // Everything survives: the reading row goes back to 40px.
        let dom = |rows: &'static [(&'static str, i32)]| {
            move |k: &str| rows.iter().find(|(rk, _)| *rk == k).map(|(_, t)| *t)
        };
        assert_eq!(
            reading_anchor_shift(&anchor, 0, dom(&[("read", -600), ("below", -520)])),
            Some(-640)
        );
        // The reading row was deleted: the row above is restored to ITS
        // offset, even though the row below survived too.
        assert_eq!(
            reading_anchor_shift(&anchor, 0, dom(&[("below", 700), ("above", 10)])),
            Some(40)
        );
        // Nothing survives: no move at all.
        assert_eq!(reading_anchor_shift(&anchor, 0, dom(&[("other", 0)])), None);
    }

    /// A height change holds the bottom edge as it was at capture: the offsets
    /// move by the change, whatever the browser did to `scrollTop` meanwhile.
    #[test]
    fn the_reading_anchor_holds_the_bottom_edge_through_a_height_change() {
        let anchor = ReadingAnchor {
            rows: vec![("read".into(), 40)],
            item_hint: 0,
            view_height: 700,
            view_width: 800,
        };
        let at = |top: i32| move |k: &str| (k == "read").then_some(top);
        // Shrunk by 200, nothing scrolled yet: scroll down by 200.
        assert_eq!(reading_anchor_shift(&anchor, 500 - 700, at(40)), Some(200));
        // Grown by 200 after the browser already clamped 150 of it (the row
        // moved down by 150): only the remaining 50, never 200 again.
        assert_eq!(reading_anchor_shift(&anchor, 900 - 700, at(190)), Some(-50));
        // Two resizes before the next capture (700 -> 600 -> 500), the first
        // already corrected: the second is measured from the capture too.
        assert_eq!(reading_anchor_shift(&anchor, 500 - 700, at(-60)), Some(100));
    }

    /// `relocate_anchor` search order and fallbacks.
    #[test]
    fn relocate_anchor_finds_shifted_heads_and_reports_lost_ones() {
        let keys = ["a", "b", "c", "d", "e"];
        let find =
            |key: &'static str, hint: usize| relocate_anchor(keys.len(), hint, |i| keys[i] == key);
        // Unshifted: found at the hint.
        assert_eq!(find("c", 2), Some(2));
        // Front prune shifted it down: found below the hint.
        assert_eq!(find("c", 3), Some(2));
        // Older history merged in above: found above the hint.
        assert_eq!(find("c", 0), Some(2));
        // Hint out of range entirely: still found.
        assert_eq!(find("e", 400), Some(4));
        // Gone: None, and `relocate_window` moves on to the next spare.
        assert_eq!(find("zz", 2), None);
        assert_eq!(relocate_anchor(0, 0, |_| true), None, "empty list");
    }

    /// #505 blocker 2: one backfill growth step must always reveal more than
    /// what is RENDERED — growing from the stale requested size can resolve to
    /// a start the anchor already renders (zero new rows, sentinel never
    /// re-fires, paging dead-ends). The trace is the review's: open at 1000
    /// items, 61 arrivals, page up.
    #[test]
    fn a_growth_step_reveals_rows_even_after_arrivals_grew_the_window() {
        let opened = HistoryWindow::resolve(1000, INITIAL_WINDOW_ITEMS, None);
        assert_eq!(opened.start, 940);
        // 61 arrivals: the anchored start holds, rendered = 121.
        let grown = HistoryWindow::resolve(1061, INITIAL_WINDOW_ITEMS, Some(opened.start));
        assert_eq!(grown.start, opened.start);
        let rendered = 1061 - grown.start;
        assert_eq!(rendered, 121);

        // The regression this pins: growing from the REQUESTED size reveals
        // nothing (min(anchor, 1061-120) = anchor).
        let dead = HistoryWindow::resolve(
            1061,
            INITIAL_WINDOW_ITEMS + WINDOW_GROWTH_ITEMS,
            Some(grown.start),
        );
        assert_eq!(
            dead.start, grown.start,
            "premise: the naive growth really does dead-end — if this stops \
             holding, the whole scenario needs rebuilding"
        );

        // The fix: grow from the rendered size.
        let requested = grown_window(INITIAL_WINDOW_ITEMS, rendered);
        assert_eq!(requested, rendered + WINDOW_GROWTH_ITEMS);
        let paged = HistoryWindow::resolve(1061, requested, Some(grown.start));
        assert!(
            paged.start < grown.start,
            "a growth step must strictly decrease start"
        );
        assert_eq!(
            grown.start - paged.start,
            WINDOW_GROWTH_ITEMS,
            "and by exactly one growth step"
        );
    }

    /// Source-grep pin: the history must render through the window. Rendering
    /// `groups` directly is the regression this guards — it is a one-token edit
    /// away, it looks harmless, and it costs nothing measurable until someone
    /// opens a room with a thousand messages in it.
    #[test]
    fn the_history_renders_through_the_window() {
        let source = include_str!("conversation.rs");
        let prod = &source[..source
            .find("#[cfg(test)]\nmod tests {")
            .expect("conversation.rs should have a `#[cfg(test)] mod tests` block")];
        let squashed: String = prod.chars().filter(|c| !c.is_whitespace()).collect();

        assert!(
            squashed.contains("HistoryWindow::resolve_held(groups.len(),requested_window,"),
            "the history must slice its display items through `HistoryWindow`"
        );
        assert!(
            squashed.contains("relocate_window(groups.len(),a,"),
            "the anchor must be re-located by IDENTITY (head key, then spare \
             keys) before resolving — a positional anchor slides the head in \
             content space whenever an at-cap room prunes a message (#505 \
             blocker 1)"
        );
        assert!(
            squashed.contains("*reader_position.window_anchor.borrow_mut()=Some(WindowAnchor{"),
            "the render must write the resolved head's identity back so the \
             NEXT render grows instead of sliding (#501)"
        );
        assert!(
            squashed.contains(
                "reader_position.window_rendered.set(history_window.end-history_window.start);"
            ),
            "the render must record the RENDERED size — the backfill growth \
             step grows from it, or paging dead-ends after arrivals (#505 \
             blocker 2)"
        );
        assert!(
            squashed.contains("*n=grown_window(*n,reader_position.window_rendered.get())"),
            "the backfill sentinel must grow through `grown_window`, from the \
             rendered size (#505 blocker 2)"
        );
        assert!(
            squashed.contains("groups[history_window.start..history_window.end].to_vec()"),
            "the render must clone only the windowed tail — cloning all groups \
             re-introduces the per-render cost the window exists to avoid"
        );
        assert!(
            squashed.contains(concat!("id:\"top-backfill", "-sentinel\"")),
            "the backfill sentinel must be rendered, or a reader can never see \
             messages older than the initial window"
        );
        // Anchored on needles unique to the ROOM-SWITCH reset. The obvious
        // needle — `window_items.set(INITIAL_WINDOW_ITEMS)` — also matches
        // the bottom-settle trim, so deleting the room reset would have
        // false-passed against it (#505 review).
        assert!(
            squashed.contains(
                "prev_render_room.set(Some(room));*reader_position.window_anchor.borrow_mut()=None;"
            ),
            "the windowing Cells must reset inline in the render on a room \
             switch — the effect-based reset runs one render too late, so \
             the new room's first frame would render at the old room's depth"
        );
        assert!(
            squashed
                .contains("prev_windowed_room.set(room);window_items.set(INITIAL_WINDOW_ITEMS);"),
            "the requested window signal must reset when the reader opens \
             another room"
        );
    }

    /// Source-grep pin, mirroring `member_info_modal`'s: the conversation's
    /// author line must take BOTH the shield's visibility and its tooltip from
    /// the shared `DeputyBadge` machinery, never a private copy of the
    /// viewer-relevance rule. Three surfaces show this shield (author line,
    /// member-list row, member-info modal); freenet/river#451 is what happens
    /// when two of them drift.
    #[test]
    fn author_deputy_badge_uses_the_shared_helper() {
        let source = include_str!("conversation.rs");
        // Cut at THIS test module specifically, by a needle that cannot match
        // itself (it contains an escaped newline in the source, not a literal
        // one). Neither `find("#[cfg(test)]")` nor `rfind` works here: the
        // first lands on the `clear_message_html_cache` helper above and hides
        // most of the file, and the last lands on whichever test module was
        // appended most recently — freenet/river#471 added one, which silently
        // made every assertion below self-satisfying because the needles were
        // then inside the scanned text.
        let prod = &source[..source
            .find("#[cfg(test)]\nmod tests {")
            .expect("conversation.rs should have a `#[cfg(test)] mod tests` block")];

        assert!(
            prod.contains("message-author-deputy-badge"),
            "the conversation must render the 🛡 author badge"
        );
        assert!(
            prod.contains("deputy_badges_for_viewer"),
            "author-badge visibility must come from the shared \
             `deputy_badges_for_viewer` map"
        );
        assert!(
            prod.contains("badge.tooltip()"),
            "the author badge's tooltip must come from `DeputyBadge::tooltip` \
             so the wording cannot drift between surfaces"
        );
    }

    /// Nicknames reach the author line through `display_nickname`, so an emoji
    /// nickname cannot render a second, fake shield beside the real one. The
    /// crate-wide render-path scan lives in `crate::util::display_name`; this
    /// is the conversation-local statement of the same requirement.
    #[test]
    fn author_names_are_stripped_of_badge_glyphs() {
        use crate::util::display_name::sanitize_display_name;
        for spoof in ["Mallory 🛡", "Mallory 👑", "Mallory \u{1F6E1}\u{FE0F}"] {
            assert_eq!(sanitize_display_name(spoof), "Mallory");
        }
    }

    /// A mention token carries a `[name]` SNAPSHOT written by the sender. When
    /// the member reference doesn't resolve, that snapshot is what renders —
    /// so `@[Admin 🛡](rv:<nobody>)` would paint a shield inside a message with
    /// no nickname involved at all. Any member can type this into any message,
    /// which makes it a broader vector than the nickname one.
    #[test]
    fn unresolved_mention_chip_snapshot_is_sanitised() {
        let unknown = MemberId(freenet_scaffold::util::FastHash(0xDEAD_BEEF));
        let me = MemberId(freenet_scaffold::util::FastHash(1));
        let token = river_core::mention::encode_mention(unknown, "Admin 🛡");

        let html = message_to_html_with_mentions(&token, &HashMap::new(), Some(me));
        assert!(
            !html.contains('\u{1F6E1}'),
            "an unresolved mention rendered a badge glyph from its snapshot: {html}"
        );
        assert!(
            html.contains("Admin"),
            "the name itself must survive: {html}"
        );
    }

    /// Same snapshot, the other renderer: the quoted reply preview resolves
    /// mention tokens to plain `@name` and falls back to the sender-written
    /// snapshot for an unknown member.
    #[test]
    fn reply_preview_sanitises_an_unresolved_mention_snapshot() {
        let unknown = MemberId(freenet_scaffold::util::FastHash(0xDEAD_BEEF));
        let token = river_core::mention::encode_mention(unknown, "Admin 🛡");

        let preview = clean_reply_preview(&token, &HashMap::new());
        assert!(
            !preview.contains('\u{1F6E1}'),
            "reply preview rendered a badge glyph from a mention snapshot: {preview}"
        );
        assert!(preview.contains("@Admin"), "got: {preview}");
    }

    // A previous revision of this branch sanitised
    // `ReplyContentV1.target_author_name` on both decode paths, because the
    // reply strip rendered that sender-written snapshot. freenet/river#480
    // removed the field from the decoder's return type entirely, so there is
    // no longer a value to sanitise: the quote author comes from
    // `resolve_member_nickname` on live state, covered by
    // `resolve_reply_strip_tests::quote_author_label_is_sanitised`.

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

    /// The `> quoted` Markdown a reader types has to reach the stylesheet as a
    /// `<blockquote>` for any of the blockquote rules below to apply at all.
    /// `message_to_html` rewrites every single newline to a hard break
    /// (`"  \n"`) before Markdown runs, so pin that the block construct still
    /// parses through that rewrite.
    #[test]
    fn markdown_quote_renders_as_blockquote() {
        let html = message_to_html("> quoted line\n\nnormal line");
        assert!(
            html.contains("<blockquote>"),
            "`> …` must render as a <blockquote> for the blockquote CSS to \
             apply: {html}"
        );
    }

    /// Extract the declaration body of a CSS rule by its exact selector text.
    ///
    /// Anchored on a LINE START (`"\n{selector} {{"`), not a bare substring:
    /// `.prose blockquote {` is a suffix of `.bg-accent .prose blockquote {`,
    /// so a substring search would silently return the wrong rule's body if
    /// the two were ever reordered in the file.
    fn css_rule_body<'a>(css: &'a str, selector: &str) -> &'a str {
        let needle = format!("\n{selector} {{");
        let start = css
            .find(&needle)
            .unwrap_or_else(|| panic!("tailwind.css should contain a `{selector}` rule"))
            + needle.len();
        let end = start
            + css[start..]
                .find('}')
                .unwrap_or_else(|| panic!("`{selector}` rule should be closed"));
        &css[start..end]
    }

    /// A quote inside the local user's own bubble must take the bubble's own
    /// text colour, not a hardcoded one.
    ///
    /// `.bg-accent` is the solid brand-blue sent-message bubble, whose text is
    /// `text-white`, and it is the SAME blue in light and dark mode. The rule
    /// used to paint the quote `rgba(0, 0, 0, 0.8)`, so quoted text rendered
    /// near-black inside a bubble of white text — 3.52:1 against the bubble,
    /// under the 4.5:1 WCAG AA floor, in BOTH colour schemes. Pinning
    /// `color: inherit` is what keeps the quote tied to the bubble's palette
    /// instead of drifting from it again.
    ///
    /// The browser-side counterpart, which measures the real composited
    /// contrast in both colour schemes rather than the declaration text, is
    /// `ui/tests/blockquote-contrast.spec.ts`.
    #[test]
    fn sent_bubble_blockquote_inherits_bubble_text_colour() {
        const TAILWIND_CSS: &str = include_str!("../../assets/tailwind.css");
        let body = css_rule_body(TAILWIND_CSS, ".bg-accent .prose blockquote");

        // Check the LAST `color:` declaration, not merely that `inherit`
        // appears somewhere in the body. CSS is last-one-wins within a rule,
        // so `color: inherit; color: #111;` reintroduces the bug in full while
        // a `contains("color: inherit")` check happily passes — a substring
        // search cannot model the cascade. Take the final declaration, which
        // is the one that actually paints.
        //
        // `border-left-color:` also ends in `color:`, hence the `-` guard.
        let last_color = body
            .split(';')
            .map(str::trim)
            .filter(|d| d.starts_with("color:"))
            .next_back()
            .unwrap_or_else(|| {
                panic!("`.bg-accent .prose blockquote` should declare a colour. Found: {body}")
            });

        assert_eq!(
            last_color, "color: inherit",
            "the winning `color` declaration in `.bg-accent .prose blockquote` \
             must be `inherit` so the quote takes the sent bubble's own text \
             colour; a hardcoded colour is what made quoted text render \
             black-on-blue beside white text. Full rule: {body}"
        );
    }

    /// Quotes outside the sent bubble use a dedicated `--color-text-quote`
    /// token rather than the app-wide `--color-text-muted`.
    ///
    /// The muted token is tuned for glanceable metadata (timestamps, field
    /// labels) and falls under 4.5:1 on two of the backgrounds a quote
    /// actually appears over, both in LIGHT mode: `--color-surface` (4.47:1)
    /// and the `bg-accent/20` outgoing DM bubble (3.96:1, the worst quote
    /// surface in the app). The dark-mode equivalents were already fine.
    ///
    /// The token must be declared in BOTH colour schemes, and the two ways to
    /// break that fail DIFFERENTLY — which is why this counts declarations
    /// instead of asserting either symptom:
    ///
    /// - Drop the DARK value and `:root`'s light value still resolves, so
    ///   every dark-mode quote silently paints the light colour (2.49:1 on a
    ///   dark received bubble — worse than the bug this PR fixes).
    /// - Drop the LIGHT value and the token is undefined everywhere `:root`
    ///   would have supplied it, so `var()` is invalid at computed-value time,
    ///   `color` inherits the body text colour, and the muted look vanishes
    ///   entirely while contrast stays high enough that no AA floor notices.
    #[test]
    fn blockquote_uses_dedicated_quote_colour_token_in_both_schemes() {
        const TAILWIND_CSS: &str = include_str!("../../assets/tailwind.css");

        let body = css_rule_body(TAILWIND_CSS, ".prose blockquote");
        assert!(
            body.contains("color: var(--color-text-quote)"),
            "`.prose blockquote` must use `--color-text-quote`, not the \
             app-wide muted token, so a quote stays above the WCAG AA floor \
             without repainting every timestamp and label. Found: {body}"
        );

        // `:root` carries the light-mode values; the dark-mode overrides live
        // in the `prefers-color-scheme: dark` block. Both must define the
        // token, so count declarations rather than merely finding one.
        let declarations = TAILWIND_CSS.matches("--color-text-quote:").count();
        assert_eq!(
            declarations, 2,
            "`--color-text-quote` must be declared exactly twice — once in \
             `:root` (light) and once under `prefers-color-scheme: dark` — \
             found {declarations}"
        );
    }

    const SAMPLE_ID: &str = "UDzGbcWrKN748tYbhvbPCCCQrZc9r9xkN3tUuun5Rts";
    /// Real-shape 44-char base58 ID for tests that need a second distinct ID.
    const SAMPLE_ID_2: &str = "EqJ5YpEEV3XLqEvKWLQHFhGAac2qXzSUoE6k2zbdnXBr";

    // ---- Share links (freenet.org/open and `freenet:`) ----

    /// River's own contract id (a vector id in the shared share-link file).
    const RIVER_ID: &str = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";
    /// Harvest's contract id, for the store-link shape Harvest shares.
    const HARVEST_ID: &str = "6FzSeAUKcqJrveKyU8RJgGKc5jRB1Z2juvxXtwTA4Em9";

    fn anchor_to(html: &str, href: &str, text: &str) -> bool {
        html.contains(&format!(
            "<a target=\"_blank\" rel=\"noopener noreferrer\" href=\"{href}\">{text}</a>"
        ))
    }

    #[test]
    fn freenet_org_open_link_opens_app_on_own_node() {
        let link = format!("https://freenet.org/open#{HARVEST_ID}/#store=ABCDEFGHJKLMNPQR");
        let html = message_to_html(&format!("my store: {link}"));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{HARVEST_ID}/#store=ABCDEFGHJKLMNPQR"),
                &link
            ),
            "a freenet.org/open link must point at the app on the reader's node, \
             keeping the original text: {html}"
        );
        assert!(!html.contains("href=\"https://freenet.org"), "{html}");
    }

    #[test]
    fn freenet_org_open_slash_form_is_converted() {
        let link = format!("https://freenet.org/open/#{RIVER_ID}/?invitation=abc123&x=y");
        let html = message_to_html(&link);
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/?invitation=abc123&amp;x=y"),
                &link.replace('&', "&amp;")
            ),
            "{html}"
        );
    }

    #[test]
    fn bare_freenet_scheme_links_are_converted() {
        for link in [
            format!("freenet:{RIVER_ID}/"),
            format!("freenet://{RIVER_ID}/"),
            format!("FREENET:{HARVEST_ID}/#store=ABCDEFGHJKLMNPQR"),
        ] {
            let html = message_to_html(&format!("open {link} please"));
            let path = crate::util::share_link::parse_share_link(&link)
                .unwrap()
                .local_path();
            assert!(anchor_to(&html, &path, &link), "{link}: {html}");
        }
    }

    #[test]
    fn bare_freenet_link_trailing_punctuation_is_not_part_of_it() {
        let html = message_to_html(&format!(
            "try freenet:{RIVER_ID}. Or (freenet:{HARVEST_ID}/)!"
        ));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}"),
                &format!("freenet:{RIVER_ID}")
            ),
            "{html}"
        );
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{HARVEST_ID}/"),
                &format!("freenet:{HARVEST_ID}/")
            ),
            "{html}"
        );
    }

    #[test]
    fn angle_bracket_freenet_autolink_is_converted() {
        // The markdown crate emits `<a href="">` for a scheme it does not
        // allow; the share-link pass gives it its real destination.
        let html = message_to_html(&format!("<freenet:{RIVER_ID}/a/b#x/../y>"));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/a/b#x/../y"),
                &format!("freenet:{RIVER_ID}/a/b#x/../y")
            ),
            "{html}"
        );
    }

    #[test]
    fn share_links_in_code_are_not_converted() {
        for text in [
            format!("`freenet:{RIVER_ID}/`"),
            format!("```\nfreenet:{RIVER_ID}/\n```"),
            format!("`https://freenet.org/open#{RIVER_ID}/`"),
        ] {
            let html = message_to_html(&text);
            assert!(!html.contains("<a "), "{text:?}: {html}");
        }
    }

    #[test]
    fn freenet_scheme_inside_other_words_or_attributes_is_not_converted() {
        for text in [
            format!("xfreenet:{RIVER_ID}/"),
            format!("![freenet:{RIVER_ID}/](https://example.com/x.png)"),
        ] {
            let html = message_to_html(&text);
            assert!(!html.contains("/v1/contract/web/"), "{text:?}: {html}");
        }
    }

    /// The destination comes only from the visible text. A share-link label
    /// over a different href goes where the label says; a share-link href
    /// behind an ordinary label is left pointing at the freenet.org page
    /// (which shows the id), never turned into a one-click open.
    #[test]
    fn share_link_destination_always_matches_visible_text() {
        let html = message_to_html(&format!(
            "[freenet:{RIVER_ID}/](https://freenet.org/open#{HARVEST_ID}/)"
        ));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/"),
                &format!("freenet:{RIVER_ID}/")
            ),
            "{html}"
        );
        assert!(!html.contains(HARVEST_ID), "{html}");

        let html = message_to_html(&format!(
            "[https://freenet.org/open#{RIVER_ID}/](https://evil.example/)"
        ));
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{RIVER_ID}/\"")),
            "{html}"
        );
        assert!(!html.contains("evil.example"), "{html}");

        let html = message_to_html(&format!(
            "[click here](https://freenet.org/open#{HARVEST_ID}/)"
        ));
        assert!(
            html.contains(&format!("href=\"https://freenet.org/open#{HARVEST_ID}/\"")),
            "a labelled share link must stay a link to the freenet.org page: {html}"
        );
        assert!(!html.contains("/v1/contract/web/"), "{html}");
    }

    #[test]
    fn share_links_untouched_without_a_gateway() {
        let link = format!("https://freenet.org/open#{RIVER_ID}/");
        let html = message_to_html_inner(&link, false);
        assert!(html.contains(&format!("href=\"{link}\"")), "{html}");
        let html = message_to_html_inner(&format!("freenet:{RIVER_ID}/"), false);
        assert!(!html.contains("<a "), "{html}");
    }

    /// Markdown syntax that is valid inside a share link must not split it:
    /// `…/a*b*c` used to render as `…/a<em>b</em>c` and link only `…/a`.
    #[test]
    fn markdown_syntax_inside_a_bare_link_does_not_split_it() {
        for rest in ["/a*b*c", "/a_b_c", "/a~~b~~c", "/x*y", "/#a*b*c"] {
            let link = format!("freenet:{RIVER_ID}{rest}");
            let html = message_to_html(&format!("see {link} now"));
            let path = format!("/v1/contract/web/{RIVER_ID}{rest}");
            assert!(anchor_to(&html, &path, &link), "{link}: {html}");
        }
    }

    #[test]
    fn emphasised_bare_link_is_linked_inside_the_emphasis() {
        let html = message_to_html(&format!(
            "*freenet:{RIVER_ID}/* and **freenet:{HARVEST_ID}/**"
        ));
        assert!(
            html.contains(&format!(
                "<em><a target=\"_blank\" rel=\"noopener noreferrer\" \
                 href=\"/v1/contract/web/{RIVER_ID}/\">freenet:{RIVER_ID}/</a></em>"
            )),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                "<strong><a target=\"_blank\" rel=\"noopener noreferrer\" \
                 href=\"/v1/contract/web/{HARVEST_ID}/\">freenet:{HARVEST_ID}/</a></strong>"
            )),
            "{html}"
        );
    }

    #[test]
    fn bare_link_at_the_very_start_of_a_message() {
        let link = format!("freenet:{RIVER_ID}/");
        let html = message_to_html(&link);
        assert!(
            anchor_to(&html, &format!("/v1/contract/web/{RIVER_ID}/"), &link),
            "{html}"
        );
    }

    /// Byte offsets from the markdown parser must line up with the source even
    /// after multi-byte text: the code-span link stays code, the other links.
    #[test]
    fn non_ascii_before_links_keeps_code_exclusion_aligned() {
        let text = format!("é ü 🦀 `freenet:{RIVER_ID}/` then freenet:{HARVEST_ID}/");
        let html = message_to_html(&text);
        assert!(
            html.contains(&format!("<code>freenet:{RIVER_ID}/</code>")),
            "{html}"
        );
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{HARVEST_ID}/"),
                &format!("freenet:{HARVEST_ID}/")
            ),
            "{html}"
        );
        assert!(
            !html.contains(&format!("/v1/contract/web/{RIVER_ID}")),
            "{html}"
        );
    }

    /// A bare link inside a construct that is not prose must never nest an
    /// anchor or land in an attribute.
    #[test]
    fn bare_links_inside_links_titles_and_footnotes_stay_plain() {
        for text in [
            format!("[see freenet:{RIVER_ID}/](https://example.com/)"),
            format!("[x](https://example.com/ \"freenet:{RIVER_ID}/\")"),
            format!("text[^freenet:{RIVER_ID}/]\n\n[^freenet:{RIVER_ID}/]: note"),
            format!("[r]: https://example.com/ \"freenet:{RIVER_ID}/\"\n\n[x][r]"),
            format!("<span title=\"freenet:{RIVER_ID}/\">x</span>"),
            format!("[x](freenet:{RIVER_ID}/)"),
            format!("![x](freenet:{RIVER_ID}/)"),
        ] {
            let html = message_to_html(&text);
            assert!(
                !html.contains(&format!("href=\"/v1/contract/web/{RIVER_ID}")),
                "{text:?}: {html}"
            );
            assert!(
                !html.contains('\u{E002}') && !html.contains("%EE%80%82"),
                "{text:?}: sentinel leaked: {html}"
            );
            // No anchor opens inside another anchor.
            for piece in html.split("<a ").skip(1) {
                let close = piece.find("</a>").unwrap_or(piece.len());
                assert!(!piece[..close].contains("<a "), "{text:?}: nested: {html}");
            }
        }
    }

    /// A message cannot forge the private-use sentinels the bare-link pass
    /// uses: they are stripped before any are inserted.
    #[test]
    fn forged_bare_link_sentinels_are_inert() {
        let text = format!("\u{E002}0\u{E003} \u{E002}1\u{E003} freenet:{RIVER_ID}/");
        let html = message_to_html(&text);
        assert_eq!(html.matches("<a ").count(), 1, "{html}");
        assert!(
            !html.contains('\u{E002}') && !html.contains('\u{E003}'),
            "{html}"
        );
    }

    #[test]
    fn share_links_render_alongside_mentions() {
        let member = MemberId(freenet_scaffold::util::FastHash(42));
        let names: HashMap<MemberId, String> = [(member, "Bob".to_string())].into();
        let token = river_core::mention::encode_mention(member, "Bob");
        let html = message_to_html_with_mentions(
            &format!("{token} try freenet:{RIVER_ID}/ and https://freenet.org/open#{HARVEST_ID}/"),
            &names,
            None,
        );
        assert!(html.contains("river-mention"), "{html}");
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/"),
                &format!("freenet:{RIVER_ID}/")
            ),
            "{html}"
        );
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{HARVEST_ID}/"),
                &format!("https://freenet.org/open#{HARVEST_ID}/")
            ),
            "{html}"
        );
    }

    /// A converted anchor carries nothing else from the message, so a `title`
    /// cannot name a different link in the tooltip.
    #[test]
    fn converted_share_link_drops_title() {
        let html = message_to_html(&format!(
            "[freenet:{RIVER_ID}/](https://x.example/ \"https://freenet.org/open#{HARVEST_ID}/\")"
        ));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/"),
                &format!("freenet:{RIVER_ID}/")
            ),
            "{html}"
        );
        assert!(!html.contains("title="), "{html}");
    }

    /// Pre-existing gateway-URL rewrite: an ENCODED dot segment is resolved by
    /// the browser just like `..`, so it must be refused too.
    #[test]
    fn gateway_url_with_encoded_dot_segments_is_not_rewritten() {
        for suffix in [
            format!("/%2e%2e/{SAMPLE_ID_2}/"),
            "/%2E%2e/%2e%2E/%2e%2e/permission/apps".to_string(),
            "/.%2e/x".to_string(),
            "/%2e./x".to_string(),
        ] {
            let url = format!("https://x.example/v1/contract/web/{SAMPLE_ID}{suffix}");
            let html = message_to_html(&format!("[River update]({url})"));
            assert!(
                !html.contains("href=\"/v1/contract/web/"),
                "{suffix}: {html}"
            );
        }
    }

    /// Message text is attacker-controlled: a crafted body must not make the
    /// bare-link scan super-linear. `freenet:freenet:…)))` made every
    /// occurrence a candidate spanning the rest of the run, trimmed one
    /// character at a time with a full parenthesis recount each step (O(n^3):
    /// 186 s for 16 KB natively). The size is small enough that a cubic
    /// regression still FINISHES (2.4 s release, far longer in a debug test
    /// build) and fails the bound, rather than hanging the test run; the fixed
    /// scan renders all of these in milliseconds.
    #[test]
    fn adversarial_bare_link_text_renders_in_bounded_time() {
        let n = 4 * 1024;
        let payloads = [
            format!("{}{}", "freenet:".repeat(n / 16), ")".repeat(n / 2)),
            format!("freenet:{}", ")".repeat(n)),
            "freenet:".repeat(n / 8),
            format!("freenet:{RIVER_ID}/ ").repeat(n / 55),
            "(freenet:x) ".repeat(n / 12),
        ];
        for payload in payloads {
            let started = std::time::Instant::now();
            let _ = message_to_html(&payload);
            let elapsed = started.elapsed();
            assert!(
                elapsed < std::time::Duration::from_secs(10),
                "rendering {} bytes took {elapsed:?}",
                payload.len()
            );
        }
    }

    /// Every path that renders user-written text as markdown: message bodies
    /// (with and without the gateway-only bare link pass, which also parses
    /// to an AST), room descriptions, bodies with mentions, and reply
    /// previews.
    fn render_every_markdown_path(text: &str) {
        let member = MemberId(freenet_scaffold::util::FastHash(7));
        let names: HashMap<MemberId, String> = [(member, "Bob".to_string())].into();
        for behind_gateway in [true, false] {
            let _ = message_to_html_inner(text, behind_gateway);
            let _ = description_to_html(text, behind_gateway);
        }
        let with_mention = format!(
            "{} {text}",
            river_core::mention::encode_mention(member, "Bob")
        );
        let _ = message_to_html_with_mentions(&with_mention, &names, Some(member));
        let _ = clean_reply_preview(text, &names);
        let _ = strip_markdown(text);
    }

    /// Inputs the unpatched `markdown` crate panics on (see
    /// `[patch.crates-io]` in the root Cargo.toml).
    #[test]
    fn malformed_markdown_renders_without_panicking() {
        let inputs = [
            // A line ending inside a link title or reference label. The
            // renderer turns every `\n` into a hard break (`"  \n"`), so a
            // plain line break inside the title is enough.
            "[a](b \"x\ny\")",
            "[a](b 'x\ny')",
            "[a](b (x\ny))",
            "![a](b \"x\ny\")",
            "[a](b \"x \ny\")",
            "[a](b \"x \r\ny\")",
            "[x](/x \"> \n\")",
            "[a][b\nc]\n\n[b c]: d",
            "[][a \n]\n\n[a ]:\0",
            // An email address in an image title that spans lines.
            "![a](b \"c@d.com\ne\")",
            // Setext underlines next to each other.
            "=\n=\n=\na\n=",
            "}\n-\n--\n]\n=",
            // A list item ending in unclosed code or HTML, then another marker.
            "1. <!--\n-",
            "*\t~~~\n1.",
            "- ```\n1)",
            // An unfinished CDATA opener, then an empty numeric reference.
            "<![C&#;",
            // A table head, then a new container on the last line.
            "a\n|-\n- <",
            "a\n|-\n> <",
        ];
        for input in inputs {
            for text in [input.to_string(), format!("freenet:{RIVER_ID}/ {input}")] {
                let rendered = std::panic::catch_unwind(|| render_every_markdown_path(&text));
                assert!(rendered.is_ok(), "rendering {text:?} panicked");
            }
        }
    }

    /// The AST helpers walk and drop a tree without recursion, so a deep tree
    /// cannot overflow the stack even if one gets past
    /// `markdown_cost_is_bounded`. Runs on a small stack to leave a margin
    /// below wasm's 1 MiB.
    #[test]
    fn deep_markdown_trees_are_walked_without_recursion() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let deep = format!("{}`a` b", ">".repeat(5_000));
                assert!(!non_prose_ranges(&deep).is_empty());
                let tree = markdown::to_mdast(&deep, &markdown::ParseOptions::gfm()).unwrap();
                let mut text = String::new();
                collect_mdast_text(&tree, &mut text);
                assert!(text.starts_with('a'), "{text}");
                drop_mdast(tree);
            })
            .expect("spawn")
            .join()
            .expect("walking a deep markdown tree panicked");
    }

    /// Deep block nesting on one line, a wide table, or too much text is
    /// shown as plain text, however the line breaks are written.
    #[test]
    fn costly_markdown_is_shown_as_plain_text() {
        let deep_list = format!("{}x", "- ".repeat(MARKDOWN_MAX_LINE_NESTING + 1));
        let deep_quote = format!("{}x", "> 1. ".repeat(MARKDOWN_MAX_LINE_NESTING));
        let wide_table = format!("{}\n{}\n|", "|a".repeat(40), "|-".repeat(40));
        for costly in [
            deep_list.clone(),
            format!("a\r{deep_list}"),
            format!("a\r\n{deep_quote}"),
            format!("[^a]: {deep_list}"),
            format!("\u{feff}{deep_list}"),
            format!("{BARE_LINK_OPEN}{deep_list}"),
            wide_table,
            "a\n".repeat(MARKDOWN_MAX_SOURCE_BYTES),
        ] {
            assert!(!markdown_cost_is_bounded(&costly), "{costly:?}");
            assert!(message_to_html(&costly).starts_with("<p>"), "{costly:?}");
        }
        for fine in [
            format!("{}x", "- ".repeat(MARKDOWN_MAX_LINE_NESTING)),
            "> quote\n- item\n  1. nested".to_string(),
            "| a | b |\n| - | - |\n| 1 | 2 |".to_string(),
            "-1 and 2.5 and -x".to_string(),
        ] {
            assert!(markdown_cost_is_bounded(&fine), "{fine:?}");
        }
        assert_eq!(line_container_depth("  > - 1) * x"), 4);
        assert_eq!(line_container_depth(">>> x"), 3);
        assert_eq!(line_container_depth("-x"), 0);
        assert_eq!(line_container_depth("2024. was"), 1);
        assert_eq!(line_container_depth("[^a]: > - x"), 3);
        assert_eq!(line_container_depth("[^a] x"), 0);
    }

    /// Markdown that renders to far more HTML than its size is shown as plain
    /// text instead.
    #[test]
    fn markdown_that_expands_too_far_is_shown_as_plain_text() {
        let text = format!("[a]: b '{}'\n\n{}", "\"".repeat(100), "[a]".repeat(300));
        assert!(markdown_cost_is_bounded(&text));
        assert!(render_gfm(&text).len() > MARKDOWN_MAX_HTML_BYTES);
        let html = message_to_html(&text);
        assert!(html.starts_with("<p>[a]: b"), "{}", &html[..200]);
        assert!(html.len() < 2 * text.len(), "{}", html.len());
        assert!(description_to_html(&text, true).starts_with("<p>[a]: b"));
        // The plain text is the message as written, without the hard-break
        // rewrite, so CRLF line breaks are not doubled.
        let crlf = text.replace('\n', "\r\n");
        let html = message_to_html(&crlf);
        assert!(html.starts_with("<p>[a]: b"), "{}", &html[..200]);
        assert!(!html.contains("  <br />"), "{}", &html[..400]);
    }

    /// A mention that markdown moves into an attribute or copies (through a
    /// reference definition's title) is not substituted there: the message
    /// is shown as plain text with one chip per mention.
    #[test]
    fn mentions_markdown_moves_or_copies_fall_back_to_plain_text() {
        let member = MemberId(freenet_scaffold::util::FastHash(7));
        let names: HashMap<MemberId, String> = [(member, "Bob".to_string())].into();
        let token = river_core::mention::encode_mention(member, "Bob");
        let copied = format!("[a]: b '{}'\n\n{}", token.repeat(20), "[a] ".repeat(150));
        let in_title = format!("[x](https://x.example \"{token}\")");
        let in_destination = format!("[x](https://x.example/{token})");
        let in_alt = format!("![{token}](https://x.example/i.png)");
        for text in [copied, in_title, in_destination, in_alt] {
            let html = message_to_html_with_mentions(&text, &names, None);
            assert!(html.starts_with("<p>"), "{html}");
            assert!(!html.contains("<a "), "{html}");
            assert!(html.len() < 20 * text.len(), "{}", html.len());
            assert!(html.contains("river-mention"), "{html}");
        }
        // A mention in link text still renders as a chip inside the link.
        let html = message_to_html_with_mentions(
            &format!("[hi {token}](https://x.example)"),
            &names,
            None,
        );
        assert!(
            html.contains("<a ") && html.contains("river-mention"),
            "{html}"
        );
    }

    /// Past the size limit, text is shown as escaped plain text with its line
    /// breaks, and mentions still become chips.
    #[test]
    fn text_past_the_markdown_limit_renders_as_plain_text() {
        let member = MemberId(freenet_scaffold::util::FastHash(7));
        let names: HashMap<MemberId, String> = [(member, "Bob".to_string())].into();
        let long = "**<b>x</b>**\n".repeat(MARKDOWN_MAX_SOURCE_BYTES / 10);
        assert!(long.len() > MARKDOWN_MAX_SOURCE_BYTES);
        let html = message_to_html(&long);
        assert!(
            html.starts_with("<p>**&lt;b&gt;x&lt;/b&gt;**<br />\n"),
            "{html}"
        );
        assert!(
            !html.contains("<strong>") && !html.contains("<b>"),
            "{html}"
        );
        let with_mention = format!(
            "{} {long}",
            river_core::mention::encode_mention(member, "Bob")
        );
        let html = message_to_html_with_mentions(&with_mention, &names, None);
        assert!(html.contains("river-mention"), "{html}");
        assert!(description_to_html(&long, true).starts_with("<p>**&lt;b&gt;"));
        // The reply preview parses only a prefix, as markdown.
        assert!(
            strip_markdown(&long).starts_with('x'),
            "{}",
            strip_markdown(&long)
        );
    }

    /// A seeded sweep over short strings of markdown syntax, the shape that
    /// found every input above. It is deterministic (fixed seed), so a failure
    /// reproduces exactly; the panic message names the input.
    #[test]
    fn generated_markdown_renders_without_panicking() {
        #[rustfmt::skip]
        const PIECES: &[&str] = &[
            "[", "]", "(", ")", "\"", "'", " ", "  ", "\t", "\n", "\n", "\r\n", "\r", "a",
            "x y", "!", ":", "<", ">", "*", "_", "~", "`", "```", "~~~", "\\", "-", "#", "|",
            "^", "=", "&", "&amp;", "&#;", "&#65;", "https://x.example", "www.a.example",
            "c@d.example", "1.", "1)", "é", "\u{a0}", "😀", "[a](b \"", "[a](b '", "[a](b (",
            "[a][", "[^", "]: ", "![", "](", "<a ", "<!--", "-->", "<![C", "    ", "> ",
            "- ", "---", "| - |", "\\\n", "[ ]", "[x]", "\0",
        ];
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..3_000 {
            let len = 1 + (next() % 24) as usize;
            let text: String = (0..len)
                .map(|_| PIECES[(next() % PIECES.len() as u64) as usize])
                .collect();
            let rendered = std::panic::catch_unwind(|| render_every_markdown_path(&text));
            assert!(rendered.is_ok(), "rendering {text:?} panicked");
        }
    }

    #[test]
    fn longest_valid_bare_link_still_converts() {
        let rest = format!("/{}", "a".repeat(1999));
        let link = format!("freenet://{RIVER_ID}{rest}");
        let html = message_to_html(&format!("{link}."));
        assert!(
            anchor_to(&html, &format!("/v1/contract/web/{RIVER_ID}{rest}"), &link),
            "a 2000-byte rest is valid and must still link"
        );
    }

    /// `[<freenet:A/>](https://x)` nests an anchor inside a link label; the
    /// inner one used to escape `finalize_anchors` with an empty href.
    #[test]
    fn nested_autolink_in_a_label_is_flattened() {
        let html = message_to_html(&format!("[<freenet:{RIVER_ID}/>](https://evil.example/)"));
        assert_eq!(html.matches("<a ").count(), 1, "{html}");
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/"),
                &format!("freenet:{RIVER_ID}/")
            ),
            "{html}"
        );
        assert!(!html.contains("evil.example"), "{html}");

        let html = message_to_html("[see <mailto:a@b.example> here](https://x.example/)");
        assert_eq!(html.matches("<a ").count(), 1, "{html}");
        assert_eq!(html.matches("</a>").count(), 1, "{html}");
        assert!(html.contains("href=\"https://x.example/\""), "{html}");
    }

    /// A link that opens a contract on the reader's node must not carry a
    /// label naming a different contract; such a label is shown as text.
    #[test]
    fn label_naming_another_contract_is_not_a_link() {
        for hidden in [
            format!("http://x.example/v1/contract/web/{SAMPLE_ID}/"),
            format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/"),
            format!("//127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/"),
            format!("/v1/contract/web/{SAMPLE_ID}/"),
            format!("http:/v1/contract/web/{SAMPLE_ID}/"),
            format!("HTTPS:../{SAMPLE_ID}/"),
        ] {
            for label in [
                format!("freenet:{RIVER_ID}/ "),
                format!("freenet:{RIVER_ID}/&#8203;"),
                format!("*freenet:{RIVER_ID}/* app"),
                format!("https://freenet.org/open#{RIVER_ID}/ "),
                format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID_2}/"),
                "freenet:raAqMhMG".to_string(),
            ] {
                let html = message_to_html(&format!("[{label}]({hidden})"));
                assert!(!html.contains("<a "), "{label:?} -> {hidden}: {html}");
            }
        }
    }

    /// Labels that name the destination's own contract, or none at all, keep
    /// their link (and the gateway rewrite).
    #[test]
    fn honest_labels_keep_their_link_and_rewrite() {
        let href = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        let local = format!("href=\"/v1/contract/web/{SAMPLE_ID}/\"");
        for label in [
            "River update".to_string(),
            "my freenet: node".to_string(),
            format!("freenet:{SAMPLE_ID} (mirror)"),
            format!("freenet:{}", &SAMPLE_ID[..8]),
            format!("freenet:{}\u{22EF}{}", &SAMPLE_ID[..8], &SAMPLE_ID[36..]),
        ] {
            let html = message_to_html(&format!("[{label}]({href})"));
            assert!(html.contains(&local), "{label:?}: {html}");
        }
        let html = message_to_html(&format!(
            "[/v1/contract/web/{SAMPLE_ID}/](/v1/contract/web/{SAMPLE_ID}/)"
        ));
        assert!(html.contains("<a "), "{html}");
        let html = message_to_html("[see /contract/web/ docs](/docs)");
        assert!(html.contains("<a "), "{html}");
        // A bare link whose href is the markdown crate's normalisation of its
        // text is still rewritten.
        let bare = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/{{x}}");
        let html = message_to_html(&bare);
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/%7Bx%7D\"")),
            "{html}"
        );
        // So is a v2 gateway URL.
        let html = message_to_html(&format!(
            "http://127.0.0.1:7509/v2/contract/web/{SAMPLE_ID}/"
        ));
        assert!(
            html.contains(&format!("href=\"/v2/contract/web/{SAMPLE_ID}/\"")),
            "{html}"
        );
    }

    /// The label check sees what the reader sees: invisible characters,
    /// character references, fancy text, homoglyphs and image alt text do not
    /// hide an id.
    #[test]
    fn disguised_ids_in_labels_are_still_seen() {
        let hidden = format!("http://x.example/v1/contract/web/{SAMPLE_ID}/");
        for label in [
            RIVER_ID.to_string(),
            format!("free&#8203;net:{RIVER_ID}/"),
            format!("freenet\u{FF1A}{RIVER_ID}/"),
            format!("open {RIVER_ID}"),
            format!("open {}", RIVER_ID.replace('a', "\u{0430}")),
            format!("open {}", RIVER_ID.replace('M', "\u{200B}M\u{2060}")),
            format!("open {}", RIVER_ID.replace('r', "&#114;")),
            format!("free\u{200B}net:{}", &RIVER_ID[..10]),
            format!("\u{1D41F}\u{1D42B}eenet:{RIVER_ID}"),
            format!("![freenet:{RIVER_ID}](x.png)"),
        ] {
            let html = message_to_html(&format!("[{label}]({hidden})"));
            assert!(
                !html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}"))
                    && !html.contains(&format!("href=\"{hidden}\"")),
                "{label:?}: {html}"
            );
        }
    }

    /// A dot segment moves where the browser lands, so a link whose path
    /// has one (or that is a contract path the parser refuses) has an unknown
    /// destination, and any contract its label names blocks it.
    #[test]
    fn dot_segments_make_the_destination_unknown() {
        let a = RIVER_ID;
        let b = SAMPLE_ID;
        for href in [
            format!("/v1/contract/web/{a}/../{b}/"),
            format!("/v1/contract/web/{a}/%2e%2e/{b}/"),
            format!("/v1/contract/web/{a}/.%2E/{b}/"),
            format!("http:/v1/contract/web/{a}/../{b}/"),
            format!("http://127.0.0.1:7509/v1/contract/web/{a}/../{b}/"),
            format!("//127.0.0.1:7509/v1/contract/web/{a}/%2e%2e/{b}/"),
            format!("http://127.0.0.1:7509/v1/./contract/web/{b}/"),
            format!("http://127.0.0.1:7509/v1/contract/web/%36%46{}/", &b[2..]),
        ] {
            for label in [format!("River: {a}"), "open freenet:raAqMhMG".to_string()] {
                let html = message_to_html(&format!("[{label}]({href})"));
                assert!(!html.contains("<a "), "{label:?} -> {href}: {html}");
            }
        }
        // A relative "bare" link hides nothing only if it parses; this one
        // shows A while resolving elsewhere.
        let rel = format!("/v1/contract/web/{a}/%2e%2e/{b}/");
        let html = message_to_html(&format!("[{rel}]({rel})"));
        assert!(!html.contains("<a "), "{html}");
    }

    /// Breaks the reader does not see (or reads as an id wrapping) do not
    /// split an id into runs too short to count.
    #[test]
    fn invisible_or_wrapping_breaks_do_not_split_a_claimed_id() {
        let (head, tail) = RIVER_ID.split_at(20);
        let hidden = format!("/v1/contract/web/{SAMPLE_ID}/");
        for label in [
            format!("{head}![]({hidden}x.png){tail}"),
            format!("{head}![Q](x.png){tail}"),
            format!("{head}\n{tail}"),
            format!("{head}\u{0591}{tail}"),
            format!("{head}\u{200A}{tail}"),
            format!("{head}\u{2009}{tail}"),
            format!("{head}\u{202F}{tail}"),
            "freenet:/raAqMhMG".to_string(),
            "freenet\u{A789}raAqMhMG".to_string(),
            format!("{head}\u{000B}{tail}"),
            format!("{head}\u{000C}{tail}"),
            format!("{head}\u{0085}{tail}"),
            format!("{head}\u{2028}{tail}"),
            format!("{head}\u{2029}{tail}"),
            format!("{head}\u{A7B3}{tail}"),
            format!("{head}\u{10317}{tail}"),
            format!("{head}\u{05BC}\u{05BC}\u{05BC}\u{05BC}{tail}"),
            format!("{head}\u{A7AB}{tail}"),
            format!("{head}\u{2D5D}{tail}"),
            format!("{head}\u{302A}{tail}"),
            format!("{head}\u{1D167}{tail}"),
            "freenet:\u{05BC}raAqMhMG".to_string(),
            "freenet:/\u{0E31}raAqMhMG".to_string(),
            "freenet\u{A4FD}raAqMhMG".to_string(),
            format!("{head}\u{222A}{tail}"),
            format!("{head}\u{2A2F}{tail}"),
            format!("{head}\u{311A}{tail}"),
            format!("{head}{}{tail}", "\u{05BC}".repeat(12)),
            format!("{head}\u{27D9}{tail}"),
            format!("freenet:{}\u{2178}GbcW", &SAMPLE_ID_2[..3]),
            format!("{head}{}{tail}", "\u{0EC8}".repeat(4)),
            format!("{head}{}{tail}", "\u{0730}".repeat(9)),
            format!("{head}\u{0E31}{}{tail}", "\u{0730}".repeat(4)),
            format!("{head}\u{222A}\u{05C4}{tail}"),
            format!("{head}\u{2A2F}\u{15F7}{tail}"),
            format!("{head}\u{2282}\u{10B3}{tail}"),
            format!("{head}\u{2E26}{tail}"),
            format!("{head}\u{3147}{tail}"),
            "freenet\u{2236}raAqMhMG".to_string(),
        ] {
            let html = message_to_html(&format!("[{label}]({hidden})"));
            assert!(
                !html.contains(&format!("href=\"{hidden}\"")),
                "{label:?}: {html}"
            );
        }
    }

    /// A tooltip is label text too: one naming another contract unlinks a
    /// link that opens on the reader's node; an honest one is kept.
    #[test]
    fn node_link_title_is_checked_like_the_label() {
        let href = format!("/v1/contract/web/{SAMPLE_ID}/");
        for text in [
            format!("[River]({href} \"freenet:{RIVER_ID}\")"),
            format!("[![River](x.png \"freenet:{RIVER_ID}\")]({href})"),
        ] {
            let html = message_to_html(&text);
            assert!(!html.contains("<a "), "{text:?}: {html}");
        }
        let html = message_to_html(&format!(
            "[Explore with Atlas](http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/ \"Atlas search engine\")"
        ));
        assert!(
            html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/\""))
                && html.contains("title=\"Atlas search engine\""),
            "{html}"
        );
    }

    #[test]
    fn round5_label_bypasses_are_closed() {
        let a = RIVER_ID;
        let b = SAMPLE_ID;
        let (head, tail) = a.split_at(20);
        for text in [
            // `://` later in a relative href does not make it absolute.
            format!("[River {a}](/v1/contract/./web/{b}/?x=://)"),
            format!("[River {a}](../{b}/#://)"),
            format!("[freenet:raAqMhMG](../{b}/#://)"),
            // Unfolded homoglyphs and zero-width marks inside an id.
            format!("[{head}\u{0261}{tail}](/v1/contract/web/{b}/)"),
            format!("[{head}\u{051A}{tail}](/v1/contract/web/{b}/)"),
            format!("[{head}\u{0417}{tail}](/v1/contract/web/{b}/)"),
            format!("[River: {head}\u{05BC}{tail}](/v1/contract/web/{b}/)"),
            format!("[open freenet:raAq\u{05BC}MhMG](/v1/contract/web/{b}/)"),
            // A look-alike that is not base58.
            format!("[River: {head}O{tail}](/v1/contract/web/{b}/)"),
            // Userinfo shows `freenet:<A>` first.
            format!(
                "[http://freenet:{a}@x.example/v1/contract/web/{b}/](http://127.0.0.1:7509/v1/contract/web/{b}/)"
            ),
        ] {
            let html = message_to_html(&text);
            assert!(!html.contains("<a "), "{text:?}: {html}");
        }
        // A bare URL whose text hides where it lands (encoded dot segment and
        // id) is not a link.
        let html = message_to_html(&format!(
            "http://127.0.0.1:7509/v1/contract/web/{a}/%2e%2e/%36%46{}/",
            &b[2..]
        ));
        assert!(!html.contains("<a "), "{html}");
        let rel = format!("/v1/contract/web/{a}/%2e%2e/{b}/#://");
        let html = message_to_html(&format!("[{rel}]({rel})"));
        assert!(!html.contains("<a "), "{html}");
    }

    #[test]
    fn claim_rule_edges() {
        let b = SAMPLE_ID;
        let href = format!("/v1/contract/web/{b}/");
        // A claim must be a PREFIX of the destination id, not just appear in it.
        let html = message_to_html(&format!("[freenet:{}]({href})", &b[3..12]));
        assert!(!html.contains("<a "), "substring, not prefix: {html}");
        let html = message_to_html(&format!("[freenet:{}]({href})", &b[..9]));
        assert!(html.contains("<a "), "a true prefix is consistent: {html}");
        // A long lowercase run (a hex hash) is not an id claim.
        let html = message_to_html(&format!(
            "[commit 3f2a9c1be47d58a6f0c2e9b1d3a4f5e6c7b8a9d0]({href})"
        ));
        assert!(html.contains("<a "), "hex hash is not a claim: {html}");
        // A URL-shaped label for the right contract that ALSO names another.
        let html = message_to_html(&format!(
            "[http://other.example/v1/contract/web/{b}/freenet:{RIVER_ID}](http://other.example/v1/contract/web/{b}/x)"
        ));
        assert!(!html.contains("<a "), "embedded second claim: {html}");
        // A bare URL whose id is percent-encoded does not show where it goes.
        let html = message_to_html(&format!(
            "http://127.0.0.1:7509/v1/contract/web/%36%46{}/",
            &b[2..]
        ));
        assert!(!html.contains("<a "), "encoded id: {html}");
    }

    /// A pasted URL is a link to what it shows, but not when the text before
    /// its contract path reads as another destination, or its tooltip does.
    #[test]
    fn pasted_url_that_poses_before_its_path_is_not_a_link() {
        let b = SAMPLE_ID;
        let a = RIVER_ID;
        for url in [
            format!("http://freenet.org:{a}@127.0.0.1:7509/v1/contract/web/{b}/#freenet"),
            format!("http://freenet:{a}@127.0.0.1:7509/v1/contract/web/{b}/?via=freenet"),
            format!("http://{a}.example/v1/contract/web/{b}/#freenet"),
            format!("http://River@127.0.0.1:7509/v1/contract/web/{b}/"),
        ] {
            for text in [url.clone(), format!("<{url}>"), format!("[{url}]({url})")] {
                let html = message_to_html(&text);
                assert!(!html.contains("<a "), "{text:?}: {html}");
            }
        }
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{b}/");
        let html = message_to_html(&format!("[{url}]({url} \"freenet:{a}\")"));
        assert!(!html.contains("<a "), "title on a pasted link: {html}");
    }

    #[test]
    fn freenet_in_prose_labels_is_not_a_claim() {
        let href = format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/");
        for label in [
            "Try it on Freenet: Harvest market",
            "Freenet: Ghostkey",
            "Get Freenet: Windows installer",
            "freenet:Harvest",
            "Freenet: GitHub mirror",
            "freenet: iPhone app",
            "Freenet: River2026 launch",
            "Freenet: Stra\u{00DF}e",
            "Join\u{00A0}the\u{00A0}Freenet\u{00A0}Official\u{00A0}River\u{00A0}Chat\u{00A0}Room",
            "FreenetのRiverでチャット、DeltaでWebサイト、AtlasでHarvest市場",
            "ติดตั้งFreenetแล้วเปิดRiverและDeltaหรือGhostKeyและHarvestได้เลย",
        ] {
            let html = message_to_html(&format!("[{label}]({href})"));
            assert!(
                html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/\"")),
                "{label:?}: {html}"
            );
        }
    }

    #[test]
    fn bare_link_in_a_table_cell_stops_at_the_cell() {
        let html = message_to_html(&format!("|a|b|\n|-|-|\n|freenet:{RIVER_ID}/x|y|"));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/x"),
                &format!("freenet:{RIVER_ID}/x")
            ),
            "{html}"
        );
        assert!(html.contains("<td>y</td>"), "{html}");
    }

    #[test]
    fn bare_link_inside_cjk_text() {
        let html = message_to_html(&format!("打开freenet:{RIVER_ID}/。"));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}/"),
                &format!("freenet:{RIVER_ID}/")
            ),
            "{html}"
        );
    }

    /// A pasted gateway URL is a link to what it shows (host and contract
    /// id first); when its path also names a contract it is shown in full
    /// rather than shortened to an 8-character id prefix.
    #[test]
    fn pasted_gateway_url_naming_another_contract_is_shown_in_full() {
        for url in [
            format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/freenet:{RIVER_ID}"),
            format!("http://127.0.0.1:7509/v1/contract/web/{SAMPLE_ID}/x/{RIVER_ID}"),
        ] {
            let html = message_to_html(&url);
            assert!(html.contains(&format!(">{url}</a>")), "{html}");
            assert!(
                html.contains(&format!("href=\"/v1/contract/web/{SAMPLE_ID}/")),
                "{html}"
            );
        }
    }

    /// River's own invite links carry a long base58 code; a pasted one must
    /// stay a (shortened) link, not be read as naming another contract.
    #[test]
    fn pasted_river_invite_link_stays_a_link() {
        let code =
            "2NEpo7TZRRrLZSi2U7MpNwwdMV3fjDaZaYcAhMR1FUD4xYLQ7mQAnfk9dVYNEUoGXxBxcMGRgHtqXPC";
        let url = format!("http://127.0.0.1:7509/v1/contract/web/{RIVER_ID}/?invitation={code}");
        for text in [
            url.clone(),
            format!("Join: {url}"),
            format!("2. Open this link: {url}"),
        ] {
            let html = message_to_html(&text);
            assert!(
                html.contains(&format!(
                    "href=\"/v1/contract/web/{RIVER_ID}/?invitation={code}\""
                )),
                "{text:?}: {html}"
            );
            assert!(
                html.contains(&format!(
                    ">freenet:{}/?invitation={code}</a>",
                    &RIVER_ID[..8]
                )),
                "{text:?}: {html}"
            );
        }
    }

    /// A share link that sits right against a non-prose range (no space)
    /// still links: the overlap sweep treats ranges as half-open.
    #[test]
    fn bare_link_right_after_a_code_span_still_links() {
        let html = message_to_html(&format!("`code`freenet:{RIVER_ID}"));
        assert!(
            anchor_to(
                &html,
                &format!("/v1/contract/web/{RIVER_ID}"),
                &format!("freenet:{RIVER_ID}")
            ),
            "{html}"
        );
    }

    /// A relative link already resolves on the reader's node, so behind a
    /// label that reads as a different Freenet link it is shown as text.
    #[test]
    fn relative_link_behind_a_freenet_looking_label_is_unlinked() {
        for text in [
            format!("[freenet:{RIVER_ID}/ ](/v1/contract/web/{SAMPLE_ID}/)"),
            format!("[**freenet:{RIVER_ID}/**](../{SAMPLE_ID}/)"),
            format!("[https://freenet.org/open#{RIVER_ID}/ ](/v1/contract/web/{SAMPLE_ID}/)"),
            format!("[freenet:{RIVER_ID}/ ](javascript:alert(1))"),
        ] {
            let html = message_to_html(&text);
            assert!(!html.contains("<a "), "{text:?}: {html}");
            assert!(html.contains("freenet"), "{text:?}: label kept: {html}");
        }
        // An ordinary relative link, and an external one, are left alone.
        let html = message_to_html(&format!("[docs](/v1/contract/web/{SAMPLE_ID}/)"));
        assert!(html.contains("<a "), "{html}");
        let html = message_to_html(&format!("[freenet:{RIVER_ID}/ ](https://example.com/)"));
        assert!(html.contains("href=\"https://example.com/\""), "{html}");
    }

    #[test]
    fn relative_href_classification() {
        for h in ["", "/v1/x", "../x", "x", "?q", "#f", "a/b:c"] {
            assert!(is_relative_href(h), "{h:?}");
        }
        for h in ["//evil.example/", "https://x", "mailto:a@b", "javascript:x"] {
            assert!(!is_relative_href(h), "{h:?}");
        }
    }

    /// The source scan and the render parse different text (a sentinel in
    /// place of the link); where they disagree, the message renders as if the
    /// bare-link pass had not run, never with a sentinel in a URL or with
    /// content lost.
    #[test]
    fn bare_link_pass_falls_back_when_the_two_parses_disagree() {
        for text in [
            format!("[x](freenet:{RIVER_ID}/(a )"),
            format!("![x](freenet:{RIVER_ID}/(a )"),
            format!("[r]: freenet:{RIVER_ID}/(a \n\n[x][r]"),
            format!("[r]: /x/freenet:{RIVER_ID}/(\n\n[r]"),
            format!("[see freenet:{RIVER_ID}/] ](https://example.com/)"),
        ] {
            let html = message_to_html(&text);
            let plain = message_to_html_inner(&text, false);
            assert!(
                !html.contains('\u{E002}') && !html.to_ascii_uppercase().contains("%EE%80%82"),
                "{text:?}: {html}"
            );
            // Same structure as a render with no bare-link pass (the gateway
            // flag only changes hrefs, and none of these has a gateway URL).
            assert_eq!(html, plain, "{text:?}");
        }
    }

    #[test]
    fn restore_accepts_only_prose_sentinels_each_exactly_once() {
        let links = vec![format!("freenet:{RIVER_ID}/")];
        let s = |i: usize| format!("{BARE_LINK_OPEN}{i}{BARE_LINK_CLOSE}");
        let ok = restore_bare_freenet_links(&format!("<p>a {} b</p>", s(0)), &links);
        assert_eq!(
            ok.as_deref(),
            Some(format!("<p>a <a href=\"\">freenet:{RIVER_ID}/</a> b</p>").as_str())
        );
        for html in [
            format!("<p><code>{}</code></p>", s(0)),
            format!("<p><a href=\"x\">{}</a></p>", s(0)),
            format!("<pre><code>{}</code></pre>", s(0)),
            format!("<img alt=\"{}\" />", s(0)),
            format!("<p>{} {}</p>", s(0), s(0)),
            "<p>missing</p>".to_string(),
            format!("<p>{}</p>", s(1)),
            format!("<a href=\"%EE%80%820%EE%80%83\">x</a><p>{}</p>", s(0)),
        ] {
            assert_eq!(restore_bare_freenet_links(&html, &links), None, "{html}");
        }
    }

    /// Candidates longer than the longest valid link plus trailing-punctuation
    /// slack are not examined at all (the scan's work bound), even if trimming
    /// would have left a valid link.
    #[test]
    fn bare_link_candidate_length_cap_is_enforced() {
        let link = format!("freenet:{RIVER_ID}");
        let short = format!("{link}{}", "!".repeat(10));
        let html = message_to_html(&short);
        assert!(html.contains("href=\"/v1/contract/web/"), "{html}");
        let over = format!("{link}{}", "!".repeat(MAX_BARE_LINK_CANDIDATE_LEN));
        let html = message_to_html(&over);
        assert!(!html.contains("<a "), "over-long candidate must be skipped");
    }

    /// Whitespace, `<` and `>` end an angle-bracket autolink, after which GFM
    /// autolinks the leading part as a plain URL. A vector containing one
    /// therefore tests markdown's tokenising, not the validator, and its
    /// prefix may legitimately be a valid link (the text shown is then that
    /// prefix, so it is still honest).
    fn ends_markdown_autolink(raw: &str) -> bool {
        raw.contains(|c: char| c.is_whitespace() || c == '<' || c == '>')
    }

    /// Through the full renderer, every shared vector must convert to exactly
    /// its `local_path` (valid) or produce no link to a node path at all
    /// (invalid). The angle-bracket autolink form keeps the text verbatim
    /// (no GFM trailing-punctuation trimming), so the check is exact.
    #[test]
    fn shared_vectors_through_the_renderer() {
        let parsed: serde_json::Value =
            serde_json::from_str(include_str!("../util/share-link-vectors.json")).unwrap();
        for v in parsed["vectors"].as_array().unwrap() {
            let raw = v["raw"].as_str().unwrap();
            let note = v["note"].as_str().unwrap_or("");
            for prefix in ["https://freenet.org/open#", "freenet:"] {
                let html = message_to_html(&format!("<{prefix}{raw}>"));
                if v["valid"].as_bool().unwrap() {
                    let path = v["local_path"].as_str().unwrap();
                    let href = format!("href=\"{}\"", escape_html_attr(path));
                    assert!(
                        html.contains(&href),
                        "valid vector ({note}) via {prefix:?}: expected {href} in {html}"
                    );
                } else if !ends_markdown_autolink(raw) {
                    assert!(
                        !html.contains("href=\"/v1/"),
                        "invalid vector ({note}) via {prefix:?} must not link \
                         to the node: {html}"
                    );
                }
            }
            // Bare text: an invalid vector must never become a node link.
            // Whitespace ends a bare candidate (as it ends any autolink), so a
            // vector containing it tests tokenising, not validation: skip it.
            if !v["valid"].as_bool().unwrap() && !raw.contains(char::is_whitespace) {
                let html = message_to_html(&format!("see freenet:{raw} now"));
                assert!(
                    !html.contains("href=\"/v1/"),
                    "invalid vector ({note}) as bare text must not link: {html}"
                );
            }
        }
    }

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
        // Its text names a contract but the dot segments move where it lands
        // (on the reader's node too, when the host is theirs), so since #733
        // it is shown as plain text rather than as a link to either place.
        assert!(
            !html.contains("<a ") && html.contains(&url),
            "a `..` gateway URL must be shown as text, never as a link: {html}"
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

    fn msg_id(seed: &[u8]) -> MessageId {
        MessageId(freenet_scaffold::util::fast_hash(seed))
    }

    /// The cached renderer must always return exactly what the uncached
    /// renderer would for the *current* inputs — a repeat call (hit) and a
    /// content edit (same id) must each reflect current state.
    #[test]
    fn cached_message_html_matches_uncached_across_input_changes() {
        clear_message_html_cache();
        let id = msg_id(b"m-cache-1");
        let alice = mid_from("00000000000000aa");
        let me = mid_from("00000000000000bb");
        let mut names = HashMap::new();
        names.insert(alice, "Alice".to_string());
        let fp = member_names_fingerprint(&names);

        // Plain (mention-free) body: repeat call returns identical HTML.
        let text = "hello **world** https://freenet.org";
        let first = render_message_html_cached(&id, text, &names, fp, Some(me));
        let second = render_message_html_cached(&id, text, &names, fp, Some(me));
        assert_eq!(first, second, "cache hit is identical");
        assert_eq!(
            first,
            message_to_html_with_mentions(text, &names, Some(me)),
            "cached output equals uncached"
        );

        // Editing the body (same id) invalidates and re-renders.
        let edited = "goodbye **world**";
        let after_edit = render_message_html_cached(&id, edited, &names, fp, Some(me));
        assert_eq!(
            after_edit,
            message_to_html_with_mentions(edited, &names, Some(me)),
            "edited content re-rendered"
        );
        assert_ne!(after_edit, first, "edit changed the HTML");
    }

    /// A mention chip renders the member's *current* nickname; renaming the
    /// member must invalidate the cached body even though its text is unchanged.
    #[test]
    fn cached_message_html_reflects_member_rename_for_mentions() {
        clear_message_html_cache();
        let id = msg_id(b"m-cache-2");
        let alice = mid_from("00000000000000aa");
        let me = mid_from("00000000000000bb");
        let token = river_core::mention::encode_mention(alice, "Alice");
        let text = format!("hey {token}!");

        let mut names = HashMap::new();
        names.insert(alice, "Alice".to_string());
        let fp1 = member_names_fingerprint(&names);
        let before = render_message_html_cached(&id, &text, &names, fp1, Some(me));
        assert!(
            before.contains("Alice"),
            "chip shows current name: {before}"
        );

        // Rename Alice -> Roberta (not a substring of the old name). Same
        // message text, new member map.
        names.insert(alice, "Roberta".to_string());
        let fp2 = member_names_fingerprint(&names);
        assert_ne!(fp1, fp2, "rename changes the member fingerprint");
        let after = render_message_html_cached(&id, &text, &names, fp2, Some(me));
        assert_eq!(
            after,
            message_to_html_with_mentions(&text, &names, Some(me)),
            "rename invalidated the cached chip"
        );
        assert!(after.contains("Roberta"), "chip shows new name: {after}");
        assert!(!after.contains("Alice"), "old name gone: {after}");
    }

    /// A mention-free body must fingerprint the same regardless of the member
    /// map, so ordinary messages survive member joins/renames in the cache; a
    /// body carrying a mention token must incorporate the member fingerprint.
    #[test]
    fn message_html_fingerprint_gates_member_inputs_on_mentions() {
        let me = mid_from("00000000000000bb");
        let plain = "just a normal message, no mentions here";
        assert_eq!(
            message_html_fingerprint(plain, 111, Some(me)),
            message_html_fingerprint(plain, 222, Some(me)),
            "mention-free fingerprint ignores member fp"
        );

        let alice = mid_from("00000000000000aa");
        let token = river_core::mention::encode_mention(alice, "Alice");
        let mentioned = format!("ping {token}");
        assert_ne!(
            message_html_fingerprint(&mentioned, 111, Some(me)),
            message_html_fingerprint(&mentioned, 222, Some(me)),
            "mention fingerprint depends on member fp"
        );
    }

    /// `prune_message_html_cache` drops entries for messages no longer visible
    /// (e.g. after a room switch) while keeping the still-visible ones.
    #[test]
    fn prune_message_html_cache_bounds_to_visible_set() {
        clear_message_html_cache();
        let me = mid_from("00000000000000bb");
        let names = HashMap::new();
        let fp = member_names_fingerprint(&names);
        let keep = msg_id(b"keep");
        let drop = msg_id(b"drop");
        render_message_html_cached(&keep, "keep me", &names, fp, Some(me));
        render_message_html_cached(&drop, "drop me", &names, fp, Some(me));
        assert_eq!(MESSAGE_HTML_CACHE.with(|c| c.borrow().len()), 2);

        let mut visible = std::collections::HashSet::new();
        visible.insert(keep.clone());
        prune_message_html_cache(&visible);

        MESSAGE_HTML_CACHE.with(|c| {
            let c = c.borrow();
            assert!(c.contains_key(&keep), "visible entry kept");
            assert!(!c.contains_key(&drop), "off-screen entry pruned");
        });
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
            Some(mid_from("00000000000000ff")),
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
        let html =
            message_to_html_with_mentions(&token, &names, Some(mid_from("0000000000000001")));
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
        let html =
            message_to_html_with_mentions(&token, &names, Some(mid_from("0000000000000002")));
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
        let html = message_to_html_with_mentions(&token, &names, Some(me));
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
            message_to_html_with_mentions(text, &names, Some(any)),
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
        let html = message_to_html_with_mentions(&text, &names, Some(mid_from("00000000000000ff")));
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
        let html =
            message_to_html_with_mentions(&token, &names, Some(mid_from("00000000000000ff")));
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
        let html =
            message_to_html_with_mentions(&legacy, &names, Some(mid_from("0000000000000001")));
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
        let html =
            message_to_html_with_mentions(&token, &names, Some(mid_from("0000000000000001")));
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
        let html = message_to_html_with_mentions(&legacy, &names, Some(me));
        assert!(
            html.contains("river-mention-self"),
            "legacy self-mention must get the self class: {html}"
        );
    }
}

/// Tests for [`resolve_reply_strip`] — the rule that a reply's quoted
/// snapshot is rendered only when it can be re-read from live room state.
///
/// These target the helper rather than `group_messages` because it is the unit
/// under test; `group_messages` itself is now callable outside a Dioxus runtime
/// and is covered by `group_messages_clock_tests`. The render side (which arm
/// of [`ReplyStrip`] produces what markup) is covered by
/// `ui/tests/message-layout.spec.ts`.
#[cfg(test)]
mod resolve_reply_strip_tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::content::{TextContentV1, CONTENT_TYPE_TEXT, TEXT_CONTENT_VERSION};
    use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
    use std::time::{Duration, SystemTime};

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn member_id_of(sk: &SigningKey) -> MemberId {
        MemberId::from(&sk.verifying_key())
    }

    fn authored(
        owner: MemberId,
        sk: &SigningKey,
        content: RoomMessageBody,
        at_secs: u64,
    ) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner,
                author: member_id_of(sk),
                time: SystemTime::UNIX_EPOCH + Duration::from_secs(at_secs),
                content,
            },
            sk,
        )
    }

    fn state(messages: Vec<AuthorizedMessageV1>) -> MessagesV1 {
        MessagesV1 {
            messages,
            actions_state: Default::default(),
        }
    }

    /// Public `member_info` naming `sk`'s member, as an unencrypted room has.
    fn named(sk: &SigningKey, nickname: &str) -> AuthorizedMemberInfo {
        AuthorizedMemberInfo::new(
            MemberInfo::new_public(member_id_of(sk), 1, nickname.to_string()),
            sk,
        )
    }

    fn info(entries: Vec<AuthorizedMemberInfo>) -> MemberInfoV1 {
        MemberInfoV1 {
            member_info: entries,
        }
    }

    fn resolve(
        reply: &AuthorizedMessageV1,
        messages: &MessagesV1,
        member_info: &MemberInfoV1,
    ) -> ReplyStrip {
        resolve_reply_strip(
            &reply.message.content,
            messages,
            member_info,
            &HashMap::new(),
            &HashMap::new(),
        )
    }

    /// Destructure a `Quote`, failing loudly with the actual variant otherwise.
    fn expect_quote(strip: ReplyStrip) -> (String, String) {
        match strip {
            ReplyStrip::Quote {
                author, preview, ..
            } => (author, preview),
            other => panic!("expected a rendered quote, got {other:?}"),
        }
    }

    /// The quote's author label is the target author's CURRENT nickname, so it
    /// goes through the same sanitiser as the message header above it. Without
    /// this, `Alice 🛡` would paint a shield into the reply strip — one line
    /// under the real author line and its real badge.
    ///
    /// This covers the LIVE path. `extract_reply_context`'s own sanitisation of
    /// the sender-written snapshot name is defence in depth: both of its
    /// callers now discard that field.
    #[test]
    fn quote_author_label_is_sanitised() {
        let owner_sk = signing_key(1);
        let alice_sk = signing_key(2);
        let bob_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);

        let quoted = authored(
            owner,
            &alice_sk,
            RoomMessageBody::public("morning".to_string()),
            10,
        );
        let reply = authored(
            owner,
            &bob_sk,
            RoomMessageBody::reply(
                "morning!".to_string(),
                quoted.id(),
                "whatever the sender claims".to_string(),
                "morning".to_string(),
            ),
            20,
        );
        let member_info = info(vec![named(&alice_sk, "Alice \u{1F6E1}")]);

        let (author, preview) = expect_quote(resolve(
            &reply,
            &state(vec![quoted, reply.clone()]),
            &member_info,
        ));
        assert_eq!(author, "Alice", "quote author kept a badge glyph");
        assert_eq!(preview, "morning");
    }

    const ABUSE: &str = "you are all worthless, buy my coin at scam.example";

    /// Build the abuse/reply pair the ban scenario uses: an abusive message and
    /// somebody quoting it, with the quote snapshotting the abusive text.
    fn abuse_and_reply(
        owner: MemberId,
        abuser_sk: &SigningKey,
        replier_sk: &SigningKey,
    ) -> (AuthorizedMessageV1, AuthorizedMessageV1) {
        let abusive = authored(
            owner,
            abuser_sk,
            RoomMessageBody::public(ABUSE.to_string()),
            10,
        );
        let reply = authored(
            owner,
            replier_sk,
            RoomMessageBody::reply(
                "please stop".to_string(),
                abusive.id(),
                "Abuser".to_string(),
                ABUSE.to_string(),
            ),
            20,
        );
        (abusive, reply)
    }

    /// The motivating case. Banning a member makes `post_apply_cleanup` purge
    /// their messages, so the only surviving copy of their text is the snapshot
    /// embedded in everyone's replies. That copy must not be rendered.
    #[test]
    fn banned_authors_quoted_text_is_not_rendered() {
        let owner_sk = signing_key(1);
        let abuser_sk = signing_key(2);
        let replier_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);
        let (abusive, reply) = abuse_and_reply(owner, &abuser_sk, &replier_sk);
        let member_info = info(vec![named(&abuser_sk, "Abuser")]);

        // Before the ban the abusive message is present, so the quote renders.
        // Pins that the assertion below is about the purge, not a dead fixture.
        let before = resolve(&reply, &state(vec![abusive, reply.clone()]), &member_info);
        assert_eq!(expect_quote(before).1, ABUSE);

        // After the ban the contract has purged the abuser's message — and
        // their `member_info` record with it.
        let after = resolve(&reply, &state(vec![reply.clone()]), &info(vec![]));
        assert_eq!(
            after,
            ReplyStrip::Unavailable,
            "a banned member's text must not survive in the quote, and the \
             attribution must be dropped with it — 'Abuser: [removed]' still \
             attributes the quoted content to them"
        );
    }

    /// Deliberate trade-off, pinned so it is not "fixed" by accident: a target
    /// that merely aged out of the bounded `recent_messages` window is hidden
    /// exactly like a purged one. Nothing distinguishes the two — the snapshot
    /// carries no `MemberId`, `MessageId` is a hash of the signature, and a
    /// banned member leaves no nickname anywhere in state to match a name
    /// against. This is why the placeholder wording must stay neutral and never
    /// claim "banned".
    #[test]
    fn quote_of_aged_out_message_is_also_hidden() {
        let owner_sk = signing_key(1);
        let alice_sk = signing_key(2);
        let bob_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);

        let old = authored(
            owner,
            &alice_sk,
            RoomMessageBody::public("ancient history".to_string()),
            1,
        );
        let reply = authored(
            owner,
            &bob_sk,
            RoomMessageBody::reply(
                "agreed".to_string(),
                old.id(),
                "Alice".to_string(),
                "ancient history".to_string(),
            ),
            2,
        );
        // `old` has since scrolled out of the window; other, newer messages
        // remain, so this is a window effect rather than an empty room. Alice
        // is still very much a member.
        let survivor = authored(
            owner,
            &alice_sk,
            RoomMessageBody::public("something newer".to_string()),
            3,
        );

        let ctx = resolve(
            &reply,
            &state(vec![reply.clone(), survivor]),
            &info(vec![named(&alice_sk, "Alice")]),
        );
        assert_eq!(ctx, ReplyStrip::Unavailable);
    }

    /// An ordinary reply to a message that is still present renders the quote,
    /// reading the CURRENT text and the CURRENT nickname so edits and renames
    /// both propagate.
    #[test]
    fn quote_shows_current_text_and_current_nickname_of_present_target() {
        let owner_sk = signing_key(1);
        let alice_sk = signing_key(2);
        let bob_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);

        let original = authored(
            owner,
            &alice_sk,
            RoomMessageBody::public("original wording".to_string()),
            10,
        );
        let reply = authored(
            owner,
            &bob_sk,
            RoomMessageBody::reply(
                "good point".to_string(),
                original.id(),
                // Stale snapshot: Alice's nickname at the time of the reply.
                "Alice (old nick)".to_string(),
                "original wording".to_string(),
            ),
            20,
        );

        let mut messages = state(vec![original.clone(), reply.clone()]);
        messages
            .actions_state
            .edited_content
            .insert(original.id(), "edited wording".to_string());

        // Alice has since renamed herself.
        let ctx = resolve(&reply, &messages, &info(vec![named(&alice_sk, "Alice")]));
        assert_eq!(
            ctx,
            ReplyStrip::Quote {
                author: "Alice".to_string(),
                preview: "edited wording".to_string(),
                target_id: original.id(),
            },
            "the quote must track edits and renames, and keep the target id so \
             scroll-to-original still works"
        );
    }

    /// Duplicate `member_info` records are legal until `post_apply_cleanup`
    /// dedups them. The quote must pick the same CANONICAL record the message
    /// header picks, or one member is labelled two different ways on screen.
    #[test]
    fn quote_author_uses_the_canonical_member_info_record() {
        let owner_sk = signing_key(1);
        let alice_sk = signing_key(2);
        let bob_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);

        let original = authored(
            owner,
            &alice_sk,
            RoomMessageBody::public("hello".to_string()),
            10,
        );
        let reply = authored(
            owner,
            &bob_sk,
            RoomMessageBody::reply(
                "hi".to_string(),
                original.id(),
                "Alice".to_string(),
                "hello".to_string(),
            ),
            20,
        );

        // Two records for Alice; the higher version is canonical.
        let canonical = AuthorizedMemberInfo::new(
            MemberInfo::new_public(member_id_of(&alice_sk), 2, "Alice v2".to_string()),
            &alice_sk,
        );
        let stale = AuthorizedMemberInfo::new(
            MemberInfo::new_public(member_id_of(&alice_sk), 1, "Alice v1".to_string()),
            &alice_sk,
        );

        // BOTH orderings. A single ordering only pins one of the two ways this
        // regresses, while looking deliberate: `[canonical, stale]` catches a
        // last-wins `.collect()` into a map but a first-match `.find()` sails
        // through it, and `[stale, canonical]` catches the reverse. Only
        // `canonical()`'s rank comparison satisfies both.
        for (order, member_info) in [
            (
                "canonical first",
                info(vec![canonical.clone(), stale.clone()]),
            ),
            ("stale first", info(vec![stale, canonical])),
        ] {
            let (author, _) = expect_quote(resolve(
                &reply,
                &state(vec![original.clone(), reply.clone()]),
                &member_info,
            ));
            assert_eq!(
                author, "Alice v2",
                "must match `member_info.canonical()`, which is what the \
                 message header uses ({order})"
            );
        }
    }

    /// Deleting a message removes its text from the room; quotes of it included.
    #[test]
    fn quote_of_deleted_message_is_hidden() {
        let owner_sk = signing_key(1);
        let alice_sk = signing_key(2);
        let bob_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);

        let original = authored(
            owner,
            &alice_sk,
            RoomMessageBody::public("regrettable message".to_string()),
            10,
        );
        let reply = authored(
            owner,
            &bob_sk,
            RoomMessageBody::reply(
                "hmm".to_string(),
                original.id(),
                "Alice".to_string(),
                "regrettable message".to_string(),
            ),
            20,
        );

        // A soft delete leaves the message in `messages` but marks it deleted.
        let mut messages = state(vec![original.clone(), reply.clone()]);
        messages.actions_state.deleted.insert(original.id());

        let ctx = resolve(&reply, &messages, &info(vec![named(&alice_sk, "Alice")]));
        assert_eq!(ctx, ReplyStrip::Unavailable);
    }

    /// The snapshot is written and signed by the REPLIER and validated by
    /// nothing, so it can name an author who did not write the target and quote
    /// text the target never contained. Both halves must come from the target.
    #[test]
    fn forged_snapshot_is_ignored_in_favour_of_the_real_target() {
        let owner_sk = signing_key(1);
        let carol_sk = signing_key(2);
        let forger_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);

        let real = authored(
            owner,
            &carol_sk,
            RoomMessageBody::public("what Carol actually said".to_string()),
            10,
        );
        let forged = authored(
            owner,
            &forger_sk,
            RoomMessageBody::reply(
                "see, Alice said it".to_string(),
                real.id(),
                "Alice".to_string(),
                "something Alice never said".to_string(),
            ),
            20,
        );
        let member_info = info(vec![named(&carol_sk, "Carol")]);

        let (author, preview) = expect_quote(resolve(
            &forged,
            &state(vec![real, forged.clone()]),
            &member_info,
        ));
        assert_eq!(author, "Carol");
        assert_eq!(preview, "what Carol actually said");

        // A snapshot pointing at a message that never existed renders nothing.
        let fabricated = authored(
            owner,
            &forger_sk,
            RoomMessageBody::reply(
                "look what Alice wrote".to_string(),
                MessageId(freenet_scaffold::util::fast_hash(b"no such message")),
                "Alice".to_string(),
                "something Alice never said".to_string(),
            ),
            30,
        );
        let ctx = resolve(&fabricated, &state(vec![fabricated.clone()]), &member_info);
        assert_eq!(ctx, ReplyStrip::Unavailable);
    }

    /// Action and event messages live in `messages` but are never rendered as
    /// rows. Quoting one would put machine copy ("[Reaction 👍 to …]", "joined
    /// the room") on screen as if it were the author's words, and point
    /// scroll-to-original at a `msg-{id}` element that does not exist.
    #[test]
    fn quote_of_an_action_or_event_message_is_hidden() {
        let owner_sk = signing_key(1);
        let alice_sk = signing_key(2);
        let bob_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);
        let member_info = info(vec![named(&alice_sk, "Alice")]);

        let anchor = authored(
            owner,
            &alice_sk,
            RoomMessageBody::public("anchor".to_string()),
            5,
        );
        let action = authored(
            owner,
            &alice_sk,
            RoomMessageBody::reaction(anchor.id(), "\u{1f44d}".to_string()),
            10,
        );
        let event = authored(owner, &alice_sk, RoomMessageBody::join_event(), 11);

        for target in [&action, &event] {
            let reply = authored(
                owner,
                &bob_sk,
                RoomMessageBody::reply(
                    "quoting a non-message".to_string(),
                    target.id(),
                    "Alice".to_string(),
                    "something plausible".to_string(),
                ),
                20,
            );
            let mut messages = state(vec![
                anchor.clone(),
                action.clone(),
                event.clone(),
                reply.clone(),
            ]);
            // Give the target an EDIT. Without this the filter is untestable:
            // resolution would already fail inside `target_plaintext` (an
            // action/event body has no text), so deleting the filter would keep
            // the test green. `effective_text` consults `edited_content` BEFORE
            // any content-type decode, and an event IS editable by its author —
            // so this is the real attack: post a join event, edit it to
            // arbitrary text, quote it. The filter is what stops that.
            messages
                .actions_state
                .edited_content
                .insert(target.id(), "attacker-chosen text".to_string());
            assert_eq!(
                resolve(&reply, &messages, &member_info),
                ReplyStrip::Unavailable,
                "a reply targeting {:?} must not render a quote",
                target.message.content.content_type()
            );
        }
    }

    /// A private target we cannot decrypt has NOT been re-read, so it must fall
    /// through to the placeholder. Quoting `decrypt_message_content`'s output
    /// would put a diagnostic string ("[Encrypted message - secret v1
    /// unavailable]") on screen as if it were the quoted author's words, while
    /// reporting the quote as verified.
    #[test]
    fn private_target_is_quoted_only_when_its_secret_is_available() {
        use crate::util::ecies::encrypt_with_symmetric_key;

        let owner_sk = signing_key(1);
        let alice_sk = signing_key(2);
        let bob_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);
        let member_info = info(vec![named(&alice_sk, "Alice")]);

        let secret_v1 = [7u8; 32];
        let (ciphertext, nonce) = encrypt_with_symmetric_key(
            &secret_v1,
            &TextContentV1::new("private words".to_string()).encode(),
        );
        let target = authored(
            owner,
            &alice_sk,
            RoomMessageBody::private(
                CONTENT_TYPE_TEXT,
                TEXT_CONTENT_VERSION,
                ciphertext,
                nonce,
                1,
            ),
            10,
        );
        // The reply itself is public so the test isolates the TARGET's
        // decryptability, which is the thing under test.
        let reply = authored(
            owner,
            &bob_sk,
            RoomMessageBody::reply(
                "responding".to_string(),
                target.id(),
                "Alice".to_string(),
                "private words".to_string(),
            ),
            20,
        );
        let messages = state(vec![target, reply.clone()]);

        // Holding v1: the target is genuinely re-read.
        let with_secret = resolve_reply_strip(
            &reply.message.content,
            &messages,
            &member_info,
            &HashMap::from([(1u32, secret_v1)]),
            &HashMap::new(),
        );
        assert_eq!(expect_quote(with_secret).1, "private words");

        // Rotated past v1 (holding only v2): genuinely unavailable. Must NOT
        // quote the "[Encrypted message - secret vN unavailable]" diagnostic.
        assert_eq!(
            resolve_reply_strip(
                &reply.message.content,
                &messages,
                &member_info,
                &HashMap::from([(2u32, [9u8; 32])]),
                &HashMap::new(),
            ),
            ReplyStrip::Unavailable,
            "a target encrypted under a rotated-past version is unavailable"
        );

        // Cold start: `RoomData.secrets` is `#[serde(skip)]`, so an established
        // private room transiently holds NOTHING on every page load. That is
        // "still decrypting", not "unavailable" — flashing the placeholder over
        // every reply on every load is the freenet/river#284 alarm class. No
        // strip at all, matching what the pre-fix code rendered here.
        assert_eq!(
            resolve_reply_strip(
                &reply.message.content,
                &messages,
                &member_info,
                &HashMap::new(),
                &HashMap::new(),
            ),
            ReplyStrip::NotAReply,
            "a room whose secrets have not rehydrated yet must not flash the \
             unavailable placeholder over every reply"
        );
    }

    /// A target whose author has no `member_info` record yet (sync lag) still
    /// renders its text, attributed to the same "Unknown" the message header
    /// uses rather than to the replier's snapshot name.
    #[test]
    fn quote_of_target_with_unsynced_author_is_attributed_unknown() {
        let owner_sk = signing_key(1);
        let alice_sk = signing_key(2);
        let bob_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);

        let original = authored(
            owner,
            &alice_sk,
            RoomMessageBody::public("hello".to_string()),
            10,
        );
        let reply = authored(
            owner,
            &bob_sk,
            RoomMessageBody::reply(
                "hi".to_string(),
                original.id(),
                "Alice".to_string(),
                "hello".to_string(),
            ),
            20,
        );

        let (author, _) = expect_quote(resolve(
            &reply,
            &state(vec![original, reply.clone()]),
            &info(vec![]),
        ));
        assert_eq!(author, "Unknown");
    }

    /// The preview is truncated so a long quoted message cannot dominate the
    /// bubble. Truncation happens after mention/markdown cleaning.
    #[test]
    fn quote_preview_is_truncated() {
        let owner_sk = signing_key(1);
        let alice_sk = signing_key(2);
        let bob_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);

        let long = "x".repeat(500);
        let original = authored(owner, &alice_sk, RoomMessageBody::public(long.clone()), 10);
        let reply = authored(
            owner,
            &bob_sk,
            RoomMessageBody::reply(
                "short".to_string(),
                original.id(),
                "Alice".to_string(),
                long,
            ),
            20,
        );

        let (_, preview) = expect_quote(resolve(
            &reply,
            &state(vec![original, reply.clone()]),
            &info(vec![named(&alice_sk, "Alice")]),
        ));
        assert_eq!(preview.chars().count(), 100);
    }

    /// A private reply whose own secret we lack cannot even be decoded, so we
    /// cannot tell what it quotes. It is still a REPLY (`content_type` is
    /// cleartext on the `Private` variant), so it reports `Unavailable` rather
    /// than silently dropping the strip — UNLESS we hold no secrets at all,
    /// which is the cold-start window and must look like nothing. riverctl
    /// reports the same for both.
    #[test]
    fn undecodable_private_reply_reports_unavailable() {
        use crate::util::ecies::encrypt_with_symmetric_key;
        use river_core::room_state::content::{ReplyContentV1, REPLY_CONTENT_VERSION};

        let owner_sk = signing_key(1);
        let alice_sk = signing_key(2);
        let bob_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);
        let member_info = info(vec![named(&alice_sk, "Alice")]);

        let secret = [3u8; 32];
        let target = authored(
            owner,
            &alice_sk,
            RoomMessageBody::public("target text".to_string()),
            10,
        );
        let (ciphertext, nonce) = encrypt_with_symmetric_key(
            &secret,
            &ReplyContentV1::new(
                "sealed reply".to_string(),
                target.id(),
                "SNAPSHOT AUTHOR".to_string(),
                "SNAPSHOT PREVIEW".to_string(),
            )
            .encode(),
        );
        let reply = authored(
            owner,
            &bob_sk,
            RoomMessageBody::private(
                river_core::room_state::content::CONTENT_TYPE_REPLY,
                REPLY_CONTENT_VERSION,
                ciphertext,
                nonce,
                4,
            ),
            20,
        );
        let messages = state(vec![target, reply.clone()]);

        // With the secret the quote resolves against the live target.
        let with_secret = resolve_reply_strip(
            &reply.message.content,
            &messages,
            &member_info,
            &HashMap::from([(4u32, secret)]),
            &HashMap::new(),
        );
        assert_eq!(expect_quote(with_secret).1, "target text");

        // Holding a DIFFERENT version — the room rotated past v4 — we cannot
        // read the reply at all, but we still know it IS one, so say so.
        assert_eq!(
            resolve_reply_strip(
                &reply.message.content,
                &messages,
                &member_info,
                &HashMap::from([(5u32, [1u8; 32])]),
                &HashMap::new(),
            ),
            ReplyStrip::Unavailable
        );

        // Holding NO secrets at all is the cold-start window, not a real
        // failure: render nothing rather than alarming the user.
        assert_eq!(
            resolve(&reply, &messages, &member_info),
            ReplyStrip::NotAReply,
            "cold start must not flash the placeholder"
        );
    }

    /// A PUBLIC reply can quote a PRIVATE target: the composer deliberately
    /// falls back to sending a reply publicly when the room secret is missing.
    /// So the target-side cold-start path is reachable even though the reply
    /// itself decoded fine, and it must behave like the reply-side one.
    #[test]
    fn public_reply_to_undecryptable_private_target() {
        use crate::util::ecies::encrypt_with_symmetric_key;
        use river_core::room_state::content::TEXT_CONTENT_VERSION;

        let owner_sk = signing_key(1);
        let alice_sk = signing_key(2);
        let bob_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);
        let member_info = info(vec![named(&alice_sk, "Alice")]);

        let secret = [8u8; 32];
        let (ciphertext, nonce) = encrypt_with_symmetric_key(
            &secret,
            &TextContentV1::new("sealed target".to_string()).encode(),
        );
        let target = authored(
            owner,
            &alice_sk,
            RoomMessageBody::private(
                CONTENT_TYPE_TEXT,
                TEXT_CONTENT_VERSION,
                ciphertext,
                nonce,
                3,
            ),
            10,
        );
        let reply = authored(
            owner,
            &bob_sk,
            RoomMessageBody::reply(
                "public reply".to_string(),
                target.id(),
                "SNAPSHOT AUTHOR".to_string(),
                "SNAPSHOT PREVIEW".to_string(),
            ),
            20,
        );
        let messages = state(vec![target, reply.clone()]);

        let resolve_with = |secrets: HashMap<u32, [u8; 32]>| {
            resolve_reply_strip(
                &reply.message.content,
                &messages,
                &member_info,
                &secrets,
                &HashMap::new(),
            )
        };

        assert_eq!(
            expect_quote(resolve_with(HashMap::from([(3u32, secret)]))).1,
            "sealed target"
        );
        assert_eq!(
            resolve_with(HashMap::from([(4u32, [1u8; 32])])),
            ReplyStrip::Unavailable,
            "rotated past the target's version is genuinely unavailable"
        );
        assert_eq!(
            resolve_with(HashMap::new()),
            ReplyStrip::NotAReply,
            "holding no secrets at all is the cold-start window, not a failure"
        );
    }

    /// A reply body carrying undecodable payload bytes. Any member can post one
    /// — the contract treats message content as opaque — and it IS a reply, so
    /// it must be reported unavailable rather than silently dropped. This also
    /// pins that `pending_decryption`'s `Private` guard is load-bearing:
    /// simplifying it to a bare `secrets.is_empty()` would swallow this case.
    #[test]
    fn undecodable_public_reply_reports_unavailable() {
        use river_core::room_state::content::{CONTENT_TYPE_REPLY, REPLY_CONTENT_VERSION};

        let owner_sk = signing_key(1);
        let bob_sk = signing_key(3);
        let owner = member_id_of(&owner_sk);

        let reply = authored(
            owner,
            &bob_sk,
            RoomMessageBody::public_raw(
                CONTENT_TYPE_REPLY,
                REPLY_CONTENT_VERSION,
                vec![0xff, 0x00, 0xff],
            ),
            20,
        );

        assert_eq!(
            resolve(&reply, &state(vec![reply.clone()]), &info(vec![])),
            ReplyStrip::Unavailable
        );
    }

    /// A plain message is not a reply, so it gets no strip of either kind.
    #[test]
    fn plain_message_has_no_reply_strip() {
        let owner_sk = signing_key(1);
        let alice_sk = signing_key(2);
        let owner = member_id_of(&owner_sk);

        let plain = authored(
            owner,
            &alice_sk,
            RoomMessageBody::public("just a message".to_string()),
            10,
        );

        let ctx = resolve(&plain, &state(vec![plain.clone()]), &info(vec![]));
        assert_eq!(ctx, ReplyStrip::NotAReply);
    }
}

/// Tests for [`group_messages`]' clock handling.
///
/// These are possible at all because the receive-time snapshot is now a
/// parameter: the function used to read the `RECEIVE_TIMES` `GlobalSignal`
/// per message, which panics outside a Dioxus runtime.
#[cfg(test)]
mod group_messages_clock_tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
    use std::time::{Duration as StdDuration, UNIX_EPOCH};

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn member_id_of(sk: &SigningKey) -> MemberId {
        MemberId::from(&sk.verifying_key())
    }

    /// A public text message from `sk`, stamped with the sender's clock.
    fn message_at(owner: MemberId, sk: &SigningKey, sent: DateTime<Utc>) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner,
                author: member_id_of(sk),
                time: UNIX_EPOCH + StdDuration::from_millis(sent.timestamp_millis().max(0) as u64),
                content: RoomMessageBody::public("hello".to_string()),
            },
            sk,
        )
    }

    fn state(messages: Vec<AuthorizedMessageV1>) -> MessagesV1 {
        MessagesV1 {
            messages,
            actions_state: Default::default(),
        }
    }

    fn info(sk: &SigningKey, nickname: &str) -> MemberInfoV1 {
        MemberInfoV1 {
            member_info: vec![AuthorizedMemberInfo::new(
                MemberInfo::new_public(member_id_of(sk), 1, nickname.to_string()),
                sk,
            )],
        }
    }

    fn group(
        messages: &MessagesV1,
        member_info: &MemberInfoV1,
        me: MemberId,
        receive_times: &ReceiveTimes,
        fallback_now: DateTime<Utc>,
    ) -> Vec<DisplayItem> {
        group_messages(
            messages,
            member_info,
            Some(me),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            // No protected names, so no ⚠ warning can fire — these tests are
            // about the clock, and an empty checker keeps them about only that.
            &ImpersonationChecker::default(),
            me,
            MessageClock {
                receive_times,
                fallback_now,
            },
        )
    }

    /// A room whose local identity is unknown must still render its history.
    ///
    /// The regression this pins is user-visible and severe: `message_groups`
    /// used to bail to `None` here, and the `None` arm renders
    /// "No messages yet. Start the conversation!" — so a room full of history
    /// would look empty. `signing_key()`'s own contract is to leave the rest of
    /// the room readable (freenet/river#555 is the precedent for this exact
    /// wrong render).
    ///
    /// The identity is needed here only for cosmetics, so it degrades to
    /// "not me" rather than to nothing.
    #[test]
    fn messages_still_render_when_the_local_identity_is_unknown() {
        let owner = signing_key(60);
        let owner_id = member_id_of(&owner);
        let a = signing_key(61);
        let b = signing_key(62);
        let now = Utc::now();
        let messages = state(vec![
            message_at(owner_id, &a, now),
            message_at(owner_id, &b, now + chrono::Duration::minutes(1)),
        ]);
        let mut member_info = info(&a, "Alice");
        member_info.member_info.extend(info(&b, "Bob").member_info);
        let receive_times = ReceiveTimes::default();

        let known = group_messages(
            &messages,
            &member_info,
            Some(member_id_of(&a)),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &ImpersonationChecker::default(),
            owner_id,
            MessageClock {
                receive_times: &receive_times,
                fallback_now: now,
            },
        );
        let unknown = group_messages(
            &messages,
            &member_info,
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &ImpersonationChecker::default(),
            owner_id,
            MessageClock {
                receive_times: &receive_times,
                fallback_now: now,
            },
        );

        // Precondition, so this cannot pass on an empty fixture.
        assert!(
            !known.is_empty(),
            "fixture must produce groups with a known identity"
        );
        assert_eq!(
            unknown.len(),
            known.len(),
            "every message must still render with no local identity — an empty \
             result is what makes the UI claim the room has no messages"
        );

        // The only difference is the cosmetic: nothing is attributed to "me".
        let self_flags: Vec<bool> = unknown
            .iter()
            .filter_map(|i| match i {
                DisplayItem::Messages(g) => Some(g.is_self),
                _ => None,
            })
            .collect();
        assert!(
            !self_flags.is_empty() && self_flags.iter().all(|f| !f),
            "with an unknown identity nothing is 'mine' — but nothing is \
             mis-attributed either"
        );
    }

    fn only_group(items: &[DisplayItem]) -> &MessageGroup {
        match items {
            [DisplayItem::Messages(g)] => g,
            other => panic!(
                "expected exactly one message group, got {} items",
                other.len()
            ),
        }
    }

    fn at(ms: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp_millis(ms).expect("valid timestamp")
    }

    /// A message from a clock that runs an hour fast is pinned to when THIS
    /// client first saw it — not to "now", which moves on every render.
    ///
    /// This is the assertion that fails if the clamp target goes back to
    /// `Utc::now()`: `now` and `first_seen` are deliberately far apart.
    #[test]
    fn future_timestamp_is_clamped_to_first_seen_not_to_now() {
        let owner = signing_key(1);
        let alice = signing_key(2);
        let owner_id = member_id_of(&owner);

        let now = at(1_700_000_000_000);
        let first_seen = now - chrono::Duration::minutes(10);
        let sent = now + chrono::Duration::hours(1);

        let msg = message_at(owner_id, &alice, sent);
        let mut receive_times = ReceiveTimes::new();
        receive_times.insert(msg.id().0 .0, first_seen.timestamp_millis() as f64);

        let items = group(
            &state(vec![msg]),
            &info(&alice, "Alice"),
            owner_id,
            &receive_times,
            now,
        );
        let g = only_group(&items);

        assert!(g.time_clamped, "a future timestamp must be flagged clamped");
        assert_eq!(
            g.messages[0].time, first_seen,
            "the clamp target must be when we first saw the message, not the \
             render's wall clock"
        );
    }

    /// Same input, two renders a minute apart: identical grouping, and both
    /// pinned to the arrival time. The old code read `Utc::now()` inside the
    /// loop, so a clock-skewed message re-timed itself on every render.
    ///
    /// The send time is deliberately far in the future in ABSOLUTE terms, not
    /// merely relative to the fixture's `now`: a fixture-relative future is
    /// already in the past by the time the suite runs, so the clamp branch
    /// would never be taken and the test could not fail. The explicit
    /// `first_seen` assertions are what make it fail rather than a
    /// `first == later` comparison, which would otherwise pass on two
    /// `Utc::now()` reads that happen to land in the same instant.
    #[test]
    fn grouping_is_idempotent_across_renders() {
        let owner = signing_key(1);
        let alice = signing_key(2);
        let owner_id = member_id_of(&owner);

        let now = at(1_700_000_000_000);
        let first_seen = now - chrono::Duration::minutes(10);
        // Year 4000-ish: later than any clock this test could run against.
        let msg = message_at(owner_id, &alice, at(64_060_588_800_000));
        let mut receive_times = ReceiveTimes::new();
        receive_times.insert(msg.id().0 .0, first_seen.timestamp_millis() as f64);

        let messages = state(vec![msg]);
        let member_info = info(&alice, "Alice");

        let first = group(&messages, &member_info, owner_id, &receive_times, now);
        let later = group(
            &messages,
            &member_info,
            owner_id,
            &receive_times,
            now + chrono::Duration::minutes(1),
        );

        assert_eq!(only_group(&first).messages[0].time, first_seen);
        assert_eq!(only_group(&later).messages[0].time, first_seen);
        assert!(
            first == later,
            "a clock-skewed message must group identically on a later render"
        );
    }

    /// No recorded arrival (an older message restored from contract state, or
    /// one whose entry aged out of the 24h window): fall back to the pass-wide
    /// "now", which is the pre-existing behaviour.
    #[test]
    fn unknown_first_seen_falls_back_to_the_pass_wide_now() {
        let owner = signing_key(1);
        let alice = signing_key(2);
        let owner_id = member_id_of(&owner);

        let now = at(1_700_000_000_000);
        let msg = message_at(owner_id, &alice, now + chrono::Duration::hours(1));

        let items = group(
            &state(vec![msg]),
            &info(&alice, "Alice"),
            owner_id,
            &ReceiveTimes::new(),
            now,
        );
        let g = only_group(&items);

        assert!(g.time_clamped);
        assert_eq!(g.messages[0].time, now);
    }

    /// The case the clamp-target change actually creates: a timestamp in the
    /// PAST relative to the render, but later than when we received the
    /// message. The old code compared against `Utc::now()`, so it left this
    /// alone; the new code clamps it, because a send time after the arrival
    /// time is not something an accurate clock produces.
    ///
    /// This is the assertion that would have caught the change slipping in
    /// unnoticed — `past_timestamps_are_never_rewritten` below passes either
    /// way, so on its own it pins nothing about this fix.
    #[test]
    fn a_timestamp_later_than_arrival_is_clamped_even_though_it_is_in_the_past() {
        let owner = signing_key(1);
        let alice = signing_key(2);
        let owner_id = member_id_of(&owner);

        let now = at(1_700_000_000_000);
        let first_seen = now - chrono::Duration::hours(2);
        let sent = now - chrono::Duration::hours(1);

        let msg = message_at(owner_id, &alice, sent);
        let mut receive_times = ReceiveTimes::new();
        receive_times.insert(msg.id().0 .0, first_seen.timestamp_millis() as f64);

        let items = group(
            &state(vec![msg]),
            &info(&alice, "Alice"),
            owner_id,
            &receive_times,
            now,
        );
        let g = only_group(&items);

        assert!(
            g.time_clamped,
            "a send time an hour after arrival is skew, even though it is in \
             the past relative to this render"
        );
        assert_eq!(g.messages[0].time, first_seen);
    }

    /// Skew inside the tolerance is left alone and NOT flagged.
    ///
    /// Without the tolerance the clamp target's move from "now at render" to
    /// "arrival" would silently widen the flag: the old comparison carried the
    /// whole propagation delay as slack, the new one carries none, so every
    /// message from any peer whose clock is a few seconds fast would render
    /// with the "clock may be wrong" marker.
    #[test]
    fn skew_within_the_tolerance_is_left_alone() {
        let owner = signing_key(1);
        let alice = signing_key(2);
        let owner_id = member_id_of(&owner);

        let now = at(1_700_000_000_000);
        let first_seen = now - chrono::Duration::minutes(10);
        // Ahead of arrival, but by less than CLOCK_SKEW_TOLERANCE_SECS.
        let sent = first_seen + chrono::Duration::seconds(CLOCK_SKEW_TOLERANCE_SECS - 1);

        let msg = message_at(owner_id, &alice, sent);
        let mut receive_times = ReceiveTimes::new();
        receive_times.insert(msg.id().0 .0, first_seen.timestamp_millis() as f64);

        let items = group(
            &state(vec![msg]),
            &info(&alice, "Alice"),
            owner_id,
            &receive_times,
            now,
        );
        let g = only_group(&items);

        assert!(
            !g.time_clamped,
            "a few seconds of clock skew is normal and must not put a \
             \"sender's clock may be wrong\" marker on the message"
        );
        assert_eq!(
            g.messages[0].time, sent,
            "and the time must be left as sent"
        );
    }

    /// A timestamp that is merely in the past is left exactly as the sender
    /// wrote it, whether or not we have an arrival time for it.
    ///
    /// Passes against the pre-fix code too — it is here to pin the unchanged
    /// half, not the change. See
    /// `a_timestamp_later_than_arrival_is_clamped_even_though_it_is_in_the_past`
    /// for the assertion that depends on the fix.
    #[test]
    fn past_timestamps_are_never_rewritten() {
        let owner = signing_key(1);
        let alice = signing_key(2);
        let owner_id = member_id_of(&owner);

        let now = at(1_700_000_000_000);
        let sent = now - chrono::Duration::hours(2);
        let msg = message_at(owner_id, &alice, sent);
        let mut receive_times = ReceiveTimes::new();
        receive_times.insert(
            msg.id().0 .0,
            (now - chrono::Duration::hours(1)).timestamp_millis() as f64,
        );

        let items = group(
            &state(vec![msg]),
            &info(&alice, "Alice"),
            owner_id,
            &receive_times,
            now,
        );
        let g = only_group(&items);

        assert!(!g.time_clamped);
        assert_eq!(g.messages[0].time, sent);
    }
}

/// Source-grep pins for the scroll wiring in [`Conversation`].
///
/// The behaviour these guard is only observable in a browser, and some of it
/// in no browser CI runs (a `scrollend`-less engine) or only on a race the
/// suite cannot force, so these make a silent revert in a refactor fail at
/// `cargo test` rather than in the field. Everything else is measured by
/// `ui/tests/conversation-autoscroll.spec.ts` and
/// `conversation-history-position.spec.ts`.
#[cfg(test)]
mod autoscroll_wiring_pins {
    /// The production half of this file, cut at the first test module.
    ///
    /// Cut by a needle that cannot match itself — it contains an escaped
    /// newline in the source, not a literal one. `rfind` would land on
    /// whichever test module was appended most recently and quietly put these
    /// needles inside the scanned text, making every assertion below
    /// self-satisfying (freenet/river#471).
    fn production_source() -> &'static str {
        let source = include_str!("conversation.rs");
        &source[..source
            .find("#[cfg(test)]\nmod tests {")
            .expect("conversation.rs should have a `#[cfg(test)] mod tests` block")]
    }

    fn dense_production_source() -> String {
        production_source()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    /// The settle listener remembers the reading position and trims at the
    /// bottom. Safari before 17.4 has no `scrollend`, so the settle is spotted
    /// by a trailing debounce on `scroll` instead. Every engine Playwright
    /// drives exposes `scrollend`, so that branch NEVER executes in CI and
    /// could be deleted or broken without a single test going red.
    #[test]
    fn the_settle_listener_has_a_debounced_fallback() {
        let prod = production_source();
        assert!(
            prod.contains("if install_scroll_settle_listener("),
            "`install_scroll_settle_listener` must actually be called from `Conversation`"
        );
        assert!(
            prod.contains("\"scrollend\""),
            "the settle is spotted once per scroll, via `scrollend`"
        );
        assert!(
            prod.contains("SCROLL_SETTLE_DEBOUNCE_MS") && prod.contains("\"scroll\""),
            "browsers without `scrollend` need the debounced `scroll` fallback; \
             nothing in the browser suite covers it, so it is pinned here"
        );
    }

    /// The scroll container must opt out of browser scroll anchoring. Safari
    /// before 27 has none, so the app's own corrections (backfill restore, head
    /// reposition, reveal restore) are the single owner of the reader's
    /// position; with anchoring on as well, every other engine would apply both
    /// and over-correct, and CI (whose WebKit anchors) could not see what iOS 26
    /// and older do.
    #[test]
    fn the_scroll_container_disables_scroll_anchoring() {
        assert!(
            dense_production_source().contains(concat!("style:\"overflow-", "anchor:none;\"")),
            "#chat-scroll-container must set `overflow-anchor: none` — Safari \
             before 27 has no scroll anchoring, so the app's own corrections \
             must be the only ones"
        );
    }

    /// The #501/#505 windowing contract: trims happen at a settle landing AT
    /// the bottom — ours included, so the scroll-to-latest button trims too —
    /// gated at the slack, so a reader even slightly above the end is never
    /// clamped to it; the deferred trim re-checks the room; the backfill
    /// restore stands down while an explicit request is pending (H3); the
    /// backfill sentinel waits for the opening (H2) and for the room-switch
    /// reset to catch up. H2 and H3 are races the browser suite catches only
    /// when they fire, so these pins are their deterministic guard.
    #[test]
    fn the_window_trims_at_the_bottom_and_backfill_waits_for_the_opening() {
        let dense = dense_production_source();
        assert!(
            dense.contains("iftrim_is_due(&container,reader.window_overgrown.get(),&reader)"),
            "the settle handler must trim the grown window when a settle — \
             the reader's or ours — lands at the exact bottom"
        );
        assert!(
            dense.contains("ifdistance>SCROLL_TOP_SLACK_PX{returnfalse;}"),
            "a trim is due only at the exact bottom (the slack), where the \
             browser's clamp keeps the newest rows in place"
        );
        assert!(
            dense.contains("ifreader.room_epoch.get()!=epoch{return;}")
                && dense.contains("trim_is_due(&c,true,&reader)"),
            "the deferred trim must re-check the room and re-measure \
             eligibility: the settle that scheduled it is a task old, and the \
             reader may have moved, hidden the panel or switched rooms since"
        );
        assert!(
            dense.contains("!trim_would_rearm_backfill(tail,container.client_height())"),
            "the trim must be geometry-gated: on a viewport tall enough that \
             the trimmed tail leaves the sentinel strip in range, trim and \
             backfill oscillate at render speed (#505 re-review)"
        );
        assert_eq!(
            dense
                .matches("ifreader_position.request.get().is_some(){return;}")
                .count(),
            2,
            "the backfill restore (#501 H3) and the head reposition must each \
             stand down while an explicit request is pending"
        );
        assert!(
            dense.contains(
                "ifhistory_window.has_older&&opening_snap_done()&&!room_changed_this_render"
            ),
            "the backfill sentinel must not mount until the room-open snap has \
             landed (#501 H2), including the one render where the previous \
             room's `opening_snap_done` is still true"
        );
    }

    /// The #505 blocker-1 compensation wiring: rows carry their item identity,
    /// the render captures the new head's pre-patch offset when rendered
    /// content above it is removed, and the reposition effect shifts the
    /// reader's offset by the measured difference. Removing any leg silently
    /// reverts to "readers crawl upward one row per arrival in every at-cap
    /// room".
    #[test]
    fn head_swaps_reposition_the_reader() {
        let dense = dense_production_source();
        assert!(
            dense.contains("\"data-item-key\":\"{key}\""),
            "history rows must carry `data-item-key` — the reposition \
             machinery locates rows by item identity"
        );
        assert!(
            dense.contains("fnhistory_row_offset_top"),
            "the reposition machinery needs the row-offset lookup"
        );
        assert!(
            dense.contains("reposition_pending.borrow_mut().take()"),
            "the head-reposition effect must consume the render's pre-patch \
             capture and shift the reader's offset"
        );
        assert!(
            dense.contains("letshift=post_top-pre_top;"),
            "the reposition must be BY MEASUREMENT (post-patch minus pre-patch \
             offset of a surviving row), not a guess"
        );
        assert!(
            dense.contains("lettarget=(pre_scroll_top+shift).max(0);"),
            "the reposition target must be computed from the PRE-patch scroll \
             offset — the browser clamps `scrollTop` down before the effect \
             runs when a patch shortens the content, and shifting from the \
             clamped value applies the clamp twice (#505 delta review)"
        );
        assert!(
            dense.contains("select_reposition_probe("),
            "the capture must pick its probe through the forward walk — a \
             head-only probe dead-fires when an at-cap drain re-keys a \
             multi-message head group, and the reader crawls one intra-group \
             line per arrival (#505 re-review blocker)"
        );
        assert!(
            dense.contains("matchhistory_row_offset_top(&anchor.probe_key)"),
            "the backfill restore must reposition by the measured probe row, \
             not the raw scrollHeight delta — the delta counts arrivals \
             appended BELOW the viewport in the same patch and over-shifts \
             the reader (#505 re-review)"
        );
    }
}

#[cfg(test)]
mod reader_state_tests {
    use super::*;

    fn id(seed: &[u8]) -> MessageId {
        MessageId(freenet_scaffold::util::fast_hash(seed))
    }

    fn range(first: &str, newest: &[u8], count: usize) -> RenderedMessages {
        RenderedMessages {
            first: first.to_string(),
            newest: Some(id(newest)),
            count,
        }
    }

    /// A contended read stands in with the last good value for the SAME room,
    /// and with nothing for any other: rows from the room the reader just left
    /// must never render in the one they opened.
    #[test]
    fn a_contended_read_reuses_only_the_same_rooms_last_good_value() {
        let cache = Some(("room a", 7));
        assert_eq!(last_good_for_room(&cache, &"room a"), Some(7));
        assert_eq!(
            last_good_for_room(&cache, &"room b"),
            None,
            "another room's cached value must not stand in"
        );
        assert_eq!(last_good_for_room(&None::<(&str, i32)>, &"room a"), None);
    }

    /// 10c decision 13: an arrival ends the hold instead of moving the view.
    /// Appended (new newest), inserted above the newest (higher count), or
    /// inserted while an at-cap drain removed the first row (new first row,
    /// same count).
    #[test]
    fn an_arrival_ends_the_end_hold() {
        let before = range("a", b"z", 10);
        for (now, what) in [
            (range("a", b"new", 11), "an appended message"),
            (
                range("b", b"new", 10),
                "an at-cap arrival that drained the first row",
            ),
            (range("a", b"z", 11), "a message inserted above the newest"),
            (
                range("b", b"z", 10),
                "an insert in the same patch as a drain",
            ),
        ] {
            assert!(
                !end_hold_survives(Some(&before), Some(&now)),
                "{what} kept the hold"
            );
        }
    }

    /// A trim or a deletion above only shrinks the range: nothing arrived, so
    /// the hold stays. Rows that only change height (decryption, an image, an
    /// edit) leave the range as it was, and the hold survives them too.
    /// Deleting the newest message changes the newest, and ends it (deleting
    /// never moves the view, decision 2).
    #[test]
    fn a_shrinking_range_keeps_the_hold_unless_the_newest_changed() {
        let before = range("a", b"z", 10);
        assert!(
            end_hold_survives(Some(&before), Some(&before.clone())),
            "a render with the same messages ended the hold"
        );
        assert!(end_hold_survives(Some(&before), Some(&range("c", b"z", 8))));
        assert!(end_hold_survives(Some(&before), Some(&range("a", b"z", 9))));
        assert!(!end_hold_survives(
            Some(&before),
            Some(&range("a", b"y", 9))
        ));
    }

    /// A backfill restore or head reposition measured before a request must
    /// not run after it: it would put the reader back where the request took
    /// them from.
    #[test]
    fn requesting_the_end_drops_pending_corrections() {
        let reader = ReaderPosition::default();
        *reader.backfill_anchor.borrow_mut() = Some(BackfillAnchor {
            probe_key: "a".to_string(),
            probe_top: 10,
            scroll_top: 20,
            scroll_height: 30,
        });
        *reader.reposition_pending.borrow_mut() = Some(RepositionAnchor {
            probe_key: "a".to_string(),
            probe_top: 10,
            scroll_top: 20,
        });

        reader.request_end(None);

        assert_eq!(reader.request.get(), Some(ScrollRequest { room: None }));
        assert!(
            reader.backfill_anchor.borrow().is_none(),
            "a backfill restore survived the request"
        );
        assert!(
            reader.reposition_pending.borrow().is_none(),
            "a head reposition survived the request"
        );
    }

    /// No rows on either side: nothing to hold.
    #[test]
    fn an_empty_range_ends_the_end_hold() {
        let before = range("a", b"z", 10);
        assert!(!end_hold_survives(None, Some(&before)));
        assert!(!end_hold_survives(Some(&before), None));
        assert!(rendered_messages(&[]).is_none());
    }
}
