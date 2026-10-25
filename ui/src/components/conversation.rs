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
use crate::components::back_to_chat::CountBadge;
use crate::components::members::{
    deputy_badges_for_viewer, impersonation_checker_for_viewer, impersonation_warning_for_display,
    privilege_in_view, DeputyBadge,
};
#[cfg(test)]
use crate::components::scroll_to_latest::is_arrival;
#[cfg(target_arch = "wasm32")]
use crate::components::scroll_to_latest::observe_newest;
use crate::components::scroll_to_latest::LatestButton;
use crate::room_data::{NotificationMode, SendMessageError};
use crate::util::confusable::{ImpersonationChecker, ImpersonationWarning};
use crate::util::display_name::display_nickname;
use crate::util::ecies::{encrypt_with_symmetric_key, unseal_text_or_placeholder};
use crate::util::{
    date_separator_labels, format_utc_as_full_datetime, format_utc_as_local_time,
    get_current_system_time, local_message_date, local_today,
};
mod avatar;
mod emoji_picker;
mod history_scroll;
mod history_window;
mod markdown;
mod mention;
mod message_actions;
mod message_input;
mod not_member_notification;
use self::emoji_picker::FREQUENT_EMOJIS;
#[cfg(target_arch = "wasm32")]
use self::history_scroll::{
    complete_scroll_request, correct_view_after_patch, follow_arrival_at_end,
    install_scroll_settle_listener, land_at_end, note_newest_seen, note_patch_while_hidden,
    observe_history_resize, ROOM_HISTORY,
};
use self::history_scroll::{remember_reading_position, resolve_rendered_range, ReaderPosition};
use self::history_window::{grown_window, BACKFILL_LEAD_PX, INITIAL_WINDOW_ITEMS};
pub(crate) use self::markdown::message_to_html;
use self::markdown::{
    clean_reply_preview, description_to_html, member_names_fingerprint, prune_message_html_cache,
    render_message_html_cached, running_behind_freenet_gateway,
};
use self::not_member_notification::NotMemberNotification;
use crate::components::conversation::message_input::MessageInput;
use chrono::{DateTime, Utc};
use dioxus::logger::tracing::*;
use dioxus::prelude::*;
use dioxus_free_icons::icons::fa_solid_icons::{
    FaBars, FaBell, FaBellSlash, FaChevronDown, FaChevronUp, FaCircleInfo, FaEllipsisVertical,
    FaFaceSmile, FaPenToSquare, FaReply, FaTrashCan, FaTriangleExclamation, FaUsers,
};
use dioxus_free_icons::Icon;
use freenet_scaffold::ComposableState;
use river_core::room_state::member::{MemberId, MembersDelta};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfoV1};
use river_core::room_state::message::{
    AuthorizedMessageV1, MessageId, MessageV1, MessagesV1, RoomMessageBody,
};
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys;

/// The `member_info` records of the owner and present members. Since
/// freenet/river#702 `member_info` can also hold records of members kept only
/// as ban evidence, who are not in the room: the @mention pickers and the name
/// lookup must not offer them.
fn present_member_infos(
    state: &river_core::room_state::ChatRoomStateV1,
) -> Vec<&AuthorizedMemberInfo> {
    let present = state.members.members_by_member_id();
    let owner_id = state.configuration.configuration.owner_member_id;
    state
        .member_info
        .member_info
        .iter()
        .filter(|info| {
            let id = info.member_info.member_id;
            id == owner_id || present.contains_key(&id)
        })
        .collect()
}

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
    time: DateTime<Utc>,
    /// The sender's unclamped time, used for display ordering (see
    /// [`NewestKey`]). The clamped `time` still controls presentation and
    /// grouping.
    sent_ms: i64,
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
    /// The sender's unclamped time, used for display ordering (see
    /// [`NewestKey`]). The clamped `last_time` still controls presentation and
    /// grouping.
    last_sent_ms: i64,
    /// The newest event folded in, for the follow and read rules (see
    /// [`display_item_newest`]).
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

/// The newest display message `item` renders: what the follow rule compares,
/// and what the read rule marks when the item is the newest one on screen.
fn display_item_newest(item: &DisplayItem) -> Option<NewestKey> {
    let (time_ms, id) = match item {
        DisplayItem::Messages(group) => {
            let message = group.messages.last()?;
            (message.sent_ms, message.message_id.clone())
        }
        DisplayItem::Event(summary) => (summary.last_sent_ms, summary.last_message_id.clone()),
    };
    Some(NewestKey { time_ms, id })
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
/// per-group component state on mid-list edits (freenet/river#326).
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
///     which makes `post_apply_cleanup` purge their messages (step 5b) AND
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
        // events render through `event_summary_parts`. Do NOT drop this filter.
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
    // own name colour, the "mention of you" chip highlight)
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
        let sent_ms = raw_time.timestamp_millis();
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
                    summary.last_sent_ms = sent_ms;
                    summary.last_message_id = message_id;
                }
            } else {
                items.push(DisplayItem::Event(EventSummary {
                    names: vec![author_name],
                    id: msg_id_str,
                    last_time: message_time,
                    last_sent_ms: sent_ms,
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
        // renders as someone else's rather than claiming an author.
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
        let receive_delay_secs = get_delay_secs_from(clock.receive_times, &message_id, sent_ms);

        let grouped_message = GroupedMessage {
            content_text: content_text.clone(),
            content_html,
            time: message_time,
            sent_ms,
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

/// A join notice's names (shown in the secondary colour) and the text after
/// them: "Alice joined", "Alice and Bob joined", "3 people joined".
fn event_summary_parts(names: &[String]) -> (Vec<String>, String) {
    match names.len() {
        1 | 2 => (names.to_vec(), " joined".to_string()),
        n => (Vec::new(), format!("{n} people joined")),
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
                // Wording note (PR #286): the
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

/// The room's newest display message, as the follow rule compares it. Ordered
/// by the sender's unclamped time, then id: display order (the clamped time is
/// for presentation and grouping only), so a later message, a message folding
/// into the last author's group and a join folding into the trailing event
/// summary all compare greater; a deletion of the newest compares less, and an edit equal.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct NewestKey {
    time_ms: i64,
    id: MessageId,
}

/// The [`NewestKey`] of the last display item, if any.
#[cfg(test)]
fn newest_key(items: &[DisplayItem]) -> Option<NewestKey> {
    items.last().and_then(display_item_newest)
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
/// id, the member names, the impersonation checker, the deputy badges and
/// the owner's member id. Shared, so the contended-read
/// cache costs a pointer, not a copy of every rendered message.
type MessageGroupsValue = Rc<(
    Vec<DisplayItem>,
    Option<MemberId>,
    HashMap<MemberId, String>,
    ImpersonationChecker,
    HashMap<MemberId, DeputyBadge>,
    MemberId,
)>;

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

/// Classes for the header description. Collapsed is one line: `truncate`
/// clips it horizontally and `max-h-[calc(1lh+4px)]` vertically, so a list,
/// a heading or a hard break cannot grow the header either. The 4px lets a
/// line made taller by inline code or an emoji (17-19px against a 16px line,
/// measured) without reaching the clamp. A list, a list item or a hard break
/// is taller than that, so it reaches the clamp and gets the chevron
/// (`header_description_overflows`). A heading alone renders at the header's
/// font size and fits. Expanded wraps and scrolls
/// so a long description stays in the header instead of pushing the messages
/// off the screen (freenet/river#718).
fn room_description_class(expanded: bool, interactive: bool) -> &'static str {
    match (expanded, interactive) {
        (true, _) => {
            "prose prose-sm dark:prose-invert max-w-none text-xs text-text-muted [&>p]:m-0 break-words cursor-pointer max-h-48 overflow-y-auto"
        }
        (false, true) => {
            "prose prose-sm dark:prose-invert max-w-none text-xs text-text-muted truncate max-h-[calc(1lh+4px)] [&>*]:m-0 [&>p]:inline cursor-pointer"
        }
        (false, false) => {
            "prose prose-sm dark:prose-invert max-w-none text-xs text-text-muted truncate max-h-[calc(1lh+4px)] [&>*]:m-0 [&>p]:inline"
        }
    }
}

/// The collapsed header line does not fit its box. Measured on the element
/// itself: a chevron on a line that already fits only makes the header taller
/// (freenet/river#718 review).
fn header_description_element() -> Option<web_sys::Element> {
    web_sys::window()?
        .document()?
        .get_element_by_id("room-header-description")
}

fn header_column_element() -> Option<web_sys::Element> {
    web_sys::window()?
        .document()?
        .get_element_by_id("room-header-column")
}

/// Skip while the description is expanded: wrapping makes it fit, and storing
/// that would hide the chevron on the way back to one line.
fn header_description_is_expanded() -> bool {
    header_description_element()
        .and_then(|element| element.get_attribute("data-expanded"))
        .is_some_and(|value| value == "true")
}

/// The description's computed `max-height` in px, `None` for `none`.
#[cfg(target_arch = "wasm32")]
fn description_max_height_px(element: &web_sys::Element) -> Option<f64> {
    web_sys::window()?
        .get_computed_style(element)
        .ok()??
        .get_property_value("max-height")
        .ok()?
        .strip_suffix("px")?
        .parse()
        .ok()
}

fn header_description_overflows() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        // Both axes, against the box the collapsed class clamps to (see
        // `room_description_class`): a long line overflows sideways, and a
        // list, a list item or a hard break is taller than the clamp. Downwards
        // it counts only once the box has reached the clamp: below it, the
        // box already holds the whole line, and the 1-3px `scrollHeight`
        // reports past it is an inline box (inline code) hanging below the
        // line box, not text the clamp hides. The 1px slack: a flex item at a
        // fractional size can round a pixel apart and look truncated when it
        // fits.
        return header_description_element().is_some_and(|element| {
            let sideways = element.scroll_width() > element.client_width() + 1;
            let clamped = description_max_height_px(&element)
                .is_some_and(|max| f64::from(element.client_height()) + 1.0 >= max);
            sideways || clamped && element.scroll_height() > element.client_height() + 1
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

fn description_click_is_link(evt: &dioxus_core::Event<MouseData>) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        return evt
            .data()
            .downcast::<web_sys::MouseEvent>()
            .and_then(|mouse| mouse.target())
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|element| element.closest("a").ok().flatten())
            .is_some();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = evt;
        false
    }
}

fn description_click_is_multi(evt: &dioxus_core::Event<MouseData>) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        return evt
            .data()
            .downcast::<web_sys::MouseEvent>()
            .is_some_and(|mouse| mouse.detail() >= 2);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = evt;
        false
    }
}

fn description_selection_is_collapsed() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|window| window.get_selection().ok().flatten())
            .is_none_or(|selection| selection.is_collapsed())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        true
    }
}

/// How long a mouse or pen click on the description waits before toggling,
/// so that the first click of a double-click (which selects a word) can be
/// cancelled by the second. 500ms is the Windows default double-click time,
/// the longest common default. A finger toggles at once (a double tap does
/// not select a word), and so does the chevron.
const DESCRIPTION_DOUBLE_CLICK_WAIT_MS: u64 = 500;
// The specs override the wait, so pin the shipped value here: anything shorter
// than the Windows default lets a slower double-click toggle under the pointer.
const _: () = assert!(DESCRIPTION_DOUBLE_CLICK_WAIT_MS >= 500);

