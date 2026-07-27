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
    static LAST_TITLE: RefCell<String> = const { RefCell::new(String::new()) };
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

/// River's logo SVG, embedded at compile time. This is the same asset the
/// `<link rel="icon">` in `app.rs` points at (`asset!("/assets/river_logo.svg")`),
/// so the shell tab and the in-iframe tab show an identical icon.
const RIVER_LOGO_SVG: &str = include_str!("../../../assets/river_logo.svg");

/// Send River's favicon to the parent shell via the `__freenet_shell__`
/// postMessage bridge.
///
/// When River runs inside the Freenet gateway's sandboxed iframe, the
/// `<link rel="icon">` set by Dioxus only applies within the iframe — the
/// parent shell tab keeps the generic Freenet favicon. This sends River's
/// logo to the shell so the browser tab shows the correct branding.
///
/// The logo is sent as a self-contained `data:image/svg+xml` URI rather than
/// a page URL. The shell only accepts `https:` and `data:` scheme favicons,
/// and a page URL resolves to `http:` whenever the gateway is served over
/// plain HTTP (local / self-hosted nodes — e.g. `http://127.0.0.1:7509`),
/// which the shell rejects. A `data:` URI is accepted regardless of how the
/// gateway is served and needs no extra cross-origin fetch.
///
/// Sent once at init — the favicon never changes. The shell registers its
/// `message` handler before River's iframe begins loading, so a single
/// fire-and-forget post is sufficient.
fn send_favicon_to_shell() {
    let Some(window) = web_sys::window() else {
        return;
    };
    // `data:image/svg+xml,<percent-encoded-svg>` — `encodeURIComponent` yields
    // a payload valid for a non-base64 data URI for any UTF-8 SVG content.
    let encoded = String::from(js_sys::encode_uri_component(RIVER_LOGO_SVG));
    let href = format!("data:image/svg+xml,{encoded}");
    let msg = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &msg,
        &JsValue::from_str("__freenet_shell__"),
        &JsValue::TRUE,
    );
    let _ = js_sys::Reflect::set(
        &msg,
        &JsValue::from_str("type"),
        &JsValue::from_str("favicon"),
    );
    let _ = js_sys::Reflect::set(&msg, &JsValue::from_str("href"), &JsValue::from_str(&href));
    // River runs inside an iframe; post to the parent shell. If there is no
    // distinct parent (not embedded) this posts to self, which is harmless —
    // same fire-and-forget pattern as `set_document_title` above.
    let target = window.parent().ok().flatten().unwrap_or(window);
    let _ = target.post_message(&msg, "*");
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
/// Counts display messages (non-action, non-deleted, non-event) authored
/// by other users that fall after `last_read_message_id`. Pure — takes a
/// borrowed `RoomData` so callers that already hold a `ROOMS` read guard
/// (the room-list badge memo, the title's cross-room sum) don't re-lock.
///
/// Room-event messages (`CONTENT_TYPE_EVENT`, e.g. "X joined the room")
/// are shown by `display_messages()` but deliberately excluded here
/// (freenet/river#500): they are ambient activity, not something waiting
/// to be read, so they must not inflate the badge or title counts.
///
/// PRIVATE on purpose. This is the mode-BLIND count, and the whole of
/// freenet/river#500 was three surfaces reaching for exactly this and so
/// badging muted rooms. [`count_unread_in_room_data_with_mode`] is the API;
/// a fourth unread surface must go through it.
fn count_unread_in_room_data(room_data: &crate::room_data::RoomData) -> usize {
    unread_candidate_messages(room_data).count()
}

/// The unread tail of `room_data` as an iterator: display messages
/// (non-action, non-deleted, non-event) authored by other users after
/// `last_read_message_id`. Shared core of both counting modes
/// ([`count_unread_in_room_data`] and the MentionsAndReplies arm of
/// [`count_unread_in_room_data_with_mode`]) so the tail-slicing and
/// exclusion rules can't drift between them.
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
fn unread_candidate_messages(
    room_data: &crate::room_data::RoomData,
) -> impl Iterator<Item = &river_core::room_state::message::AuthorizedMessageV1> {
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
        // Mirror `MessagesV1::display_messages` (skip action and deleted
        // msgs), plus skip room events — see the doc comment above.
        .filter(|m| {
            !m.message.content.is_action()
                && !m.message.content.is_event()
                && !recent.actions_state.deleted.contains(&m.id())
        })
        .filter(move |m| m.message.author != self_member_id)
}

/// Whether a body we could NOT read (`try_decrypt_message_content` returned
/// `None`) should nevertheless count toward the MentionsAndReplies badge,
/// because we cannot yet tell whether it mentions the user.
///
/// The answer is yes for EXACTLY ONE state: the room's secrets map is empty.
/// `RoomData.secrets` is `#[serde(skip)]`, so every cold start of an
/// established private room lands here, and it resolves within seconds when
/// `repopulate_secrets_from_state` rehydrates the map. Counting through that
/// window is what stops a real mention hiding behind a silent zero.
///
/// Every other unreadable state does NOT count, and the reason is that none of
/// them is reliably transient:
///
/// * A version OLDER than everything we hold is a rotation the user joined
///   after; they will never read it.
/// * A version NEWER than everything we hold looks like a blob in flight, but
///   need not be: a member who was offline across a rotation may never receive
///   one (`MessagesV1`'s own docs describe a member missing a blob at
///   `current_version` as a supported, unrecoverable state), and a removed
///   member's blobs are pruned outright while their client keeps ingesting
///   messages at the new version.
/// * A secret PRESENT at the right version that does not decrypt the body is
///   overwritten by `repopulate_secrets_from_state` only if the contract
///   carries an owner-signed blob for THIS member at THAT version. If it does
///   not, the wrong key stays.
///
/// Counting any of those puts a number on the badge, the tab title and the
/// hamburger that no amount of reading can clear, pointing at a message the
/// user cannot open — which is the freenet/river#500 symptom in a narrower
/// configuration, and strictly worse than the under-count it avoids. An
/// under-count here is temporary and self-healing: the message is unreadable,
/// so the user could not act on the mention anyway, and both the message and
/// the badge appear together if the secret ever arrives.
fn unreadable_body_may_become_readable(
    msg: &river_core::room_state::message::AuthorizedMessageV1,
    secrets: &std::collections::HashMap<u32, [u8; 32]>,
) -> bool {
    match &msg.message.content {
        river_core::room_state::message::RoomMessageBody::Private { .. } => secrets.is_empty(),
        // A public body always reads; this is only reached defensively.
        river_core::room_state::message::RoomMessageBody::Public { .. } => false,
    }
}

