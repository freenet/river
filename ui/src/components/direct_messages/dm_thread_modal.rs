//! Per-pair DM thread modal: decrypts inbound DMs, composes outbound ones,
//! and offers a "Purge thread" button that produces a fresh
//! `AuthorizedRecipientPurges` envelope tombstoning every inbound DM in the
//! current view.

use crate::components::app::{mark_needs_sync, ROOMS};
use crate::components::direct_messages::{mark_thread_read, DM_DRAFT, OPEN_DM_THREAD};
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::direct_messages::{
    advance_recipient_purges, compose_direct_message, open_direct_message, pair_message_count,
    DirectMessagesDelta, PurgeToken, MAX_DM_CIPHERTEXT_BYTES, MAX_DM_MESSAGES_PER_PAIR,
};
use river_core::room_state::member::MemberId;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use std::time::{SystemTime, UNIX_EPOCH};

/// Loose body cap (in bytes) to keep us under `MAX_DM_CIPHERTEXT_BYTES` once
/// envelope overhead is accounted for. 32 KiB - 256 byte safety margin.
const DM_BODY_BYTE_CAP: usize = MAX_DM_CIPHERTEXT_BYTES - 256;

/// Result of applying an outbound DM to the local `ROOMS` state. The
/// `send` closure uses this to map back to a user-facing error after the
/// write-lock drops.
enum ApplyOutcome {
    Applied,
    /// Room was unloaded between compose and apply.
    RoomGone,
    /// A concurrent inbound (or re-rendered state) pushed the per-pair
    /// pair count up to the cap before our delta could land. Better to
    /// surface this here than to let the contract silently drop the
    /// message.
    CapHit,
    DeltaFailed,
}

#[component]
pub fn DmThreadModal() -> Element {
    let active = *OPEN_DM_THREAD.read();
    let Some((room, peer)) = active else {
        return rsx! {};
    };

    rsx! {
        DmThreadModalBody { room, peer }
    }
}

