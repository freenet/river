//! "Direct Messages" section in the left rail (under Rooms).
//!
//! Lists every DM thread the local user has, across ALL rooms — not just
//! the currently-selected room — so a user focused on Room A can still
//! see they have unread DMs from a member of Room B. Replaces the
//! previous "Direct Messages" button buried in the Members panel, which
//! was confusing per zorolin's 2026-05-16 feedback in the official room.
//!
//! Click a thread → opens [`DmThreadModal`] for that (room, peer). The
//! thread modal handles the actual conversation; this component is
//! purely a launcher.
//!
//! Each row also carries a rollover **Archive** ✕ button (issue #266 —
//! the previous "Hide" button in the modal header sat next to the close
//! ✕ and was repeatedly mistaken for it). On desktop the button is
//! hidden until the row is hovered/focused; on mobile it's dimmed always-
//! visible so it's tappable without a hover state. Archived threads stay
//! out of the rail until either side sends a new DM. The "Archived (N)"
//! link at the bottom of the section lists currently-archived threads
//! and offers per-row Un-archive, closing #266.
//!
//! Hidden when empty so the rail doesn't show an empty section on first
//! load. Sorts unread threads first, then by most-recent message time.
//!
//! Terminology note: the on-wire data shape and the internal Rust APIs
//! still use the original "hide" / `hidden_threads` / `hide_dm_thread`
//! names — renaming them would force a delegate migration for zero
//! functional benefit. The user-facing surface is "Archive" everywhere
//! visible.

use crate::components::app::chat_delegate::{hide_dm_thread, unhide_dm_thread};
use crate::components::app::ROOMS;
use crate::components::direct_messages::{
    is_thread_hidden_for, open_dm_thread, DM_LAST_SEEN, HIDDEN_DM_THREADS,
};
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::prelude::*;
use dioxus_free_icons::{
    icons::fa_solid_icons::{FaEnvelope, FaXmark},
    Icon,
};
use ed25519_dalek::VerifyingKey;
use river_core::chat_delegate::HiddenDmThreadEntry;
use river_core::room_state::member::MemberId;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-(room, peer) "Archived — Undo" toast state. Cleared automatically
/// when the next render after `expires_at_ms` happens (the rail re-runs
/// on every `HIDDEN_DM_THREADS` write). Kept module-private — the rail
/// is the only surface that creates toasts and the only surface that
/// consumes them.
#[derive(Clone, PartialEq, Debug)]
struct ArchiveToast {
    room: VerifyingKey,
    peer: MemberId,
    /// Display label so the toast can still render its `{peer_nickname}` after
    /// the underlying row has disappeared from `ROOMS` (e.g. room churn).
    peer_nickname: String,
    /// `Date.now()`-style milliseconds at which the toast should disappear.
    expires_at_ms: u64,
}

/// Single most-recent toast. We don't queue them — back-to-back archives
/// just refresh the toast with the most recent action, which matches
/// Gmail/WhatsApp behaviour and keeps the UX simple.
static ARCHIVE_TOAST: GlobalSignal<Option<ArchiveToast>> = Global::new(|| None);

/// How long the "Archived — Undo" toast stays visible. ~5s matches the
/// "destructive-undo affordance" timing used elsewhere (Gmail's archive,
/// Signal's mark-as-unread).
const ARCHIVE_TOAST_DURATION_MS: u64 = 5_000;

#[component]
pub fn DmRailSection() -> Element {
    let threads = use_memo(move || build_view().unwrap_or_default());
    let threads_value = threads.read().clone();

    // Reading the toast signal here subscribes the rail to its writes so
    // a `set(None)` from the timeout reaction re-renders this component
    // and the toast disappears.
    //
    // `try_read` (not `read`) is the repo-standard pattern (AGENTS.md
    // "Dioxus WASM Signal Safety Rules") — on Firefox/mobile the
    // `ARCHIVE_TOAST.write()` Drop handler fires subscriber notifications
    // synchronously, which could re-enter this read while the write
    // guard's RefCell borrow is still held. `try_read` returns `Err`
    // instead of panicking; on contention we treat the toast as absent
    // for THIS render and the next clean signal write repaints us. The
    // P1 multi-model review finding pinned this regression — Codex
    // flagged it before merge.
    let toast = ARCHIVE_TOAST.try_read().ok().and_then(|g| g.clone());

    // Archived count for the "Archived (N)" link.
    //
    // The naive implementation (count `HIDDEN_DM_THREADS.len()`) over-
    // reports: a hidden entry whose thread has since been revived by
    // a strictly-newer message is correctly shown on the rail but
    // stays in `HIDDEN_DM_THREADS` until its `hidden_at_ts` is
    // overwritten by the next archive click. The Codex P2 review
    // finding flagged this — the count must apply the same revival
    // predicate as `build_view`'s `filter_rail_entries`.
    //
    // `current_archived_count` walks ROOMS to compute the per-pair
    // `last_any_ts` and runs `count_currently_archived`. On contention
    // (any `try_read` failing) we fall back to 0 so the link disappears
    // for THIS render — a brief mid-write flicker is preferable to
    // showing a stale count.
    let archived_count = use_memo(move || current_archived_count().unwrap_or(0));
    let archived_count = *archived_count.read();

    let mut archived_panel_open: Signal<bool> = use_signal(|| false);

    // If there's nothing to show in the rail AND no archive entries AND
    // no active toast, render nothing — keeps the rail visually quiet on
    // first load.
    if threads_value.is_empty() && archived_count == 0 && toast.is_none() {
        return rsx! {};
    }

    let archive_label = format!("Archived ({})", archived_count);
    let panel_is_open = *archived_panel_open.read();

    rsx! {
        div { class: "px-4 py-2 flex items-center justify-between border-t border-border mt-2",
            h2 { class: "text-sm font-semibold text-text-muted uppercase tracking-wide flex items-center gap-2",
                Icon { width: 14, height: 14, icon: FaEnvelope }
                span { "Direct Messages" }
            }
        }
        if !threads_value.is_empty() {
            ul { class: "px-2 py-1 space-y-0.5",
                for entry in threads_value.iter() {
                    DmRailRow { key: "{entry.room:?}_{entry.peer}", entry: entry.clone() }
                }
            }
        }
        if archived_count > 0 {
            div { class: "px-3 pb-2",
                button {
                    class: "text-xs text-text-muted hover:text-text underline-offset-2 hover:underline transition-colors",
                    onclick: move |_| {
                        let next = !*archived_panel_open.peek();
                        archived_panel_open.set(next);
                    },
                    "{archive_label}"
                }
                if panel_is_open {
                    ArchivedThreadsPanel {}
                }
            }
        }
        if let Some(t) = toast.as_ref() {
            ArchiveToastView { toast: t.clone() }
        }
    }
}