/// Count the unread messages in `room_data` that its
/// [`NotificationMode`](crate::room_data::NotificationMode) surfaces
/// (freenet/river#500). This is THE per-room badge value: the room-list
/// badge, the document-title `(N)` total, and the mobile hamburger badge
/// all sum these same per-room values, so every surface agrees.
///
/// * `All` — every unread other-authored display message
///   ([`count_unread_in_room_data`]).
/// * `MentionsAndReplies` — only unread messages that @mention the user or
///   reply to one of their messages (the same
///   [`crate::components::app::notifications::mentions_or_replies_to_self`]
///   predicate that gates browser notifications). Zero qualifying messages
///   means no badge even when other unreads exist.
/// * `Muted` — always 0; a muted room never badges and never inflates the
///   totals, matching the modal's "Never notify for this room" wording.
///
/// # Unreadable private messages, during the cold-start window only
///
/// In MentionsAndReplies mode a body we cannot decrypt is counted while
/// `room_data.secrets` is EMPTY, because there is no way to tell whether it
/// mentions the user and the state resolves in seconds. `RoomData.secrets` is
/// `#[serde(skip)]`, so every cold start of an established private room is in
/// it; without this, such a room would badge 0 on the row, the title AND the
/// hamburger until `repopulate_secrets_from_state` runs, hiding real mentions.
/// The count resolves to the true mention count once the secrets arrive.
///
/// Bodies that stay unreadable with secrets in hand do NOT count. See
/// [`unreadable_body_may_become_readable`] for why none of those states is
/// reliably transient, and why a permanent over-count is worse than a
/// self-healing under-count.
///
/// # Cost
///
/// The mention scan decrypts message content, so it runs only over the
/// unread tail (the `start..` slice inside [`unread_candidate_messages`])
/// and only for rooms in MentionsAndReplies mode; All and Muted rooms never
/// decrypt anything. That tail is NOT reliably small: `start` is 0 whenever
/// the room has never been opened or the marker aged out of the buffer, and
/// those are exactly the states a mentions-mode room lives in (a room is set
/// to mentions-only precisely so it stops being opened), so the tail is
/// routinely the whole `max_recent_messages` buffer (default 100,
/// owner-configurable with no upper bound). Each private message costs a
/// fresh AES-GCM key schedule plus a decrypt, a CBOR decode and a mention
/// parse, so the scan is memoized per room by
/// [`cached_mention_unread_count`] — see its docs for why that matters.
pub fn count_unread_in_room_data_with_mode(
    room_data: &crate::room_data::RoomData,
    mode: crate::room_data::NotificationMode,
) -> usize {
    use crate::room_data::NotificationMode;
    match mode {
        NotificationMode::All => count_unread_in_room_data(room_data),
        NotificationMode::Muted => 0,
        NotificationMode::MentionsAndReplies => cached_mention_unread_count(room_data),
    }
}

/// Uncached core of the MentionsAndReplies count. Callers should go through
/// [`cached_mention_unread_count`].
fn compute_mention_unread_count(room_data: &crate::room_data::RoomData) -> usize {
    use crate::components::app::notifications::{
        text_mentions_or_replies_to_self, SelfAuthoredIndex,
    };

    let self_member_id: MemberId = room_data.self_sk.verifying_key().into();
    let recent = &room_data.room_state.recent_messages.messages;
    // One lazily-built index of self-authored message ids for the whole
    // scan: the reply check would otherwise re-hash every message in the
    // buffer (`id()` is a 64-byte rolling hash) once per reply candidate.
    let self_authored = SelfAuthoredIndex::new(recent, self_member_id);

    unread_candidate_messages(room_data)
        .filter(|m| {
            match crate::components::conversation::try_decrypt_message_content(
                &m.message.content,
                &room_data.secrets,
            ) {
                // Readable: the ordinary decision, against the text we just
                // decrypted so the body is not decrypted twice.
                Some(text) => {
                    text_mentions_or_replies_to_self(m, &text, &room_data.secrets, &self_authored)
                }
                // Unreadable: count it only while there is reason to expect it
                // to become readable — see the predicate's docs.
                None => unreadable_body_may_become_readable(m, &room_data.secrets),
            }
        })
        .count()
}

thread_local! {
    /// Per-room memo for the MentionsAndReplies unread count, keyed by room
    /// owner key and validated by [`mention_count_fingerprint`].
    static MENTION_COUNT_CACHE: RefCell<
        std::collections::HashMap<ed25519_dalek::VerifyingKey, (u64, usize)>,
    > = RefCell::new(std::collections::HashMap::new());
}

#[cfg(test)]
thread_local! {
    /// Test-only counter of full (cache-missing) mention scans, so a test can
    /// prove the memo actually skips recompute.
    ///
    /// `thread_local`, which is NOT per-test: `cargo test -- --test-threads=1`
    /// runs every test on one thread and shares this counter. Read it as a
    /// DELTA around the calls under test, never as an absolute.
    static MENTION_SCAN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Upper bound on memo entries. The natural bound is "rooms the user has in
/// mentions mode", which is tiny, but a session that joins and leaves many
/// rooms would otherwise accumulate dead keys; clearing wholesale on
/// overflow costs one recompute per live room and keeps this O(1) to reason
/// about.
const MENTION_COUNT_CACHE_MAX: usize = 256;

/// Fingerprint of every input [`compute_mention_unread_count`] reads, cheap
/// enough (all O(1)) to compute on every call:
///
/// * `messages.len()` and the first/last message id — any append, eviction
///   or replacement of the buffer changes one of them;
/// * `last_read_message_id` — moves the tail's start;
/// * every `(version, secret)` PAIR, sorted — arriving secrets change
///   decryptability, which is what resolves the fail-safe over-count down to
///   the true count. Hashing `secrets.len()` alone is NOT enough:
///   `repopulate_secrets_from_state` overwrites a wrong invitation-supplied
///   secret with the authoritative owner-signed one AT THE SAME VERSION, so
///   the length is unchanged and a stale under-count would survive the very
///   event that fixes it (freenet/river#500 review);
/// * `deleted.len()` — deletions change which messages are candidates;
/// * the local member id — an identity import re-keys "self".
fn mention_count_fingerprint(room_data: &crate::room_data::RoomData) -> u64 {
    use std::hash::{Hash, Hasher};
    let recent = &room_data.room_state.recent_messages;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    recent.messages.len().hash(&mut h);
    recent.messages.first().map(|m| m.id()).hash(&mut h);
    recent.messages.last().map(|m| m.id()).hash(&mut h);
    room_data.last_read_message_id.hash(&mut h);
    let mut secrets: Vec<_> = room_data.secrets.iter().collect();
    secrets.sort_unstable_by_key(|(version, _)| **version);
    secrets.hash(&mut h);
    recent.actions_state.deleted.len().hash(&mut h);
    MemberId::from(room_data.self_sk.verifying_key()).hash(&mut h);
    h.finish()
}

/// Memoized [`compute_mention_unread_count`].
///
/// The scan is expensive (see [`count_unread_in_room_data_with_mode`]) and
/// runs up to three times per `ROOMS` write, because all three consumers are
/// always mounted: the room-list badge memo, the conversation header's
/// hamburger badge memo, and `update_document_title`. Without a memo this
/// reproduces the shape of a documented incident in the sibling render path
/// — re-parsing every buffered message body on each update was "a major
/// source of the mobile jank users reported", fixed by `MESSAGE_HTML_CACHE`
/// in `conversation.rs` — except worse placed, since these consumers run for
/// rooms the user is not even looking at.
///
/// Returns a value always identical to calling the uncached form; the memo
/// only skips redundant rescans.
fn cached_mention_unread_count(room_data: &crate::room_data::RoomData) -> usize {
    let fp = mention_count_fingerprint(room_data);
    let key = room_data.owner_vk;

    if let Some(hit) = MENTION_COUNT_CACHE.with(|cache| {
        cache
            .borrow()
            .get(&key)
            .filter(|(cached_fp, _)| *cached_fp == fp)
            .map(|(_, count)| *count)
    }) {
        return hit;
    }

    let count = compute_mention_unread_count(room_data);
    #[cfg(test)]
    MENTION_SCAN_COUNT.with(|c| c.set(c.get() + 1));

    MENTION_COUNT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= MENTION_COUNT_CACHE_MAX && !cache.contains_key(&key) {
            cache.clear();
        }
        cache.insert(key, (fp, count));
    });
    count
}

