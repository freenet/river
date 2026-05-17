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
//! Hidden when empty so the rail doesn't show an empty section on first
//! load. Sorts unread threads first, then by most-recent message time.

use crate::components::app::ROOMS;
use crate::components::direct_messages::{
    is_thread_hidden_for, open_dm_thread, DM_LAST_SEEN, HIDDEN_DM_THREADS,
};
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::prelude::*;
use dioxus_free_icons::{icons::fa_solid_icons::FaEnvelope, Icon};
use ed25519_dalek::VerifyingKey;
use river_core::chat_delegate::HiddenDmThreadEntry;
use river_core::room_state::member::MemberId;
use std::collections::HashMap;

#[component]
pub fn DmRailSection() -> Element {
    let threads = use_memo(move || build_view().unwrap_or_default());
    let threads_value = threads.read().clone();

    if threads_value.is_empty() {
        return rsx! {};
    }

    rsx! {
        div { class: "px-4 py-2 flex items-center justify-between border-t border-border mt-2",
            h2 { class: "text-sm font-semibold text-text-muted uppercase tracking-wide flex items-center gap-2",
                Icon { width: 14, height: 14, icon: FaEnvelope }
                span { "Direct Messages" }
            }
        }
        ul { class: "px-2 py-1 space-y-0.5",
            for entry in threads_value.iter() {
                DmRailRow { key: "{entry.room:?}_{entry.peer}", entry: entry.clone() }
            }
        }
    }
}

#[component]
fn DmRailRow(entry: DmRailEntry) -> Element {
    let room = entry.room;
    let peer = entry.peer;
    let click = move |_| {
        open_dm_thread(room, peer);
    };
    rsx! {
        li {
            button {
                class: "w-full text-left px-3 py-1.5 rounded-lg text-sm transition-colors text-text hover:bg-surface flex items-center gap-2",
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
        // and the "outbound revives" invariant that's checked by
        // `outbound_message_revives_hidden_thread` below.
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
}
