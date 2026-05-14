//! DM inbox: lists every open DM thread the local user has in the current
//! room, with an unread-count badge derived from [`super::DM_LAST_SEEN`].
//! Click a row -> opens [`super::dm_thread_modal::DmThreadModal`].

use crate::components::app::{CURRENT_ROOM, ROOMS};
use crate::components::direct_messages::{
    open_dm_thread, DM_INBOX_OPEN, DM_LAST_SEEN, OPEN_DM_THREAD,
};
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use river_core::room_state::member::MemberId;
use std::collections::HashMap;

#[component]
pub fn DmInboxModal() -> Element {
    if !*DM_INBOX_OPEN.read() {
        return rsx! {};
    }
    let Some(room) = CURRENT_ROOM.read().owner_key else {
        return rsx! {};
    };

    let view = use_memo(move || build_view(room));

    let close = move |_| {
        crate::util::defer(move || {
            *DM_INBOX_OPEN.write() = false;
        });
    };

    let view_value = view.read();
    let Some(threads) = view_value.as_ref() else {
        return rsx! {};
    };

    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center",
            div {
                class: "absolute inset-0 bg-black/50",
                onclick: close,
            }
            div {
                class: "relative z-10 w-full max-w-md mx-4 bg-panel rounded-xl shadow-xl border border-border max-h-[80vh] flex flex-col",
                div { class: "flex items-center justify-between px-5 py-4 border-b border-border",
                    h2 { class: "text-lg font-semibold text-text", "Direct Messages" }
                    button {
                        class: "p-1 text-text-muted hover:text-text transition-colors text-xl",
                        onclick: close,
                        "✕"
                    }
                }
                div { class: "flex-1 overflow-y-auto px-2 py-3",
                    if threads.is_empty() {
                        p { class: "text-sm text-text-muted px-3",
                            "No direct messages yet. Open a member from the right panel and click \"Send Direct Message\" to start one."
                        }
                    } else {
                        for entry in threads.iter() {
                            DmInboxRow {
                                key: "{entry.peer}",
                                room: room,
                                entry: entry.clone()
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn DmInboxRow(room: VerifyingKey, entry: InboxEntry) -> Element {
    let open = {
        let peer = entry.peer;
        move |_| {
            // Close inbox + open the thread modal.
            crate::util::defer(move || {
                *DM_INBOX_OPEN.write() = false;
                *OPEN_DM_THREAD.write() = Some((room, peer));
            });
            open_dm_thread(room, peer); // belt-and-braces: ensure thread modal renders
        }
    };
    rsx! {
        button {
            class: "w-full flex items-center justify-between gap-3 px-3 py-2 rounded-lg hover:bg-surface transition-colors text-left",
            onclick: open,
            div { class: "flex-1 min-w-0",
                div { class: "text-sm font-medium text-text truncate", "{entry.peer_nickname}" }
                div { class: "text-xs text-text-muted truncate",
                    "Last DM: {entry.last_timestamp_label}"
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

#[derive(Clone, PartialEq)]
struct InboxEntry {
    peer: MemberId,
    peer_nickname: String,
    /// Unix timestamp of the most recent message either direction; 0 if
    /// none. Drives the sort order so threads without any messages sink
    /// to the bottom regardless of nickname.
    last_any_ts: u64,
    last_timestamp_label: String,
    unread: usize,
}

fn build_view(room: VerifyingKey) -> Option<Vec<InboxEntry>> {
    let rooms = ROOMS.try_read().ok()?;
    let room_data = rooms.map.get(&room)?;
    let self_id = MemberId::from(&room_data.self_sk.verifying_key());

    // Nickname lookup (skip the local user — we render only the peers).
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

    // Group every DM the local user is party to by counterparty.
    struct Acc {
        last_inbound_ts: u64,
        last_any_ts: u64,
        unread: usize,
    }
    let mut per_peer: HashMap<MemberId, Acc> = HashMap::new();
    let last_seen_snapshot = DM_LAST_SEEN.read().clone();
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
        let entry = per_peer.entry(peer).or_insert(Acc {
            last_inbound_ts: 0,
            last_any_ts: 0,
            unread: 0,
        });
        entry.last_any_ts = entry.last_any_ts.max(msg.message.timestamp);
        if is_self_recipient {
            entry.last_inbound_ts = entry.last_inbound_ts.max(msg.message.timestamp);
            let cutoff = last_seen_snapshot.get(&(room, peer)).copied().unwrap_or(0);
            if msg.message.timestamp > cutoff {
                entry.unread += 1;
            }
        }
    }

    let mut entries: Vec<InboxEntry> = per_peer
        .into_iter()
        .map(|(peer, acc)| InboxEntry {
            peer,
            peer_nickname: nicknames
                .get(&peer)
                .cloned()
                .unwrap_or_else(|| short_member_id(&peer)),
            last_any_ts: acc.last_any_ts,
            last_timestamp_label: if acc.last_any_ts == 0 {
                "(no messages)".to_string()
            } else {
                format_local_time(acc.last_any_ts)
            },
            unread: acc.unread,
        })
        .collect();

    // Unread first, then most-recent message first. Sort on the u64
    // timestamp rather than the formatted label so threads with no
    // messages don't lexically outrank dated ones, and so two threads
    // whose newest message falls in the same minute order by actual
    // seconds.
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

fn format_local_time(unix_secs: u64) -> String {
    let st = std::time::SystemTime::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(unix_secs))
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let dt: chrono::DateTime<chrono::Utc> = st.into();
    let local: chrono::DateTime<chrono::Local> = dt.into();
    local.format("%Y-%m-%d %H:%M").to_string()
}