thread_local! {
    /// Test-only override of [`DESCRIPTION_DOUBLE_CLICK_WAIT_MS`], set by the
    /// `setDescriptionClickWaitMs` hook in `test_hooks.rs`, so a spec can make
    /// the wait long enough that it cannot expire during the steps it checks.
    static DESCRIPTION_CLICK_WAIT_OVERRIDE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

#[allow(dead_code)] // Only the example-data test hooks call it.
pub(crate) fn set_description_click_wait_ms(ms: u64) {
    DESCRIPTION_CLICK_WAIT_OVERRIDE.with(|wait| wait.set(Some(ms)));
}

fn description_click_wait_ms() -> u64 {
    DESCRIPTION_CLICK_WAIT_OVERRIDE
        .with(|wait| wait.get())
        .unwrap_or(DESCRIPTION_DOUBLE_CLICK_WAIT_MS)
}

/// Mark the description with a waiting click's state, so a test can tell
/// "the wait has not ended" from "it ended without toggling": while a click
/// waits, `data-click-pending` holds its number; when that click's wait ends,
/// `data-click-outcome` is `toggled` or `cancelled`. Plain DOM attributes,
/// not rendered state, so they cost no render. Only the example-data build the
/// specs run sets them.
fn mark_description_click(armed: u64, outcome: Option<&str>) {
    if !cfg!(feature = "example-data") {
        return;
    }
    let Some(element) = header_description_element() else {
        return;
    };
    match outcome {
        None => {
            let _ = element.remove_attribute("data-click-outcome");
            let _ = element.set_attribute("data-click-pending", &armed.to_string());
        }
        Some(outcome) => {
            if element.get_attribute("data-click-pending") == Some(armed.to_string()) {
                let _ = element.remove_attribute("data-click-pending");
            }
            let _ = element.set_attribute("data-click-outcome", outcome);
        }
    }
}

fn note_description_overflow(description_overflows: &mut Signal<bool>) {
    if header_description_is_expanded() {
        return;
    }
    let next = header_description_overflows();
    let changed = description_overflows
        .try_read()
        .is_ok_and(|current| *current != next);
    if changed {
        description_overflows.set(next);
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
    let mut newest_in_view = use_signal(|| true);
    // How many trailing display items the history renders. Grows only when the
    // reader reaches the top of what is rendered — see `INITIAL_WINDOW_ITEMS`.
    let mut window_items = use_signal(|| INITIAL_WINDOW_ITEMS);
    // See `ReaderPosition`.
    let reader_position = use_hook(|| Rc::new(ReaderPosition::default()));
    // Whether the opening snap for the CURRENT room has landed. The backfill
    // sentinel only mounts once this is true: a freshly-opened >window room
    // renders at `scrollTop = 0` for a beat before the snap runs, and a
    // sentinel mounted during that beat is intersecting, fires, and cascades
    // the backfill until the whole room is rendered (#501). A signal, not a
    // `Cell`, because the sentinel's `if` in rsx renders from it. Set by
    // `complete_scroll_request`.
    let mut opening_snap_done = use_signal(|| false);

    // Reset the windowing Cells the moment THIS render is for a different
    // room — not only in the effect below, which runs AFTER the first render
    // of the new room has already resolved against the OLD room's anchor and
    // requested size (cloning and patching up to a fully-backfilled room's
    // depth for one wasted frame; #505). Cells are safe to write
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
            // room (#505).
            reader_position.request_end(room);
            reader_position.extend_newer.set(false);
            reader_position
                .room_epoch
                .set(reader_position.room_epoch.get().wrapping_add(1));
            // The old room's hold and range end with it.
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
                newest_in_view.set(true);
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

    // Which room's header description is expanded. Compared to the open room
    // in render, and cleared there when the open room changes, so coming back
    // to a room shows its description collapsed again (freenet/river#718).
    let mut expanded_description_room = use_signal(|| None::<ed25519_dalek::VerifyingKey>);
    // True when the collapsed line does not fit. Stays true while expanded so
    // the chevron does not vanish on the way back to one line.
    let description_overflows = use_signal(|| false);
    // Bumped by every click on the description, every toggle (chevron, text,
    // or a waiting click landing) and every room change. A waiting mouse
    // click toggles only if nothing bumped it meanwhile, so neither a
    // double-click, nor a chevron press, nor a room switch (even A to B and
    // back to A) is followed by a stale toggle.
    let description_toggle_gen = use_hook(|| Rc::new(std::cell::Cell::new(0u64)));
    let description_seen_room =
        use_hook(|| Rc::new(std::cell::Cell::new(None::<ed25519_dalek::VerifyingKey>)));
    let description_pressed_with: PressRecord = use_hook(|| Rc::new(std::cell::RefCell::new(None)));
    {
        let mut description_overflows = description_overflows;
        use_effect(move || {
            crate::util::signal_guard::anchor();
            let Ok(_html) = current_room_description_html.try_read() else {
                crate::util::signal_guard::schedule_nudge();
                return;
            };
            // Read so a collapse remeasures. Do not return early when some
            // OTHER room is the expanded one: that left the flag stuck and
            // the next room showed a chevron on a line that already fit.
            let Ok(_expanded_room) = expanded_description_room.try_read() else {
                crate::util::signal_guard::schedule_nudge();
                return;
            };
            crate::util::defer(move || note_description_overflow(&mut description_overflows));
        });
    }
    // Width changes (rotation, a narrow window, the mobile panel coming back)
    // do not change the description text, so nothing above re-runs. Watch the
    // header column, not the description: the description's own width
    // depends on whether the chevron is showing.
    #[cfg(target_arch = "wasm32")]
    {
        type DescriptionWatch = (
            web_sys::Element,
            web_sys::ResizeObserver,
            wasm_bindgen::closure::Closure<dyn FnMut(js_sys::Array)>,
        );
        let description_overflows = description_overflows;
        let watch = use_hook(|| Rc::new(std::cell::RefCell::new(None::<DescriptionWatch>)));
        {
            // The observer and its callback are owned here, not leaked, so a
            // header node change replaces them and an unmount releases them.
            let watch = watch.clone();
            use_drop(move || {
                if let Some((_, observer, _)) = watch.borrow_mut().take() {
                    observer.disconnect();
                }
            });
        }
        use_effect(move || {
            use wasm_bindgen::prelude::*;

            crate::util::signal_guard::anchor();
            let Ok(_html) = current_room_description_html.try_read() else {
                crate::util::signal_guard::schedule_nudge();
                return;
            };
            let Ok(_room) = CURRENT_ROOM.try_read() else {
                crate::util::signal_guard::schedule_nudge();
                return;
            };
            let Some(column) = header_column_element() else {
                return;
            };
            let same = watch
                .borrow()
                .as_ref()
                .is_some_and(|(attached, _, _)| attached == &column);
            if same {
                return;
            }
            if let Some((_, observer, _)) = watch.borrow_mut().take() {
                observer.disconnect();
            }
            let mut description_overflows = description_overflows;
            let cb = Closure::wrap(Box::new(move |_: js_sys::Array| {
                crate::util::defer(move || note_description_overflow(&mut description_overflows));
            }) as Box<dyn FnMut(js_sys::Array)>);
            if let Ok(observer) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
                observer.observe(&column);
                *watch.borrow_mut() = Some((column, observer, cb));
            }
        });
    }

    // Collapse the description when the open room changes. Deferred because
    // this is render; checked again when it runs so a quick switch back does
    // not clear the wrong room.
    if let Ok(open_room) = CURRENT_ROOM.try_read().map(|room| room.owner_key) {
        if description_seen_room.replace(open_room) != open_room {
            description_toggle_gen.set(description_toggle_gen.get().wrapping_add(1));
        }
        let stale = expanded_description_room
            .try_read()
            .is_ok_and(|expanded| expanded.is_some() && *expanded != open_room);
        if stale {
            crate::util::defer(move || {
                let expanded = *expanded_description_room.peek();
                if expanded.is_some() && expanded != CURRENT_ROOM.peek().owner_key {
                    expanded_description_room.set(None);
                }
            });
        }
    }

    // Memoize expensive message grouping (decryption + markdown parsing)
    // This prevents re-computing on every render/keystroke
    // Returns a `MessageGroupsValue` (groups, self_member_id, member_names,
    // impersonation checker, deputy badges, owner id) so we can highlight the
    // user's reactions and show names, warnings and badges by the reactors.
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
                let member_names: HashMap<MemberId, String> = present_member_infos(room_state)
                    .into_iter()
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
                let owner_id = MemberId::from(&key);
                let deputy_badges = match self_member_id {
                    Some(self_member_id) => deputy_badges_for_viewer(
                        &room_state.members,
                        &room_state.member_info,
                        &room_data.secrets,
                        owner_id,
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
                    owner_id,
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
                    owner_id,
                    MessageClock {
                        receive_times: &receive_times,
                        // One "now" for the whole pass. Only reached by
                        // messages with no recorded arrival time.
                        fallback_now: Utc::now(),
                    },
                );
                return Some(Rc::new((
                    groups,
                    self_member_id,
                    member_names,
                    impersonation,
                    deputy_badges,
                    owner_id,
                )));
            }
            None
        });
        *last_good_groups.borrow_mut() = Some((key, groups.clone()));
        groups
    });

    // The Latest observer on `bottom-sentinel` (`observe_newest`).
    #[cfg(target_arch = "wasm32")]
    let observer_reader = reader_position.clone();
    #[cfg(target_arch = "wasm32")]
    use_effect(move || {
        let reader_position = observer_reader.clone();
        let observer = observe_newest(&ROOM_HISTORY, newest_in_view, move |intersecting| {
            // The reader scrolled to the newest message, or it came back
            // into view: the read rule may mark it.
            if intersecting {
                note_newest_seen(&reader_position);
            }
        });
        // Leaked, so it never disconnects: the Conversation component is
        // mounted once and never unmounted (hidden/shown via CSS), so this
        // leak is bounded, and `use_effect` has no cleanup hook.
        std::mem::forget(observer);
    });

    // The settle listener and the resize observer. Installed once each, in
    // one effect, because they must outlive every re-render of the history.
    // Reading `message_groups` only makes the effect re-runnable, so an
    // attempt that found no container yet (nothing rendered) gets another
    // chance; each flag is set only on success.
    #[cfg(target_arch = "wasm32")]
    {
        let reader = reader_position.clone();
        let listening = use_hook(|| Rc::new(std::cell::Cell::new(false)));
        let observing = use_hook(|| Rc::new(std::cell::Cell::new(false)));
        use_effect(move || {
            let _retry_on_content_change = message_groups.read().is_some();
            if !listening.get() {
                listening.set(install_scroll_settle_listener(window_items, reader.clone()));
            }
            if !observing.get() {
                observing.set(observe_history_resize(reader.clone(), opening_snap_done));
            }
        });
    }

    // After each render that can move rows: a patch behind a hidden panel is
    // noted for the reveal, then the patch corrections, which stand down
    // while a request is pending, then the pending request, then the read
    // rule against what the render left on screen.
    #[cfg(target_arch = "wasm32")]
    {
        let reader_position = reader_position.clone();
        use_effect(move || {
            // Subscribe: a backfill and a newer page change `window_items`,
            // and heads swap on content changes too.
            let _ = message_groups.read().is_some();
            let _ = window_items();
            note_patch_while_hidden(&reader_position);
            correct_view_after_patch(&reader_position);
            complete_scroll_request(&reader_position, opening_snap_done);
            note_newest_seen(&reader_position);
        });
    }

    // Back in the foreground (the tab became visible, a DM thread closed):
    // whatever is on screen now counts as seen, which nothing else would
    // notice (no scroll, no resize, no observer change). Never scrolls.
    #[cfg(target_arch = "wasm32")]
    {
        let reader_position = reader_position.clone();
        use_effect(move || {
            // Anchored before the fallible reads, this effect's only
            // subscriptions (freenet/river#555).
            crate::util::signal_guard::anchor();
            if crate::components::app::document_title::DOCUMENT_VISIBLE
                .try_read()
                .is_err()
            {
                crate::util::signal_guard::schedule_nudge();
                return;
            }
            if crate::components::direct_messages::OPEN_DM_THREAD
                .try_read()
                .is_err()
            {
                crate::util::signal_guard::schedule_nudge();
                return;
            }
            note_newest_seen(&reader_position);
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
                // handler's stack (freenet/river#512).
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
                // handler's stack (freenet/river#512).
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
                // handler's stack (freenet/river#512).
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
                // unrelated message to the end (#402).
                let reader_position = reader_position.clone();
                // Retained even though this block no longer awaits anything:
                // it keeps the body off the event handler's stack. The guard
                // that actually protects the ROOMS write is the
                // `crate::util::defer` below (a real setTimeout) — inlining
                // this block would put everything up to it back on the
                // handler's stack (freenet/river#512).
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
                                // The composer is not offered without the
                                // secret (see `missing_room_secret`); this
                                // is the backstop. A public body in a private
                                // room is not admitted.
                                error!(
                                    "Cannot send: this private room's secret is not on this device"
                                );
                                return;
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
                                // Backstop, as for a reply above.
                                error!(
                                    "Cannot send: this private room's secret is not on this device"
                                );
                                return;
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
                    #[cfg(all(
                        target_arch = "wasm32",
                        feature = "example-data",
                        feature = "no-sync"
                    ))]
                    let signature = crate::test_hooks::forge_if_requested(signature);
                    crate::util::debug_log("[send] signed OK");

                    let auth_message = AuthorizedMessageV1::with_signature(message, signature);
                    let sent_id = auth_message.id();

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
                                } else if !room_data
                                    .room_state
                                    .recent_messages
                                    .messages
                                    .iter()
                                    .any(|m| m.id() == sent_id)
                                {
                                    // The room skips (does not reject) a
                                    // message it does not admit, so a send
                                    // that applied without error may still
                                    // have added nothing.
                                    error!("Failed to apply message delta: the room did not admit the message");
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
                            // rooms (#402).
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
        div { class: "flex-1 flex flex-col min-w-0 bg-panel",
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
                    let room_key = CURRENT_ROOM.read().owner_key;
                    let description_expanded = expanded_description_room.read().as_ref()
                        == room_key.as_ref()
                        && room_key.is_some();
                    let description_can_toggle =
                        description_expanded || *description_overflows.read();
                    let toggle_description = {
                        let room_key = room_key;
                        let toggle_gen = description_toggle_gen.clone();
                        move || {
                            toggle_gen.set(toggle_gen.get().wrapping_add(1));
                            crate::util::defer(move || {
                                // A delayed description click can land after
                                // a room switch; it belongs to the old room.
                                if CURRENT_ROOM.peek().owner_key != room_key {
                                    return;
                                }
                                let open = expanded_description_room.peek().as_ref()
                                    == room_key.as_ref();
                                expanded_description_room.set(if open { None } else { room_key });
                            });
                        }
                    };
                    rsx! {
                        // Full width, left-aligned like the rows below it: no
                        // centred column leaving a void either side.
                        div { class: "flex-shrink-0 px-3 md:pl-5 md:pr-3 py-3 border-b border-border bg-panel",
                            div {
                                "data-testid": "room-header-row",
                                class: "flex items-center justify-between gap-2 md:gap-3",
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
                                    CountBadge { count: panel_unread(), position: "top-0 right-0", testid: "hamburger-unread-badge" }
                                }
                                // Description is a sibling of the title button, not a child:
                                // `<a>` is interactive content and cannot be nested inside
                                // `<button>` per the HTML spec. Nesting also bubbles link
                                // clicks to the modal-opening onclick handler.
                                div { id: "room-header-column", class: "min-w-0 flex-1",
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
                                        div { class: "flex items-start gap-1 min-w-0",
                                            div {
                                                id: "room-header-description",
                                                "data-testid": "room-header-description",
                                                "data-expanded": if description_expanded { "true" } else { "false" },
                                                class: room_description_class(
                                                    description_expanded,
                                                    description_can_toggle,
                                                ),
                                                onpointerdown: {
                                                    let pressed = description_pressed_with.clone();
                                                    move |e: PointerEvent| {
                                                        record_press(&pressed, "description", e.data().pointer_type());
                                                    }
                                                },
                                                onpointercancel: {
                                                    let pressed = description_pressed_with.clone();
                                                    move |_| {
                                                        pressed.borrow_mut().take();
                                                    }
                                                },
                                                onclick: {
                                                    let toggle_description = toggle_description.clone();
                                                    let toggle_gen = description_toggle_gen.clone();
                                                    let pressed = description_pressed_with.clone();
                                                    move |evt| {
                                                        // Every click, the second of a
                                                        // double-click included, cancels a
                                                        // toggle still waiting below.
                                                        let armed = toggle_gen.get().wrapping_add(1);
                                                        toggle_gen.set(armed);
                                                        let pressed_with = take_fresh_press(&pressed, "description");
                                                        if description_click_is_link(&evt)
                                                            || description_click_is_multi(&evt)
                                                            || !description_selection_is_collapsed()
                                                            || !*description_overflows.peek()
                                                                && !description_expanded
                                                        {
                                                            return;
                                                        }
                                                        // A finger toggles at once: a double tap
                                                        // does not select a word. A pen waits like
                                                        // a mouse, since a pen double-tap selects a
                                                        // word in desktop browsers.
                                                        if pressed_with.as_deref() == Some("touch") {
                                                            toggle_description();
                                                            return;
                                                        }
                                                        // The first click of a double-click has
                                                        // detail 1, so it cannot be told apart
                                                        // here. Wait out the double-click window:
                                                        // toggling now would reflow the text under
                                                        // the pointer, and the second click would
                                                        // select a different word or none.
                                                        let toggle_gen = toggle_gen.clone();
                                                        let toggle_description = toggle_description.clone();
                                                        let wait_ms = description_click_wait_ms();
                                                        mark_description_click(armed, None);
                                                        crate::util::safe_spawn_local(async move {
                                                            crate::util::sleep(crate::util::millis(wait_ms)).await;
                                                            let toggles = toggle_gen.get() == armed
                                                                && description_selection_is_collapsed();
                                                            mark_description_click(
                                                                armed,
                                                                Some(if toggles { "toggled" } else { "cancelled" }),
                                                            );
                                                            if toggles {
                                                                toggle_description();
                                                            }
                                                        });
                                                    }
                                                },
                                                dangerous_inner_html: "{desc_html}"
                                            }
                                            if description_can_toggle {
                                                button {
                                                    "data-testid": "room-description-toggle",
                                                    // 24px square (WCAG 2.2 target size) around a
                                                    // 14px icon. The negative margin keeps its
                                                    // layout height at 12px, under the 16px line,
                                                    // so the header does not grow.
                                                    class: "flex-shrink-0 inline-flex items-center justify-center w-6 h-6 -my-1.5 rounded-lg text-text-muted hover:text-accent hover:bg-surface transition-colors",
                                                    "aria-expanded": if description_expanded { "true" } else { "false" },
                                                    "aria-label": if description_expanded { "Hide room description" } else { "Show room description" },
                                                    title: if description_expanded { "Hide room description" } else { "Show room description" },
                                                    onclick: {
                                                        let toggle_description = toggle_description.clone();
                                                        move |_| toggle_description()
                                                    },
                                                    if description_expanded {
                                                        Icon { icon: FaChevronUp, width: 14, height: 14 }
                                                    } else {
                                                        Icon { icon: FaChevronDown, width: 14, height: 14 }
                                                    }
                                                }
                                            }
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
            // With no room, `hidden` gives the panel to the no-room footer
            // below; the history stays mounted for the next room's opening.
            div {
                class: if current_room_data.is_some() { "flex-1 min-h-0 relative" } else { "hidden" },
                div {
                    // `overflow-x-hidden` is a backstop: the popovers open toward
                    // the roomier side, so at most a few px can pass the edge;
                    // clip that rather than show a horizontal scrollbar. #402.
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
                    "data-testid": "chat-scroll-container",
                    // Full width: rows and their hover band span the column, and
                    // the body text keeps its own 76ch measure.
                    div { class: "py-4", id: "chat-content",
                    {
                        // Use memoized message groups to avoid expensive re-computation on keystrokes
                        if current_room_data.is_some() {
                            match message_groups.read().as_deref() {
                                Some((
                                    groups,
                                    self_member_id,
                                    member_names,
                                    impersonation,
                                    deputy_badges,
                                    owner_id,
                                )) => {
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
                                    // Before the range resolves, so a follow
                                    // selects the latest range in this render.
                                    #[cfg(target_arch = "wasm32")]
                                    follow_arrival_at_end(
                                        groups,
                                        CURRENT_ROOM.peek().owner_key,
                                        &reader_position,
                                    );
                                    let (history_window, head_removed) = resolve_rendered_range(
                                        groups,
                                        requested_window,
                                        CURRENT_ROOM.peek().owner_key,
                                        &reader_position,
                                    );
                                    // A request about to move the view anyway
                                    // needs no compensation.
                                    if head_removed && reader_position.request.get().is_none() {
                                        // The DOM still shows the previous
                                        // render here.
                                        #[cfg(target_arch = "wasm32")]
                                        reader_position.capture_before_patch();
                                    }
                                    let withheld_newer = groups.len() - history_window.end;
                                    let groups =
                                        groups[history_window.start..history_window.end].to_vec();
                                    let self_member_id = *self_member_id;
                                    let owner_id = *owner_id;
                                    let member_names = member_names.clone();
                                    let impersonation = impersonation.clone();
                                    let deputy_badges = deputy_badges.clone();
                                    // This viewer's avatar hue for each author in the
                                    // rendered range, under the room's salt. No salt
                                    // (not fetched yet, or the delegate has no key for
                                    // the room) leaves every avatar neutral; there is
                                    // deliberately no other source (`avatar_salt.rs`).
                                    let author_hues: HashMap<MemberId, u16> = {
                                        let owner_vk = CURRENT_ROOM.peek().owner_key;
                                        let salt = owner_vk.and_then(|vk| {
                                            crate::avatar_salt::ensure_avatar_salt(vk.to_bytes());
                                            match crate::avatar_salt::AVATAR_SALTS.try_read() {
                                                Ok(salts) => salts.get(&vk.to_bytes()).copied(),
                                                Err(_) => {
                                                    // No subscription this render; the
                                                    // nudge renders it again (#555).
                                                    crate::util::signal_guard::anchor();
                                                    crate::util::signal_guard::schedule_nudge();
                                                    None
                                                }
                                            }
                                        });
                                        match (salt, owner_vk, current_room_data.as_ref()) {
                                            (Some(salt), Some(owner_vk), Some(rd)) => groups
                                                .iter()
                                                .filter_map(|item| match item {
                                                    DisplayItem::Messages(g) => Some(g.author_id),
                                                    DisplayItem::Event(_) => None,
                                                })
                                                .collect::<HashSet<MemberId>>()
                                                .into_iter()
                                                .filter_map(|author| {
                                                    avatar::member_vk_bytes(&rd.room_state, &owner_vk, author)
                                                        .map(|vk| (author, avatar::display_hue(&salt, &vk)))
                                                })
                                                .collect(),
                                            _ => HashMap::new(),
                                        }
                                    };
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
                                        // Outside the history list, so it
                                        // adds no row of its own to the
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
                                                                // Before the re-render; see
                                                                // `restore_after_patch`.
                                                                #[cfg(target_arch = "wasm32")]
                                                                reader_position.capture_before_patch();
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
                                            // `.msg-history` (main.css) spaces the items and
                                            // cancels the newest row's bottom padding.
                                            class: "msg-history",
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
                                                let impersonation = impersonation.clone();
                                                let deputy_badges = deputy_badges.clone();
                                                let author_hues = author_hues.clone();
                                                move |row| {
                                                let handle_toggle_reaction = handle_toggle_reaction.clone();
                                                let handle_edit_message = handle_edit_message.clone();
                                                let member_names = member_names.clone();
                                                let impersonation = impersonation.clone();
                                                let deputy_badges = deputy_badges.clone();
                                                match row {
                                                    DisplayRow::DateSeparator { key, label } => rsx! {
                                                        // A hairline with the label (`.msg-day`).
                                                        div {
                                                            key: "{key}",
                                                            "data-testid": "date-separator",
                                                            class: "msg-day",
                                                            "{label}"
                                                        }
                                                    },
                                                    DisplayRow::Item(DisplayItem::Event(summary)) => {
                                                        let (names, rest) = event_summary_parts(&summary.names);
                                                        let time = format_utc_as_local_time(summary.last_time.timestamp_millis());
                                                        let key = summary.id.clone();
                                                        rsx! {
                                                            div {
                                                                key: "{key}",
                                                                "data-item-key": "{key}",
                                                                // For `capture_reading_anchor`.
                                                                "data-anchor-key": "{key}",
                                                                "data-testid": "join-notice",
                                                                class: "msg-event",
                                                                span { class: "msg-gutter", {line_icon(JOIN_ICON_PATH, 14)} }
                                                                span { class: "min-w-0 [overflow-wrap:anywhere]",
                                                                    if let Some(first) = names.first() {
                                                                        span { class: "font-bold text-text-secondary", "{first}" }
                                                                    }
                                                                    if let Some(second) = names.get(1) {
                                                                        " and "
                                                                        span { class: "font-bold text-text-secondary", "{second}" }
                                                                    }
                                                                    "{rest} · {time}"
                                                                }
                                                            }
                                                        }
                                                    }
                                                    DisplayRow::Item(DisplayItem::Messages(group)) => {
                                                        let key = group.messages[0].id.clone();
                                                        let author_hue = author_hues.get(&group.author_id).copied();
                                                        rsx! {
                                                            // The wrapper exists to carry
                                                            // `data-item-key`, which marks the
                                                            // item rows the scroll code
                                                            // measures, since a
                                                            // component cannot carry a DOM
                                                            // attribute directly. It is the
                                                            // keyed list child, so diffing
                                                            // is unchanged.
                                                            div {
                                                                key: "{key}",
                                                                "data-item-key": "{key}",
                                                                class: "msg-group",
                                                                MessageGroupComponent {
                                                                group: group,
                                                                self_member_id: self_member_id,
                                                                member_names: member_names,
                                                                impersonation: impersonation,
                                                                deputy_badges: deputy_badges,
                                                                owner_id: owner_id,
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
                                                                author_hue: author_hue,
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
                    // newer-history strip before it has no height). Watched by
                    // `observe_newest` and measured by `HistoryDom::end_seen`.
                    div {
                        id: "bottom-sentinel",
                        class: "h-px pointer-events-none",
                    }
                }
            }
                // Latest (#402): shown while the newest message's bottom is off
                // screen, or a held range withholds newer items.
                if !newest_in_view() || reader_position.has_newer.get() {
                    LatestButton {
                        aria_label: "Scroll to latest messages",
                        test_id: "scroll-to-bottom",
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
                                if let Some(container) = ROOM_HISTORY.container() {
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
                    if let Some((groups, _, _, _, _, _)) = message_groups.read().as_deref() {
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
                            // Same shape as the notices below: a private room's
                            // messages are encrypted with its secret, and a
                            // message sent without it would not be admitted.
                            Ok(()) if room_data.missing_room_secret() => rsx! {
                                div {
                                    "data-testid": "missing-room-secret-notice",
                                    class: "px-4 py-3 mx-4 mb-4 bg-error-bg text-red-700 dark:text-red-400 rounded-lg text-sm",
                                    "This private room's key hasn't reached this device yet, so you can't send messages here until it does."
                                }
                            },
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
                                let mut mention_members: Vec<(MemberId, String)> =
                                    present_member_infos(&room_data.room_state)
                                    .into_iter()
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
                                            let handle = handle_send_message.clone();
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
                                CountBadge { count: panel_unread(), position: "top-0 right-0", testid: "hamburger-unread-badge" }
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
                        // socket is down (#509).
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
                    onclick: move |_| crate::util::defer(move || pending_delete.set(None)),
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
                                onclick: move |_| crate::util::defer(move || pending_delete.set(None)),
                                "Cancel"
                            }
                            button {
                                class: "px-4 py-2 rounded-lg bg-red-500 hover:bg-red-600 text-white transition-colors",
                                onclick: move |_| {
                                    let msg_id_opt = pending_delete.peek().clone();
                                    if let Some(msg_id) = msg_id_opt {
                                        handle_delete_message(msg_id);
                                    }
                                    crate::util::defer(move || pending_delete.set(None));
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

/// Position of an open reactor list. Stored as one signal so the flip, the
/// anchor, and the caps update together.
#[derive(Clone, PartialEq)]
struct ReactionOwnersPopover {
    key: String,
    /// The row's `msg-{id}` suffix, for a focus fallback when the chip is gone.
    message_id: String,
    above: bool,
    align_right: bool,
    max_h: f64,
    max_w: f64,
}

const REACTION_OWNERS_POPOVER_HEIGHT_PX: f64 = 160.0;

fn coarse_pointer() -> bool {
    web_sys::window()
        .and_then(|w| {
            w.match_media("(hover: none), (any-pointer: coarse)")
                .ok()
                .flatten()
        })
        .is_some_and(|mq| mq.matches())
}

/// The last `pointerdown` on one of a component's controls: the control's
/// key, the `pointerType`, and when (ms). A click reads it to learn what
/// pressed it. Not a signal: nothing renders from it.
type PressRecord = Rc<std::cell::RefCell<Option<(String, String, f64)>>>;

/// A press older than this belongs to some other gesture: a press dragged
/// off the control (no click, and no `pointercancel` either), followed later
/// by a click that had no press of its own. A mouse held on a reaction chip
/// for longer than this therefore opens the list rather than removing.
const PRESS_FRESH_MS: f64 = 1500.0;

/// Monotonic milliseconds (`performance.now()`), so a wall-clock change
/// cannot make a press look fresh or stale.
fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|window| window.performance())
            .map_or(0.0, |performance| performance.now())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0.0
    }
}

fn record_press(record: &PressRecord, key: &str, pointer_type: String) {
    *record.borrow_mut() = Some((key.to_string(), pointer_type, now_ms()));
}

/// The `pointerType` of `key`'s own press, if it was recent. Consumes the
/// record either way, so one press answers one click.
fn take_fresh_press(record: &PressRecord, key: &str) -> Option<String> {
    record
        .borrow_mut()
        .take()
        .filter(|(pressed, _, at)| pressed == key && now_ms() - at <= PRESS_FRESH_MS)
        .map(|(_, pointer_type, _)| pointer_type)
}

/// What a pointer of this type does to a chip: `Some(true)` opens the list,
/// `Some(false)` removes, `None` is unknown.
fn pointer_type_opens_owner_list(pointer_type: &str) -> Option<bool> {
    match pointer_type {
        "touch" => Some(true),
        // A pencil has no hover tooltip. On a touch device it should open
        // the list, the same as a finger.
        "pen" => Some(coarse_pointer()),
        "mouse" => Some(false),
        _ => None,
    }
}

/// A finger, or a pen on a coarse screen, opens the list. A mouse removes.
///
/// `pressed_with` is the `pointerType` of the chip's own fresh `pointerdown`
/// (see [`take_fresh_press`]), never the click's: iOS Safari 18.2 and later
/// report a tap's `click` as `"mouse"` while its `pointerdown` says `"touch"`
/// (WebKit bug 282988), and so does Playwright's WebKit. A click with no
/// fresh press of its own (assistive technology activating the chip) opens
/// the list: only a mouse removes in one step, and only when it is known to
/// be a mouse.
fn gesture_opens_owner_list(pressed_with: Option<&str>) -> bool {
    pressed_with
        .and_then(pointer_type_opens_owner_list)
        .unwrap_or(true)
}

/// Where the reactor list for a chip opens, from a point on the chip (the
/// tap, or the chip's centre for a key press): below unless there is more
/// room above, and toward whichever side of the window has more room.
fn reaction_owners_popover_at(
    key: String,
    message_id: String,
    x: f64,
    y: f64,
) -> ReactionOwnersPopover {
    let (above, max_h) = vertical_placement(y, REACTION_OWNERS_POPOVER_HEIGHT_PX);
    let win_w = web_sys::window()
        .and_then(|w| w.inner_width().ok())
        .and_then(|v| v.as_f64())
        .unwrap_or(400.0);
    let space_right = (win_w - x - 8.0).max(128.0);
    let space_left = (x - 8.0).max(128.0);
    let align_right = space_left > space_right;
    let max_w = if align_right { space_left } else { space_right };
    ReactionOwnersPopover {
        key,
        message_id,
        above,
        align_right,
        max_h,
        max_w,
    }
}

/// Where the inline emoji picker opens from its "+", set when it opens (#402).
#[derive(Clone, Copy, Default, PartialEq)]
struct PickerPlacement {
    above: bool,
    /// Leftward (`right-0`).
    align_right: bool,
}

/// The centre of the element with this id, in client coordinates.
fn element_centre(id: &str) -> Option<(f64, f64)> {
    let element = web_sys::window()?.document()?.get_element_by_id(id)?;
    let rect = element.get_bounding_client_rect();
    Some((
        rect.left() + rect.width() / 2.0,
        rect.top() + rect.height() / 2.0,
    ))
}

fn reaction_owners_dialog_id(key: &str) -> String {
    format!("reaction-owners-{key}")
}

fn reaction_chip_id(key: &str) -> String {
    format!("reaction-chip-{key}")
}

/// Focus without scrolling. Moving focus between a chip and its list must not
/// move the history: only an arrival does (history-scrolling.md).
#[cfg(target_arch = "wasm32")]
fn focus_without_scrolling(element: &web_sys::Element) -> bool {
    let Some(element) = element.dyn_ref::<web_sys::HtmlElement>() else {
        return false;
    };
    let options = web_sys::FocusOptions::new();
    options.set_prevent_scroll(true);
    element.focus_with_options(&options).is_ok()
}

/// Whether a popover of `height` opened at viewport `y` goes above (only when
/// it doesn't fit below and there is more room above, within the chat
/// scrollport), and its max height: the space on that side less the 8px gap
/// each way, so an `overflow-y-auto` popover scrolls instead of clipping (#402).
fn vertical_placement(y: f64, height: f64) -> (bool, f64) {
    vertical_placement_from(y, y, height)
}

/// [`vertical_placement`] for a popover hung off an element spanning viewport
/// `top..bottom`: it opens below from `bottom`, or above from `top`.
fn vertical_placement_from(top: f64, bottom: f64, height: f64) -> (bool, f64) {
    let (scroll_top, scroll_bottom) = chat_scrollport_bounds();
    let (below, above_space) = (scroll_bottom - bottom, top - scroll_top);
    let above = below < height && above_space > below;
    let max_h = ((if above { above_space } else { below }) - 16.0).max(1.0);
    (above, max_h)
}

/// The inline picker's height: 16 emoji in four rows of four, plus padding.
const INLINE_PICKER_HEIGHT_PX: f64 = 140.0;

fn chat_scrollport_bounds() -> (f64, f64) {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("chat-scroll-container"))
        .map(|el| {
            let rect = el.get_bounding_client_rect();
            (rect.top(), rect.bottom())
        })
        .unwrap_or((60.0, 600.0))
}

/// Where `msg-{msg_id}`'s inline picker opens, measured from its "+" wrapper,
/// never from a click: a key press reports no position, and the kebab's React
/// is tapped nowhere near "+".
///
/// - Leftward (`right-0`) when the wrapper sits right of the chat scroller's
///   centre, so the picker opens toward the roomier side.
/// - Above when it does not fit below the wrapper inside the scroller and
///   there is more room above ([`vertical_placement_from`]).
///
/// Unmeasurable, it opens below and rightward.
fn inline_picker_placement(msg_id: &str) -> PickerPlacement {
    let measure = || {
        let document = web_sys::window()?.document()?;
        let plus = document
            .get_element_by_id(&format!("msg-{msg_id}"))?
            .query_selector("[data-testid=\"add-reaction-button\"]")
            .ok()??
            .parent_element()?
            .get_bounding_client_rect();
        let scroller = document
            .get_element_by_id("chat-scroll-container")?
            .get_bounding_client_rect();
        let (above, _) =
            vertical_placement_from(plus.top(), plus.bottom(), INLINE_PICKER_HEIGHT_PX);
        Some(PickerPlacement {
            above,
            align_right: plus.left() + plus.right() > scroller.left() + scroller.right(),
        })
    };
    measure().unwrap_or_default()
}

/// A ⚠ or 🛡 beside a name, its tooltip also its accessible label.
fn name_mark(
    testid: &'static str,
    class: &'static str,
    tooltip: String,
    glyph: &'static str,
) -> Element {
    rsx! {
        span { "data-testid": testid, class, title: "{tooltip}", "aria-label": "{tooltip}", {glyph} }
    }
}

/// A role tag after an author's name ("OWNER", "DEPUTY"): small outlined
/// caps, an optional line icon, its tooltip also its accessible label.
fn name_tag(
    testid: &'static str,
    tooltip: String,
    icon_path: Option<&'static str>,
    label: &'static str,
) -> Element {
    rsx! {
        span {
            "data-testid": testid,
            class: "self-center inline-flex items-center gap-[3px] px-[5px] leading-4 text-[10.5px] font-bold uppercase tracking-[0.03em] text-text-secondary border border-border-hover rounded cursor-default",
            title: "{tooltip}",
            "aria-label": "{tooltip}",
            if let Some(path) = icon_path {
                {line_icon(path, 11)}
            }
            {label}
        }
    }
}

/// A 24-unit line icon (stroke, no fill) at `size` px, decorative.
fn line_icon(icon_d: &'static str, size: u32) -> Element {
    rsx! {
        svg {
            "aria-hidden": "true",
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.8",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            class: "flex-none",
            path { d: icon_d }
        }
    }
}

/// Line-icon paths (24-unit, stroked), as in the design mockup.
const SHIELD_ICON_PATH: &str = "M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10";
const JOIN_ICON_PATH: &str = "M15 3h4a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-4M10 17l5-5-5-5M15 12H3";
const REPLY_ICON_PATH: &str = "M9 14 4 9l5-5M20 20v-7a4 4 0 0 0-4-4H4";
const REACT_ICON_PATH: &str =
    "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM8 14s1.5 2 4 2 4-2 4-2M9 9h.01M15 9h.01";
const EDIT_ICON_PATH: &str = "M12 20h9M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4z";
const DELETE_ICON_PATH: &str = "M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6M10 11v6M14 11v6";

/// ⚠ and 🛡 for one reactor. `reactor_id` is the member whose name is shown.
fn reaction_owner_marks(
    reactor_id: MemberId,
    display_name: &str,
    impersonation: &ImpersonationChecker,
    owner_id: MemberId,
    deputy_badges: &HashMap<MemberId, DeputyBadge>,
) -> (Option<ImpersonationWarning>, Option<DeputyBadge>) {
    (
        impersonation_warning_for_display(
            impersonation,
            reactor_id,
            display_name,
            privilege_in_view(reactor_id, owner_id, deputy_badges),
        ),
        deputy_badges.get(&reactor_id).cloned(),
    )
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
    impersonation: ImpersonationChecker,
    deputy_badges: HashMap<MemberId, DeputyBadge>,
    owner_id: MemberId,
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
    /// The author's avatar hue under this viewer's salt for the room, or
    /// `None` for the neutral disc (no salt yet, or an author the room no
    /// longer lists).
    author_hue: Option<u16>,
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
    // composer — mirrors `PickerPlacement::above` for the emoji picker (#402).
    let mut menu_show_above: Signal<bool> = use_signal(|| false);

    // Max height (px) for the kebab action menu, measured at tap time as the
    // actual space available on the chosen side within the chat scrollport. The
    // menu is `overflow-y-auto`, so on a very short/landscape viewport where it
    // fits neither side fully it scrolls internally instead of being clipped by
    // the scroll container with Edit/Delete unreachable (#402).
    let mut menu_max_h: Signal<f64> = use_signal(|| 0.0);

    let mut picker_placement: Signal<PickerPlacement> = use_signal(PickerPlacement::default);
    // Measures the "+" now, then places and opens the picker in one deferred
    // step (.claude/rules/dioxus-signal-safety.md). Every open path (the "+",
    // the hover bar's React, the kebab's React) comes through here.
    let open_inline_picker = move |msg_id: &str| {
        let placement = inline_picker_placement(msg_id);
        let picker_id = format!("inline-{msg_id}");
        crate::util::defer(move || {
            picker_placement.set(placement);
            open_emoji_picker.set(Some(picker_id));
            open_action_menu.set(None);
        });
    };
    let mut open_reaction_owners: Signal<Option<ReactionOwnersPopover>> = use_signal(|| None);
    {
        let live_owner_keys: HashSet<String> = group
            .messages
            .iter()
            .flat_map(|msg| {
                let id = msg.id.clone();
                msg.reactions
                    .keys()
                    .map(move |emoji| format!("{id}:{emoji}"))
            })
            .collect();
        let owners_stale = open_reaction_owners
            .read()
            .as_ref()
            .is_some_and(|popup| !live_owner_keys.contains(&popup.key));
        if owners_stale {
            let mut open_reaction_owners = open_reaction_owners;
            crate::util::defer(move || {
                let still = open_reaction_owners
                    .peek()
                    .as_ref()
                    .is_some_and(|popup| !live_owner_keys.contains(&popup.key));
                if still {
                    open_reaction_owners.set(None);
                }
            });
        }
    }
    // The last `pointerdown` on a reaction chip in this group. The chip's
    // click consumes it; see `gesture_opens_owner_list` for why the click's
    // own type is not used.
    let chip_pressed_with: PressRecord = use_hook(|| Rc::new(std::cell::RefCell::new(None)));
    {
        // (key, message id) of the list that was open on the last run.
        let owners_focus = use_hook(|| Rc::new(std::cell::RefCell::new(None::<(String, String)>)));
        let open_reaction_owners = open_reaction_owners;
        use_effect(move || {
            // An infallible read, deliberately. `open_reaction_owners` is this
            // component's own `use_signal`, written only from `defer()`
            // callbacks, so no write borrow can be live while this effect
            // runs, and the render body reads it the same way. `try_read()`
            // would add a failure path, plus the anchor and nudge
            // (dioxus-signal-safety.md, freenet/river#555) to keep the effect
            // subscribed through it, for contention that cannot happen here.
            let current = open_reaction_owners
                .read()
                .as_ref()
                .map(|popup| (popup.key.clone(), popup.message_id.clone()));
            let previous = owners_focus.replace(current.clone());
            #[cfg(not(target_arch = "wasm32"))]
            let _ = previous;
            #[cfg(target_arch = "wasm32")]
            if current != previous {
                crate::util::defer(move || {
                    let Some(document) = web_sys::window().and_then(|window| window.document())
                    else {
                        return;
                    };
                    if let Some((key, _)) = current {
                        if let Some(dialog) =
                            document.get_element_by_id(&reaction_owners_dialog_id(&key))
                        {
                            focus_without_scrolling(&dialog);
                        }
                        return;
                    }
                    let Some((key, message_id)) = previous else {
                        return;
                    };
                    if let Some(chip) = document.get_element_by_id(&reaction_chip_id(&key)) {
                        focus_without_scrolling(&chip);
                        return;
                    }
                    // The chip is gone: Remove took the only reaction with that
                    // emoji. The nearest control left on the message takes
                    // focus: its "+" while other chips remain, else its React
                    // action, which is the kebab on touch and the hover bar's
                    // React otherwise (a message with no reactions has no
                    // visible "+"). Each is tried until one actually holds
                    // focus, since a hidden one refuses it. If the message is
                    // gone too, focus stays where the browser put it.
                    let Some(row) = document.get_element_by_id(&format!("msg-{message_id}")) else {
                        return;
                    };
                    for selector in [
                        "[data-testid=\"add-reaction-button\"].has-reactions",
                        "[data-testid=\"message-kebab\"]",
                        "[data-testid=\"message-hover-actions\"] button",
                    ] {
                        let Some(control) = row.query_selector(selector).ok().flatten() else {
                            continue;
                        };
                        if focus_without_scrolling(&control)
                            && document.active_element().as_ref() == Some(&control)
                        {
                            return;
                        }
                    }
                });
            }
        });
    }

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

    // Your own name is the link colour; others' names underline on hover
    // (`.msg-author-name` in main.css) and keep the text colour, so a name
    // under the pointer never reads as yours.
    let name_colour = if is_self { "text-link" } else { "text-text" };
    let author_initials = avatar::initials(&group.author_name);
    let avatar_class = format!(
        "msg-avatar{}{}",
        if author_hue.is_some() {
            " msg-avatar-hued"
        } else {
            ""
        },
        if is_self { " msg-avatar-self" } else { "" },
    );
    let avatar_style = avatar::avatar_style(author_hue);
    let author_is_owner = group.author_id == owner_id;
    let author_id = group.author_id;
    let open_author_info = move |_: MouseEvent| {
        crate::util::defer(move || {
            MEMBER_INFO_MODAL.with_mut(|signal| signal.member = Some(author_id));
        });
    };

    rsx! {
        div {
            {
                group.messages.into_iter().enumerate().map(move |(idx, msg)| {
                        let is_first = idx == 0;
                        let has_reactions = !msg.reactions.is_empty();
                        let reply_strip_val = msg.reply_strip.clone();
                        let row_time_ms = msg.time.timestamp_millis();

                        rsx! {
                            // One row per message (`.msg-row` in main.css): the
                            // avatar gutter, then the content column.
                            div {
                                key: "{msg.id}",
                                id: "msg-{msg.id}",
                                // For `capture_reading_anchor`; unlike the
                                // group key, a message id never re-keys.
                                "data-anchor-key": "{msg.id}",
                                "data-testid": "message-row",
                                "data-message-self": "{is_self}",
                                class: "msg-row group",
                                div {
                                    class: "msg-gutter",
                                    if is_first {
                                        // Decorative for assistive tech: the name
                                        // button beside it is the accessible
                                        // control, so this is no tab stop either.
                                        div {
                                            "data-testid": "message-author-avatar",
                                            "aria-hidden": "true",
                                            class: "{avatar_class}",
                                            style: avatar_style.clone(),
                                            onclick: open_author_info,
                                            "{author_initials}"
                                        }
                                    } else {
                                        span {
                                            "data-testid": "message-gutter-time",
                                            class: "msg-gutter-time",
                                            title: format_utc_as_full_datetime(row_time_ms),
                                            {format_utc_as_local_time(row_time_ms)}
                                        }
                                    }
                                }
                                div {
                                    "data-testid": "message-content",
                                    // `relative` anchors the hover bar (over its
                                    // top right) and the kebab (just past its
                                    // right edge, in the touch gutter).
                                    class: "msg-content relative min-w-0",
                                    if is_first {
                                        div {
                                            "data-testid": "message-group-header",
                                            class: "flex flex-wrap items-baseline gap-x-2 mb-0.5 leading-5",
                                            // A native button: Enter and Space fire
                                            // its click, so no key handler.
                                            button {
                                                r#type: "button",
                                                "data-testid": "message-author-name",
                                                class: "msg-author-name text-[15px] font-bold text-left min-w-0 [overflow-wrap:anywhere] cursor-pointer rounded-sm focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-[var(--color-link)] {name_colour}",
                                                title: "Member ID: {author_id}",
                                                onclick: open_author_info,
                                                "{group.author_name}"
                                            }
                                            // Role tags. The owner tag reads the room's
                                            // owner id, the same fact as the member
                                            // list's crown and its tooltip.
                                            if author_is_owner {
                                                {name_tag("message-author-owner-tag", "Room Owner".to_string(), None, "Owner")}
                                            }
                                            // Deputy tag. Same visibility rule and the
                                            // same tooltip as the member-list row and the
                                            // member-info modal chip, so the three read
                                            // as one badge. The nickname above cannot
                                            // forge it: it is styled markup, and
                                            // nicknames are stripped of emoji by
                                            // `crate::util::display_name`.
                                            {group.author_badge.as_ref().map(|badge| name_tag("message-author-deputy-badge", badge.tooltip(), Some(SHIELD_ICON_PATH), "Deputy"))}
                                            // Impersonation warning, after the tags.
                                            // The tag and the warning are NOT mutually
                                            // exclusive — two deputies whose names
                                            // collide each carry a tag AND a warning —
                                            // so these really are two positions and
                                            // both can be filled at once. Do not
                                            // collapse them into one slot, and do not
                                            // suppress the warning when a tag is
                                            // present: a deputised sockpuppet carries a
                                            // genuine tag, and suppressing on it is the
                                            // exact immunity
                                            // `a_deputised_sockpuppet_cannot_suppress_its_own_warning`
                                            // denies.
                                            //
                                            // The nickname above cannot forge this glyph: U+26A0 is
                                            // inside the range `display_name::is_display_hidden`
                                            // strips, so it can never survive into a rendered
                                            // nickname. `the_warning_glyph_cannot_appear_in_a_nickname`
                                            // pins that. The label after it is decorative
                                            // for assistive tech: the glyph's own label
                                            // is the full tooltip.
                                            if let Some(warning) = group.author_impersonation.as_ref() {
                                                span { class: "self-center inline-flex items-center gap-1 text-xs font-medium text-warning-text",
                                                    {name_mark("message-author-impersonation-warning", "cursor-default", warning.tooltip(), crate::util::confusable::WARNING_GLYPH)}
                                                    span { "aria-hidden": "true", class: "cursor-default", title: warning.tooltip(), "Similar name" }
                                                }
                                            }
                                            span {
                                                class: "text-xs text-text-muted cursor-default tabular-nums",
                                                class: if time_clamped { "italic opacity-70" },
                                                title: "{full_time_str}",
                                                if time_clamped { "~{time_str}" } else { "{time_str}" }
                                            }
                                        }
                                    }
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
                                                    class: "p-3 rounded-2xl bg-surface",
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
                                                                crate::util::defer(move || editing_message.set(None));
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
                                                                let msg_id = msg_id.clone();
                                                                let changed = !new_text.is_empty() && new_text != original;
                                                                crate::util::defer(move || {
                                                                    if changed {
                                                                        on_edit.call((msg_id, new_text));
                                                                    }
                                                                    editing_message.set(None);
                                                                });
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
                                                            class: "w-full min-h-[240px] p-2 rounded-lg text-sm resize-y focus:outline-none bg-bg text-text border border-border",
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
                                                                    class: "flex-shrink-0 px-3 py-1.5 text-xs rounded-lg bg-surface text-text hover:bg-border",
                                                                    onclick: move |_| crate::util::defer(move || editing_message.set(None)),
                                                                    "Cancel (Esc)"
                                                                }
                                                                button {
                                                                    class: "flex-shrink-0 px-3 py-1.5 text-xs rounded-lg font-medium hover:opacity-90",
                                                                    style: if over_limit {
                                                                        "background-color: #9ca3af; color: white; cursor: not-allowed; opacity: 0.6;"
                                                                    } else {
                                                                        "background-color: var(--color-accent); color: white;"
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
                                                                        let msg_id = save_msg_id.clone();
                                                                        let changed = !new_text.is_empty() && new_text != save_original;
                                                                        crate::util::defer(move || {
                                                                            if changed {
                                                                                on_edit.call((msg_id, new_text));
                                                                            }
                                                                            editing_message.set(None);
                                                                        });
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
                                                // Reply-quote strip, above the body. Exactly one arm
                                                // renders, enforced by the type: an unverifiable quote
                                                // is `Unavailable`, which carries no author or preview
                                                // to render.
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
                                                                    class: "reply-strip-unavailable min-w-0 w-full max-w-[76ch] text-[13px] leading-[18px] mb-[3px] italic text-text-quote",
                                                                    // The arrow is decorative; the sentence
                                                                    // after it is what a screen reader needs.
                                                                    span { "aria-hidden": "true", class: "inline-block align-[-2px] mr-1.5 not-italic", {line_icon(REPLY_ICON_PATH, 13)} }
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
                                                                        class: "reply-strip min-w-0 w-full max-w-[76ch] text-[13px] leading-[18px] mb-[3px] rounded-sm cursor-pointer text-text-quote",
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
                                                                        span { "aria-hidden": "true", class: "inline-block align-[-2px] mr-1.5", {line_icon(REPLY_ICON_PATH, 13)} }
                                                                        span { class: "font-bold text-text-secondary mr-1.5", "{author}" }
                                                                        span { "{preview}" }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                // Message body, with "(edited)" after it. `[overflow-wrap:anywhere]`
                                                // breaks a long URL only where it would overflow the column.
                                                div {
                                                    "data-testid": "message-body",
                                                    class: "text-[15px] leading-[22px] text-text max-w-[76ch]",
                                                    div {
                                                        class: "prose prose-sm dark:prose-invert max-w-none [overflow-wrap:anywhere]",
                                                        dangerous_inner_html: "{msg.content_html}"
                                                    }
                                                    if msg.edited {
                                                        span { class: "text-xs ml-2 text-text-muted", "(edited)" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Hover action bar: React and Reply for all, Edit and
                                    // Delete for own (`.hover-actions` in main.css). Not
                                    // rendered while the row is being edited.
                                    if editing_message.read().as_ref() != Some(&msg.id) {
                                        {
                                            let msg_id_str_for_edit = msg.id.clone();
                                            let msg_id_for_react = msg.id.clone();
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
                                                    // Hidden and hit-transparent until its row is hovered
                                                    // or holds focus: a touch tap can't fire an invisible
                                                    // action (#402) and an idle bar never covers Latest.
                                                    // It and the closed kebab are z-20, under Latest (z-30)
                                                    // and backdrops (z-40).
                                                    "data-testid": "message-hover-actions",
                                                    class: "hover-actions",
                                                    button {
                                                        r#type: "button",
                                                        title: "React",
                                                        "aria-label": "React",
                                                        onclick: move |e: MouseEvent| {
                                                            e.stop_propagation();
                                                            open_inline_picker(&msg_id_for_react);
                                                        },
                                                        {line_icon(REACT_ICON_PATH, 18)}
                                                    }
                                                    button {
                                                        r#type: "button",
                                                        title: "Reply",
                                                        "aria-label": "Reply",
                                                        onclick: move |_| {
                                                            let ctx = ReplyContext {
                                                                message_id: msg_id_for_reply.clone(),
                                                                author_name: reply_author_name.clone(),
                                                                content_preview: reply_text_preview.clone(),
                                                            };
                                                            crate::util::defer(move || on_reply.call(ctx));
                                                        },
                                                        {line_icon(REPLY_ICON_PATH, 18)}
                                                    }
                                                    if is_self {
                                                        button {
                                                            r#type: "button",
                                                            title: "Edit message",
                                                            "aria-label": "Edit message",
                                                            onclick: move |_| {
                                                                let (t, id) = (current_text.clone(), msg_id_str_for_edit.clone());
                                                                crate::util::defer(move || {
                                                                    edit_text.set(t);
                                                                    editing_message.set(Some(id));
                                                                });
                                                            },
                                                            {line_icon(EDIT_ICON_PATH, 17)}
                                                        }
                                                        button {
                                                            r#type: "button",
                                                            title: "Delete message",
                                                            "aria-label": "Delete message",
                                                            onclick: move |_| {
                                                                let id = msg_id_for_delete.clone();
                                                                crate::util::defer(move || on_request_delete.call(id));
                                                            },
                                                            {line_icon(DELETE_ICON_PATH, 17)}
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // Touch-only kebab action menu (#402). The hover
                                    // action bar above is `display: none` where there is
                                    // no hover pointer (main.css), so it can never appear
                                    // on a touch device. `.touch-actions` (main.css) reveals
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
                                                // Positioned in the row's right gutter with
                                                // `left-full` (NOT `translate`): a transform
                                                // would become the containing block for the `fixed`
                                                // dismiss backdrop below, shrinking it to this element
                                                // instead of the viewport (#402).
                                                // Closed, it is z-20 like the hover bar. Raise the OPEN
                                                // wrapper above sibling kebabs: later ones paint above an open popover, so
                                                // without this a nearby message's kebab could sit over the
                                                // menu rows and steal the tap. `z-[60]` lifts the whole
                                                // open popover+backdrop above them (and its backdrop then
                                                // covers those kebabs, so a tap on one just dismisses). (#402)
                                                class: format!(
                                                    "touch-actions absolute top-1 left-full ml-1 {}",
                                                    if menu_open { "z-[60]" } else { "z-20" },
                                                ),
                                                // Kebab toggle button
                                                button {
                                                    // Bare in its own gutter, so it does not compete with
                                                    // the message; the press shows the surface.
                                                    class: "flex items-center justify-center w-8 h-8 rounded-full text-text-muted active:bg-surface aria-expanded:bg-surface",
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
                                                            // An own menu (4 rows) is taller than a received one
                                                            // (2 rows), so it flips up sooner.
                                                            let menu_height = if is_self { 200.0 } else { 110.0 };
                                                            let (above, max_h) =
                                                                vertical_placement(e.client_coordinates().y, menu_height);
                                                            let id = msg_id_kebab_toggle.clone();
                                                            // Defer signal writes out of the event handler per
                                                            // .claude/rules/dioxus-signal-safety.md (Firefox-mobile
                                                            // re-entrant borrow crashes).
                                                            crate::util::defer(move || {
                                                                menu_show_above.set(above);
                                                                menu_max_h.set(max_h);
                                                                // Dismiss any open reaction picker so the two
                                                                // popovers can't stack (#402).
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
                                                        // Opens leftward from the right-gutter kebab;
                                                        // `max-w` clamps it to the viewport at 320px.
                                                        class: format!(
                                                            "absolute z-50 right-0 min-w-[8rem] max-w-[calc(100vw-1rem)] overflow-y-auto bg-panel rounded-lg shadow-lg border border-border py-1 flex flex-col {}",
                                                            if *menu_show_above.read() { "bottom-full mb-1" } else { "top-full mt-1" },
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
                                                                // Placed from this message's "+", like every
                                                                // other open path (#402).
                                                                open_inline_picker(&msg_id_menu_react);
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
                                                "data-testid": "message-reaction-row",
                                                // A message with no reactions has no reaction line:
                                                // its "+" stays in the DOM, collapsed and inert, only
                                                // as the anchor the picker opens from (the hover
                                                // bar's React, the kebab's React). With reactions,
                                                // the chips and a visible "+" form the line.
                                                class: if has_reactions { "flex flex-wrap items-center gap-[5px] mt-[5px] mb-0.5" } else { "flex h-0" },
                                                // Existing reactions (clickable to toggle if user has reacted)
                                                {
                                                    let mut sorted_reactions: Vec<_> = msg.reactions.iter().collect();
                                                    sorted_reactions.sort_by_key(|(emoji, _)| emoji.as_str());
                                                    sorted_reactions.into_iter().map(|(emoji, reactors)| {
                                                        let count = reactors.len();
                                                        let is_user_reaction = self_member_id.is_some_and(|me| reactors.contains(&me));
                                                        let emoji_for_click = emoji.clone();
                                                        let msg_id_for_click = msg_id_react.clone();

                                                        // "You" is not a nickname, so it is not checked
                                                        // for impersonation. Everyone else gets the
                                                        // same ⚠ / 🛡 as the author line.
                                                        let shown_reactors: Vec<_> = reactors.iter().map(|reactor_id| {
                                                            let is_you = Some(*reactor_id) == self_member_id;
                                                            let name = if is_you {
                                                                "You".to_string()
                                                            } else {
                                                                member_names.get(reactor_id)
                                                                    .cloned()
                                                                    .unwrap_or_else(|| "Unknown".to_string())
                                                            };
                                                            let (warning, badge) = if is_you {
                                                                (None, deputy_badges.get(reactor_id).cloned())
                                                            } else {
                                                                reaction_owner_marks(
                                                                    *reactor_id,
                                                                    &name,
                                                                    &impersonation,
                                                                    owner_id,
                                                                    &deputy_badges,
                                                                )
                                                            };
                                                            (name, warning, badge)
                                                        }).collect();
                                                        let names_str = shown_reactors
                                                            .iter()
                                                            .map(|(name, _, _)| name.as_str())
                                                            .collect::<Vec<_>>()
                                                            .join(", ");
                                                        let tooltip = if is_user_reaction {
                                                            format!("{names_str} (click to remove)")
                                                        } else {
                                                            names_str
                                                        };
                                                        let owners_key = format!("{msg_id_for_inline}:{emoji}");
                                                        let (
                                                            owners_open,
                                                            owners_above,
                                                            owners_align_right,
                                                            owners_max_h,
                                                            owners_max_w,
                                                        ) = {
                                                            let open = open_reaction_owners.read();
                                                            let mine = open.as_ref().filter(|popup| popup.key == owners_key);
                                                            (
                                                                mine.is_some(),
                                                                mine.is_some_and(|popup| popup.above),
                                                                mine.is_some_and(|popup| popup.align_right),
                                                                mine.map(|popup| popup.max_h).unwrap_or(REACTION_OWNERS_POPOVER_HEIGHT_PX),
                                                                mine.map(|popup| popup.max_w).unwrap_or(240.0),
                                                            )
                                                        };
                                                        let msg_id_for_remove = msg_id_for_click.clone();
                                                        let emoji_for_remove = emoji_for_click.clone();
                                                        let owners_key_for_click = owners_key.clone();
                                                        let owners_key_for_key = owners_key.clone();
                                                        let owners_key_for_press = owners_key.clone();
                                                        let chip_pressed_with_click = chip_pressed_with.clone();
                                                        let chip_pressed_with_down = chip_pressed_with.clone();
                                                        let chip_pressed_with_cancel = chip_pressed_with.clone();
                                                        let msg_id_for_owners = msg_id_for_inline.clone();
                                                        let msg_id_for_owners_key = msg_id_for_inline.clone();
                                                        let chip_id = reaction_chip_id(&owners_key);
                                                        let chip_id_for_key = chip_id.clone();
                                                        let owners_dialog_id = reaction_owners_dialog_id(&owners_key);

                                                        rsx! {
                                                            div {
                                                                key: "{emoji}",
                                                                class: format!(
                                                                    "relative inline-flex items-center {}",
                                                                    if owners_open { "z-[60]" } else { "" }
                                                                ),
                                                                if owners_open {
                                                                    div {
                                                                        class: "fixed inset-0 z-40",
                                                                        onclick: move |_| {
                                                                            crate::util::defer(move || open_reaction_owners.set(None));
                                                                        },
                                                                    }
                                                                    div {
                                                                        id: "{owners_dialog_id}",
                                                                        "data-testid": "reaction-owners",
                                                                        role: "dialog",
                                                                        tabindex: "-1",
                                                                        "aria-label": "Who reacted",
                                                                        class: format!(
                                                                            "absolute z-50 w-max min-w-[8rem] overflow-y-auto bg-panel rounded-lg shadow-lg border border-border py-1 flex flex-col {} {}",
                                                                            if owners_above { "bottom-full mb-1" } else { "top-full mt-1" },
                                                                            if owners_align_right { "right-0" } else { "left-0" }
                                                                        ),
                                                                        style: format!("max-height: {owners_max_h}px; max-width: {owners_max_w}px"),
                                                                        onclick: move |e: MouseEvent| e.stop_propagation(),
                                                                        onkeydown: move |e: KeyboardEvent| {
                                                                            if e.key() == Key::Escape {
                                                                                e.prevent_default();
                                                                                crate::util::defer(move || open_reaction_owners.set(None));
                                                                            }
                                                                        },
                                                                        if is_user_reaction {
                                                                            button {
                                                                                "data-testid": "reaction-remove",
                                                                                class: "px-3 py-2 text-sm text-left text-red-500 hover:bg-error-bg",
                                                                                onclick: move |_| {
                                                                                    let id = msg_id_for_remove.clone();
                                                                                    let emoji = emoji_for_remove.clone();
                                                                                    crate::util::defer(move || {
                                                                                        on_react.call((id, emoji));
                                                                                        open_reaction_owners.set(None);
                                                                                    });
                                                                                },
                                                                                "Remove"
                                                                            }
                                                                        }
                                                                        {shown_reactors.iter().enumerate().map(|(index, (name, warning, badge))| {
                                                                            rsx! {
                                                                                div {
                                                                                    key: "{index}",
                                                                                    class: "flex items-baseline gap-1 px-3 py-1.5 text-sm text-text text-left",
                                                                                    span {
                                                                                        "data-testid": "reaction-owner",
                                                                                        class: "[overflow-wrap:anywhere]",
                                                                                        "{name}"
                                                                                    }
                                                                                    {warning.as_ref().map(|warning| name_mark("reaction-owner-impersonation-warning", "cursor-default", warning.tooltip(), crate::util::confusable::WARNING_GLYPH))}
                                                                                    {badge.as_ref().map(|badge| name_mark("reaction-owner-deputy-badge", "cursor-default", badge.tooltip(), "🛡"))}
                                                                                }
                                                                            }
                                                                        })}
                                                                    }
                                                                }
                                                                span {
                                                                    id: "{chip_id}",
                                                                    "data-testid": "reaction-chip",
                                                                    role: "button",
                                                                    tabindex: "0",
                                                                    "aria-expanded": if owners_open { "true" } else { "false" },
                                                                    "aria-haspopup": "dialog",
                                                                    // A pill; your own reaction is the link
                                                                    // colour on the soft accent, like a mention.
                                                                    class: format!(
                                                                        "inline-flex items-center gap-1 h-6 px-2 rounded-full border text-[13px] font-bold {}",
                                                                        if is_user_reaction {
                                                                            "cursor-pointer border-[color-mix(in_srgb,var(--color-link)_50%,transparent)] bg-accent-soft text-link"
                                                                        } else {
                                                                            "cursor-default border-border bg-rail text-text-secondary"
                                                                        }
                                                                    ),
                                                                    title: "{tooltip}",
                                                                    onpointerdown: move |e: PointerEvent| {
                                                                        record_press(
                                                                            &chip_pressed_with_down,
                                                                            &owners_key_for_press,
                                                                            e.data().pointer_type(),
                                                                        );
                                                                    },
                                                                    onpointercancel: move |_| {
                                                                        chip_pressed_with_cancel.borrow_mut().take();
                                                                    },
                                                                    onclick: move |e: MouseEvent| {
                                                                        let click_y = e.client_coordinates().y;
                                                                        let click_x = e.client_coordinates().x;
                                                                        // Only this chip's own recent press counts.
                                                                        let pressed_with = take_fresh_press(
                                                                            &chip_pressed_with_click,
                                                                            &owners_key_for_click,
                                                                        );
                                                                        if gesture_opens_owner_list(pressed_with.as_deref()) {
                                                                            let popover = reaction_owners_popover_at(
                                                                                owners_key_for_click.clone(),
                                                                                msg_id_for_owners.clone(),
                                                                                click_x,
                                                                                click_y,
                                                                            );
                                                                            crate::util::defer(move || {
                                                                                let already_open = open_reaction_owners
                                                                                    .peek()
                                                                                    .as_ref()
                                                                                    .is_some_and(|popup| popup.key == popover.key);
                                                                                if already_open {
                                                                                    open_reaction_owners.set(None);
                                                                                } else {
                                                                                    open_emoji_picker.set(None);
                                                                                    open_action_menu.set(None);
                                                                                    open_reaction_owners.set(Some(popover));
                                                                                }
                                                                            });
                                                                        } else if is_user_reaction {
                                                                            let id = msg_id_for_click.clone();
                                                                            let emoji = emoji_for_click.clone();
                                                                            crate::util::defer(move || {
                                                                                on_react.call((id, emoji));
                                                                            });
                                                                        }
                                                                    },
                                                                    onkeydown: move |e: KeyboardEvent| {
                                                                        let key = e.key();
                                                                        if key == Key::Escape {
                                                                            e.prevent_default();
                                                                            crate::util::defer(move || open_reaction_owners.set(None));
                                                                            return;
                                                                        }
                                                                        // A `role="button"` answers Enter and Space.
                                                                        // Both open the list, never remove: removing
                                                                        // stays one step away, behind the list's
                                                                        // Remove, and a key press has no hover title
                                                                        // to say who reacted.
                                                                        let space = matches!(&key, Key::Character(c) if c == " ");
                                                                        if key != Key::Enter && !space {
                                                                            return;
                                                                        }
                                                                        // Space would scroll the history.
                                                                        e.prevent_default();
                                                                        let (x, y) = element_centre(&chip_id_for_key).unwrap_or((0.0, 0.0));
                                                                        let popover = reaction_owners_popover_at(
                                                                            owners_key_for_key.clone(),
                                                                            msg_id_for_owners_key.clone(),
                                                                            x,
                                                                            y,
                                                                        );
                                                                        crate::util::defer(move || {
                                                                            open_emoji_picker.set(None);
                                                                            open_action_menu.set(None);
                                                                            open_reaction_owners.set(Some(popover));
                                                                        });
                                                                    },
                                                                    span { class: "text-[15px] leading-none", "{emoji}" }
                                                                    span { class: "tabular-nums", "{count}" }
                                                                }
                                                            }
                                                        }
                                                    })
                                                }
                                                // Inline add reaction button (same line height as reactions)
                                                div {
                                                    // Raise the whole picker (grid + z-40 backdrop) above the
                                                    // closed z-20 kebabs and hover bars while it's open, so a
                                                    // nearby one can't paint over the emoji grid and steal a tap
                                                    // (mirrors the action menu's z-[60] behaviour). (#402)
                                                    // Collapsed (no reactions), it takes no hits, so it
                                                    // never sits over the next row; the backdrop and the
                                                    // picker take them back.
                                                    class: format!(
                                                        "relative group/react inline-flex items-center {} {}",
                                                        if is_inline_picker_open { "z-[60]" } else { "" },
                                                        if has_reactions { "" } else { "h-0 pointer-events-none" },
                                                    ),
                                                    // Invisible backdrop when picker is open
                                                    if is_inline_picker_open {
                                                        div {
                                                            class: "fixed inset-0 z-40 pointer-events-auto",
                                                            onclick: move |_| crate::util::defer(move || open_emoji_picker.set(None)),
                                                        }
                                                    }
                                                    button {
                                                        "data-testid": "add-reaction-button",
                                                        class: format!(
                                                            "add-reaction-btn inline-flex items-center justify-center text-xl leading-none hover:scale-110 {}",
                                                            if has_reactions { "has-reactions" } else { "invisible h-0 overflow-hidden" }
                                                        ),
                                                        title: "Add reaction",
                                                        onclick: {
                                                            let picker_id = format!("inline-{}", msg_id_for_inline);
                                                            let msg_id = msg_id_for_inline.clone();
                                                            move |e: MouseEvent| {
                                                                e.stop_propagation();
                                                                if open_emoji_picker.peek().as_ref() == Some(&picker_id) {
                                                                    crate::util::defer(move || open_emoji_picker.set(None));
                                                                } else {
                                                                    open_inline_picker(&msg_id);
                                                                }
                                                            }
                                                        },
                                                        "+"
                                                    }
                                                    // Inline emoji picker; placed by `open_inline_picker`.
                                                    if is_inline_picker_open {
                                                        div {
                                                            "data-testid": "emoji-picker",
                                                            class: format!(
                                                                "absolute p-1.5 bg-panel rounded-xl shadow-xl border border-border z-50 grid pointer-events-auto {} {}",
                                                                if picker_placement.read().above { "bottom-full mb-1" } else { "top-full mt-1" },
                                                                if picker_placement.read().align_right { "right-0" } else { "left-0" },
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
                                                                            let (id, emoji) = (msg_id.clone(), emoji_str.clone());
                                                                            crate::util::defer(move || {
                                                                                on_react.call((id, emoji));
                                                                                open_emoji_picker.set(None);
                                                                            });
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
                        }
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_join_notice_names_at_most_two_members() {
        let names = |n: usize| (0..n).map(|i| format!("M{i}")).collect::<Vec<_>>();
        assert_eq!(
            event_summary_parts(&names(1)),
            (names(1), " joined".to_string())
        );
        assert_eq!(
            event_summary_parts(&names(2)),
            (names(2), " joined".to_string())
        );
        assert_eq!(
            event_summary_parts(&names(3)),
            (vec![], "3 people joined".to_string())
        );
    }

    /// The @mention pickers and name lookup offer the owner and present
    /// members, not identities held only as ban evidence.
    #[test]
    fn present_member_infos_leaves_out_evidence_only_records() {
        use ed25519_dalek::SigningKey;
        use river_core::room_state::member::{AuthorizedMember, Member};
        use river_core::room_state::member_info::MemberInfo;
        let owner = SigningKey::from_bytes(&[1u8; 32]);
        let present = SigningKey::from_bytes(&[2u8; 32]);
        let removed = SigningKey::from_bytes(&[3u8; 32]);
        let owner_id = MemberId::from(&owner.verifying_key());
        let record = |sk: &SigningKey| {
            AuthorizedMember::new(
                Member {
                    owner_member_id: owner_id,
                    invited_by: owner_id,
                    member_vk: sk.verifying_key(),
                },
                &owner,
            )
        };
        let info = |sk: &SigningKey| {
            AuthorizedMemberInfo::new_with_member_key(
                MemberInfo::new_public(MemberId::from(&sk.verifying_key()), 1, "n".into()),
                sk,
            )
        };
        let mut state = river_core::room_state::ChatRoomStateV1::default();
        state.configuration.configuration.owner_member_id = owner_id;
        state.members.members.push(record(&present));
        state.ban_evidence.members.push(record(&removed));
        state.member_info.member_info = vec![info(&owner), info(&present), info(&removed)];
        let mut ids: Vec<MemberId> = present_member_infos(&state)
            .iter()
            .map(|i| i.member_info.member_id)
            .collect();
        ids.sort();
        let mut expected = vec![owner_id, MemberId::from(&present.verifying_key())];
        expected.sort();
        assert_eq!(ids, expected);
    }

    /// Both @mention name sources read `present_member_infos`, not the raw
    /// `member_info` vector.
    #[test]
    fn mention_name_sources_read_present_member_infos() {
        let src = include_str!("conversation.rs");
        for needle in [
            "let mut mention_members: Vec<(MemberId, String)> =\n                                    present_member_infos(&room_data.room_state)",
            "let member_names: HashMap<MemberId, String> = present_member_infos(room_state)",
        ] {
            assert!(src.contains(needle), "missing: {needle}");
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

    #[test]
    fn a_join_folding_into_the_trailing_summary_raises_the_newest_key() {
        let summary = |last_time_ms: i64, seed: &[u8]| {
            DisplayItem::Event(EventSummary {
                names: vec!["Ann".into()],
                id: "summary".into(),
                last_time: DateTime::from_timestamp_millis(last_time_ms).unwrap(),
                last_sent_ms: last_time_ms,
                last_message_id: MessageId(freenet_scaffold::util::fast_hash(seed)),
            })
        };
        let before = newest_key(&[summary(1_000, b"join-1")]);
        // The same summary item, now holding a second join.
        let after = newest_key(&[summary(2_000, b"join-2")]);
        assert!(is_arrival(before.as_ref(), after.as_ref(), false));
        assert_eq!(newest_key(&[]), None);
    }

    /// Source-grep pin: the history must render through the window. Rendering
    /// `groups` directly is the regression this guards — it is a one-token edit
    /// away, it looks harmless, and it costs nothing measurable until someone
    /// opens a room with a thousand messages in it.
    #[test]
    fn the_history_renders_through_the_window() {
        // The render, and the range resolution in `history_scroll.rs`.
        let squashed: String = [
            ("conversation.rs", include_str!("conversation.rs")),
            (
                "history_scroll.rs",
                include_str!("conversation/history_scroll.rs"),
            ),
        ]
        .into_iter()
        .flat_map(|(file, source)| {
            source[..source
                .find("#[cfg(test)]\nmod tests {")
                .unwrap_or_else(|| panic!("{file} should have a `#[cfg(test)] mod tests` block"))]
                .chars()
        })
        .filter(|c| !c.is_whitespace())
        .collect();

        assert!(
            squashed.contains("resolve_rendered_range(groups,requested_window,")
                && squashed.contains("HistoryWindow::resolve_held(groups.len(),requested_window,"),
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
            squashed.contains("*reader.window_anchor.borrow_mut()=Some(WindowAnchor{"),
            "the render must write the resolved head's identity back so the \
             NEXT render grows instead of sliding (#501)"
        );
        assert!(
            squashed
                .contains("reader.window_rendered.set(history_window.end-history_window.start);"),
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
        // false-passed against it (#505).
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
    /// row. Truncation happens after mention/markdown cleaning.
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

    /// A tail stamped far ahead with no arrival time is clamped to the pass's
    /// "now", which moves on every pass; its arrival identity must not.
    #[test]
    fn the_newest_key_ignores_the_fallback_clock() {
        let owner = signing_key(1);
        let alice = signing_key(2);
        let owner_id = member_id_of(&owner);

        let now = at(1_700_000_000_000);
        let messages = state(vec![message_at(
            owner_id,
            &alice,
            now + chrono::Duration::minutes(10),
        )]);
        let member_info = info(&alice, "Alice");
        let receive_times = ReceiveTimes::new();

        let first = group(&messages, &member_info, owner_id, &receive_times, now);
        let later = group(
            &messages,
            &member_info,
            owner_id,
            &receive_times,
            now + chrono::Duration::seconds(30),
        );
        assert_ne!(
            only_group(&first).messages[0].time,
            only_group(&later).messages[0].time,
            "premise: the display time follows the fallback clock"
        );

        assert_eq!(newest_key(&first), newest_key(&later));
    }

    /// A new tail clamped to an arrival earlier than the previous tail's
    /// unclamped time is still later in display order: an arrival.
    #[test]
    fn a_new_tail_clamped_below_the_previous_one_is_an_arrival() {
        let owner = signing_key(1);
        let alice = signing_key(2);
        let owner_id = member_id_of(&owner);

        let received = at(1_700_000_000_000);
        let a = message_at(owner_id, &alice, received + chrono::Duration::seconds(50));
        let b = message_at(owner_id, &alice, received + chrono::Duration::seconds(130));
        let mut receive_times = ReceiveTimes::new();
        receive_times.insert(a.id().0 .0, received.timestamp_millis() as f64);
        receive_times.insert(
            b.id().0 .0,
            (received + chrono::Duration::seconds(10)).timestamp_millis() as f64,
        );
        let member_info = info(&alice, "Alice");
        let now = received + chrono::Duration::seconds(20);

        let before = group(
            &state(vec![a.clone()]),
            &member_info,
            owner_id,
            &receive_times,
            now,
        );
        let after = group(
            &state(vec![a, b]),
            &member_info,
            owner_id,
            &receive_times,
            now,
        );
        let tail = |items: &[DisplayItem]| match items.last() {
            Some(DisplayItem::Messages(g)) => g.messages.last().expect("a message").time,
            _ => panic!("expected a message group last"),
        };
        assert!(
            tail(&after) < tail(&before),
            "premise: the new tail displays earlier than the previous one"
        );

        assert!(is_arrival(
            newest_key(&before).as_ref(),
            newest_key(&after).as_ref(),
            false
        ));
    }
}
