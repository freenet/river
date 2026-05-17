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

#[derive(Clone, PartialEq)]
struct DmRailEntry {
    room: VerifyingKey,
    peer: MemberId,
    peer_nickname: String,
    room_name: String,
    last_any_ts: u64,
    unread: usize,
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

        for (peer, acc) in per_peer {
            // Issue freenet/river#261: skip threads the local user has
            // hidden, unless a newer message has arrived since the
            // hide. `is_thread_hidden_for` does the strict `<=` check
            // on `hidden_at_ts`. An outbound DM the user sends to a
            // hidden peer also revives the thread because the new
            // message's timestamp is `> hidden_at_ts` (see
            // `dm_thread_modal::DmThreadModalBody::do_send` — same
            // `unix_now()` shim used for both the message and the
            // hide cutoff).
            if let Some(ref hidden_map) = hidden {
                if is_thread_hidden_for(hidden_map, owner_vk, peer, acc.last_any_ts) {
                    continue;
                }
            }
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