#[component]
fn DmRailRow(entry: DmRailEntry) -> Element {
    let room = entry.room;
    let peer = entry.peer;
    let nickname = entry.peer_nickname.clone();
    let last_any_ts = entry.last_any_ts;
    let click = move |_| {
        open_dm_thread(room, peer);
    };

    // `group` + `group-hover:opacity-100` keeps the ✕ off-screen at rest
    // on desktop; on mobile (`<md`) it stays dimmed but visible so the
    // hover-only affordance doesn't strand touch users. `group-focus-within`
    // mirrors hover for keyboard users tab-stopping into the row.
    let archive_click = move |evt: Event<MouseData>| {
        // The archive button is a sibling of the row's "open thread"
        // button (not nested inside it), so the row click handler
        // doesn't fire on its own. `stop_propagation` is defensive
        // belt-and-braces: it costs nothing and protects against a
        // future refactor that wraps the row in a clickable
        // container — without it, archiving from such a container
        // would open the thread on the same click.
        evt.stop_propagation();
        archive_row(room, peer, &nickname, last_any_ts);
    };

    let archive_title =
        "Archive this conversation. It will return if either of you sends a new DM.";

    rsx! {
        li {
            div { class: "group relative w-full",
                button {
                    class: "w-full text-left pl-3 pr-9 py-1.5 rounded-lg text-sm transition-colors text-text hover:bg-surface flex items-center gap-2",
                    onclick: click,
                    div { class: "flex-1 min-w-0",
                        div { class: "truncate text-sm",
                            "{entry.peer_nickname}"
                        }
                        div { class: "truncate text-[10px] text-text-muted",
                            "in {entry.room_name}"
                        }
                    }
                    if entry.unread > 0 {
                        span { class: "ml-2 inline-flex items-center justify-center px-2 py-0.5 rounded-full text-xs font-medium bg-accent text-white",
                            "{entry.unread}"
                        }
                    }
                }
                button {
                    class: "absolute right-1 top-1/2 -translate-y-1/2 p-1 rounded text-text-muted \
                            opacity-40 md:opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 \
                            hover:text-red-400 hover:bg-surface focus:opacity-100 \
                            transition-opacity transition-colors",
                    title: "{archive_title}",
                    "aria-label": "{archive_title}",
                    onclick: archive_click,
                    Icon { width: 12, height: 12, icon: FaXmark }
                }
            }
        }
    }
}

/// Pure helper extracted from `DmRailRow`'s archive ✕ click handler so
/// the toast bookkeeping can be unit-tested without standing up a Dioxus
/// runtime. Returns the toast that `ARCHIVE_TOAST` would be set to, or
/// `None` if `now_ms` was unavailable.
fn build_archive_toast(
    room: VerifyingKey,
    peer: MemberId,
    peer_nickname: &str,
    now_ms: u64,
) -> ArchiveToast {
    ArchiveToast {
        room,
        peer,
        peer_nickname: peer_nickname.to_string(),
        expires_at_ms: now_ms.saturating_add(ARCHIVE_TOAST_DURATION_MS),
    }
}