#[component]
fn DmThreadModalBody(room: VerifyingKey, peer: MemberId) -> Element {
    let mut draft = use_signal(String::new);
    let mut send_error: Signal<Option<String>> = use_signal(|| None);

    // Drain any DM_DRAFT seeded by the invite-via-DM picker (#252) once
    // it matches this (room, peer). If the user has ALREADY typed
    // something into the composer (e.g. picker invoked over an open
    // thread), append the invite below the existing draft separated by
    // blank lines — never silently overwrite (Code-first / Skeptical
    // found that the previous version clobbered the user's text).
    use_effect(move || {
        let pending = {
            let g = DM_DRAFT.try_read().ok();
            g.and_then(|opt| opt.clone())
                .filter(|(r, p, _)| *r == room && *p == peer)
        };
        if let Some((_, _, body)) = pending {
            let existing = draft.read().clone();
            let merged = if existing.trim().is_empty() {
                body
            } else {
                format!("{}\n\n{}", existing.trim_end(), body)
            };
            draft.set(merged);
            crate::util::defer(move || {
                *DM_DRAFT.write() = None;
            });
        }
    });

    // Pull the rendered messages + counterparty nickname once per read of
    // ROOMS. We materialise to plain Strings so the rsx! macro below doesn't
    // hold a ROOMS borrow across spawn_local.
    let view = use_memo({
        move || {
            let rooms = ROOMS.try_read().ok()?;
            let room_data = rooms.map.get(&room)?;

            let self_sk = room_data.self_sk.clone();
            let self_id = MemberId::from(&self_sk.verifying_key());
            let owner_id = MemberId::from(&room);

            // Nickname / membership lookup.
            let nicknames = room_data
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
                            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                            Err(_) => info.member_info.preferred_nickname.to_string_lossy(),
                        },
                    )
                })
                .collect::<std::collections::HashMap<_, _>>();

            let peer_nickname = nicknames
                .get(&peer)
                .cloned()
                .unwrap_or_else(|| short_member_id(&peer));

            // Peer must still be a member for sends to be accepted; surface
            // that constraint to the user up front.
            let peer_still_member = peer == owner_id
                || room_data
                    .room_state
                    .members
                    .members
                    .iter()
                    .any(|m| m.member.id() == peer);

            let mut latest_inbound_ts: u64 = 0;
            let mut rendered: Vec<RenderedDm> = Vec::new();
            for msg in &room_data.room_state.direct_messages.messages {
                let is_self_sender = msg.message.sender == self_id;
                let is_self_recipient = msg.message.recipient == self_id;
                let between_us = (is_self_sender && msg.message.recipient == peer)
                    || (is_self_recipient && msg.message.sender == peer);
                if !between_us {
                    continue;
                }

                if is_self_recipient {
                    latest_inbound_ts = latest_inbound_ts.max(msg.message.timestamp);
                }

                let (body, kind) = if is_self_recipient {
                    match open_direct_message(&self_sk, msg) {
                        Ok(bytes) => (
                            String::from_utf8_lossy(&bytes).into_owned(),
                            BodyKind::Plaintext,
                        ),
                        Err(err) => (
                            // Skeptical reviewer caught: putting `<...>`
                            // through `message_to_html` produces mangled
                            // markup because `<unable:` looks like a
                            // markdown autolink scheme. Tag the kind so
                            // the renderer skips markdown for these.
                            format!("unable to decrypt: {}", err),
                            BodyKind::Placeholder,
                        ),
                    }
                } else {
                    // Outbound — we don't keep plaintext locally, mirror
                    // riverctl's behaviour. Same markdown-mangling
                    // concern as above.
                    ("sent — ciphertext only".to_string(), BodyKind::Placeholder)
                };

                rendered.push(RenderedDm {
                    outgoing: is_self_sender,
                    timestamp: msg.message.timestamp,
                    body,
                    kind,
                    token: msg.purge_token(),
                });
            }
            rendered.sort_by_key(|d| d.timestamp);

            Some(ViewData {
                peer_nickname,
                peer_still_member,
                messages: rendered,
                latest_inbound_ts,
            })
        }
    });

    let view_value = view.read();
    let Some(view_data) = view_value.as_ref() else {
        return rsx! { div { "Room state not available" } };
    };

    // Mark thread read once we have actually rendered the latest inbound
    // timestamp. `defer` keeps the write off the current render frame.
    if view_data.latest_inbound_ts > 0 {
        let ts = view_data.latest_inbound_ts;
        mark_thread_read(room, peer, ts);
    }

    let peer_label = view_data.peer_nickname.clone();
    let peer_still_member = view_data.peer_still_member;

    let close = move |_| {
        crate::util::defer(move || {
            *OPEN_DM_THREAD.write() = None;
        });
    };

    // No-arg send callback so both `onclick` and `onkeydown` (Enter)
    // can invoke it. `mut` because Dioxus signal `.set()` borrows the
    // closure as `FnMut`.
    let mut do_send = move || {
        let body = draft.read().clone();
        if body.trim().is_empty() {
            return;
        }
        if body.len() > DM_BODY_BYTE_CAP {
            send_error.set(Some(format!(
                "Message too long: {} bytes (cap is {} bytes)",
                body.len(),
                DM_BODY_BYTE_CAP
            )));
            return;
        }
        send_error.set(None);

        let Some(room_data) = ROOMS
            .try_read()
            .ok()
            .and_then(|r| r.map.get(&room).cloned())
        else {
            error!("DM send: room data missing");
            return;
        };

        let self_sk = room_data.self_sk.clone();
        let self_id: MemberId = (&self_sk.verifying_key()).into();
        let peer_vk = match resolve_peer_vk(&room_data, room, peer) {
            Some(vk) => vk,
            None => {
                send_error.set(Some(
                    "Recipient is not currently a member of the room.".into(),
                ));
                return;
            }
        };
        if self_id == peer {
            send_error.set(Some("Cannot DM yourself.".into()));
            return;
        }

        // Per-pair cap: contract `apply_delta` silently drops overflow.
        // Surface as a user-visible error instead of "successful" sends
        // that disappear into the void.
        let existing = pair_message_count(&room_data.room_state.direct_messages, self_id, peer);
        if existing >= MAX_DM_MESSAGES_PER_PAIR {
            send_error.set(Some(format!(
                    "Per-pair cap reached ({}/{}). Ask the recipient to purge older DMs in this thread before sending more.",
                    existing, MAX_DM_MESSAGES_PER_PAIR
                )));
            return;
        }

        let body_bytes = body.into_bytes();
        wasm_bindgen_futures::spawn_local(async move {
            let now = unix_now();
            let auth =
                match compose_direct_message(&self_sk, &peer_vk, &room, now, now, &body_bytes) {
                    Ok(a) => a,
                    Err(e) => {
                        error!("compose_direct_message failed: {}", e);
                        send_error.set(Some(format!("Failed to compose DM: {}", e)));
                        return;
                    }
                };

            let delta = ChatRoomStateV1Delta {
                direct_messages: Some(DirectMessagesDelta {
                    new_messages: vec![auth.clone()],
                    advanced_purges: vec![],
                }),
                ..Default::default()
            };

            let params = ChatRoomParametersV1 { owner: room };
            crate::util::defer(move || {
                let outcome = ROOMS.with_mut(|rooms| {
                    let Some(rd) = rooms.map.get_mut(&room) else {
                        return ApplyOutcome::RoomGone;
                    };
                    // Re-check the per-pair cap inside the write-lock:
                    // an incoming peer-side DM could have arrived
                    // between the pre-flight check and this defer
                    // tick. Skeptical-review #3.
                    if pair_message_count(&rd.room_state.direct_messages, self_id, peer)
                        >= MAX_DM_MESSAGES_PER_PAIR
                    {
                        return ApplyOutcome::CapHit;
                    }
                    let parent = rd.room_state.clone();
                    if let Err(e) = rd.room_state.apply_delta(&parent, &params, &Some(delta)) {
                        error!("DM apply_delta failed: {:?}", e);
                        ApplyOutcome::DeltaFailed
                    } else {
                        ApplyOutcome::Applied
                    }
                });
                match outcome {
                    ApplyOutcome::Applied => {
                        info!("DM appended locally; marking room for sync");
                        mark_needs_sync(room);
                        draft.set(String::new());
                    }
                    ApplyOutcome::CapHit => {
                        send_error.set(Some(format!(
                                "Per-pair cap reached ({}/{}) while sending. Ask the recipient to purge older DMs in this thread before sending more.",
                                MAX_DM_MESSAGES_PER_PAIR, MAX_DM_MESSAGES_PER_PAIR
                            )));
                    }
                    ApplyOutcome::RoomGone => {
                        send_error.set(Some("Room is no longer loaded.".into()));
                    }
                    ApplyOutcome::DeltaFailed => {
                        send_error.set(Some(
                            "Failed to add DM to local state (verify it via console).".into(),
                        ));
                    }
                }
            });
        });
    };

    let purge_thread = {
        move |_| {
            // Re-read tokens INSIDE the click closure (not from the
            // already-rendered memo) so any DM that arrived between the
            // last render and the click is also tombstoned. Without this,
            // a fast inbound during the user's hesitation survives the
            // purge.
            let Some(room_data) = ROOMS
                .try_read()
                .ok()
                .and_then(|r| r.map.get(&room).cloned())
            else {
                error!("DM purge: room data missing");
                return;
            };
            let self_sk = room_data.self_sk.clone();
            let self_id: MemberId = (&self_sk.verifying_key()).into();

            let tokens: Vec<PurgeToken> = room_data
                .room_state
                .direct_messages
                .messages
                .iter()
                .filter(|m| m.message.recipient == self_id && m.message.sender == peer)
                .map(|m| m.purge_token())
                .collect();
            if tokens.is_empty() {
                send_error.set(Some("No inbound DMs to purge in this thread.".into()));
                return;
            }

            let previous = room_data
                .room_state
                .direct_messages
                .purges
                .iter()
                .find(|p| p.recipient_id == self_id)
                .cloned();
            let envelope =
                match advance_recipient_purges(&self_sk, &room, previous.as_ref(), tokens) {
                    Ok(e) => e,
                    Err(e) => {
                        warn!("advance_recipient_purges failed: {}", e);
                        send_error.set(Some(format!("Purge build failed: {}", e)));
                        return;
                    }
                };

            let delta = ChatRoomStateV1Delta {
                direct_messages: Some(DirectMessagesDelta {
                    new_messages: vec![],
                    advanced_purges: vec![envelope.clone()],
                }),
                ..Default::default()
            };
            let params = ChatRoomParametersV1 { owner: room };

            crate::util::defer(move || {
                let applied = ROOMS.with_mut(|rooms| {
                    let Some(rd) = rooms.map.get_mut(&room) else {
                        return false;
                    };
                    let parent = rd.room_state.clone();
                    if let Err(e) = rd.room_state.apply_delta(&parent, &params, &Some(delta)) {
                        error!("DM purge apply_delta failed: {:?}", e);
                        false
                    } else {
                        true
                    }
                });
                if applied {
                    mark_needs_sync(room);
                } else {
                    send_error.set(Some(
                        "Failed to apply purge envelope (verify it via console).".into(),
                    ));
                }
            });
        }
    };

    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center",
            // Backdrop
            div {
                class: "absolute inset-0 bg-black/50",
                onclick: close,
            }
            // Modal body
            div {
                class: "relative z-10 w-full max-w-lg mx-4 bg-panel rounded-xl shadow-xl border border-border flex flex-col max-h-[80vh]",
                // Header
                div { class: "flex items-center justify-between px-5 py-4 border-b border-border",
                    h2 { class: "text-lg font-semibold text-text",
                        "Direct messages with "
                        span { class: "text-accent", "{peer_label}" }
                    }
                    button {
                        class: "p-1 text-text-muted hover:text-text transition-colors text-xl",
                        onclick: close,
                        "✕"
                    }
                }

                // Thread body
                div { class: "flex-1 overflow-y-auto px-5 py-4 space-y-2",
                    if view_data.messages.is_empty() {
                        p { class: "text-sm text-text-muted italic",
                            "No messages yet. Say hello!"
                        }
                    } else {
                        for (idx, m) in view_data.messages.iter().enumerate() {
                            DmBubble { key: "{idx}_{m.timestamp}", message: m.clone() }
                        }
                    }
                }

                // Footer with composer + actions
                div { class: "border-t border-border px-5 py-3 space-y-2",
                    if let Some(err) = send_error.read().as_ref() {
                        div { class: "text-xs text-red-400", "{err}" }
                    }
                    if !peer_still_member {
                        div { class: "text-xs text-yellow-400",
                            "This member is not currently in the room — outbound DMs will be rejected by the contract."
                        }
                    }
                    div { class: "flex items-end gap-2",
                        textarea {
                            class: "flex-1 px-3 py-2 bg-surface border border-border rounded-lg text-sm text-text resize-none min-h-[2.5rem] max-h-32",
                            placeholder: "Type a direct message...",
                            value: "{draft.read()}",
                            oninput: move |e| draft.set(e.value()),
                            // Match the room-message composer: Enter sends,
                            // Shift+Enter inserts a newline. Without this
                            // the composer felt broken (zorolin reported
                            // on 2026-05-16).
                            onkeydown: move |e| {
                                if e.key() == Key::Enter && !e.modifiers().shift() {
                                    e.prevent_default();
                                    if !draft.read().trim().is_empty() && peer_still_member {
                                        do_send();
                                    }
                                }
                            },
                            disabled: !peer_still_member,
                        }
                        button {
                            class: "px-3 py-2 bg-accent hover:bg-accent-hover disabled:opacity-50 text-white text-sm font-medium rounded-lg transition-colors",
                            disabled: draft.read().trim().is_empty() || !peer_still_member,
                            onclick: move |_| do_send(),
                            "Send"
                        }
                    }
                    div { class: "flex justify-between items-center pt-1",
                        span { class: "text-[10px] text-text-muted",
                            "End-to-end encrypted to this member's room key. The contract enforces caps; the gateway can't read content."
                        }
                        button {
                            class: "text-xs text-text-muted hover:text-red-400 transition-colors",
                            onclick: purge_thread,
                            title: "Tombstone every inbound DM in this thread on the network.",
                            "Purge thread"
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, PartialEq)]
struct ViewData {
    peer_nickname: String,
    peer_still_member: bool,
    messages: Vec<RenderedDm>,
    latest_inbound_ts: u64,
}

/// Inbound DM body presentation.
#[derive(Clone, PartialEq)]
enum BodyKind {
    /// User-supplied plaintext — runs through the markdown / linkify pass
    /// so URLs (notably invite links — see #252) become anchors.
    Plaintext,
    /// Local UI string (outbound bubble, "decrypt failed", etc.). Rendered
    /// as a muted text node, never through markdown — keeps the markdown
    /// crate from autolinking literal placeholders like
    /// `"unable to decrypt: …"` into broken `unable:` schemes.
    Placeholder,
}

#[derive(Clone, PartialEq)]
struct RenderedDm {
    outgoing: bool,
    timestamp: u64,
    body: String,
    kind: BodyKind,
    token: PurgeToken,
}

#[component]
fn DmBubble(message: RenderedDm) -> Element {
    let ts_label = format_local_time(message.timestamp);
    let align_class = if message.outgoing {
        "self-end bg-accent/20 text-text"
    } else {
        "self-start bg-surface text-text"
    };
    let bubble_body = match message.kind {
        BodyKind::Plaintext => {
            // Reuse the room-message linkify path so pasted URLs (notably
            // invite links shared via DM — see #252) render as clickable
            // anchors, and Freenet web-contract URLs get host-stripped to
            // a same-origin path the recipient's gateway can serve. The
            // `prose prose-sm` wrapper matches `conversation.rs:2061` so
            // multi-paragraph bodies don't collapse to a single block.
            let body_html = crate::components::conversation::message_to_html(&message.body);
            rsx! {
                div {
                    class: "prose prose-sm max-w-[80%] px-3 py-2 rounded-lg text-sm break-words {align_class}",
                    dangerous_inner_html: "{body_html}",
                }
            }
        }
        BodyKind::Placeholder => {
            rsx! {
                div {
                    class: "max-w-[80%] px-3 py-2 rounded-lg text-xs italic text-text-muted {align_class}",
                    "{message.body}"
                }
            }
        }
    };
    rsx! {
        div { class: "flex flex-col",
            {bubble_body}
            span {
                class: if message.outgoing { "self-end text-[10px] text-text-muted mt-0.5" } else { "self-start text-[10px] text-text-muted mt-0.5" },
                "{ts_label}"
            }
        }
    }
}

fn resolve_peer_vk(
    room_data: &crate::room_data::RoomData,
    owner_vk: VerifyingKey,
    peer: MemberId,
) -> Option<VerifyingKey> {
    let owner_id = MemberId::from(&owner_vk);
    if peer == owner_id {
        return Some(owner_vk);
    }
    room_data
        .room_state
        .members
        .members
        .iter()
        .find(|m| m.member.id() == peer)
        .map(|m| m.member.member_vk)
}

fn short_member_id(id: &MemberId) -> String {
    id.to_string().chars().take(8).collect()
}

fn unix_now() -> u64 {
    // `SystemTime::now()` panics on `wasm32-unknown-unknown` (the JS
    // platform-time stub is not implemented). Route through
    // `crate::util::get_current_system_time` which uses the
    // `wasm_bindgen` `Date.now()` shim on Wasm and `SystemTime::now()` on
    // native.
    crate::util::get_current_system_time()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn format_local_time(unix_secs: u64) -> String {
    let datetime = SystemTime::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(unix_secs))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let dt: chrono::DateTime<chrono::Utc> = datetime.into();
    let local: chrono::DateTime<chrono::Local> = dt.into();
    local.format("%Y-%m-%d %H:%M:%S").to_string()
}