/// Count total unread messages across all rooms — room messages plus
/// inbound direct messages whose timestamp is newer than the per-pair
/// last-seen value the user advanced by opening the corresponding DM
/// thread (see [`crate::components::direct_messages::DM_LAST_SEEN`]).
///
/// DM unread is tab-title-relevant because the issue lists "incoming DM
/// notifications + unread tracking" as a single line item — without this
/// the inbox badge would update but the document title wouldn't.
///
/// Room totals go through the same mode-aware per-room values as the
/// room-list badge ([`count_unread_in_room_data_with_mode`], via the
/// shared [`count_unread_excluding_room`] sum with no exclusion), so a
/// Muted room never inflates the tab title and a MentionsAndReplies room
/// contributes only its qualifying messages (freenet/river#500). DM
/// counts are unaffected — DMs have no per-thread mode.
pub fn count_total_unread_messages() -> usize {
    let Ok(rooms) = ROOMS.try_read() else {
        return 0;
    };
    let room_unread: usize =
        count_unread_excluding_room(&rooms.map, &rooms.notification_modes, None);
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
    let hidden = match crate::components::direct_messages::HIDDEN_DM_THREADS.try_read() {
        Ok(g) => g.clone(),
        Err(_) => return 0,
    };
    count_unread_dms_with(&rooms.map, &last_seen, &hidden)
}

/// Pure core of [`count_unread_dms`], mirroring the DM rail's per-thread
/// accumulation (`dm_rail_section.rs`): a thread's unread only counts if
/// the thread is actually visible in the panel. A hidden (archived)
/// thread is skipped unless a message STRICTLY newer than its
/// `hidden_at_ts` revived it — the same `is_thread_hidden_for` rule
/// `filter_rail_entries` applies, and revival considers messages in both
/// directions, exactly like the rail's `last_any_ts`. Without this
/// filter the unread tallies (title, hamburger badge) count messages the
/// user has no visible thread for and no way to clear.
fn count_unread_dms_with(
    map: &std::collections::HashMap<ed25519_dalek::VerifyingKey, crate::room_data::RoomData>,
    last_seen: &std::collections::HashMap<(ed25519_dalek::VerifyingKey, MemberId), u64>,
    hidden: &std::collections::HashMap<
        (ed25519_dalek::VerifyingKey, MemberId),
        river_core::chat_delegate::HiddenDmThreadEntry,
    >,
) -> usize {
    let mut total = 0usize;
    for (owner_key, room_data) in map {
        let self_id: MemberId = room_data.self_sk.verifying_key().into();

        // Per-peer accumulation: unread inbound messages plus the newest
        // timestamp in either direction (the revival clock).
        struct Acc {
            last_any_ts: u64,
            unread: usize,
        }
        let mut per_peer: std::collections::HashMap<MemberId, Acc> =
            std::collections::HashMap::new();
        for msg in &room_data.room_state.direct_messages.messages {
            let is_self_sender = msg.message.sender == self_id;
            let is_self_recipient = msg.message.recipient == self_id;
            if !is_self_sender && !is_self_recipient {
                continue;
            }
            let peer = if is_self_sender {
                msg.message.recipient
            } else {
                msg.message.sender
            };
            let acc = per_peer.entry(peer).or_insert(Acc {
                last_any_ts: 0,
                unread: 0,
            });
            if msg.message.timestamp > acc.last_any_ts {
                acc.last_any_ts = msg.message.timestamp;
            }
            if is_self_recipient {
                let cutoff = last_seen.get(&(*owner_key, peer)).copied().unwrap_or(0);
                if msg.message.timestamp > cutoff {
                    acc.unread += 1;
                }
            }
        }

        for (peer, acc) in per_peer {
            if crate::components::direct_messages::is_thread_hidden_for(
                hidden,
                owner_key,
                peer,
                acc.last_any_ts,
            ) {
                continue;
            }
            total += acc.unread;
        }
    }
    total
}

/// Sum mode-aware unread messages across every room in `map` except
/// `exclude` (`None` excludes nothing — the document-title total).
///
/// Each room contributes [`count_unread_in_room_data_with_mode`] under its
/// entry in `modes` (absent = `All`, the same default the notification
/// gate uses), so every unread surface sums identical per-room values
/// (freenet/river#500).
///
/// No signal reads, so the exclusion + mode logic is unit-testable. NOT pure,
/// though: the MentionsAndReplies arm reads and writes a `thread_local` memo
/// (see [`cached_mention_unread_count`]). Its value is unaffected — the memo is
/// validated by [`mention_count_fingerprint`] — but a test that hardcodes a
/// room key shares cache entries with every other test on the thread, so use a
/// fresh key per test.
///
/// The signal-reading wrappers are [`count_total_unread_messages`] and
/// [`count_unread_behind_rooms_panel`].
pub fn count_unread_excluding_room(
    map: &std::collections::HashMap<ed25519_dalek::VerifyingKey, crate::room_data::RoomData>,
    modes: &std::collections::HashMap<
        ed25519_dalek::VerifyingKey,
        crate::room_data::NotificationMode,
    >,
    exclude: Option<&ed25519_dalek::VerifyingKey>,
) -> usize {
    map.iter()
        .filter(|(owner_key, _)| Some(*owner_key) != exclude)
        .map(|(owner_key, room_data)| {
            let mode = modes.get(owner_key).copied().unwrap_or_default();
            count_unread_in_room_data_with_mode(room_data, mode)
        })
        .sum()
}