/// Wire up: archive the (room, peer) thread, schedule the toast, and
/// schedule the auto-dismiss. Called from the ✕ rollover button.
fn archive_row(room: VerifyingKey, peer: MemberId, peer_nickname: &str, last_any_ts: u64) {
    // The `hidden_at_ts` follows the same semantics as the modal-driven
    // hide: capture the most-recent message timestamp (or wall-clock
    // seconds if the thread is empty) so the rail filter's strict-`<=`
    // check revives the thread the moment a fresher message lands.
    let cutoff = if last_any_ts > 0 {
        last_any_ts
    } else {
        unix_now_secs()
    };
    hide_dm_thread(room, peer, cutoff);

    let now_ms = unix_now_ms();
    let toast = build_archive_toast(room, peer, peer_nickname, now_ms);
    let expires_at_ms = toast.expires_at_ms;
    crate::util::defer(move || {
        *ARCHIVE_TOAST.write() = Some(toast);
    });

    // Auto-dismiss: wait `ARCHIVE_TOAST_DURATION_MS`, then clear the
    // toast iff it's still the one we set. Without the "is it still
    // ours" check, a rapid second archive would reset the timer but the
    // FIRST timeout's tick would still cancel the SECOND toast early.
    crate::util::safe_spawn_local(async move {
        crate::util::sleep(crate::util::millis(ARCHIVE_TOAST_DURATION_MS)).await;
        crate::util::defer(move || {
            ARCHIVE_TOAST.with_mut(|cell| {
                if let Some(current) = cell.as_ref() {
                    if current.expires_at_ms == expires_at_ms {
                        *cell = None;
                    }
                }
            });
        });
    });
}

#[component]
fn ArchiveToastView(toast: ArchiveToast) -> Element {
    let toast_room = toast.room;
    let toast_peer = toast.peer;
    let undo = move |_| {
        unhide_dm_thread(toast_room, toast_peer);
        crate::util::defer(move || {
            *ARCHIVE_TOAST.write() = None;
        });
    };
    let dismiss = move |_| {
        crate::util::defer(move || {
            *ARCHIVE_TOAST.write() = None;
        });
    };
    let label = format!("Archived conversation with {}", toast.peer_nickname);
    rsx! {
        // Bottom-center toast. `fixed bottom-4 left-1/2 -translate-x-1/2`
        // positions it independent of the rail's scroll/layout. `z-50`
        // matches the modal stack so it doesn't sit underneath an open
        // DM thread modal.
        div {
            class: "fixed bottom-4 left-1/2 -translate-x-1/2 z-50",
            role: "status",
            "aria-live": "polite",
            div { class: "flex items-center gap-3 bg-panel text-text border border-border rounded-lg shadow-lg px-4 py-2 text-sm",
                span { "{label}" }
                button {
                    class: "text-accent hover:underline font-medium",
                    onclick: undo,
                    "Undo"
                }
                button {
                    class: "text-text-muted hover:text-text px-1",
                    onclick: dismiss,
                    "aria-label": "Dismiss",
                    Icon { width: 10, height: 10, icon: FaXmark }
                }
            }
        }
    }
}

#[component]
fn ArchivedThreadsPanel() -> Element {
    let entries = use_memo(move || build_archived_view().unwrap_or_default());
    let entries_value = entries.read().clone();
    if entries_value.is_empty() {
        return rsx! {
            div { class: "mt-2 text-xs text-text-muted italic",
                "No archived conversations."
            }
        };
    }
    rsx! {
        ul { class: "mt-2 space-y-1",
            for entry in entries_value.iter() {
                ArchivedThreadRow { key: "{entry.room:?}_{entry.peer}", entry: entry.clone() }
            }
        }
    }
}

