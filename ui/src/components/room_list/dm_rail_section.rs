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
//! ✕ and was repeatedly mistaken for it). Reveal is gated on *hover
//! capability*, not viewport width (issue #462): on a hover-pointer
//! device the button is `opacity-0` until the row is hovered/focused;
//! on a touch device (`@media (hover: none)`) `.dm-archive-btn` in
//! `main.css` forces it fully visible with a 44px tap target, because
//! the `group-hover` reveal Tailwind emits is itself wrapped in
//! `@media (hover: hover)` and can never fire there. This mirrors the
//! `.hover-actions` / `.touch-actions` split #402 introduced for the
//! message action bar. Archived threads stay out of the rail until
//! either side sends a new DM. The "Archived (N)" link at the bottom of
//! the section lists currently-archived threads and offers per-row
//! Un-archive, closing #266.
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
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Monotonic counter producing unique identity tokens for `ArchiveToast`
/// instances. Replaces the previous "use `expires_at_ms` as identity"
/// approach (M2 from skeptical review on PR #275) — `unix_now_ms()`
/// collisions on rapid clicks / mobile timer coalescing produced
/// premature auto-dismiss of a second toast by the first toast's
/// timeout. Monotonic counter has no collision risk.
static ARCHIVE_TOAST_TOKEN: AtomicU64 = AtomicU64::new(1);

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
    /// Monotonic identity token. Used by the auto-dismiss timeout to
    /// determine "is the current toast still mine?" — see M2 fix.
    token: u64,
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
    let threads = use_memo(build_view);
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
    // (any `try_read` failing) it serves the last clean count and
    // schedules a self-nudge — a transient 0 here could combine with an
    // empty thread list to satisfy the empty-state early return below
    // and unmount the whole DIRECT MESSAGES section for a pass.
    let archived_count = use_memo(current_archived_count);
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
    let last_inbound_ts = entry.last_inbound_ts;
    let click = move |_| {
        open_dm_thread(room, peer);
    };

    // `group` + `group-hover:opacity-100` keeps the ✕ off-screen at rest
    // on a hover-pointer device. On a touch device the `.dm-archive-btn`
    // rule in main.css (`@media (hover: none)`) forces it visible with a
    // real tap target, because Tailwind wraps `group-hover:*` in
    // `@media (hover: hover)` so it can never fire on touch (#462).
    // `group-focus-within` mirrors hover for keyboard users tab-stopping
    // into the row.
    let archive_click = move |evt: Event<MouseData>| {
        // The archive button is a sibling of the row's "open thread"
        // button (not nested inside it), so the row click handler
        // doesn't fire on its own. `stop_propagation` is defensive
        // belt-and-braces: it costs nothing and protects against a
        // future refactor that wraps the row in a clickable
        // container — without it, archiving from such a container
        // would open the thread on the same click.
        evt.stop_propagation();
        archive_row(room, peer, &nickname, last_inbound_ts);
    };

    let archive_title =
        "Archive this conversation. It will return if either of you sends a new DM.";

    rsx! {
        li {
            div { class: "group relative w-full",
                button {
                    // `dm-rail-row-btn`: main.css widens the right padding on
                    // touch devices so the enlarged always-visible archive ✕
                    // (below) doesn't overlap the nickname / unread badge (#462).
                    class: "dm-rail-row-btn w-full text-left pl-3 pr-9 py-1.5 rounded-lg text-sm transition-colors text-text hover:bg-surface flex items-center gap-2",
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
                    // `dm-archive-btn`: opacity-0 at rest so the hover reveal
                    // stays clean on desktop; main.css forces it visible with a
                    // 44px tap target under `@media (hover: none)` (#462).
                    class: "dm-archive-btn absolute right-1 top-1/2 -translate-y-1/2 p-1 rounded text-text-muted \
                            flex items-center justify-center opacity-0 \
                            group-hover:opacity-100 group-focus-within:opacity-100 \
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
        token: ARCHIVE_TOAST_TOKEN.fetch_add(1, Ordering::Relaxed),
    }
}

/// Max timestamp of a DM the peer sent US for `(room, peer)`, read from live
/// `ROOMS` state rather than from a rendered rail row.
///
/// Returns `None` when `ROOMS` is contended or the room is absent, so the
/// caller can fall back to the row's value (issue freenet/river#526 — see
/// [`archive_row`] property 2 for why the rendered prop alone is not enough).
///
/// Pure-ish: the scan itself is exercised through
/// [`max_inbound_ts_from_triples`], which is unit-tested; this wrapper only
/// does the signal read.
fn current_last_inbound_ts(room: &VerifyingKey, peer: MemberId) -> Option<u64> {
    use dioxus::prelude::ReadableExt;
    let rooms = ROOMS.try_read().ok()?;
    let room_data = rooms.map.get(room)?;
    let self_id = room_data.self_member_id()?;
    Some(max_inbound_ts_from_triples(
        dm_message_triples(room_data),
        self_id,
        peer,
    ))
}

/// Pure helper behind [`current_last_inbound_ts`]: max timestamp over DMs
/// sent BY `peer` TO `self_id`. Zero when the pair has no inbound DMs.
///
/// Pinned by the `max_inbound_ts_from_triples_*` tests.
pub(crate) fn max_inbound_ts_from_triples(
    messages: impl IntoIterator<Item = (MemberId, MemberId, u64)>,
    self_id: MemberId,
    peer: MemberId,
) -> u64 {
    messages
        .into_iter()
        .filter(|(sender, recipient, _)| *sender == peer && *recipient == self_id)
        .map(|(_, _, ts)| ts)
        .max()
        .unwrap_or(0)
}

/// The `hidden_at_ts` an Archive click records, from the row's rendered
/// inbound clock and the live one (`None` when `ROOMS` was contended).
///
/// Issue freenet/river#526 turns on two properties, both of them here:
///
/// 1. INBOUND-ONLY inputs. `last_any_ts` also covers our own outbound DMs,
///    whose timestamp is our LOCAL wall clock. A thread where we replied last
///    would then be archived against OUR clock while
///    `should_unhide_for_inbound_dm` compares a SENDER's timestamp — so a peer
///    DM already in flight across the click lands at or below the cutoff and
///    goes invisible on every surface: no rail row, no unread badge
///    (`document_title::count_unread_dms_with` applies the same filter), no
///    notification (DMs never raise one).
///
/// 2. MAX of rendered and live. The row prop can lag — `build_view` serves
///    `LAST_GOOD_RAIL` verbatim on a contended pass, and a DM can land in
///    `ROOMS` between the memo's last clean pass and the click. The membership
///    sweep is atomic per pair but the RE-LAND is not, so a peer restoring
///    only a subset also leaves the row low. A cutoff below the pair's true
///    inbound max lets the rest re-land looking newer than the archive, which
///    is #526 itself. Live wins when it is readable, the row is the fallback
///    when it is not.
///
///    Cost, stated precisely: the live read can include an inbound DM that
///    landed in `ROOMS` after the memo's last pass and so was never RENDERED.
///    Archiving then covers a message the user never saw, and it stays hidden
///    until the peer writes again. That is a deliberate trade — swallowing one
///    message until the next beats destroying the archive outright — but it is
///    NOT, as an earlier draft of this comment claimed, limited to messages the
///    user had already seen.
///
/// The result is never clamped against wall-clock time, and that is
/// deliberate. An earlier revision clamped it at `now + MAX_DM_FUTURE_SKEW_SECS`
/// to blunt a forged-future timestamp, which broke the load-bearing invariant
/// `every existing inbound DM satisfies ts <= cutoff`: the rail filter compares
/// the UNCLAMPED observed clock, so the row stayed visible (Archive became a
/// no-op that still showed an "Archived" toast) AND the next sweep-and-re-offer
/// saw `ts > cutoff` and destroyed the persisted archive — #526 itself,
/// narrowed to skew-violating peers. Clamping one side of a comparison is what
/// created the hole. The forged-timestamp concern is instead handled where it
/// cannot break this invariant: [`should_unhide_for_inbound_dm`] bounds the
/// INCOMING timestamp instead. That bound is narrower than it sounds — it
/// binds only once a forgery has already inflated the cutoff, stopping a peer
/// escalating to a larger forgery; a first forgery can still revive a
/// normally-archived thread. It is safe-directional either way: the bounded
/// value is never above the raw one, so it can only make the gate more
/// conservative.
///
/// Residual, documented rather than clamped: a peer who stamps a DM far in the
/// future sets a correspondingly far-future cutoff, so their later genuine DMs
/// stay archived until real time passes it. That hides messages from ONE peer
/// the user already chose to archive, and never destroys state. The real remedy
/// for a hostile peer is a block/ignore list, freenet/river#461.
///
/// Pinned by the `archive_cutoff_*` tests.
pub(crate) fn archive_cutoff(row_inbound_ts: u64, live_inbound_ts: Option<u64>) -> u64 {
    std::cmp::max(row_inbound_ts, live_inbound_ts.unwrap_or(0))
}

/// Wire up: archive the (room, peer) thread, schedule the toast, and
/// schedule the auto-dismiss. Called from the ✕ rollover button.
fn archive_row(room: VerifyingKey, peer: MemberId, peer_nickname: &str, row_last_inbound_ts: u64) {
    let cutoff = archive_cutoff(row_last_inbound_ts, current_last_inbound_ts(&room, peer));

    let now_ms = unix_now_ms();
    let toast = build_archive_toast(room, peer, peer_nickname, now_ms);
    let token = toast.token;

    // Order matters: write the toast BEFORE calling `hide_dm_thread`.
    // M3 fix from the skeptical review: if we hide first and toast second,
    // the render between the two defers (HIDDEN_DM_THREADS write fires
    // before ARCHIVE_TOAST write) can transiently satisfy the rail's
    // empty-state early-return — the rail unmounts and the toast write
    // arrives at a detached component. Writing the toast first guarantees
    // `toast.is_none()` is false at every intermediate render, so the
    // rail stays mounted across the hide.
    crate::util::defer(move || {
        *ARCHIVE_TOAST.write() = Some(toast);
    });

    hide_dm_thread(room, peer, cutoff);

    // Auto-dismiss: wait `ARCHIVE_TOAST_DURATION_MS`, then clear the
    // toast iff it's still the one we set. Identity is the monotonic
    // `token`, not `expires_at_ms` — same-millisecond clicks no longer
    // collide (M2 fix from the skeptical review).
    crate::util::safe_spawn_local(async move {
        crate::util::sleep(crate::util::millis(ARCHIVE_TOAST_DURATION_MS)).await;
        crate::util::defer(move || {
            ARCHIVE_TOAST.with_mut(|cell| {
                if let Some(current) = cell.as_ref() {
                    if current.token == token {
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
    /// Newest INBOUND timestamp — what the archive filter compares against.
    pub(crate) last_inbound_ts: u64,
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
/// - Entry's `last_inbound_ts > hidden_at_ts` → present (newer message
///   revived the thread, regardless of direction).
/// The comparison is INBOUND-only (issue freenet/river#526): archive means
/// "hidden until the peer writes again". Our own outbound sends revive the
/// thread through the unconditional `unhide_dm_thread` in `do_send` /
/// `send_structured_dm`, so they need no representation here - and including
/// them would put a locally-clocked timestamp on one side of the comparison,
/// which is what made an in-flight inbound DM invisible on every surface.
///
/// - Entry's `last_inbound_ts <= hidden_at_ts` → omitted.
///
/// Pinned by `filter_rail_entries_*` tests in this module.
pub(crate) fn filter_rail_entries(
    entries: Vec<DmRailEntry>,
    hidden: &HashMap<(VerifyingKey, MemberId), HiddenDmThreadEntry>,
) -> Vec<DmRailEntry> {
    entries
        .into_iter()
        .filter(|e| !is_thread_hidden_for(hidden, &e.room, e.peer, e.last_inbound_ts))
        .collect()
}

/// Pure helper deciding what the ACTIVE rail may show given the outcome
/// of the `HIDDEN_DM_THREADS` read (issue #499 mechanism 2).
///
/// * `hidden = Some(map)` — clean read: apply the #261 archive filter.
/// * `hidden = None` — the signal was contended. The old behaviour
///   failed OPEN (skipped the filter), flashing every archived thread
///   into the active rail for a render. Failing closed to an EMPTY list
///   instead would blink the whole rail (the other #499 symptom), so we
///   return `last_good`: the most recent successfully-filtered rail. By
///   construction it contains no thread that was archived as of the
///   last clean pass (a thread whose archive click is the very write
///   we're contending with may persist briefly — matching what was on
///   screen the instant before the click, never resurrecting older
///   archived threads). Staleness is bounded: every degraded pass
///   schedules a rail nudge (`schedule_rail_nudge`), so the memo
///   re-polls one macrotask later in a clean context rather than
///   waiting for an unrelated signal write.
///
/// Invariants, in priority order: (1) archived threads never appear in
/// the active rail; (2) the rail collapses as rarely as possible.
/// Pinned by the `resolve_active_entries_*` tests.
pub(crate) fn resolve_active_entries(
    entries: Vec<DmRailEntry>,
    hidden: Option<&HashMap<(VerifyingKey, MemberId), HiddenDmThreadEntry>>,
    last_good: &[DmRailEntry],
) -> Vec<DmRailEntry> {
    match hidden {
        Some(h) => filter_rail_entries(entries, h),
        None => last_good.to_vec(),
    }
}

/// Total-order sort for the rail: unread desc, most-recent activity
/// desc, then `(room key bytes, peer id)` as a deterministic tiebreak.
/// Entries accumulate through a fresh `HashMap` each pass, so without
/// the tiebreak threads tied on `(unread, last_any_ts)` shuffled
/// position between renders (issue #499 mechanism 4). Pinned by the
/// `sort_rail_entries_*` tests.
pub(crate) fn sort_rail_entries(entries: &mut [DmRailEntry]) {
    entries.sort_by(|a, b| {
        b.unread
            .cmp(&a.unread)
            .then_with(|| b.last_any_ts.cmp(&a.last_any_ts))
            .then_with(|| a.room.as_bytes().cmp(b.room.as_bytes()))
            .then_with(|| a.peer.cmp(&b.peer))
    });
}

/// Per-peer DM activity accumulated from one room's message list.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct PeerDmActivity {
    /// Newest timestamp in EITHER direction. Drives recency sorting.
    pub(crate) last_any_ts: u64,
    /// Newest timestamp of a DM the PEER sent to us. Drives the archive
    /// filter (issue freenet/river#526) — see [`archive_row`] for why the
    /// archive question is "has the peer written since I archived", and
    /// never "has anything happened".
    pub(crate) last_inbound_ts: u64,
    pub(crate) unread: usize,
}

/// Pure helper: fold one room's DM stream into per-peer activity.
/// `messages` yields `(sender, recipient, timestamp)` triples; anything
/// not involving `self_id` is skipped.
///
/// `last_seen` is `Some(map)` when `DM_LAST_SEEN` read cleanly and
/// `None` when the signal was contended. Issue #499 mechanism 1: a
/// contended read must degrade to "no unread info for this pass" —
/// entries are still produced, with `unread = 0` and correct recency —
/// instead of aborting the whole rail build (which blanked the entire
/// active list while "Archived (N)" stayed put). `build_view` then
/// backfills the zeroed unread counts from the last good rail (see
/// [`backfill_unread_from_last_good`]) so the degrade doesn't reorder
/// the unread-first sort. A peer merely ABSENT from a `Some` map still
/// means cutoff 0 (thread never read). Pinned by the
/// `accumulate_peer_activity_*` tests.
pub(crate) fn accumulate_peer_activity(
    room: &VerifyingKey,
    self_id: MemberId,
    messages: impl IntoIterator<Item = (MemberId, MemberId, u64)>,
    last_seen: Option<&HashMap<(VerifyingKey, MemberId), u64>>,
) -> HashMap<MemberId, PeerDmActivity> {
    let mut per_peer: HashMap<MemberId, PeerDmActivity> = HashMap::new();
    for (sender, recipient, timestamp) in messages {
        let is_self_sender = sender == self_id;
        let is_self_recipient = recipient == self_id;
        if !is_self_sender && !is_self_recipient {
            continue;
        }
        let peer = if is_self_sender { recipient } else { sender };
        let acc = per_peer.entry(peer).or_insert(PeerDmActivity {
            last_any_ts: 0,
            last_inbound_ts: 0,
            unread: 0,
        });
        if timestamp > acc.last_any_ts {
            acc.last_any_ts = timestamp;
        }
        if is_self_recipient {
            if timestamp > acc.last_inbound_ts {
                acc.last_inbound_ts = timestamp;
            }
            if let Some(seen) = last_seen {
                let cutoff = seen.get(&(*room, peer)).copied().unwrap_or(0);
                if timestamp > cutoff {
                    acc.unread += 1;
                }
            }
        }
    }
    per_peer
}

/// Pure helper: carry per-pair unread counts forward from the last good
/// rail when the CURRENT pass had no readable `DM_LAST_SEEN` (issue
/// #499 review follow-up). Zeroing unread on a contended pass is not
/// merely cosmetic: unread is the PRIMARY sort key, so a transient 0
/// reorders the rail (unread rows drop from the top) for that pass and
/// snaps back a moment later — the same visual flap #499 is about.
/// Pairs absent from `last_good` keep `unread = 0`: we have no better
/// information for them. Recency (`last_any_ts`) is deliberately NOT
/// backfilled — the current pass computed it from live room state.
/// Pinned by the `backfill_unread_from_last_good_*` tests.
pub(crate) fn backfill_unread_from_last_good(
    entries: &mut [DmRailEntry],
    last_good: &[DmRailEntry],
) {
    let prev: HashMap<(VerifyingKey, MemberId), usize> = last_good
        .iter()
        .map(|e| ((e.room, e.peer), e.unread))
        .collect();
    for e in entries.iter_mut() {
        if let Some(unread) = prev.get(&(e.room, e.peer)) {
            e.unread = *unread;
        }
    }
}

/// Project a room's authorized DM list into the `(sender, recipient,
/// timestamp)` triples [`accumulate_peer_activity`] consumes. Shared by
/// all three builders so their per-pair `last_any_ts` scans can't
/// drift apart.
fn dm_message_triples(
    room_data: &crate::room_data::RoomData,
) -> impl Iterator<Item = (MemberId, MemberId, u64)> + '_ {
    room_data
        .room_state
        .direct_messages
        .messages
        .iter()
        .map(|msg| {
            (
                msg.message.sender,
                msg.message.recipient,
                msg.message.timestamp,
            )
        })
}

thread_local! {
    /// Last rail built by a FULLY-clean `build_view` pass (hide-list
    /// AND unread cutoffs both read cleanly — see the gated write at
    /// the end of `build_view`; a pass with degraded unread would
    /// poison the unread backfill that reads from here). Serves as the
    /// degrade value when a signal read is contended: showing the
    /// last-good rail is strictly better than blanking it (issue #499's
    /// headline symptom), and — unlike the old skip-the-filter
    /// fallback — can never flash archived threads into the active
    /// list, because every cached list already went through
    /// `filter_rail_entries`. Staleness is bounded by the rail nudge
    /// (every degraded pass schedules one, so the memo re-polls a
    /// macrotask later). Plain `thread_local` (not a signal): the UI is
    /// single-threaded WASM and writing a signal from inside a memo
    /// would re-trigger reactivity.
    static LAST_GOOD_RAIL: RefCell<Vec<DmRailEntry>> = const { RefCell::new(Vec::new()) };

    /// Last archived count computed by a clean `current_archived_count`
    /// pass. Served on contended passes: a transient 0 there could
    /// combine with an empty thread list to satisfy `DmRailSection`'s
    /// empty-state early return and unmount the whole DIRECT MESSAGES
    /// section for a pass. Same clean-pass-only write discipline as
    /// `LAST_GOOD_RAIL`.
    static LAST_GOOD_ARCHIVED_COUNT: Cell<usize> = const { Cell::new(0) };

    /// "A rail nudge is already queued" latch, so N degraded builder
    /// passes in one render schedule exactly ONE deferred tick bump
    /// (storm-proof). Cleared when the deferred bump runs.
    static RAIL_NUDGE_PENDING: Cell<bool> = const { Cell::new(false) };
}

fn last_good_rail() -> Vec<DmRailEntry> {
    LAST_GOOD_RAIL.with(|c| c.borrow().clone())
}

fn set_last_good_rail(entries: &[DmRailEntry]) {
    LAST_GOOD_RAIL.with(|c| *c.borrow_mut() = entries.to_vec());
}

fn last_good_archived_count() -> usize {
    LAST_GOOD_ARCHIVED_COUNT.with(|c| c.get())
}

fn set_last_good_archived_count(count: usize) {
    LAST_GOOD_ARCHIVED_COUNT.with(|c| c.set(count));
}

/// Rebuild-nudge channel for the rail's three builder memos (issue #499
/// review follow-up). Each builder anchors its subscription set with an
/// infallible read of this tick BEFORE any fallible `try_read`, so a
/// contended poll can never leave a memo with zero subscriptions — and,
/// because every degraded pass schedules a deferred bump via
/// [`schedule_rail_nudge`], the tick is also the RECOVERY channel: the
/// memo re-polls one macrotask later instead of serving last-good data
/// until some unrelated user action happens to write a signal it reads.
static RAIL_REBUILD_TICK: GlobalSignal<u64> = Global::new(|| 0);

/// Schedule a one-shot deferred bump of [`RAIL_REBUILD_TICK`]. Called
/// from every degraded (contended-read) builder pass.
///
/// Self-terminating: the bump runs through `crate::util::defer` (the
/// clean execution context the dioxus-signal-safety rules require for
/// signal mutations), and the re-poll it triggers happens in a later,
/// borrow-free context — so the retried pass reads cleanly and
/// schedules no further nudge. If contention somehow persists, the
/// retried pass schedules exactly one more; the loop converges the
/// moment a pass reads cleanly. Storm-proof via `RAIL_NUDGE_PENDING`:
/// however many degrade paths fire in one render, only one bump queues.
fn schedule_rail_nudge() {
    if RAIL_NUDGE_PENDING.with(|c| c.replace(true)) {
        return; // a bump is already queued
    }
    crate::util::defer(|| {
        RAIL_NUDGE_PENDING.with(|c| c.set(false));
        RAIL_REBUILD_TICK.with_mut(|t| *t = t.wrapping_add(1));
    });
}

/// Pure helper: project the archived viewer's rows from the in-memory
/// hide map plus the per-room display data and a per-pair
/// `last_inbound_ts` map (max INBOUND DM timestamp per `(room, peer)` in
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
    last_inbound_ts: &HashMap<(VerifyingKey, MemberId), u64>,
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
            let ts = last_inbound_ts.get(&(*room, *peer)).copied().unwrap_or(0);
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
    last_inbound_ts: &HashMap<(VerifyingKey, MemberId), u64>,
) -> usize {
    hidden
        .iter()
        .filter(|((room, peer), _)| {
            let ts = last_inbound_ts.get(&(*room, *peer)).copied().unwrap_or(0);
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
    // RAIL_REBUILD_TICK is read (infallibly) BEFORE the fallible reads:
    // `try_read() -> Err` registers no subscription (dioxus-signal-safety
    // rules) and each memo run clears the previous subscription set, so
    // without this anchor one contended poll would leave the memo with
    // ZERO subscriptions — permanently frozen (issue #499 mechanism 3;
    // mirrors the CURRENT_ROOM guard in room_list.rs). The tick is only
    // written through `defer` from clean contexts, so the infallible
    // read cannot hit a live write borrow — and each degraded pass
    // below bumps it (via `schedule_rail_nudge`) so the retry is one
    // macrotask away.
    let _ = RAIL_REBUILD_TICK.read();
    let Ok(rooms) = ROOMS.try_read() else {
        schedule_rail_nudge();
        return None;
    };
    let hidden = match HIDDEN_DM_THREADS.try_read() {
        Ok(h) => h.clone(),
        Err(_) => {
            schedule_rail_nudge();
            return None;
        }
    };

    // Materialise per-room display data once and compute the per-pair
    // max DM timestamp at the same time. Both decryption and the
    // timestamp scan are cheap (we already do them on the main rail
    // path). The shared scan is the load-bearing fix for the Codex
    // P2 finding — without filtering by current `last_any_ts`, a
    // revived thread shows on the rail AND in the archived viewer,
    // confusing the user about whether it's archived.
    let mut room_meta: HashMap<VerifyingKey, ArchivedRoomMeta> = HashMap::new();
    let mut last_inbound_ts: HashMap<(VerifyingKey, MemberId), u64> = HashMap::new();
    for (owner_vk, room_data) in &rooms.map {
        // A room whose local identity is unknown contributes no DM rows:
        // every row here is defined relative to "self", so there is nothing
        // meaningful to compute for it.
        let Some(self_id) = room_data.self_member_id() else {
            continue;
        };
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
                    crate::util::display_name::display_nickname(
                        &info.member_info.preferred_nickname,
                        &room_data.secrets,
                    ),
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
        // outbound DMs — the same accumulator `build_view` uses (unread
        // info irrelevant here, so `last_seen = None`). We do NOT
        // pre-filter by `hidden`; the strict-`<=` revival rule is
        // applied inside `build_archived_rows`.
        for (peer, activity) in
            accumulate_peer_activity(owner_vk, self_id, dm_message_triples(room_data), None)
        {
            last_inbound_ts.insert((*owner_vk, peer), activity.last_inbound_ts);
        }
    }

    Some(build_archived_rows(&hidden, &room_meta, &last_inbound_ts))
}

/// Compute the current archived count (post revival-filter) for the
/// "Archived (N)" link. Reads `ROOMS` + `HIDDEN_DM_THREADS` and runs
/// the same scan as `build_archived_view` but without materialising
/// the per-pair display metadata — saves a HashMap of decrypted
/// nicknames per render when the viewer is closed (the common case).
fn current_archived_count() -> usize {
    // Same subscription anchor as `build_view` / `build_archived_view`:
    // an infallible read must precede the first fallible `try_read`,
    // otherwise one contended poll leaves this memo with zero
    // subscriptions and the count freezes for the session (issue #499
    // mechanism 3; mirrors the CURRENT_ROOM guard in room_list.rs).
    //
    // Contended passes serve the last CLEAN count instead of 0: a
    // transient 0 could combine with an empty thread list to satisfy
    // the component's empty-state early return and unmount the whole
    // DIRECT MESSAGES section for a pass. The nudge re-polls one
    // macrotask later.
    let _ = RAIL_REBUILD_TICK.read();
    let Ok(rooms) = ROOMS.try_read() else {
        schedule_rail_nudge();
        return last_good_archived_count();
    };
    let Ok(hidden) = HIDDEN_DM_THREADS.try_read() else {
        schedule_rail_nudge();
        return last_good_archived_count();
    };
    let mut last_inbound_ts: HashMap<(VerifyingKey, MemberId), u64> = HashMap::new();
    for (owner_vk, room_data) in &rooms.map {
        // A room whose local identity is unknown contributes no DM rows:
        // every row here is defined relative to "self", so there is nothing
        // meaningful to compute for it.
        let Some(self_id) = room_data.self_member_id() else {
            continue;
        };
        for (peer, activity) in
            accumulate_peer_activity(owner_vk, self_id, dm_message_triples(room_data), None)
        {
            last_inbound_ts.insert((*owner_vk, peer), activity.last_inbound_ts);
        }
    }
    let count = count_currently_archived(&hidden, &last_inbound_ts);
    set_last_good_archived_count(count);
    count
}

fn build_view() -> Vec<DmRailEntry> {
    // RAIL_REBUILD_TICK is read (infallibly) BEFORE the fallible reads:
    // `try_read() -> Err` registers no subscription (dioxus-signal-safety
    // rules) and each memo run clears the previous subscription set, so
    // without this anchor a single contended `ROOMS` poll would leave the
    // memo with ZERO subscriptions — a permanently empty rail, since
    // `DmRailSection` never unmounts (issue #499 mechanism 3; mirrors
    // the CURRENT_ROOM guard in room_list.rs). The tick is only written
    // through `defer` from clean contexts, so the infallible read cannot
    // hit a live write borrow — and every degraded pass below bumps it
    // (via `schedule_rail_nudge`) so the retry is one macrotask away,
    // not parked until some unrelated signal write.
    let _ = RAIL_REBUILD_TICK.read();

    let Ok(rooms) = ROOMS.try_read() else {
        // ROOMS is mid-write: reuse the last successfully-built rail
        // rather than blanking the whole list for this pass (issue #499
        // mechanism 1's symptom shape). The scheduled nudge re-polls
        // this memo one macrotask later, when the write has finished.
        schedule_rail_nudge();
        return last_good_rail();
    };
    if rooms.map.is_empty() {
        set_last_good_rail(&[]);
        return Vec::new();
    }

    // Unread cutoffs. Contention here must NOT abort the rail (#499
    // mechanism 1 — the old `?` blanked the entire active list while
    // "Archived (N)" stayed put): degrade to `None`, which
    // `accumulate_peer_activity` renders as "unread = 0 for this pass";
    // the backfill below then restores the last-known unread per pair
    // so the unread-first sort doesn't transiently reorder.
    let last_seen = match DM_LAST_SEEN.try_read() {
        Ok(s) => Some(s.clone()),
        Err(_) => {
            schedule_rail_nudge();
            None
        }
    };

    // Snapshot the hide-list (#261). `try_read` keeps us cooperative
    // with any in-flight `defer`-scheduled mutation. On contention
    // (`None`) the archive filter must fail CLOSED — see
    // `resolve_active_entries`: the old fail-open path flashed every
    // archived thread into the active rail for a render (#499
    // mechanism 2). Successful `try_read` registers the memo's
    // subscription so subsequent hide/unhide writes re-run this build.
    let hidden = match HIDDEN_DM_THREADS.try_read() {
        Ok(h) => Some(h.clone()),
        Err(_) => {
            schedule_rail_nudge();
            None
        }
    };

    let mut entries: Vec<DmRailEntry> = Vec::new();
    for (owner_vk, room_data) in &rooms.map {
        // A room whose local identity is unknown contributes no DM rows:
        // every row here is defined relative to "self", so there is nothing
        // meaningful to compute for it.
        let Some(self_id) = room_data.self_member_id() else {
            continue;
        };

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
                    crate::util::display_name::display_nickname(
                        &info.member_info.preferred_nickname,
                        &room_data.secrets,
                    ),
                )
            })
            .collect();

        // Per-peer accumulator — shared with the archived builders via
        // `accumulate_peer_activity` (a contended `last_seen` degrades
        // unread to 0 there, issue #499 mechanism 1).
        let per_peer = accumulate_peer_activity(
            owner_vk,
            self_id,
            dm_message_triples(room_data),
            last_seen.as_ref(),
        );

        // Build the candidate entries for this room. The hide-filter
        // step runs once at the end (over all rooms' candidates) so it
        // is pure and unit-testable via `filter_rail_entries`. See
        // that helper's doc-comment for the #261 strict-`<=` semantics
        // and, since #526, the
        // `filter_rail_entries_newer_outbound_does_not_revive_hidden`
        // test for why that clock is inbound-only.
        for (peer, activity) in per_peer {
            entries.push(DmRailEntry {
                room: *owner_vk,
                peer,
                peer_nickname: nicknames
                    .get(&peer)
                    .cloned()
                    .unwrap_or_else(|| short_member_id(&peer)),
                room_name: room_name.clone(),
                last_any_ts: activity.last_any_ts,
                last_inbound_ts: activity.last_inbound_ts,
                unread: activity.unread,
            });
        }
    }

    // Issue freenet/river#261: drop hidden entries — failing CLOSED to
    // the last good rail when the hide-list read was contended (#499
    // mechanism 2; see `resolve_active_entries` for the full decision).
    let mut entries = LAST_GOOD_RAIL
        .with(|cache| resolve_active_entries(entries, hidden.as_ref(), &cache.borrow()));

    // Contended DM_LAST_SEEN: the pass computed every entry with
    // unread = 0, and unread is the PRIMARY sort key — carry each
    // pair's last-known unread forward from the last good rail so rows
    // don't transiently drop from the top and snap back. (When `hidden`
    // was ALSO contended, `entries` IS the last good rail and the
    // backfill is a no-op.)
    if last_seen.is_none() {
        LAST_GOOD_RAIL.with(|cache| backfill_unread_from_last_good(&mut entries, &cache.borrow()));
    }

    // Unread threads first, then most-recent, then a deterministic
    // tiebreak (#499 mechanism 4). Re-sorting the cached list on the
    // contended path is a no-op because the order is total.
    sort_rail_entries(&mut entries);

    // Cache only a FULLY-clean pass — hide-list AND unread cutoffs both
    // read cleanly. A hidden-contended pass would cache the cache (no
    // information), and a last_seen-contended pass would poison the
    // unread counts the backfill above reads from.
    if hidden.is_some() && last_seen.is_some() {
        set_last_good_rail(&entries);
    }

    entries
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

    // ===== archive_cutoff (issue freenet/river#526) =====

    /// The rendered row wins when it is ahead of live state.
    #[test]
    fn archive_cutoff_takes_the_row_when_it_leads() {
        assert_eq!(archive_cutoff(1_500, Some(1_000)), 1_500);
    }

    /// Live state wins when it is ahead — the row can lag behind a DM that
    /// landed in `ROOMS` after the memo's last clean pass.
    #[test]
    fn archive_cutoff_takes_live_when_it_leads() {
        assert_eq!(archive_cutoff(1_000, Some(1_500)), 1_500);
    }

    /// A contended `ROOMS` read degrades to the rendered row, never to zero.
    #[test]
    fn archive_cutoff_falls_back_to_the_row_when_live_is_unreadable() {
        assert_eq!(archive_cutoff(1_200, None), 1_200);
    }

    /// A thread the peer has never written to archives at 0, which the
    /// `<=` filter still treats as hidden.
    #[test]
    fn archive_cutoff_is_zero_when_peer_never_wrote() {
        assert_eq!(archive_cutoff(0, Some(0)), 0);
    }

    /// The invariant the gate depends on: the cutoff is never BELOW the
    /// pair's true inbound maximum, so no already-existing inbound DM can
    /// look newer than the archive when it re-lands.
    ///
    /// An earlier revision clamped the cutoff at `now + MAX_DM_FUTURE_SKEW_SECS`,
    /// which broke exactly this for a peer whose clock leads ours: Archive
    /// became a visible no-op AND the next sweep-and-re-offer destroyed the
    /// persisted archive.
    #[test]
    fn archive_cutoff_is_never_below_the_true_inbound_max() {
        let now = 5_000u64;
        let skew = river_core::room_state::direct_messages::MAX_DM_FUTURE_SKEW_SECS;
        // A peer stamping well beyond the skew bound.
        let forged = now + skew + 100_000;
        let cutoff = archive_cutoff(forged, Some(forged));
        assert!(
            cutoff >= forged,
            "the cutoff must cover every inbound DM that exists, however \
             implausibly stamped - otherwise the rail keeps showing the row \
             and a re-land destroys the archive (#526)"
        );
    }

    /// Issue freenet/river#526: `archive_row` must feed `archive_cutoff` the
    /// row's INBOUND clock and the LIVE recompute, and must not reintroduce a
    /// wall-clock term. `archive_row` needs the Dioxus runtime, so this is a
    /// source pin.
    #[test]
    fn archive_row_uses_inbound_and_live_cutoff() {
        let src = dm_rail_production_stripped();
        let marker = "fnarchive_row(";
        let split_at = src
            .find(marker)
            .expect("archive_row must exist - this pin is targeting the wrong path");
        let rest = &src[split_at + marker.len()..];
        // `ArchiveToastView` is the next item AFTER `archive_row`; the
        // previously-used `build_archive_toast` is defined BEFORE it, so that
        // bound never matched and the "segment" was the rest of the file.
        let end = rest
            .find("fnArchiveToastView")
            .expect("ArchiveToastView must follow archive_row - bound is stale");
        let seg = &rest[..end];

        assert!(
            seg.contains("archive_cutoff(row_last_inbound_ts,current_last_inbound_ts(&room,peer))"),
            "archive_row must build the cutoff from the row's inbound clock \
             and the LIVE recompute (#526)."
        );
        assert!(
            !seg.contains("unix_now_secs()"),
            "the cutoff must carry no wall-clock term (#526): clamping one \
             side of the comparison breaks `every existing inbound DM is <= \
             cutoff` and both re-opens the bug and makes Archive a no-op."
        );
    }

    /// The row must hand `archive_row` its INBOUND clock. `DmRailEntry` still
    /// carries `last_any_ts` for sorting, so passing the wrong field compiles
    /// silently and restores the pre-fix cutoff.
    #[test]
    fn rail_row_passes_the_inbound_clock_to_archive_row() {
        let src = dm_rail_production_stripped();
        assert!(
            src.contains("archive_row(room,peer,&nickname,last_inbound_ts)"),
            "DmRailRow must pass last_inbound_ts to archive_row (#526); \
             last_any_ts includes our own outbound DMs and their local clock."
        );
        assert!(
            src.contains("letlast_inbound_ts=entry.last_inbound_ts;"),
            "the row must read the inbound clock off the entry (#526)."
        );
    }

    /// Both archived-view projections must feed the INBOUND clock, or the
    /// "Archived (N)" badge and the panel desynchronise from the rail: a
    /// thread with a newer outbound would drop out of the panel while the rail
    /// still hides it, leaving it invisible in both places with no un-archive.
    #[test]
    fn archived_projections_use_the_inbound_clock() {
        let src = dm_rail_production_stripped();
        assert_eq!(
            src.matches("last_inbound_ts.insert((*owner_vk,peer),activity.last_inbound_ts);")
                .count(),
            2,
            "build_archived_view and current_archived_count must both project \
             the inbound clock (#526)."
        );
        assert!(
            src.contains("last_inbound_ts:activity.last_inbound_ts,"),
            "build_view must carry the inbound clock onto each DmRailEntry (#526)."
        );
    }

    /// Issue freenet/river#526: `archive_cutoff` must carry NO wall-clock
    /// term.
    ///
    /// This has to be a source pin. `archive_cutoff` takes no injectable
    /// `now`, so a behavioural test can only feed it fixture timestamps —
    /// which sit astronomically below real `unix_now_secs()`, making any
    /// re-introduced `.min(now + skew)` inert and the test green. A clamp
    /// mutation survived exactly that way in an earlier round.
    ///
    /// Why it must stay clamp-free: the rail filter compares the UNCLAMPED
    /// observed inbound clock against this value. Clamping only this side
    /// breaks `every existing inbound DM is <= cutoff`, which makes Archive a
    /// visible no-op that still shows a success toast AND lets the next
    /// sweep-and-re-offer destroy the persisted archive. The forged-timestamp
    /// concern belongs on the INCOMING timestamp in
    /// `should_unhide_for_inbound_dm`, where it cannot break the invariant.
    #[test]
    fn archive_cutoff_carries_no_wall_clock_term() {
        let src = dm_rail_production_stripped();
        let marker = "fnarchive_cutoff(";
        let split_at = src
            .find(marker)
            .expect("archive_cutoff must exist - this pin targets the wrong path");
        let rest = &src[split_at + marker.len()..];
        let end = rest
            .find("fnarchive_row(")
            .expect("archive_row must follow archive_cutoff - bound is stale");
        let seg = &rest[..end];

        assert!(
            !seg.contains("unix_now_secs()"),
            "archive_cutoff must not clamp against wall-clock time (#526): \
             clamping one side of the comparison re-opens the bug and turns \
             Archive into a no-op that still reports success."
        );
        assert!(
            !seg.contains("MAX_DM_FUTURE_SKEW_SECS"),
            "the skew bound belongs on the INCOMING timestamp in \
             should_unhide_for_inbound_dm, not on the cutoff (#526)."
        );
        assert!(
            seg.contains("std::cmp::max(row_inbound_ts,live_inbound_ts.unwrap_or(0))"),
            "archive_cutoff must be the plain max of the rendered and live \
             inbound clocks (#526)."
        );
    }

    /// Issue freenet/river#526: the Undo toast and the Archived-panel
    /// Un-archive are explicit USER ACTIONS and must keep calling the
    /// UNCONDITIONAL `unhide_dm_thread`.
    ///
    /// Routing them through `unhide_dm_thread_if_dm_is_newer` for
    /// "consistency" would make both silent no-ops: at the moment the user
    /// clicks Undo, the thread's inbound clock is by construction at or below
    /// the cutoff just written, so the gate returns false and the entry
    /// survives. The button would appear to do nothing.
    #[test]
    fn explicit_user_actions_keep_the_unconditional_unhide() {
        let src = dm_rail_production_stripped();
        assert!(
            !src.contains(&format!("{}{}", "unhide_dm_thread_if_dm_is_newer", "(")),
            "the rail's user-action unhides must NOT use the cutoff-gated form \
             (#526) - Undo and Un-archive would become silent no-ops."
        );
        assert_eq!(
            src.matches(&format!("{}{}", "unhide_dm_thread", "("))
                .count(),
            2,
            "exactly two unconditional unhide call sites are expected in the \
             rail: the Undo toast and the Archived-panel Un-archive."
        );
    }

    /// The rail filter must compare the INBOUND clock, never `last_any_ts`.
    #[test]
    fn rail_filter_compares_inbound_timestamp() {
        let src = dm_rail_production_stripped();
        assert!(
            src.contains("is_thread_hidden_for(hidden,&e.room,e.peer,e.last_inbound_ts)"),
            "filter_rail_entries must compare last_inbound_ts (#526). Using \
             last_any_ts lets our own outbound DM's local-clock timestamp \
             decide whether a peer's message is visible."
        );
    }

    // ===== max_inbound_ts_from_triples =====

    #[test]
    fn max_inbound_ts_ignores_outbound_and_third_parties() {
        let me = MemberId(FastHash(1));
        let peer = MemberId(FastHash(2));
        let other = MemberId(FastHash(3));
        let msgs = vec![
            (peer, me, 100),    // inbound  - counts
            (me, peer, 900),    // outbound - must NOT count
            (other, me, 800),   // inbound from someone else - must NOT count
            (peer, other, 700), // third-party - must NOT count
            (peer, me, 250),    // inbound  - counts, newest
        ];
        assert_eq!(max_inbound_ts_from_triples(msgs, me, peer), 250);
    }

    #[test]
    fn max_inbound_ts_is_zero_when_peer_never_wrote() {
        let me = MemberId(FastHash(1));
        let peer = MemberId(FastHash(2));
        // A thread that is entirely our own outbound DMs.
        let msgs = vec![(me, peer, 500), (me, peer, 900)];
        assert_eq!(max_inbound_ts_from_triples(msgs, me, peer), 0);
    }

    /// The reply-then-archive scenario that this revision exists to fix: the
    /// thread's newest message is OUTBOUND, so `last_any_ts` is our own clock.
    /// Archiving must not hide a peer DM whose timestamp sits below it.
    #[test]
    fn filter_does_not_hide_thread_on_a_newer_outbound_message() {
        let room = sk(1).verifying_key();
        // Peer's newest inbound is 1010; our reply at 1012 is newer overall.
        let e = entry_with_inbound(room, 11, 1_012, 1_010, 1);
        // Archived at 1000 (the peer's previous DM). Their 1010 must revive it.
        let mut hidden = HashMap::new();
        hidden.insert(
            (room, e.peer),
            HiddenDmThreadEntry {
                room_owner_vk: room.to_bytes(),
                peer: e.peer,
                hidden_at_ts: 1_000,
            },
        );
        let out = filter_rail_entries(vec![e.clone()], &hidden);
        assert_eq!(
            out.len(),
            1,
            "the peer's DM at 1010 is past the 1000 cutoff, so the thread must \
             be visible even though our own reply at 1012 is newer (#526)"
        );

        // And the inbound clock, not the outbound one, is what decides: with
        // the cutoff above the peer's newest inbound, it stays archived.
        let mut hidden2 = HashMap::new();
        hidden2.insert(
            (room, e.peer),
            HiddenDmThreadEntry {
                room_owner_vk: room.to_bytes(),
                peer: e.peer,
                hidden_at_ts: 1_010,
            },
        );
        assert!(
            filter_rail_entries(vec![e], &hidden2).is_empty(),
            "our outbound at 1012 must NOT drag the thread back into the rail"
        );
    }

    fn entry(room: VerifyingKey, peer_seed: i64, last_any_ts: u64, unread: usize) -> DmRailEntry {
        DmRailEntry {
            room,
            peer: MemberId(FastHash(peer_seed)),
            peer_nickname: format!("peer-{peer_seed}"),
            room_name: "room".into(),
            last_any_ts,
            // Default fixture: treat the whole thread as inbound, which is
            // what these tests meant before the archive filter became
            // inbound-only (issue freenet/river#526). Cases that need the two
            // to differ use `entry_with_inbound`.
            last_inbound_ts: last_any_ts,
            unread,
        }
    }

    /// Rail entry whose newest message is OUTBOUND: `last_any_ts` is above
    /// `last_inbound_ts`. This is the shape that made an in-flight inbound DM
    /// invisible before the archive filter became inbound-only (#526).
    fn entry_with_inbound(
        room: VerifyingKey,
        peer_seed: i64,
        last_any_ts: u64,
        last_inbound_ts: u64,
        unread: usize,
    ) -> DmRailEntry {
        DmRailEntry {
            last_inbound_ts,
            ..entry(room, peer_seed, last_any_ts, unread)
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

    /// Issue freenet/river#526 INVERTED this test. It previously asserted that
    /// an outbound DM revives a hidden thread through
    /// `last_any_ts > hidden_at_ts` — which is precisely the behaviour that
    /// made an in-flight INBOUND DM invisible, because our own outbound
    /// timestamp is our LOCAL wall clock and it was deciding whether a peer's
    /// message showed.
    ///
    /// Outbound sends still revive the thread, but through the unconditional
    /// `unhide_dm_thread` in `do_send` / `send_structured_dm` — i.e. by
    /// REMOVING the entry, which `filter_rail_entries_unhide_reappears`
    /// covers. The filter itself must ignore outbound entirely.
    #[test]
    fn filter_rail_entries_newer_outbound_does_not_revive_hidden() {
        let room = sk(1).verifying_key();
        // Our reply at 1_500 is the newest message; the peer's newest is 900.
        let entries = vec![entry_with_inbound(room, 11, 1_500, 900, 0)];
        let hidden = HashMap::from([hidden_at(room, 11, 1_000)]);

        assert!(
            filter_rail_entries(entries, &hidden).is_empty(),
            "our own outbound DM at 1500 must NOT revive the thread: the \
             archive clock is inbound-only, and the peer's newest (900) is \
             below the 1000 cutoff (#526)"
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

    /// M2 fix from PR #275 skeptical review: two `build_archive_toast`
    /// calls at the SAME `now_ms` must produce distinct identity tokens.
    /// Before the fix, identity was `expires_at_ms` — same-millisecond
    /// clicks collided and the first toast's timeout could clear the
    /// second toast early. Tokens come from an atomic counter so
    /// collisions are structurally impossible.
    #[test]
    fn build_archive_toast_same_ms_has_distinct_tokens() {
        let room = sk(1).verifying_key();
        let peer = MemberId(FastHash(11));
        let t1 = build_archive_toast(room, peer, "alice", 1_000);
        let t2 = build_archive_toast(room, peer, "alice", 1_000);
        assert_eq!(t1.expires_at_ms, t2.expires_at_ms);
        assert_ne!(
            t1.token, t2.token,
            "back-to-back archives at the same millisecond must have \
             distinct identity tokens — the auto-dismiss timeout's \
             'is it still mine?' check compares tokens, not expiries"
        );
    }

    /// Issue #499 mechanism 1: a contended `DM_LAST_SEEN` read
    /// (`last_seen = None`) must degrade to "unread = 0 for this pass"
    /// — the entries are still produced with correct recency — instead
    /// of aborting the whole rail build (the old `?` blanked the entire
    /// active list while "Archived (N)" stayed put).
    #[test]
    fn accumulate_peer_activity_contended_last_seen_degrades_unread_to_zero() {
        let room = sk(1).verifying_key();
        let self_id = MemberId(FastHash(1));
        let peer = MemberId(FastHash(2));
        // Two inbound DMs, never read (no cutoff recorded anywhere).
        let msgs = vec![(peer, self_id, 100u64), (peer, self_id, 200u64)];

        // Clean pass: both count as unread.
        let clean = accumulate_peer_activity(&room, self_id, msgs.clone(), Some(&HashMap::new()));
        assert_eq!(
            clean[&peer].unread, 2,
            "precondition: clean pass counts unread"
        );
        assert_eq!(clean[&peer].last_any_ts, 200);

        // Contended pass: entry still produced, unread degraded to 0.
        let degraded = accumulate_peer_activity(&room, self_id, msgs, None);
        assert_eq!(
            degraded[&peer].unread, 0,
            "contended DM_LAST_SEEN must degrade unread to 0, not drop the entry"
        );
        assert_eq!(
            degraded[&peer].last_any_ts, 200,
            "recency must be unaffected by the unread degrade"
        );
    }

    /// Baseline semantics of the shared accumulator: outbound messages
    /// bump recency but never unread; inbound messages at or below the
    /// per-pair cutoff are read; third-party traffic is skipped
    /// entirely. (Behaviour lifted verbatim from the old inline loop in
    /// `build_view` — this pins the extraction.)
    #[test]
    fn accumulate_peer_activity_applies_cutoffs_and_direction() {
        let room = sk(1).verifying_key();
        let self_id = MemberId(FastHash(1));
        let peer = MemberId(FastHash(2));
        let third_a = MemberId(FastHash(3));
        let third_b = MemberId(FastHash(4));
        let msgs = vec![
            (self_id, peer, 100u64),    // outbound: recency only
            (peer, self_id, 150u64),    // inbound, <= cutoff: read
            (peer, self_id, 250u64),    // inbound, > cutoff: unread
            (self_id, peer, 400u64),    // outbound, NEWEST overall
            (third_a, third_b, 999u64), // not ours: skipped
        ];
        let mut last_seen = HashMap::new();
        last_seen.insert((room, peer), 200u64);

        let result = accumulate_peer_activity(&room, self_id, msgs, Some(&last_seen));
        assert_eq!(
            result.len(),
            1,
            "third-party traffic must not create entries"
        );
        assert_eq!(result[&peer].unread, 1);
        // Recency covers BOTH directions, so our own 400 wins.
        assert_eq!(result[&peer].last_any_ts, 400);
        // The ARCHIVE clock covers inbound only (issue freenet/river#526).
        // The outbound-newest fixture above is what makes this assertion able
        // to fail: hoisting the `last_inbound_ts` bump out of the
        // `is_self_recipient` branch - a plausible "fold the two maxes
        // together" simplification - would yield 400 here and silently restore
        // the bug for the rail, the archived panel and the archived count.
        assert_eq!(
            result[&peer].last_inbound_ts, 250,
            "our own outbound DM must NOT advance the archive clock (#526)"
        );
    }

    /// Clean hide-list read: `resolve_active_entries` applies the #261
    /// archive filter exactly like `filter_rail_entries`.
    #[test]
    fn resolve_active_entries_clean_read_applies_archive_filter() {
        let room = sk(1).verifying_key();
        let live = entry(room, 11, 2_000, 1);
        let archived = entry(room, 22, 1_000, 0);
        let hidden = HashMap::from([hidden_at(room, 22, 1_000)]);

        let out = resolve_active_entries(vec![live.clone(), archived], Some(&hidden), &[]);
        assert_eq!(out, vec![live]);
    }

    /// Issue #499 mechanism 2: a contended `HIDDEN_DM_THREADS` read
    /// must fail CLOSED. The old behaviour skipped the filter and
    /// flashed every archived thread into the active rail for a
    /// render; the fix returns the last successfully-filtered rail —
    /// so the archived thread never appears AND the rail doesn't
    /// collapse to empty.
    #[test]
    fn resolve_active_entries_contended_hidden_never_yields_archived() {
        let room = sk(1).verifying_key();
        let live = entry(room, 11, 2_000, 1);
        let archived = entry(room, 22, 1_000, 0);
        let hidden = HashMap::from([hidden_at(room, 22, 1_000)]);

        // A previous CLEAN pass produced the last-good rail.
        let last_good =
            resolve_active_entries(vec![live.clone(), archived.clone()], Some(&hidden), &[]);
        assert_eq!(
            last_good,
            vec![live.clone()],
            "precondition: clean pass filters"
        );

        // Contended pass over the same candidates.
        let out = resolve_active_entries(vec![live.clone(), archived.clone()], None, &last_good);
        assert!(
            !out.contains(&archived),
            "an archived thread must NEVER appear in the active rail, \
             even while the hide-list signal is contended"
        );
        assert_eq!(
            out,
            vec![live],
            "the rail must keep the last good list rather than collapse to empty"
        );
    }

    /// Cold-start corner of the same degrade: contended hide-list with
    /// no previous clean pass shows nothing — briefly showing an empty
    /// rail is preferable to flashing archived threads into it.
    #[test]
    fn resolve_active_entries_contended_hidden_with_no_history_shows_nothing() {
        let room = sk(1).verifying_key();
        let archived = entry(room, 22, 1_000, 0);
        let out = resolve_active_entries(vec![archived], None, &[]);
        assert!(
            out.is_empty(),
            "with no last-good rail, the contended-hidden pass must show \
             nothing rather than unfiltered (possibly archived) entries"
        );
    }

    /// Primary sort keys are unchanged: unread desc, then recency desc.
    #[test]
    fn sort_rail_entries_orders_unread_then_recency() {
        let room = sk(1).verifying_key();
        let mut entries = vec![
            entry(room, 11, 3_000, 0), // read, most recent
            entry(room, 22, 1_000, 2), // unread, older
            entry(room, 33, 2_000, 0), // read, mid
        ];
        sort_rail_entries(&mut entries);
        assert_eq!(entries[0].peer, MemberId(FastHash(22)), "unread first");
        assert_eq!(entries[1].peer, MemberId(FastHash(11)), "then recency desc");
        assert_eq!(entries[2].peer, MemberId(FastHash(33)));
    }

    /// Issue #499 mechanism 4: entries tied on `(unread, last_any_ts)`
    /// must sort identically regardless of input order. They accumulate
    /// through a fresh `HashMap` each pass, so without the total-order
    /// tiebreak tied threads shuffled position between renders.
    #[test]
    fn sort_rail_entries_ties_are_deterministic_across_input_orders() {
        let room_a = sk(1).verifying_key();
        let room_b = sk(2).verifying_key();
        // Four entries fully tied on both primary sort keys.
        let e1 = entry(room_a, 11, 1_000, 0);
        let e2 = entry(room_a, 22, 1_000, 0);
        let e3 = entry(room_b, 11, 1_000, 0);
        let e4 = entry(room_b, 22, 1_000, 0);

        let mut fwd = vec![e1.clone(), e2.clone(), e3.clone(), e4.clone()];
        let mut rev = vec![e4, e3, e2, e1];
        sort_rail_entries(&mut fwd);
        sort_rail_entries(&mut rev);
        assert_eq!(
            fwd, rev,
            "ties must sort identically regardless of the (HashMap-derived) \
             input order — `sort_by` is stable, so without a total order the \
             output inherits the per-pass-random input order"
        );
    }

    /// Issue #499 mechanism 3: each fallible builder must register an
    /// INFALLIBLE signal read before its first fallible `try_read`. In
    /// Dioxus 0.7 an Err `try_read` registers no subscription and each
    /// memo run clears the previous subscription set, so a builder
    /// whose FIRST read is fallible can come out of one contended poll
    /// with zero subscriptions — permanently frozen (`DmRailSection`
    /// never unmounts). Mirrors the `CURRENT_ROOM.read()` guard in
    /// `room_list.rs`. The anchor is `RAIL_REBUILD_TICK`, which doubles
    /// as the recovery channel: EVERY degraded pass must ALSO schedule
    /// a rail nudge (a deferred tick bump), so recovery is one
    /// macrotask away rather than parked until unrelated user action —
    /// each segment is checked for its full PER-DEGRADE-ARM count of
    /// nudge calls (3 in `build_view`: ROOMS / DM_LAST_SEEN / HIDDEN;
    /// 2 in each archived builder: ROOMS / HIDDEN). A bare "at least
    /// one" check would let a refactor drop the nudge from the ROOMS
    /// arm alone — the most critical arm, since that pass leaves the
    /// memo subscribed ONLY to the tick — while the pin stayed green.
    ///
    /// Source-scrape because the builders need a Dioxus runtime to run.
    /// Conventions: match WHITESPACE-STRIPPED source so rustfmt
    /// reflowing can't fake a result; cut at `mod tests` (this file has
    /// exactly one) so these needles can't satisfy their own check;
    /// bound each builder's segment by the NEXT function head so a
    /// guard in one builder can't vacuously cover another. The
    /// non-test comments deliberately avoid the literal guard/fallible
    /// needles (the tick static's name + `.read()` adjacent, or a
    /// dotted `try_read` call) so a comment can't satisfy the pin
    /// either. Reordering these functions breaks the segment lookups
    /// loudly (expect-panic), not silently.
    #[test]
    fn rail_builders_read_infallible_guard_before_first_fallible_read() {
        let src = include_str!("dm_rail_section.rs");
        let body = &src[..src.find("mod tests").unwrap_or(src.len())];
        let stripped: String = body.chars().filter(|c| !c.is_whitespace()).collect();

        // (builder head, next-function head bounding the segment,
        //  expected schedule_rail_nudge() call count = degrade-arm count)
        let cases = [
            ("fnbuild_archived_view()", "fncurrent_archived_count()", 2),
            ("fncurrent_archived_count()", "fnbuild_view()", 2),
            ("fnbuild_view()", "fnshort_member_id(", 3),
        ];
        for (head, next_head, expected_nudges) in cases {
            let start = stripped
                .find(head)
                .unwrap_or_else(|| panic!("{head} not found — function renamed/removed?"));
            let end = stripped[start..]
                .find(next_head)
                .map(|i| start + i)
                .unwrap_or_else(|| {
                    panic!("{next_head} not found after {head} — functions reordered?")
                });
            let seg = &stripped[start..end];

            let guard = seg.find("RAIL_REBUILD_TICK.read()").unwrap_or_else(|| {
                panic!("{head} lost its infallible RAIL_REBUILD_TICK.read() subscription anchor")
            });
            let fallible = seg.find(".try_read(").unwrap_or_else(|| {
                panic!("{head} has no fallible read — pin needs updating if that's intended")
            });
            assert!(
                guard < fallible,
                "{head}: the infallible RAIL_REBUILD_TICK.read() anchor (at {guard}) must \
                 come BEFORE the first fallible try_read (at {fallible}) — otherwise a \
                 contended poll leaves the memo with zero subscriptions (issue #499)"
            );
            let nudges = seg.matches("schedule_rail_nudge()").count();
            assert!(
                nudges >= expected_nudges,
                "{head} has {nudges} schedule_rail_nudge() call(s), expected at \
                 least {expected_nudges} (one per degrade arm) — EVERY contended-read \
                 arm must queue a deferred tick bump so the memo re-polls one \
                 macrotask later instead of serving stale data until unrelated \
                 user action. Dropping the nudge from even one arm (e.g. the \
                 ROOMS arm, whose pass leaves the memo subscribed only to the \
                 tick) reopens the parked-stale window. If a degrade arm was \
                 deliberately removed, update this count alongside it."
            );
        }
    }

    /// Review follow-up wiring pins for the last-good degrade caches
    /// (same source-scrape conventions as the pin above):
    ///
    /// 1. `build_view` may write `LAST_GOOD_RAIL` only from a FULLY-
    ///    clean pass (`hidden` AND `last_seen` both read cleanly). An
    ///    ungated write from a `last_seen`-contended pass would cache
    ///    unread = 0 for every pair, poisoning the very backfill that
    ///    reads from the cache.
    /// 2. `build_view` must backfill unread from the last good rail on
    ///    a `last_seen`-contended pass — unread is the PRIMARY sort
    ///    key, so a transient 0 reorders the rail (the reviewers
    ///    overturned the earlier "cosmetic" claim).
    /// 3. `current_archived_count` must serve the last clean count on
    ///    contention (never a transient 0, which can unmount the whole
    ///    section via the empty-state early return) and record the
    ///    count on a clean pass.
    #[test]
    fn rail_last_good_caches_have_clean_pass_write_discipline() {
        let src = include_str!("dm_rail_section.rs");
        let body = &src[..src.find("mod tests").unwrap_or(src.len())];
        let stripped: String = body.chars().filter(|c| !c.is_whitespace()).collect();

        let view_start = stripped
            .find("fnbuild_view()")
            .expect("build_view not found");
        let view_end = stripped[view_start..]
            .find("fnshort_member_id(")
            .map(|i| view_start + i)
            .expect("short_member_id must follow build_view");
        let view_seg = &stripped[view_start..view_end];
        assert!(
            view_seg
                .contains("ifhidden.is_some()&&last_seen.is_some(){set_last_good_rail(&entries);}"),
            "build_view's LAST_GOOD_RAIL write must be gated on BOTH hidden and \
             last_seen reading cleanly — an ungated (or half-gated) write lets a \
             degraded pass poison the cache the degrade paths serve from"
        );
        assert!(
            view_seg.contains(
                "iflast_seen.is_none(){LAST_GOOD_RAIL.with(|cache|backfill_unread_from_last_good("
            ),
            "build_view must backfill unread from LAST_GOOD_RAIL when last_seen \
             was contended — without it the unread-first sort transiently \
             reorders (unread is the primary sort key, not a cosmetic badge)"
        );

        let count_start = stripped
            .find("fncurrent_archived_count()")
            .expect("current_archived_count not found");
        let count_end = stripped[count_start..]
            .find("fnbuild_view()")
            .map(|i| count_start + i)
            .expect("build_view must follow current_archived_count");
        let count_seg = &stripped[count_start..count_end];
        assert!(
            count_seg.contains("schedule_rail_nudge();returnlast_good_archived_count();"),
            "current_archived_count's contended arms must nudge and serve the \
             last clean count — a transient 0 can combine with an empty thread \
             list to unmount the whole DIRECT MESSAGES section"
        );
        assert!(
            count_seg.contains("set_last_good_archived_count(count);"),
            "current_archived_count must record the count from a clean pass — \
             otherwise the contended arms serve a stale default forever"
        );
    }

    /// Review follow-up (issue #499): a `last_seen`-contended pass
    /// computes unread = 0 for every entry, and unread is the PRIMARY
    /// sort key — the backfill must restore each pair's last-known
    /// unread so rows keep their order across the degraded pass.
    #[test]
    fn backfill_unread_from_last_good_preserves_unread_and_order() {
        let room = sk(1).verifying_key();
        // Last good rail (fully-clean pass): peer 11 has 3 unread and
        // sorts first; peer 22 read, more recent.
        let mut last_good = vec![entry(room, 11, 1_000, 3), entry(room, 22, 2_000, 0)];
        sort_rail_entries(&mut last_good);
        assert_eq!(
            last_good[0].peer,
            MemberId(FastHash(11)),
            "precondition: unread row sorts first on the clean pass"
        );

        // Degraded pass: same pairs, unread zeroed by the contended
        // DM_LAST_SEEN read. Without the backfill, peer 22 (more
        // recent) would sort first — a visible transient reorder.
        let mut degraded = vec![entry(room, 11, 1_000, 0), entry(room, 22, 2_000, 0)];
        backfill_unread_from_last_good(&mut degraded, &last_good);
        sort_rail_entries(&mut degraded);
        assert_eq!(
            degraded[0].peer,
            MemberId(FastHash(11)),
            "backfill must restore unread so the degraded pass keeps the \
             clean pass's order"
        );
        assert_eq!(
            degraded[0].unread, 3,
            "cached unread must be carried forward"
        );
        assert_eq!(degraded[1].unread, 0);
    }

    /// Pairs absent from the last good rail keep unread = 0 — the
    /// backfill has no better information for them (e.g. a thread whose
    /// first message arrived during the contended pass).
    #[test]
    fn backfill_unread_from_last_good_leaves_uncached_pairs_at_zero() {
        let room = sk(1).verifying_key();
        let last_good = vec![entry(room, 11, 1_000, 3)];
        let mut degraded = vec![entry(room, 11, 1_000, 0), entry(room, 22, 2_000, 0)];
        backfill_unread_from_last_good(&mut degraded, &last_good);
        assert_eq!(degraded[0].unread, 3);
        assert_eq!(
            degraded[1].unread, 0,
            "a pair the cache has never seen must stay at unread = 0"
        );
        // Recency is never backfilled — the current pass computed it
        // from live room state.
        assert_eq!(degraded[1].last_any_ts, 2_000);
    }

    /// The archived-count last-good cell round-trips (thread_local, so
    /// each test thread starts at the default 0). Wiring — clean-pass
    /// writes, contended-pass serves — is pinned by
    /// `rail_last_good_caches_have_clean_pass_write_discipline`.
    #[test]
    fn last_good_archived_count_round_trip() {
        assert_eq!(last_good_archived_count(), 0, "default is 0");
        set_last_good_archived_count(7);
        assert_eq!(last_good_archived_count(), 7);
        set_last_good_archived_count(0);
        assert_eq!(last_good_archived_count(), 0);
    }

    // ---- #462: archive ✕ must be reachable on touch devices ----
    //
    // The archive reveal can't be driven end-to-end in Playwright through the
    // real rail (the example-data build ships no DMs, so no row renders — see
    // `ui/tests/dm-archive-ux.spec.ts`), so the CASCADE is pinned in the
    // browser by `dm-archive-touch.spec.ts`, which probes the shipped
    // stylesheets directly. These two pin the SOURCE side: that the markup
    // still carries the hooks that stylesheet targets, and that the rule is
    // still inside a pointer-capability media query rather than at top level.
    //
    // Both cut production off at `mod tests` for the reason spelled out on
    // `rail_builders_read_infallible_guard_before_first_fallible_read` above:
    // `include_str!` reads this file back, assert MESSAGES included, so an
    // uncut needle is satisfied by the test that is supposed to be checking it.

    const DM_RAIL_SRC: &str = include_str!("dm_rail_section.rs");
    const MAIN_CSS: &str = include_str!("../../../assets/main.css");

    /// This file's production half, with the test module (and its assert
    /// messages) cut off.
    fn dm_rail_production() -> &'static str {
        &DM_RAIL_SRC[..DM_RAIL_SRC
            .find("mod tests")
            .expect("dm_rail_section.rs has exactly one `mod tests`")]
    }

    /// [`dm_rail_production`] with all whitespace removed, so a source pin
    /// survives a rustfmt pass that re-wraps the code it matches.
    fn dm_rail_production_stripped() -> String {
        dm_rail_production()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    /// The archive ✕ must gate its reveal on pointer capability, not the
    /// viewport breakpoint. Gating on the breakpoint was the #462 bug: a
    /// tablet clears the breakpoint yet has no hover pointer, so the button
    /// became invisible-but-tappable there.
    #[test]
    fn archive_button_not_viewport_gated() {
        let prod = dm_rail_production();
        // Whitespace-stripped and anchored on the CLASS ATTRIBUTE, not a bare
        // substring: `dm-archive-btn` also appears in prose above, so a
        // file-wide `contains` stays green even if the class is deleted from
        // the markup — which is the one mutation this test exists to catch.
        let squashed: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            squashed.contains(concat!("class:\"", "dm-archive-btn")),
            "the archive ✕ must carry the `dm-archive-btn` class so main.css \
             can force it visible on touch (#462)"
        );
        assert!(
            squashed.contains(concat!("class:\"", "dm-rail-row-btn")),
            "the rail row must carry `dm-rail-row-btn` so its right padding \
             widens on touch and the enlarged ✕ does not overlap the nickname \
             or unread badge (#462)"
        );
        // The reveal itself, in the class attribute rather than in prose.
        assert!(
            squashed.contains("group-hover:opacity-100"),
            "the archive ✕ must reveal via `group-hover` (pointer-capability \
             gated) rather than a viewport breakpoint (#462)"
        );
        assert!(
            squashed.contains("opacity-0"),
            "the archive ✕ must be `opacity-0` at rest, or the hover reveal on \
             a mouse-only desktop is meaningless (#462)"
        );
        // Reconstruct the forbidden token from fragments so this file (which
        // `include_str!` reads back) does not itself contain the literal.
        assert!(
            !prod.contains(concat!("md:", "opacity-0")),
            "the archive ✕ must not gate its reveal on the md viewport \
             breakpoint — a tablet clears the breakpoint but has no hover \
             pointer, which is exactly the #462 invisible-but-tappable bug"
        );
    }

    /// main.css must force `.dm-archive-btn` visible with a real tap target
    /// inside a POINTER-CAPABILITY media query, and widen the row padding.
    ///
    /// The containment check is a brace scan, not a prefix search. `main.css`
    /// already opens a touch media query for the #402 message-action bar well
    /// above this rule, so "some `@media` opens somewhere earlier in the file"
    /// is satisfied by ANY placement below it — including moving the rule out
    /// to top level, which would force the ✕ permanently visible on a
    /// mouse-only desktop and silently destroy the hover reveal.
    #[test]
    fn touch_devices_reveal_archive_button() {
        let idx = MAIN_CSS
            .find(".dm-archive-btn")
            .expect("main.css must style `.dm-archive-btn` (#462)");

        // Walk every `@media` before the rule, tracking brace depth, and keep
        // the innermost one still open at `idx`.
        let mut enclosing: Option<&str> = None;
        let mut depth_of: Vec<(usize, &str)> = Vec::new();
        let mut depth = 0usize;
        let mut pending: Option<&str> = None;
        for (i, ch) in MAIN_CSS[..idx].char_indices() {
            if MAIN_CSS[i..].starts_with("@media") {
                let rest = &MAIN_CSS[i..];
                let brace = rest.find('{').unwrap_or(0);
                pending = Some(rest[..brace].trim());
            }
            match ch {
                '{' => {
                    depth += 1;
                    if let Some(q) = pending.take() {
                        depth_of.push((depth, q));
                    }
                }
                '}' => {
                    depth_of.retain(|(d, _)| *d < depth);
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
        }
        if let Some((_, q)) = depth_of.last() {
            enclosing = Some(q);
        }
        let query = enclosing.unwrap_or_else(|| {
            panic!(
                "`.dm-archive-btn` is at top level in main.css — it must sit \
                 inside a pointer-capability media query, or the ✕ is forced \
                 permanently visible on a mouse-only desktop and the hover \
                 reveal is gone (#462)"
            )
        });
        assert!(
            query.contains("any-pointer: coarse"),
            "the rule enclosing `.dm-archive-btn` is `{query}`, which must \
             include `any-pointer: coarse`. `hover: none` alone reports the \
             PRIMARY input, so a touchscreen laptop answers `hover: hover` and \
             keeps an invisible-but-tappable button — the #462 hazard moved to \
             another device class"
        );

        // Slice by CHAR boundary, not byte arithmetic: main.css contains ✕, —
        // and ⋮ in its comments, so a fixed byte window can land mid-codepoint
        // and panic with an error that looks nothing like the real failure.
        let rule = MAIN_CSS[idx..]
            .split_once('}')
            .map(|(head, _)| head)
            .unwrap_or(&MAIN_CSS[idx..]);
        assert!(
            rule.contains("opacity: 1"),
            "the touch rule must set the archive ✕ to full opacity (#462)"
        );
        assert!(
            rule.contains("min-height"),
            "the touch rule must give the archive ✕ a real tap target (#462)"
        );
        assert!(
            MAIN_CSS.contains(".dm-rail-row-btn"),
            "the row padding must widen on touch so the enlarged ✕ doesn't \
             overlap the nickname / unread badge (#462)"
        );
    }
}