/// Count unread messages waiting *behind* the mobile rooms panel: rooms
/// OTHER than the currently-open one, plus inbound direct messages (the
/// DM rail lives in that same panel).
///
/// Drives the badge on the mobile hamburger buttons in the conversation
/// header. The current room is excluded because its messages are on
/// screen and marked read on open — counting them would only make the
/// badge flicker (mirrors the `!is_current` guard on the room-list
/// badge in `room_list.rs`).
pub fn count_unread_behind_rooms_panel() -> usize {
    // CURRENT_ROOM is read (infallibly) BEFORE the fallible ROOMS read:
    // `try_read() -> Err` registers no subscription (dioxus-signal-safety
    // rules), so with the reads inverted an Err poll would leave the
    // consuming memo with zero subscriptions — permanently frozen, badge
    // silently stuck. The non-try read first guarantees at least the
    // CURRENT_ROOM subscription always survives (same pattern as
    // `current_room_label` in conversation.rs).
    let current = CURRENT_ROOM.read().owner_key;
    let Ok(rooms) = ROOMS.try_read() else {
        return 0;
    };
    count_unread_excluding_room(&rooms.map, &rooms.notification_modes, current.as_ref())
        + count_unread_dms(&rooms)
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

    // Send our favicon to the parent shell so the browser tab shows the River
    // logo instead of the default Freenet favicon. The shell accepts this via
    // the __freenet_shell__ postMessage bridge.
    send_favicon_to_shell();

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

    /// Source-grep pin (freenet/river#500 review C2): every cross-room total
    /// must go through the MODE-AWARE sum.
    ///
    /// `count_total_unread_messages` (the tab title) and
    /// `count_unread_behind_rooms_panel` (the mobile hamburger) are one line
    /// each, and reverting either to `rooms.map.values().map(...).sum()`
    /// re-creates the reported bug — a muted room inflating the total —
    /// while leaving the whole Rust suite and the whole browser suite green.
    /// The hamburger is covered end-to-end by `room-unread-badge.spec.ts`'s
    /// sum check; the title cannot be, because
    /// `title-unread-badge.spec.ts` shows the visible→hidden transition marks
    /// every room read, so the fixture's title never carries an `(N)`.
    #[test]
    fn every_cross_room_total_is_mode_aware() {
        let source = include_str!("document_title.rs");
        let prod = &source[..source
            .find("#[cfg(test)]\nmod tests {")
            .expect("document_title.rs should have a `#[cfg(test)] mod tests` block")];
        // Whole-line comments only, so prose cannot satisfy the search and a
        // `//` inside a string literal cannot truncate a real call away.
        let code: String = prod
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let squashed: String = code.chars().filter(|c| !c.is_whitespace()).collect();

        for (total, marker) in [
            (
                "count_total_unread_messages",
                "fncount_total_unread_messages()",
            ),
            (
                "count_unread_behind_rooms_panel",
                "fncount_unread_behind_rooms_panel()",
            ),
        ] {
            let start = squashed.find(marker).unwrap_or_else(|| {
                panic!("`{total}` is gone — re-anchor this pin, do not delete it")
            });
            // Whole-line comments (including `///`) were filtered out above, so
            // the next `pub fn` is the only usable end marker.
            let body = &squashed[start..];
            let end = body.find("pubfn").unwrap_or(body.len());
            assert!(
                body[..end]
                    .contains("count_unread_excluding_room(&rooms.map,&rooms.notification_modes,"),
                "`{total}` must sum through `count_unread_excluding_room` WITH \
                 the notification modes — summing the mode-blind per-room count \
                 is freenet/river#500"
            );
        }
    }

    use crate::constants::ROOM_CONTRACT_WASM;
    use crate::room_data::{NotificationMode, RoomData};
    use crate::util::to_cbor_vec;
    use ed25519_dalek::{SigningKey, VerifyingKey};
    use freenet_stdlib::prelude::{ContractCode, ContractKey, Parameters};
    use river_core::room_state::message::{AuthorizedMessageV1, MessageV1, RoomMessageBody};
    use river_core::room_state::ChatRoomParametersV1;
    use river_core::ChatRoomStateV1;
    use std::collections::HashMap;
    use std::time::{Duration, UNIX_EPOCH};

    /// Build a signed message from `author_sk` with an arbitrary body,
    /// distinct per `n` (the timestamp orders the buffer).
    fn msg_with_body(
        author_sk: &SigningKey,
        owner_vk: &VerifyingKey,
        n: u64,
        content: RoomMessageBody,
    ) -> AuthorizedMessageV1 {
        AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: MemberId::from(owner_vk),
                author: MemberId::from(&author_sk.verifying_key()),
                content,
                time: UNIX_EPOCH + Duration::from_secs(n),
            },
            author_sk,
        )
    }

    /// Build a signed display (text) message from `author_sk`, distinct per `n`.
    fn msg(author_sk: &SigningKey, owner_vk: &VerifyingKey, n: u64) -> AuthorizedMessageV1 {
        msg_with_body(
            author_sk,
            owner_vk,
            n,
            RoomMessageBody::public(format!("message {n}")),
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
            self_nickname: None,
            previous_contract_key: None,
            invitation_secrets: HashMap::new(),
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
    fn event_messages_are_not_counted() {
        // freenet/river#500: room-event messages ("X joined the room",
        // CONTENT_TYPE_EVENT) are ambient activity, not something to read —
        // they must not inflate the unread badge. A "3" badge that is 2
        // messages plus a join overstates what's waiting for the user.
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let messages = vec![
            msg(&owner_sk, &owner_vk, 1),
            msg_with_body(&owner_sk, &owner_vk, 2, RoomMessageBody::join_event()),
            msg(&owner_sk, &owner_vk, 3),
        ];
        let rd = room(self_sk, owner_vk, messages, None);
        // 3 other-authored messages, 1 is a join event → 2 unread.
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
        // `display_messages()` filtered to other authors AND to non-event
        // messages — the ONE deliberate divergence from `display_messages()`
        // is that room events render in the conversation but never count as
        // unread (freenet/river#500). If the predicates drift further, this
        // fails.
        let (self_sk, self_vk) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let self_id: MemberId = (&self_vk).into();
        let messages = vec![
            msg(&owner_sk, &owner_vk, 1),
            msg(&owner_sk, &owner_vk, 2),
            msg(&self_sk, &owner_vk, 3),
            msg_with_body(&owner_sk, &owner_vk, 4, RoomMessageBody::join_event()),
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
            .filter(|m| m.message.author != self_id && !m.message.content.is_event())
            .count();
        assert_eq!(count_unread_in_room_data(&rd), expected);
    }

    #[test]
    fn excluding_the_current_room_omits_its_unread() {
        // The mobile hamburger badge counts unread in OTHER rooms only —
        // the current room's messages are on screen and marked read on
        // open, so including them would make the badge flicker.
        let (self_sk, _) = keypair();
        let (owner_a_sk, owner_a_vk) = keypair();
        let (owner_b_sk, owner_b_vk) = keypair();
        let room_a = room(
            self_sk.clone(),
            owner_a_vk,
            vec![
                msg(&owner_a_sk, &owner_a_vk, 1),
                msg(&owner_a_sk, &owner_a_vk, 2),
            ],
            None,
        );
        let room_b = room(
            self_sk,
            owner_b_vk,
            vec![msg(&owner_b_sk, &owner_b_vk, 1)],
            None,
        );
        let mut map = HashMap::new();
        map.insert(owner_a_vk, room_a);
        map.insert(owner_b_vk, room_b);
        let modes = HashMap::new();

        // Excluding room A (2 unread) leaves only room B's single unread.
        assert_eq!(
            count_unread_excluding_room(&map, &modes, Some(&owner_a_vk)),
            1
        );
        // No current room (welcome screen): every room counts.
        assert_eq!(count_unread_excluding_room(&map, &modes, None), 3);
        // Excluding a key not in the map changes nothing.
        let (_, other_vk) = keypair();
        assert_eq!(
            count_unread_excluding_room(&map, &modes, Some(&other_vk)),
            3
        );
    }

    /// Build a message from `author_sk` that @mentions `mention_of`.
    fn mention_msg(
        author_sk: &SigningKey,
        owner_vk: &VerifyingKey,
        n: u64,
        mention_of: MemberId,
    ) -> AuthorizedMessageV1 {
        let text = format!(
            "hey {}!",
            river_core::mention::encode_mention(mention_of, "Someone")
        );
        msg_with_body(author_sk, owner_vk, n, RoomMessageBody::public(text))
    }

    #[test]
    fn muted_room_counts_zero_despite_unreads() {
        // freenet/river#500: a Muted room must never badge and never feed
        // the title/hamburger totals, however many unreads it holds.
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let messages = vec![
            msg(&owner_sk, &owner_vk, 1),
            msg(&owner_sk, &owner_vk, 2),
            msg(&owner_sk, &owner_vk, 3),
        ];
        let rd = room(self_sk, owner_vk, messages, None);
        // The messages ARE unread under All…
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::All),
            3
        );
        // …but Muted always counts zero.
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::Muted),
            0
        );
    }

    #[test]
    fn all_mode_matches_the_plain_count() {
        // `All` (the default) must be exactly the historical behaviour —
        // the same value `count_unread_in_room_data` returns.
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let messages = vec![
            msg(&owner_sk, &owner_vk, 1),
            msg(&owner_sk, &owner_vk, 2),
            msg(&owner_sk, &owner_vk, 3),
        ];
        let marker = messages[0].id();
        let rd = room(self_sk, owner_vk, messages, Some(marker));
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::All),
            count_unread_in_room_data(&rd)
        );
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::All),
            2
        );
    }

    #[test]
    fn mentions_mode_counts_only_qualifying_messages() {
        // freenet/river#500: a MentionsAndReplies room badges the count of
        // unread messages that @mention the user or reply to one of their
        // messages — the same predicate that gates browser notifications.
        let (self_sk, self_vk) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let self_id: MemberId = (&self_vk).into();

        // Self-authored target for the reply (never unread itself).
        let target = msg_with_body(
            &self_sk,
            &owner_vk,
            1,
            RoomMessageBody::public("my message".to_string()),
        );
        let messages = vec![
            target.clone(),
            // Plain other-authored message: unread under All, but not a
            // mention/reply.
            msg(&owner_sk, &owner_vk, 2),
            // @mention of self.
            mention_msg(&owner_sk, &owner_vk, 3, self_id),
            // Reply to self's message.
            msg_with_body(
                &owner_sk,
                &owner_vk,
                4,
                RoomMessageBody::reply(
                    "agreed".to_string(),
                    target.id(),
                    "Me".to_string(),
                    "my message".to_string(),
                ),
            ),
        ];
        let rd = room(self_sk, owner_vk, messages, None);
        // All mode sees 3 unread (plain + mention + reply)…
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::All),
            3
        );
        // …MentionsAndReplies counts only the mention and the reply.
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::MentionsAndReplies),
            2
        );
    }

    #[test]
    fn mentions_mode_with_no_qualifying_messages_counts_zero() {
        // Zero qualifying messages means NO badge — even when other unread
        // messages exist (they'd notify nobody, so they don't badge).
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let (_, third_vk) = keypair();
        let messages = vec![
            msg(&owner_sk, &owner_vk, 1),
            // A mention of someone ELSE doesn't qualify.
            mention_msg(&owner_sk, &owner_vk, 2, (&third_vk).into()),
        ];
        let rd = room(self_sk, owner_vk, messages, None);
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::All),
            2
        );
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::MentionsAndReplies),
            0
        );
    }

    #[test]
    fn mentions_mode_ignores_a_mention_before_the_last_read_marker() {
        // The mention count is scoped to the unread tail: a mention the user
        // has already read must not keep the badge lit. (This pins the
        // COUNT, not the scan bound — the scan bound is pinned by
        // `mention_count_memo_skips_recompute_until_inputs_change`.)
        let (self_sk, self_vk) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let self_id: MemberId = (&self_vk).into();
        let messages = vec![
            mention_msg(&owner_sk, &owner_vk, 1, self_id), // read mention
            msg(&owner_sk, &owner_vk, 2),                  // unread, plain
        ];
        let marker = messages[0].id();
        let rd = room(self_sk, owner_vk, messages, Some(marker));
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::MentionsAndReplies),
            0
        );
    }

    #[test]
    fn mentions_mode_with_an_evicted_marker_counts_the_whole_buffer() {
        // When `last_read_message_id` has aged out of the bounded buffer the
        // tail falls back to everything (the documented `start = 0` case) —
        // in mentions mode that means every qualifying message in the
        // buffer, not zero. This is also the state a mentions-mode room
        // typically lives in, since it is set to mentions-only precisely so
        // the user stops opening it.
        let (self_sk, self_vk) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let self_id: MemberId = (&self_vk).into();
        let evicted = msg(&owner_sk, &owner_vk, 99);
        let messages = vec![
            mention_msg(&owner_sk, &owner_vk, 1, self_id),
            msg(&owner_sk, &owner_vk, 2),
            mention_msg(&owner_sk, &owner_vk, 3, self_id),
        ];
        let rd = room(self_sk, owner_vk, messages, Some(evicted.id()));
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::MentionsAndReplies),
            2
        );
    }

    #[test]
    fn mentions_mode_skips_a_reply_whose_target_left_the_buffer() {
        // Reply authorship can only be confirmed against messages still in
        // the buffer. With the target evicted we cannot tell whether it was
        // the user's, so the shared predicate conservatively says no — a
        // miss rather than a badge for someone else's conversation.
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let target = msg_with_body(
            &self_sk,
            &owner_vk,
            1,
            RoomMessageBody::public("my old message".to_string()),
        );
        // `target` is deliberately NOT placed in the room's buffer.
        let messages = vec![msg_with_body(
            &owner_sk,
            &owner_vk,
            2,
            RoomMessageBody::reply(
                "re".to_string(),
                target.id(),
                "Me".to_string(),
                "my old message".to_string(),
            ),
        )];
        let rd = room(self_sk, owner_vk, messages, None);
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::All),
            1
        );
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::MentionsAndReplies),
            0
        );
    }

    /// Build a private (sealed) message body under `secret` at `version`.
    fn private_msg(
        author_sk: &SigningKey,
        owner_vk: &VerifyingKey,
        n: u64,
        text: String,
        secret: &[u8; 32],
        version: u32,
    ) -> AuthorizedMessageV1 {
        use river_core::room_state::content::{TextContentV1, CONTENT_TYPE_TEXT};
        let (ciphertext, nonce) = crate::util::ecies::encrypt_with_symmetric_key(
            secret,
            &TextContentV1::new(text).encode(),
        );
        msg_with_body(
            author_sk,
            owner_vk,
            n,
            RoomMessageBody::private(CONTENT_TYPE_TEXT, 1, ciphertext, nonce, version),
        )
    }

    #[test]
    fn mentions_mode_counts_undecryptable_private_messages() {
        // freenet/river#500 fail-safe: `decrypt_message_content` returns a
        // PLACEHOLDER (not an error) when the secret is missing, so a
        // mention check against it is always false. Counting undecryptable
        // messages is the only way a real mention isn't silently hidden.
        //
        // This is the EVERY-COLD-START state for a private room:
        // `RoomData.secrets` is `#[serde(skip)]`, so the map is empty until
        // `repopulate_secrets_from_state` rehydrates it.
        let (self_sk, self_vk) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let self_id: MemberId = (&self_vk).into();
        let secret = [7u8; 32];
        let mention_text = format!(
            "hey {}!",
            river_core::mention::encode_mention(self_id, "Me")
        );
        let messages = vec![
            private_msg(&owner_sk, &owner_vk, 1, mention_text, &secret, 1),
            private_msg(
                &owner_sk,
                &owner_vk,
                2,
                "just chatting".to_string(),
                &secret,
                1,
            ),
        ];
        let mut rd = room(self_sk, owner_vk, messages, None);

        // (a) Secrets not yet available: neither body can be inspected, so
        // both count. Non-zero is the point — a silent 0 would hide a real
        // mention behind an empty badge.
        assert!(rd.secrets.is_empty());
        let unresolved =
            count_unread_in_room_data_with_mode(&rd, NotificationMode::MentionsAndReplies);
        assert_eq!(
            unresolved, 2,
            "undecryptable private messages must count, not silently read as 'not a mention'"
        );

        // (b) Once the secret arrives the count resolves DOWN to the true
        // mention count. (Inserting a secret changes `secrets.len()`, which
        // is part of the memo fingerprint, so the memo must not serve the
        // stale 2 here — that is also a live guard on the fingerprint.)
        rd.secrets.insert(1, secret);
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::MentionsAndReplies),
            1,
            "with secrets available the count must resolve to the true mention count"
        );
    }

    /// freenet/river#500 review: the fail-safe is bounded to the COLD-START
    /// window, so a body that stays unreadable with secrets in hand does not
    /// count.
    ///
    /// Three states reach this, and none of them is reliably transient: a
    /// version older than everything held (a rotation the user joined after), a
    /// version newer than everything held (which need NOT be a blob in flight —
    /// a member offline across a rotation, or one whose blobs were pruned on
    /// removal, never receives it), and a secret present at the right version
    /// that does not decrypt (only overwritten if the contract carries an
    /// owner-signed blob for this member at that version).
    ///
    /// Counting any of them would put a number on the badge, the title and the
    /// hamburger that no amount of reading can clear — the #500 symptom in a
    /// narrower configuration.
    ///
    /// Each case gets its OWN room key: the memo is a `thread_local` keyed by
    /// `owner_vk`, and `cargo test -- --test-threads=1` shares it across tests.
    #[test]
    fn mentions_mode_does_not_count_unreadable_bodies_once_secrets_are_held() {
        let (self_sk, self_vk) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let self_id: MemberId = (&self_vk).into();
        let real_secret = [7u8; 32];
        let wrong_secret = [8u8; 32];
        let mention_text = format!(
            "hey {}!",
            river_core::mention::encode_mention(self_id, "Me")
        );

        // (a) A secret PRESENT at the message's version but WRONG. Premise
        // first: the wrong key must genuinely fail to decrypt, or this is
        // measuring the missing-secret path instead.
        let mut wrong_key_room = room(
            self_sk.clone(),
            owner_vk,
            vec![private_msg(
                &owner_sk,
                &owner_vk,
                1,
                mention_text.clone(),
                &real_secret,
                1,
            )],
            None,
        );
        wrong_key_room.secrets.insert(1, wrong_secret);
        assert!(
            crate::components::conversation::try_decrypt_message_content(
                &wrong_key_room.room_state.recent_messages.messages[0]
                    .message
                    .content,
                &wrong_key_room.secrets,
            )
            .is_none(),
            "premise: the wrong secret must not decrypt the body"
        );
        assert_eq!(
            count_unread_in_room_data_with_mode(
                &wrong_key_room,
                NotificationMode::MentionsAndReplies
            ),
            0,
            "a wrong key at a held version is not reliably transient, so it \
             must not count"
        );

        // (b) A version OLDER than everything held: joined after the rotation.
        let (other_sk, _) = keypair();
        let (owner2_sk, owner2_vk) = keypair();
        let mut old_room = room(
            other_sk,
            owner2_vk,
            vec![private_msg(
                &owner2_sk,
                &owner2_vk,
                1,
                "from before you joined".to_string(),
                &real_secret,
                1,
            )],
            None,
        );
        old_room.secrets.insert(2, [4u8; 32]);
        assert_eq!(
            count_unread_in_room_data_with_mode(&old_room, NotificationMode::MentionsAndReplies),
            0,
            "a permanently unreadable older-version body must not count"
        );

        // (c) A version NEWER than everything held. This one LOOKS transient,
        // and the first version of the fix treated it as such — but a member
        // offline across a rotation, or one whose blobs were pruned on removal,
        // never receives it, so counting it is unbounded.
        let (third_sk, _) = keypair();
        let (owner3_sk, owner3_vk) = keypair();
        let mut new_room = room(
            third_sk,
            owner3_vk,
            vec![private_msg(
                &owner3_sk,
                &owner3_vk,
                1,
                "just rotated".to_string(),
                &real_secret,
                9,
            )],
            None,
        );
        new_room.secrets.insert(2, [4u8; 32]);
        assert_eq!(
            count_unread_in_room_data_with_mode(&new_room, NotificationMode::MentionsAndReplies),
            0,
            "a newer-version body is not guaranteed to arrive, so it must not \
             count indefinitely"
        );
    }

    /// The memo must notice a secret being replaced AT THE SAME VERSION.
    ///
    /// `repopulate_secrets_from_state` overwrites a wrong invitation-supplied
    /// secret with the authoritative owner-signed one at the same version, so
    /// `secrets.len()` is unchanged — a length-only fingerprint would keep
    /// serving the count computed against the wrong key.
    #[test]
    fn a_same_version_secret_overwrite_invalidates_the_memo() {
        let (self_sk, self_vk) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let self_id: MemberId = (&self_vk).into();
        let real_secret = [1u8; 32];
        let wrong_secret = [2u8; 32];
        let mention_text = format!(
            "hey {}!",
            river_core::mention::encode_mention(self_id, "Me")
        );
        let mut rd = room(
            self_sk,
            owner_vk,
            vec![private_msg(
                &owner_sk,
                &owner_vk,
                1,
                mention_text,
                &real_secret,
                1,
            )],
            None,
        );

        rd.secrets.insert(1, wrong_secret);
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::MentionsAndReplies),
            0,
            "unreadable with secrets in hand, so not counted"
        );

        rd.secrets.insert(1, real_secret);
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::MentionsAndReplies),
            1,
            "the authoritative secret arrived at the SAME version and the body \
             turns out to be a mention — a `secrets.len()` fingerprint would \
             still be serving the stale 0"
        );
    }

    #[test]
    fn all_mode_is_unaffected_by_undecryptable_private_messages() {
        // The fail-safe is scoped to mentions mode: `All` never inspects
        // content, so undecryptable bodies count exactly like any other
        // unread message (no double-counting, no behaviour change).
        let (self_sk, _) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let secret = [9u8; 32];
        let messages = vec![
            private_msg(&owner_sk, &owner_vk, 1, "one".to_string(), &secret, 3),
            private_msg(&owner_sk, &owner_vk, 2, "two".to_string(), &secret, 3),
        ];
        let rd = room(self_sk, owner_vk, messages, None);
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::All),
            2
        );
    }

    /// Number of full (cache-missing) mention scans on this thread.
    fn mention_scans() -> usize {
        MENTION_SCAN_COUNT.with(|c| c.get())
    }

    #[test]
    fn mention_count_memo_skips_recompute_until_inputs_change() {
        // The mentions scan decrypts bodies over what is routinely the whole
        // message buffer, and runs up to three times per ROOMS write (rail
        // badge, hamburger badge, document title). The memo is what keeps
        // that affordable — this pins that it actually skips recompute, and
        // that it still invalidates when the room changes.
        //
        // Deltas (not absolute counts) and a per-test random room key keep
        // this correct whether or not the harness shares a thread between
        // tests.
        let (self_sk, self_vk) = keypair();
        let (owner_sk, owner_vk) = keypair();
        let self_id: MemberId = (&self_vk).into();
        let messages = vec![
            mention_msg(&owner_sk, &owner_vk, 1, self_id),
            msg(&owner_sk, &owner_vk, 2),
        ];
        let mut rd = room(self_sk, owner_vk, messages, None);

        let baseline = mention_scans();
        let first = count_unread_in_room_data_with_mode(&rd, NotificationMode::MentionsAndReplies);
        assert_eq!(first, 1);
        assert_eq!(
            mention_scans(),
            baseline + 1,
            "the first call must actually compute"
        );

        // Two more reads with identical inputs — the other two always-mounted
        // consumers — must be served from the memo.
        for _ in 0..2 {
            assert_eq!(
                count_unread_in_room_data_with_mode(&rd, NotificationMode::MentionsAndReplies),
                first
            );
        }
        assert_eq!(
            mention_scans(),
            baseline + 1,
            "memo recomputed despite unchanged inputs"
        );

        // A new message must invalidate the memo and change the answer.
        rd.room_state
            .recent_messages
            .messages
            .push(mention_msg(&owner_sk, &owner_vk, 3, self_id));
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::MentionsAndReplies),
            2,
            "memo served a stale count after a new message arrived"
        );
        assert_eq!(
            mention_scans(),
            baseline + 2,
            "a changed room must trigger exactly one recompute"
        );

        // Marking the room read must also invalidate it.
        let latest = rd.room_state.recent_messages.messages.last().unwrap().id();
        rd.last_read_message_id = Some(latest);
        assert_eq!(
            count_unread_in_room_data_with_mode(&rd, NotificationMode::MentionsAndReplies),
            0,
            "memo served a stale count after the last-read marker moved"
        );
        // …by RECOMPUTING. Without this leg, an implementation that recomputed
        // on every call would satisfy every assertion above — the memo would be
        // doing nothing and the test would still be green.
        assert_eq!(
            mention_scans(),
            baseline + 3,
            "the read-marker move must have caused exactly one more recompute"
        );
    }

    #[test]
    fn totals_sum_the_same_per_mode_values() {
        // freenet/river#500: the document-title total and the mobile
        // hamburger total must sum the SAME mode-aware per-room values the
        // room-list badge shows, so every surface agrees.
        let (self_sk, self_vk) = keypair();
        let self_id: MemberId = (&self_vk).into();
        let (owner_a_sk, owner_a_vk) = keypair(); // All (default): 2 unread
        let (owner_b_sk, owner_b_vk) = keypair(); // MentionsAndReplies: 1 of 3 qualifies
        let (owner_c_sk, owner_c_vk) = keypair(); // Muted: 5 unread, counts 0

        let room_a = room(
            self_sk.clone(),
            owner_a_vk,
            vec![
                msg(&owner_a_sk, &owner_a_vk, 1),
                msg(&owner_a_sk, &owner_a_vk, 2),
            ],
            None,
        );
        let room_b = room(
            self_sk.clone(),
            owner_b_vk,
            vec![
                msg(&owner_b_sk, &owner_b_vk, 1),
                mention_msg(&owner_b_sk, &owner_b_vk, 2, self_id),
                msg(&owner_b_sk, &owner_b_vk, 3),
            ],
            None,
        );
        let room_c = room(
            self_sk,
            owner_c_vk,
            (1..=5).map(|n| msg(&owner_c_sk, &owner_c_vk, n)).collect(),
            None,
        );
        let mut map = HashMap::new();
        map.insert(owner_a_vk, room_a);
        map.insert(owner_b_vk, room_b);
        map.insert(owner_c_vk, room_c);

        let mut modes = HashMap::new();
        // Room A has no entry → defaults to All, like the notification gate.
        modes.insert(owner_b_vk, NotificationMode::MentionsAndReplies);
        modes.insert(owner_c_vk, NotificationMode::Muted);

        // Title total (no exclusion): 2 (All) + 1 (mention) + 0 (muted).
        assert_eq!(count_unread_excluding_room(&map, &modes, None), 3);
        // Hamburger total with room A open: 1 + 0.
        assert_eq!(
            count_unread_excluding_room(&map, &modes, Some(&owner_a_vk)),
            1
        );
        // Hamburger total with the mentions room open: 2 + 0.
        assert_eq!(
            count_unread_excluding_room(&map, &modes, Some(&owner_b_vk)),
            2
        );
        // Excluding the MUTED room changes nothing — it was already
        // contributing 0, so opening it can't move the hamburger total.
        assert_eq!(
            count_unread_excluding_room(&map, &modes, Some(&owner_c_vk)),
            3
        );
    }

    /// Build a direct message. The counters never verify signatures, so
    /// any signature over any bytes suffices.
    fn dm(
        sender: MemberId,
        recipient: MemberId,
        ts: u64,
        signer: &SigningKey,
    ) -> river_core::room_state::direct_messages::AuthorizedDirectMessage {
        use ed25519_dalek::Signer;
        river_core::room_state::direct_messages::AuthorizedDirectMessage {
            message: river_core::room_state::direct_messages::DirectMessage {
                sender,
                recipient,
                timestamp: ts,
                ciphertext: vec![1, 2, 3],
            },
            sender_signature: signer.sign(b"test-dm"),
        }
    }

    #[test]
    fn dm_unread_counts_inbound_and_respects_last_seen() {
        let (self_sk, self_vk) = keypair();
        let (_owner_sk, owner_vk) = keypair();
        let (peer_sk, peer_vk) = keypair();
        let self_id: MemberId = (&self_vk).into();
        let peer_id: MemberId = (&peer_vk).into();

        let mut rd = room(self_sk, owner_vk, vec![], None);
        rd.room_state.direct_messages.messages = vec![
            dm(peer_id, self_id, 100, &peer_sk),
            dm(peer_id, self_id, 200, &peer_sk),
            // Outbound (self → peer): never unread.
            dm(self_id, peer_id, 300, &peer_sk),
        ];
        let mut map = HashMap::new();
        map.insert(owner_vk, rd);

        // No last-seen: both inbound messages count, outbound doesn't.
        assert_eq!(
            count_unread_dms_with(&map, &HashMap::new(), &HashMap::new()),
            2
        );
        // Seen up to ts=100: only the ts=200 inbound counts.
        let mut seen = HashMap::new();
        seen.insert((owner_vk, peer_id), 100u64);
        assert_eq!(count_unread_dms_with(&map, &seen, &HashMap::new()), 1);
    }

    #[test]
    fn dm_unread_skips_hidden_threads_until_revived() {
        // A hidden (archived) thread is invisible in the DM rail, so its
        // unread must not feed the badge/title tallies — until a message
        // STRICTLY newer than hidden_at_ts revives it (the rail's
        // `is_thread_hidden_for` strict-<= rule).
        let (self_sk, self_vk) = keypair();
        let (_owner_sk, owner_vk) = keypair();
        let (peer_sk, peer_vk) = keypair();
        let self_id: MemberId = (&self_vk).into();
        let peer_id: MemberId = (&peer_vk).into();

        let mut rd = room(self_sk, owner_vk, vec![], None);
        rd.room_state.direct_messages.messages = vec![dm(peer_id, self_id, 100, &peer_sk)];
        let mut map = HashMap::new();
        map.insert(owner_vk, rd);

        let mut hidden = HashMap::new();
        hidden.insert(
            (owner_vk, peer_id),
            river_core::chat_delegate::HiddenDmThreadEntry {
                room_owner_vk: owner_vk.to_bytes(),
                peer: peer_id,
                hidden_at_ts: 100,
            },
        );
        // Hidden at the newest message's ts (<=): thread invisible → 0.
        assert_eq!(count_unread_dms_with(&map, &HashMap::new(), &hidden), 0);

        // A strictly newer inbound message revives the thread: both its
        // unread messages count again (matching the rail badge).
        map.get_mut(&owner_vk)
            .unwrap()
            .room_state
            .direct_messages
            .messages
            .push(dm(peer_id, self_id, 150, &peer_sk));
        assert_eq!(count_unread_dms_with(&map, &HashMap::new(), &hidden), 2);
    }

    #[test]
    fn dm_unread_outbound_message_revives_hidden_thread() {
        // The revival clock counts BOTH directions (the rail's
        // `last_any_ts`): replying into a hidden thread makes it visible
        // again, so its older unread inbound must count again too.
        let (self_sk, self_vk) = keypair();
        let (_owner_sk, owner_vk) = keypair();
        let (peer_sk, peer_vk) = keypair();
        let self_id: MemberId = (&self_vk).into();
        let peer_id: MemberId = (&peer_vk).into();

        let mut rd = room(self_sk, owner_vk, vec![], None);
        rd.room_state.direct_messages.messages = vec![
            dm(peer_id, self_id, 90, &peer_sk),  // inbound, unread
            dm(self_id, peer_id, 150, &peer_sk), // outbound, after hide
        ];
        let mut map = HashMap::new();
        map.insert(owner_vk, rd);

        let mut hidden = HashMap::new();
        hidden.insert(
            (owner_vk, peer_id),
            river_core::chat_delegate::HiddenDmThreadEntry {
                room_owner_vk: owner_vk.to_bytes(),
                peer: peer_id,
                hidden_at_ts: 90,
            },
        );
        // last_any_ts = 150 (outbound) > hidden_at 90 → revived → the
        // ts=90 inbound counts.
        assert_eq!(count_unread_dms_with(&map, &HashMap::new(), &hidden), 1);
    }

    #[test]
    fn dm_unread_ignores_third_party_messages() {
        // DMs between two OTHER members (present in replicated room
        // state) must contribute nothing to the local user's count.
        let (self_sk, _self_vk) = keypair();
        let (_owner_sk, owner_vk) = keypair();
        let (peer_sk, peer_vk) = keypair();
        let (_other_sk, other_vk) = keypair();
        let peer_id: MemberId = (&peer_vk).into();
        let other_id: MemberId = (&other_vk).into();

        let mut rd = room(self_sk, owner_vk, vec![], None);
        rd.room_state.direct_messages.messages = vec![dm(peer_id, other_id, 100, &peer_sk)];
        let mut map = HashMap::new();
        map.insert(owner_vk, rd);

        assert_eq!(
            count_unread_dms_with(&map, &HashMap::new(), &HashMap::new()),
            0
        );
    }
}