#[component]
fn ArchivedThreadRow(entry: ArchivedEntry) -> Element {
    let room = entry.room;
    let peer = entry.peer;
    let unarchive = move |_| {
        unhide_dm_thread(room, peer);
    };
    rsx! {
        li {
            div { class: "flex items-center justify-between gap-2 text-xs px-2 py-1 rounded hover:bg-surface",
                div { class: "min-w-0 flex-1",
                    div { class: "truncate text-text", "{entry.peer_nickname}" }
                    div { class: "truncate text-[10px] text-text-muted", "in {entry.room_name}" }
                }
                button {
                    class: "text-accent hover:underline text-xs",
                    onclick: unarchive,
                    "Un-archive"
                }
            }
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub(crate) struct DmRailEntry {
    pub(crate) room: VerifyingKey,
    pub(crate) peer: MemberId,
    pub(crate) peer_nickname: String,
    pub(crate) room_name: String,
    pub(crate) last_any_ts: u64,
    pub(crate) unread: usize,
}

#[derive(Clone, PartialEq, Debug)]
struct ArchivedEntry {
    room: VerifyingKey,
    peer: MemberId,
    peer_nickname: String,
    room_name: String,
}

/// Pure helper: drop rail entries whose `(room, peer)` is currently
/// hidden AND whose latest message timestamp does not exceed the
/// recorded `hidden_at_ts`. Issue freenet/river#261.
///
/// The "user-visible feature" of #261 is exactly "this thread no
/// longer appears in the rail." `build_view` collects candidate
/// entries from room state and `DM_LAST_SEEN`; this function does the
/// final filter step so it can be unit-tested without standing up a
/// full Dioxus runtime.
///
/// Rules (matches `chat_delegate::is_thread_hidden` strict `<=`):
/// - Entry's `(room, peer)` absent from `hidden` → present.
/// - Entry's `last_any_ts > hidden_at_ts` → present (newer message
///   revived the thread, regardless of direction).
/// - Entry's `last_any_ts <= hidden_at_ts` → omitted.
///
/// Pinned by `filter_rail_entries_*` tests in this module.
pub(crate) fn filter_rail_entries(
    entries: Vec<DmRailEntry>,
    hidden: &HashMap<(VerifyingKey, MemberId), HiddenDmThreadEntry>,
) -> Vec<DmRailEntry> {
    entries
        .into_iter()
        .filter(|e| !is_thread_hidden_for(hidden, &e.room, e.peer, e.last_any_ts))
        .collect()
}

/// Pure helper: project the archived viewer's rows from the in-memory
/// hide map plus the per-room display data and a per-pair
/// `last_any_ts` map (the max DM timestamp for each `(room, peer)` in
/// current room state). Entries whose thread has been revived by a
/// strictly-newer message (`is_thread_hidden_for` returns false) are
/// SKIPPED so the viewer agrees with the rail filter — without this,
/// the rail correctly re-shows a revived row but the "Archived (N)"
/// count and viewer keep listing it as still archived (Codex P2 review
/// finding on PR #275).
///
/// Sorted by (room_name, peer_nickname) so the viewer is stable across
/// renders. Pulled out of `build_archived_view` so the filter +
/// projection can be unit-tested independently of the Dioxus runtime.
fn build_archived_rows(
    hidden: &HashMap<(VerifyingKey, MemberId), HiddenDmThreadEntry>,
    room_meta: &HashMap<VerifyingKey, ArchivedRoomMeta>,
    last_any_ts: &HashMap<(VerifyingKey, MemberId), u64>,
) -> Vec<ArchivedEntry> {
    let mut out: Vec<ArchivedEntry> = hidden
        .iter()
        .filter(|((room, peer), _entry)| {
            // Stale-revival check: a hidden entry whose thread now has
            // a strictly-newer message is treated as not-archived (the
            // rail shows the row again). Pairs with no recorded
            // `last_any_ts` — typically the owning room is no longer
            // loaded — fall back to 0 so the strict-`<=` rule still
            // treats them as hidden (the rail filter would otherwise
            // not surface them either).
            let ts = last_any_ts.get(&(*room, *peer)).copied().unwrap_or(0);
            is_thread_hidden_for(hidden, room, *peer, ts)
        })
        .map(|((room, peer), _entry)| {
            let meta = room_meta.get(room);
            let room_name = meta
                .map(|m| m.room_name.clone())
                .unwrap_or_else(|| "(unknown room)".to_string());
            let peer_nickname = meta
                .and_then(|m| m.nicknames.get(peer).cloned())
                .unwrap_or_else(|| short_member_id(peer));
            ArchivedEntry {
                room: *room,
                peer: *peer,
                peer_nickname,
                room_name,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        a.room_name
            .cmp(&b.room_name)
            .then_with(|| a.peer_nickname.cmp(&b.peer_nickname))
    });
    out
}

/// Pure helper: count how many hidden entries WOULD survive the
/// archived viewer's revival filter. Used to keep "Archived (N)" in
/// sync with the viewer rows. Same predicate as `build_archived_rows`,
/// extracted so the count stays correct even when the viewer isn't
/// open (we don't materialise rows on every render — that costs a
/// nickname / room-name decrypt per entry).
fn count_currently_archived(
    hidden: &HashMap<(VerifyingKey, MemberId), HiddenDmThreadEntry>,
    last_any_ts: &HashMap<(VerifyingKey, MemberId), u64>,
) -> usize {
    hidden
        .iter()
        .filter(|((room, peer), _)| {
            let ts = last_any_ts.get(&(*room, *peer)).copied().unwrap_or(0);
            is_thread_hidden_for(hidden, room, *peer, ts)
        })
        .count()
}

#[derive(Clone, PartialEq, Debug)]
struct ArchivedRoomMeta {
    room_name: String,
    nicknames: HashMap<MemberId, String>,
}

fn build_archived_view() -> Option<Vec<ArchivedEntry>> {
    let rooms = ROOMS.try_read().ok()?;
    let hidden = HIDDEN_DM_THREADS.try_read().ok()?.clone();

    // Materialise per-room display data once and compute the per-pair
    // max DM timestamp at the same time. Both decryption and the
    // timestamp scan are cheap (we already do them on the main rail
    // path). The shared scan is the load-bearing fix for the Codex
    // P2 finding — without filtering by current `last_any_ts`, a
    // revived thread shows on the rail AND in the archived viewer,
    // confusing the user about whether it's archived.
    let mut room_meta: HashMap<VerifyingKey, ArchivedRoomMeta> = HashMap::new();
    let mut last_any_ts: HashMap<(VerifyingKey, MemberId), u64> = HashMap::new();
    for (owner_vk, room_data) in &rooms.map {
        let self_id: MemberId = room_data.self_sk.verifying_key().into();
        let sealed_name = &room_data
            .room_state
            .configuration
            .configuration
            .display
            .name;
        let room_name = match unseal_bytes_with_secrets(sealed_name, &room_data.secrets) {
            Ok(b) => String::from_utf8_lossy(&b).to_string(),
            Err(_) => sealed_name.to_string_lossy(),
        };
        let nicknames: HashMap<MemberId, String> = room_data
            .room_state
            .member_info
            .member_info
            .iter()
            .map(|info| {
                (
                    info.member_info.member_id,
                    match unseal_bytes_with_secrets(
                        &info.member_info.preferred_nickname,
                        &room_data.secrets,
                    ) {
                        Ok(b) => String::from_utf8_lossy(&b).to_string(),
                        Err(_) => info.member_info.preferred_nickname.to_string_lossy(),
                    },
                )
            })
            .collect();
        room_meta.insert(
            *owner_vk,
            ArchivedRoomMeta {
                room_name,
                nicknames,
            },
        );

        // Max DM timestamp per (this room, peer) across both inbound and
        // outbound DMs — same accumulator shape as `build_view`. We do
        // NOT pre-filter by `hidden` here; the strict-`<=` revival rule
        // is applied inside `build_archived_rows`.
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
            let entry = last_any_ts.entry((*owner_vk, peer)).or_insert(0);
            if msg.message.timestamp > *entry {
                *entry = msg.message.timestamp;
            }
        }
    }

    Some(build_archived_rows(&hidden, &room_meta, &last_any_ts))
}

/// Compute the current archived count (post revival-filter) for the
/// "Archived (N)" link. Reads `ROOMS` + `HIDDEN_DM_THREADS` and runs
/// the same scan as `build_archived_view` but without materialising
/// the per-pair display metadata — saves a HashMap of decrypted
/// nicknames per render when the viewer is closed (the common case).
fn current_archived_count() -> Option<usize> {
    let rooms = ROOMS.try_read().ok()?;
    let hidden = HIDDEN_DM_THREADS.try_read().ok()?;
    let mut last_any_ts: HashMap<(VerifyingKey, MemberId), u64> = HashMap::new();
    for (owner_vk, room_data) in &rooms.map {
        let self_id: MemberId = room_data.self_sk.verifying_key().into();
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
            let entry = last_any_ts.entry((*owner_vk, peer)).or_insert(0);
            if msg.message.timestamp > *entry {
                *entry = msg.message.timestamp;
            }
        }
    }
    Some(count_currently_archived(&hidden, &last_any_ts))
}

fn build_view() -> Option<Vec<DmRailEntry>> {
    let rooms = ROOMS.try_read().ok()?;
    if rooms.map.is_empty() {
        return Some(Vec::new());
    }
    let last_seen = DM_LAST_SEEN.try_read().ok()?.clone();
    // Snapshot the hide-list (#261). `try_read` keeps us cooperative
    // with any in-flight `defer`-scheduled mutation. On contention we
    // silently treat the list as empty for THIS render — a hidden
    // thread briefly re-appearing during a write storm is preferable
    // to dropping the entire rail. Successful `try_read` registers
    // the memo's subscription so subsequent hide/unhide writes
    // re-run this build (Dioxus signal-safety semantics).
    let hidden = HIDDEN_DM_THREADS.try_read().ok().map(|h| h.clone());

    let mut entries: Vec<DmRailEntry> = Vec::new();
    for (owner_vk, room_data) in &rooms.map {
        let self_id: MemberId = room_data.self_sk.verifying_key().into();

        // Decrypted room name for display.
        let sealed_name = &room_data
            .room_state
            .configuration
            .configuration
            .display
            .name;
        let room_name = match unseal_bytes_with_secrets(sealed_name, &room_data.secrets) {
            Ok(b) => String::from_utf8_lossy(&b).to_string(),
            Err(_) => sealed_name.to_string_lossy(),
        };

        // Nickname lookup per member id.
        let nicknames: HashMap<MemberId, String> = room_data
            .room_state
            .member_info
            .member_info
            .iter()
            .map(|info| {
                (
                    info.member_info.member_id,
                    match unseal_bytes_with_secrets(
                        &info.member_info.preferred_nickname,
                        &room_data.secrets,
                    ) {
                        Ok(b) => String::from_utf8_lossy(&b).to_string(),
                        Err(_) => info.member_info.preferred_nickname.to_string_lossy(),
                    },
                )
            })
            .collect();

        // Per-peer accumulator.
        struct Acc {
            last_any_ts: u64,
            unread: usize,
        }
        let mut per_peer: HashMap<MemberId, Acc> = HashMap::new();
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
                let cutoff = last_seen.get(&(*owner_vk, peer)).copied().unwrap_or(0);
                if msg.message.timestamp > cutoff {
                    acc.unread += 1;
                }
            }
        }

        // Build the candidate entries for this room. The hide-filter
        // step runs once at the end (over all rooms' candidates) so it
        // is pure and unit-testable via `filter_rail_entries`. See
        // that helper's doc-comment for the #261 strict-`<=` semantics
        // and the `filter_rail_entries_newer_outbound_revives_hidden`
        // test for the "outbound revives" invariant.
        for (peer, acc) in per_peer {
            entries.push(DmRailEntry {
                room: *owner_vk,
                peer,
                peer_nickname: nicknames
                    .get(&peer)
                    .cloned()
                    .unwrap_or_else(|| short_member_id(&peer)),
                room_name: room_name.clone(),
                last_any_ts: acc.last_any_ts,
                unread: acc.unread,
            });
        }
    }

    // Issue freenet/river#261: drop hidden entries. When the signal
    // read was contended we silently treat the list as empty for THIS
    // render — a hidden thread briefly re-appearing during a write
    // storm is preferable to dropping the entire rail.
    let filtered = match hidden {
        Some(ref h) => filter_rail_entries(entries, h),
        None => entries,
    };
    let mut entries = filtered;

    // Unread threads first, then most-recent.
    entries.sort_by(|a, b| {
        b.unread
            .cmp(&a.unread)
            .then_with(|| b.last_any_ts.cmp(&a.last_any_ts))
    });

    Some(entries)
}

fn short_member_id(id: &MemberId) -> String {
    id.to_string().chars().take(8).collect()
}

fn unix_now_secs() -> u64 {
    crate::util::get_current_system_time()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn unix_now_ms() -> u64 {
    crate::util::get_current_system_time()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    //! Unit tests for the `DmRailSection` pure filter helper. Pin the
    //! user-visible "hidden threads disappear from the rail" behaviour
    //! of issue freenet/river#261 — and its corollary "an outbound DM
    //! revives a hidden thread" from the Codex P1 fix.
    //!
    //! These tests exercise the pure helper extracted from `build_view`;
    //! the full `build_view` requires a Dioxus runtime + signal context
    //! to call, so it cannot be unit-tested directly. The extraction
    //! keeps the test surface aligned with the user-visible behaviour
    //! (the test reviewer's BLOCKING finding on PR #265).
    use super::*;
    use freenet_scaffold::util::FastHash;

    fn sk(seed: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
    }

    fn entry(room: VerifyingKey, peer_seed: i64, last_any_ts: u64, unread: usize) -> DmRailEntry {
        DmRailEntry {
            room,
            peer: MemberId(FastHash(peer_seed)),
            peer_nickname: format!("peer-{peer_seed}"),
            room_name: "room".into(),
            last_any_ts,
            unread,
        }
    }

    fn hidden_at(
        room: VerifyingKey,
        peer_seed: i64,
        hidden_at_ts: u64,
    ) -> ((VerifyingKey, MemberId), HiddenDmThreadEntry) {
        let peer = MemberId(FastHash(peer_seed));
        (
            (room, peer),
            HiddenDmThreadEntry {
                room_owner_vk: room.to_bytes(),
                peer,
                hidden_at_ts,
            },
        )
    }

    /// Baseline: a hidden entry whose `(room, peer)` matches and whose
    /// `last_any_ts == hidden_at_ts` (strict `<=`) is omitted from the
    /// rail.
    #[test]
    fn filter_rail_entries_omits_hidden_thread() {
        let room = sk(1).verifying_key();
        let entries = vec![entry(room, 11, 1_000, 0)];
        let hidden = HashMap::from([hidden_at(room, 11, 1_000)]);

        let result = filter_rail_entries(entries, &hidden);
        assert!(
            result.is_empty(),
            "hidden thread with equal-ts must be filtered out"
        );
    }

    /// Codex P1 invariant + #261 "newer inbound message revives":
    /// an inbound DM arriving after the hide MUST re-surface the
    /// thread (its `last_any_ts > hidden_at_ts`).
    #[test]
    fn filter_rail_entries_newer_inbound_revives_hidden() {
        let room = sk(1).verifying_key();
        // last_any_ts is 1500, hidden at 1000 — strictly newer revives.
        let entries = vec![entry(room, 11, 1_500, 1)];
        let hidden = HashMap::from([hidden_at(room, 11, 1_000)]);

        let result = filter_rail_entries(entries, &hidden);
        assert_eq!(result.len(), 1, "newer inbound DM must revive thread");
        assert_eq!(result[0].last_any_ts, 1_500);
        assert_eq!(
            result[0].unread, 1,
            "unread accumulator must pass through filter"
        );
    }

    /// #261 "outbound revives": an outbound DM (reflected purely
    /// through `last_any_ts > hidden_at_ts` since outbound messages
    /// also bump `acc.last_any_ts` in `build_view`) MUST re-surface
    /// the thread. This is the rail-side mirror of the Codex P1
    /// explicit `unhide_dm_thread` call in `dm_thread_modal::do_send`.
    /// Even if the hide map were never cleared, a strictly-newer
    /// outbound timestamp must override.
    #[test]
    fn filter_rail_entries_newer_outbound_revives_hidden() {
        let room = sk(1).verifying_key();
        // Outbound: zero unread (sender's own message), but
        // last_any_ts moved past the hide cutoff.
        let entries = vec![entry(room, 11, 1_500, 0)];
        let hidden = HashMap::from([hidden_at(room, 11, 1_000)]);

        let result = filter_rail_entries(entries, &hidden);
        assert_eq!(
            result.len(),
            1,
            "newer outbound DM must revive thread (last_any_ts > hidden_at_ts)"
        );
    }

    /// Scope check: hiding `(room A, peer X)` MUST NOT hide
    /// `(room B, peer X)` — the same peer in a different room is a
    /// different thread.
    #[test]
    fn filter_rail_entries_hide_is_scoped_per_room() {
        let room_a = sk(1).verifying_key();
        let room_b = sk(2).verifying_key();
        let entries = vec![entry(room_a, 11, 1_000, 0), entry(room_b, 11, 1_000, 2)];
        // Hide ONLY in room A.
        let hidden = HashMap::from([hidden_at(room_a, 11, 1_000)]);

        let result = filter_rail_entries(entries, &hidden);
        assert_eq!(result.len(), 1, "hide in room A must not leak into room B");
        assert_eq!(result[0].room, room_b);
        assert_eq!(result[0].unread, 2);
    }

    /// Scope check: hiding `(room A, peer X)` MUST NOT hide
    /// `(room A, peer Y)` — different peers in the same room are
    /// different threads.
    #[test]
    fn filter_rail_entries_hide_is_scoped_per_peer() {
        let room = sk(1).verifying_key();
        let entries = vec![entry(room, 11, 1_000, 0), entry(room, 22, 1_000, 0)];
        // Hide ONLY peer 11.
        let hidden = HashMap::from([hidden_at(room, 11, 1_000)]);

        let result = filter_rail_entries(entries, &hidden);
        assert_eq!(result.len(), 1, "hide of peer 11 must not affect peer 22");
        assert_eq!(result[0].peer, MemberId(FastHash(22)));
    }

    /// Unhide: removing the hide entry from the map MUST cause the
    /// thread to reappear (regardless of `last_any_ts`). This is the
    /// rail-side observable for `unhide_dm_thread`.
    #[test]
    fn filter_rail_entries_unhide_reappears() {
        let room = sk(1).verifying_key();
        let entries = vec![entry(room, 11, 1_000, 0)];

        // First: hidden.
        let hidden = HashMap::from([hidden_at(room, 11, 1_000)]);
        assert!(
            filter_rail_entries(entries.clone(), &hidden).is_empty(),
            "precondition: thread is hidden"
        );

        // Then: unhide (empty map) — must reappear.
        let unhidden: HashMap<(VerifyingKey, MemberId), HiddenDmThreadEntry> = HashMap::new();
        let result = filter_rail_entries(entries, &unhidden);
        assert_eq!(
            result.len(),
            1,
            "after unhide, the thread must be visible again"
        );
    }

    /// Empty hidden map is a no-op fast-path: every entry passes
    /// through unmodified. Pins the optimisation `build_view` relies
    /// on (`hidden.is_empty()` is the common case during normal app
    /// operation).
    #[test]
    fn filter_rail_entries_empty_hidden_passes_all_through() {
        let room = sk(1).verifying_key();
        let entries = vec![entry(room, 11, 1_000, 0), entry(room, 22, 2_000, 3)];
        let hidden: HashMap<(VerifyingKey, MemberId), HiddenDmThreadEntry> = HashMap::new();

        let result = filter_rail_entries(entries.clone(), &hidden);
        assert_eq!(result, entries);
    }

    /// Archived viewer: rows from the in-memory hide map are projected
    /// through the per-room display data, falling back to a short
    /// peer-id when nicknames are unavailable. Sorted by
    /// (room_name, peer_nickname) for stable rendering.
    #[test]
    fn build_archived_rows_projects_and_sorts() {
        let room_a = sk(1).verifying_key();
        let room_b = sk(2).verifying_key();
        let mut hidden = HashMap::new();
        hidden.extend([
            hidden_at(room_a, 11, 1_000),
            hidden_at(room_a, 22, 1_000),
            hidden_at(room_b, 11, 1_000),
        ]);

        let mut nicknames_a = HashMap::new();
        nicknames_a.insert(MemberId(FastHash(11)), "alice".into());
        nicknames_a.insert(MemberId(FastHash(22)), "bob".into());
        let nicknames_b = HashMap::new(); // Peer 11 in room B → falls back to short id.

        let mut room_meta = HashMap::new();
        room_meta.insert(
            room_a,
            ArchivedRoomMeta {
                room_name: "A-Room".into(),
                nicknames: nicknames_a,
            },
        );
        room_meta.insert(
            room_b,
            ArchivedRoomMeta {
                room_name: "B-Room".into(),
                nicknames: nicknames_b,
            },
        );

        // No newer messages for any of the hidden pairs — so each one
        // is still archived.
        let last_any_ts = HashMap::new();
        let rows = build_archived_rows(&hidden, &room_meta, &last_any_ts);
        assert_eq!(rows.len(), 3, "all hidden pairs surface in the viewer");
        // Sort order: room A's rows precede room B's; within room A,
        // alice precedes bob.
        assert_eq!(rows[0].room_name, "A-Room");
        assert_eq!(rows[0].peer_nickname, "alice");
        assert_eq!(rows[1].room_name, "A-Room");
        assert_eq!(rows[1].peer_nickname, "bob");
        assert_eq!(rows[2].room_name, "B-Room");
        // Peer 11 in room B uses the short-id fallback.
        assert_ne!(
            rows[2].peer_nickname, "alice",
            "fallback must NOT leak across rooms"
        );
    }

    /// Archived viewer: a hidden `(room, peer)` whose owning room is
    /// no longer in `ROOMS` (e.g. the user left the room while it had
    /// an archived DM) renders with the "(unknown room)" placeholder
    /// rather than disappearing — otherwise the user has no path to
    /// un-archive and the entry sits in delegate storage forever.
    #[test]
    fn build_archived_rows_falls_back_when_room_missing() {
        let room = sk(1).verifying_key();
        let hidden = HashMap::from([hidden_at(room, 11, 1_000)]);
        let room_meta: HashMap<VerifyingKey, ArchivedRoomMeta> = HashMap::new();
        let last_any_ts = HashMap::new();

        let rows = build_archived_rows(&hidden, &room_meta, &last_any_ts);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].room_name, "(unknown room)");
    }

    /// Codex P2 fix: a thread whose `last_any_ts` is strictly newer
    /// than its `hidden_at_ts` (i.e. revived by a newer DM) MUST be
    /// dropped from the archived viewer. Otherwise the rail shows the
    /// row (because `filter_rail_entries` revives it) AND the
    /// "Archived (N)" viewer still lists it, leaving the user
    /// confused about whether the thread is archived or not.
    #[test]
    fn build_archived_rows_skips_revived_thread() {
        let room = sk(1).verifying_key();
        let hidden = HashMap::from([hidden_at(room, 11, 1_000)]);
        let mut room_meta = HashMap::new();
        room_meta.insert(
            room,
            ArchivedRoomMeta {
                room_name: "Room".into(),
                nicknames: HashMap::new(),
            },
        );
        // Last message at 1500 — strictly later than `hidden_at_ts =
        // 1000`, so the rail's `filter_rail_entries` would have
        // surfaced the row. The archived viewer must agree.
        let mut last_any_ts = HashMap::new();
        last_any_ts.insert((room, MemberId(FastHash(11))), 1_500u64);

        let rows = build_archived_rows(&hidden, &room_meta, &last_any_ts);
        assert!(
            rows.is_empty(),
            "revived thread must NOT appear in the archived viewer"
        );

        let count = count_currently_archived(&hidden, &last_any_ts);
        assert_eq!(count, 0, "count must agree with the viewer rows");
    }

    /// Same predicate from the count helper's side: a hidden entry
    /// whose `last_any_ts <= hidden_at_ts` still counts as archived,
    /// even if `last_any_ts` was never recorded (room not loaded →
    /// fall back to 0, which is `<= 1000`).
    #[test]
    fn count_currently_archived_keeps_stale_hidden_entries() {
        let room = sk(1).verifying_key();
        let hidden = HashMap::from([hidden_at(room, 11, 1_000)]);

        // Case A: no entry in last_any_ts (room not loaded).
        let last_empty = HashMap::new();
        assert_eq!(
            count_currently_archived(&hidden, &last_empty),
            1,
            "unloaded room's archived entry must still count"
        );

        // Case B: last_any_ts == hidden_at_ts → still archived
        // (strict `<=`, matching `is_thread_hidden`).
        let mut last_equal = HashMap::new();
        last_equal.insert((room, MemberId(FastHash(11))), 1_000u64);
        assert_eq!(
            count_currently_archived(&hidden, &last_equal),
            1,
            "equal-ts must still count as archived (strict <=)"
        );
    }

    /// Pin the toast helper's expiry math. A back-to-back archive
    /// produces a NEW `expires_at_ms`, so the first auto-dismiss's
    /// "is it still mine?" check fails and the second toast survives
    /// its own full duration.
    #[test]
    fn build_archive_toast_advances_expiry_per_call() {
        let room = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let t1 = build_archive_toast(room, peer, "alice", 1_000);
        let t2 = build_archive_toast(room, peer, "alice", 2_000);
        assert_eq!(t1.expires_at_ms, 1_000 + ARCHIVE_TOAST_DURATION_MS);
        assert_eq!(t2.expires_at_ms, 2_000 + ARCHIVE_TOAST_DURATION_MS);
        assert_ne!(
            t1.expires_at_ms, t2.expires_at_ms,
            "back-to-back archives must produce distinct expiries — \
             otherwise the first auto-dismiss tick would cancel the second toast"
        );
    }

    /// Saturation: a `now_ms` near `u64::MAX` must not overflow when
    /// adding the duration. Defensive — `Date.now()` is far from
    /// `u64::MAX` in practice, but pinning this prevents a future
    /// "let's switch to nanoseconds" change from producing a wrap-around
    /// toast that auto-dismisses instantly.
    #[test]
    fn build_archive_toast_saturates_on_overflow() {
        let room = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let t = build_archive_toast(room, peer, "alice", u64::MAX);
        assert_eq!(t.expires_at_ms, u64::MAX);
    }
}
