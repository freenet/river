//! Per-pair DM thread modal: decrypts inbound DMs, composes outbound ones,
//! and offers a "Delete their messages" button that produces a fresh
//! `AuthorizedRecipientPurges` envelope tombstoning every inbound DM in the
//! current view.
//!
//! The header used to carry a "Hide" button alongside the close ✕; that was
//! moved to the per-row rollover ✕ in [`crate::components::room_list::dm_rail_section`]
//! after #266 reported it was visually ambiguous with the close button.
//! "Delete their messages" now goes through a confirmation modal before
//! firing the destructive `purge_thread` flow.

use crate::components::app::chat_delegate::{save_outbound_dm, unhide_dm_thread};
use crate::components::app::{mark_needs_sync, ROOMS};
use crate::components::direct_messages::{
    lookup_outbound_plaintext, mark_thread_read, DM_DRAFT, OPEN_DM_THREAD, OUTBOUND_DMS,
};
use crate::components::members::Invitation;
use crate::components::room_list::receive_invitation_modal::present_invitation;
use crate::room_data::SendMessageError;
use crate::util::ecies::unseal_bytes_with_secrets;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::VerifyingKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::direct_messages::{
    advance_recipient_purges, compose_direct_message, open_direct_message, pair_message_count,
    DirectMessagesDelta, PurgeToken, MAX_DM_CIPHERTEXT_BYTES, MAX_DM_MESSAGES_PER_PAIR,
};
use river_core::room_state::dm_body::{decode_body, DirectMessageBody, InvitePayload};
use river_core::room_state::member::MemberId;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Loose body cap (in bytes) to keep us under `MAX_DM_CIPHERTEXT_BYTES` once
/// envelope overhead is accounted for. 32 KiB - 256 byte safety margin.
const DM_BODY_BYTE_CAP: usize = MAX_DM_CIPHERTEXT_BYTES - 256;

/// Monotonic counter bumped every time the local user sends a DM from
/// any open thread modal. The auto-scroll effect reads this via
/// `.peek()` (non-reactive) to distinguish "user sent a message"
/// (always scroll) from "peer sent a message" (only scroll if reader
/// is near the bottom). Wrap-around is fine — the effect compares for
/// inequality with a stored previous value.
///
/// Lives at module scope rather than inside the modal so a re-render
/// caused by `OPEN_DM_THREAD` flipping doesn't reset it; the effect
/// uses a `use_hook` Cell to remember the previous value across its
/// own re-runs.
///
/// Why no `.read()` (reactive) subscription: per AGENTS.md "Dioxus
/// WASM Signal Safety Rules", a subscriber's `.read()` can panic if
/// the write that triggered the notification still holds the write
/// guard's RefCell borrow at notify time. The subscription is supplied
/// instead by the parent memo's read of `ROOMS` (the bump always
/// happens in the same defer block as the apply_delta that grows
/// message_count, so the effect re-fires via the message_count path).
/// Any future writer that bumps this counter WITHOUT also growing
/// `message_count` must add an explicit re-render trigger or the
/// auto-scroll will miss its bump.
static OUTBOUND_SEND_COUNTER: GlobalSignal<u64> = Global::new(|| 0);

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
    /// `apply_delta` returned Ok but the DM did not actually land in
    /// `direct_messages.messages` — the contract silently dropped it
    /// (typically: sender or recipient is not in members and no rejoin
    /// bundle was supplied). Codex P2 defence-in-depth (PR #269 review).
    SilentDrop,
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
    //
    // CRITICAL: the DM_DRAFT clear MUST be synchronous, BEFORE we
    // mutate `draft`. Two prior bugs (issue #267 — Ivvor's "tab locks
    // up at Generating…" report on 2026-05-17) both stemmed from
    // deferring the clear:
    //
    // 1. The effect subscribes to DM_DRAFT. If the clear runs in a
    //    `defer()` (setTimeout(0) → macrotask), Dioxus's re-render
    //    triggered by `draft.set()` (microtask) sees the still-Some
    //    DM_DRAFT, re-fires the effect, appends `body` again, and
    //    loops forever — the project rule "Never defer signal
    //    clears in `use_effect`" exists for exactly this case
    //    (AGENTS.md "Dioxus WASM Signal Safety Rules").
    // 2. `draft.read()` here also subscribes the effect to the local
    //    `draft` signal; once we set it, the re-fire is guaranteed.
    //    Using `.peek()` makes the read non-reactive, which is the
    //    correct semantics regardless: we only ever want to merge
    //    once per DM_DRAFT arrival.
    use_effect(move || {
        let pending = {
            let g = DM_DRAFT.try_read().ok();
            g.and_then(|opt| opt.clone())
                .filter(|(r, p, _)| *r == room && *p == peer)
        };
        if let Some((_, _, body)) = pending {
            // Clear DM_DRAFT SYNCHRONOUSLY before any further state
            // mutation. The write itself doesn't re-fire the effect
            // because Dioxus dedups same-tick subscriber notifications,
            // and the synchronous clear ensures any deferred re-fire
            // sees `None` and exits cleanly.
            *DM_DRAFT.write() = None;
            // `peek()` (not `read()`) — never subscribe the effect to
            // its own writes.
            let existing = draft.peek().clone();
            let merged = merge_invite_into_draft(&existing, &body);
            draft.set(merged);
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

            // Snapshot the outbound-DM cache once so each rendered
            // outbound bubble does an O(1) HashMap lookup instead of
            // re-acquiring the signal guard per message. Reading via
            // `try_read` here registers the memo's subscription to
            // [`OUTBOUND_DMS`], so a successful save_outbound_dm write
            // also re-runs this memo and the bubble flips from
            // placeholder to plaintext (see AGENTS.md "Dioxus WASM
            // Signal Safety": the subscription is registered ONLY on
            // the success path).
            let outbound_cache = OUTBOUND_DMS.try_read().ok().map(|g| g.clone());

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
                        Ok(bytes) => match decode_body(&bytes) {
                            Ok(DirectMessageBody::Text { text }) => (text, BodyKind::Plaintext),
                            Ok(DirectMessageBody::Invite(payload)) => {
                                // Resolve a friendly room label and the
                                // "already a member" flag for the card.
                                // We treat "loaded but observer-only" as
                                // NOT-a-member: the user can join via
                                // the invite even though they have the
                                // room loaded. `can_participate()` is
                                // the canonical check used elsewhere in
                                // the codebase. Computed here under the
                                // existing `rooms` read so the card
                                // render path itself is purely
                                // declarative.
                                let target_room_data = rooms.map.get(&payload.room_owner_vk);
                                let card_state = classify_invite_card_state(
                                    target_room_data.map(|rd| rd.can_participate()),
                                );
                                let room_label = target_room_data
                                    .map(|rd| {
                                        let sealed =
                                            &rd.room_state.configuration.configuration.display.name;
                                        match unseal_bytes_with_secrets(sealed, &rd.secrets) {
                                            Ok(b) => String::from_utf8_lossy(&b).to_string(),
                                            Err(_) => sealed.to_string_lossy(),
                                        }
                                    })
                                    .unwrap_or_else(|| {
                                        format!("Room {}", short_vk_prefix(&payload.room_owner_vk))
                                    });
                                let personal = payload.personal_message.clone();
                                (
                                    String::new(),
                                    BodyKind::Invite(Box::new(InviteCardData {
                                        payload: *payload,
                                        room_label,
                                        card_state,
                                        personal_message: personal,
                                    })),
                                )
                            }
                            Err(err) => (
                                // Body bytes didn't decode (malformed new-
                                // format CBOR). Surface as a placeholder
                                // rather than a card or text — same UX as
                                // the decrypt-failed branch.
                                format!("unable to decode invite: {}", err),
                                BodyKind::Placeholder,
                            ),
                        },
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
                    // Outbound: check the delegate-backed plaintext
                    // cache (#256). Hit → render the original plaintext
                    // through the markdown path; miss → fall through to
                    // the legacy "ciphertext only" placeholder for DMs
                    // sent before the cache shipped or on a second
                    // device without the cache yet hydrated. Goes
                    // through the shared pure helper
                    // `lookup_outbound_plaintext` so the regression
                    // tests in `direct_messages.rs` pin THIS behaviour.
                    let token = msg.purge_token();
                    let resolved = outbound_cache.as_ref().and_then(|cache| {
                        lookup_outbound_plaintext(cache, &room, &msg.message.recipient, &token).ok()
                    });
                    match resolved {
                        Some(plaintext) => (plaintext, BodyKind::Plaintext),
                        None => ("sent — ciphertext only".to_string(), BodyKind::Placeholder),
                    }
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

    // Auto-scroll behaviour (Phase 3 of #243 invite-DM redesign):
    //
    //   1. Modal mount       → jump to bottom (always, instant).
    //   2. Outbound send     → scroll to bottom (always, smooth).
    //   3. Inbound new msg   → scroll only if the user was already
    //                          near the bottom (within ~50px). Don't
    //                          yank a reader who's scrolled up.
    //
    // For the effect to re-fire when a new bubble lands we need an
    // actual subscribed signal read inside the closure. Dioxus 0.7's
    // `use_effect` only re-runs when a signal that was `.read()`
    // SUCCESSFULLY inside the closure body changes — capturing a
    // plain `usize` `message_count` does not create a subscription,
    // and `.peek()` on `OUTBOUND_SEND_COUNTER` is also non-reactive.
    // PR #278's Codex round-1 fix replaced the previous `.read()` on
    // `OUTBOUND_SEND_COUNTER` with `.peek()` for re-entrancy safety;
    // that left the effect with no reactive read at all (issue #283).
    //
    // Mirror the conversation.rs pattern: the LAST DM bubble's
    // `onmounted` updates `last_dm_bubble`, and the effect reads
    // `last_dm_bubble()` (calls the Signal as a function = subscribing
    // read). Whenever a new bubble mounts at a different `Rc` identity,
    // the effect re-fires. `OUTBOUND_SEND_COUNTER.peek()` stays
    // non-reactive — the bubble mount provides the trigger; the
    // counter is just consulted to classify the trigger as
    // outbound-vs-inbound.
    let last_dm_bubble: Signal<Option<Rc<MountedData>>> = use_signal(|| None);
    let first_scroll_done = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(false)));
    let prev_outbound_bump = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(0u64)));
    #[cfg(target_arch = "wasm32")]
    {
        let first_scroll_done = first_scroll_done.clone();
        let prev_outbound_bump = prev_outbound_bump.clone();
        use_effect(move || {
            // Read the last-bubble signal as a SUBSCRIBING read so the
            // effect re-runs whenever a fresh bubble mounts (i.e. new
            // DM lands or the modal opens with messages already in
            // view). Without this, Dioxus has no signal to watch and
            // the effect runs exactly once on mount — leaving auto-
            // scroll silently broken on subsequent messages (#283).
            let trigger = last_dm_bubble();
            if trigger.is_none() {
                // No bubble has mounted yet (empty-thread state or
                // first render before mounts arrive). Stay subscribed
                // and bail until the first mount lands.
                return;
            }
            let outbound_bump_now = *OUTBOUND_SEND_COUNTER.peek();
            let prev_bump = prev_outbound_bump.get();
            let outbound_changed = outbound_bump_now != prev_bump;
            prev_outbound_bump.set(outbound_bump_now);

            let is_first = !first_scroll_done.get();
            if is_first {
                first_scroll_done.set(true);
            }
            // For inbound-only triggers we want to scroll only if the
            // user is near the bottom right now. Read the DOM
            // synchronously before any further layout shifts hit the
            // viewport — we're inside the effect, post-render.
            let near_bottom = is_near_bottom("dm-scroll-container", 50.0);
            // Trigger types:
            //   * is_first         — mount: always jump (instant).
            //   * outbound_changed — user sent: always (smooth).
            //   * else             — inbound: only when near bottom.
            let should_scroll = is_first || outbound_changed || near_bottom;
            if !should_scroll {
                return;
            }
            let behavior = if is_first {
                web_sys::ScrollBehavior::Instant
            } else {
                web_sys::ScrollBehavior::Smooth
            };
            crate::util::safe_spawn_local(async move {
                scroll_dm_container_to_bottom(behavior);
            });
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // Touch the values so they're not flagged as unused on native.
        let _ = (&last_dm_bubble, &first_scroll_done, &prev_outbound_bump);
    }

    let peer_label = view_data.peer_nickname.clone();
    let peer_still_member = view_data.peer_still_member;

    let close = move |_| {
        crate::util::defer(move || {
            *OPEN_DM_THREAD.write() = None;
        });
    };

    // Confirmation modal for the destructive "Delete their messages" action.
    // Single-click would erase every inbound DM from `peer` with no undo
    // (#266). The confirmation gate forces a deliberate second click; the
    // primary Cancel/Esc path closes it without mutating anything.
    let mut confirm_delete_open: Signal<bool> = use_signal(|| false);

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
                    "This thread is full ({}/{} messages). Ask them to delete some of the older messages you've sent.",
                    existing, MAX_DM_MESSAGES_PER_PAIR
                )));
            return;
        }

        let plaintext = body.clone();
        let body_bytes = body.into_bytes();
        // Bug #1 (Ivvor, Matrix 2026-05-16): an invited-but-inactive sender
        // can be pruned from `members.members` by `post_apply_cleanup`,
        // after which the contract's `DirectMessagesV1::apply_delta`
        // silent-drops any DM whose sender isn't currently in members. The
        // regular message-send path bundles a rejoin delta
        // (`MembersDelta` + `member_info`) to re-add the pruned sender —
        // do the same here so DMs from a pruned-but-invited sender land
        // atomically. `MembersV1` precedes `DirectMessagesV1` in
        // `ChatRoomStateV1`'s field order, so by the time the DM
        // sub-state apply runs the sender is back in members. Pinned by
        // `pruned_sender_can_dm_when_bundling_rejoin_delta` in
        // `common/tests/direct_messages_test.rs`. Returns `(None, None)`
        // when not pruned, which `ChatRoomStateV1Delta` accepts as no-op.
        let (rejoin_members, rejoin_member_info) = room_data.build_rejoin_delta();
        // Codex P2 (PR #269 review): if the sender is pruned AND
        // `build_rejoin_delta` returned no credentials (e.g. an imported
        // identity missing `self_authorized_member`), the contract will
        // silent-drop the DM. The local `apply_delta` succeeds in that
        // case but the message never lands, leaving the composer
        // empty-looking-successful. Surface the failure up front instead.
        let self_in_members = self_id == MemberId::from(&room)
            || room_data
                .room_state
                .members
                .members
                .iter()
                .any(|m| m.member.id() == self_id);
        if !self_in_members && rejoin_members.is_none() {
            send_error.set(Some(
                "You're not currently in this room's member list and no \
                 rejoin credentials are stored locally. Reload the room or \
                 re-accept your invitation before sending a DM."
                    .into(),
            ));
            return;
        }
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

            // Capture the metadata we need for the outbound-plaintext
            // cache (#256) BEFORE moving `auth` into the delta below.
            let purge_token = auth.purge_token();
            let dm_timestamp = auth.message.timestamp;

            let delta = ChatRoomStateV1Delta {
                members: rejoin_members,
                member_info: rejoin_member_info,
                direct_messages: Some(DirectMessagesDelta {
                    new_messages: vec![auth.clone()],
                    advanced_purges: vec![],
                }),
                ..Default::default()
            };

            let params = ChatRoomParametersV1 { owner: room };
            let auth_sig = auth.sender_signature;
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
                        return ApplyOutcome::DeltaFailed;
                    }
                    // Codex P2 defence-in-depth (PR #269 review):
                    // `DirectMessagesV1::apply_delta` silently drops
                    // DMs whose sender or recipient is not in members
                    // and returns Ok. Verify our DM actually landed
                    // before reporting success — otherwise the UI
                    // would clear the composer and the user would
                    // think the message was sent.
                    let landed = rd
                        .room_state
                        .direct_messages
                        .messages
                        .iter()
                        .any(|m| m.sender_signature == auth_sig);
                    if !landed {
                        warn!(
                            "DM apply_delta returned Ok but the message was \
                             silently dropped by the contract (membership check?)"
                        );
                        return ApplyOutcome::SilentDrop;
                    }
                    // #310: apply_delta's MessagesV1 step always re-runs the
                    // public-only rebuild_actions_state, which wipes private
                    // edits/deletes/reactions. Re-derive them with decryption
                    // so sending a DM doesn't transiently revert an edited
                    // message. No-op on public rooms.
                    rd.rebuild_private_actions_state();
                    ApplyOutcome::Applied
                });
                match outcome {
                    ApplyOutcome::Applied => {
                        info!("DM appended locally; marking room for sync");
                        mark_needs_sync(room);
                        // Bump the outbound-send counter so the
                        // auto-scroll effect notices the user just sent
                        // a message and snaps to the bottom (regardless
                        // of prior scroll position). See the effect in
                        // `DmThreadModalBody`.
                        //
                        // Compute `next` BEFORE taking the write guard
                        // — read/write in the same statement would let
                        // the read and write guard temporaries overlap
                        // until the end of the statement, risking a
                        // RefCell/Dioxus borrow panic on the send
                        // success path (Codex P1 finding on PR #278).
                        let next_outbound_counter = OUTBOUND_SEND_COUNTER.peek().wrapping_add(1);
                        *OUTBOUND_SEND_COUNTER.write() = next_outbound_counter;
                        // Persist plaintext for the sender's own view
                        // (#256). Cache write + delegate save happen
                        // inside `save_outbound_dm` via `defer` /
                        // `safe_spawn_local`.
                        save_outbound_dm(room, self_id, peer, purge_token, dm_timestamp, plaintext);
                        // Issue freenet/river#261 (Codex P1): if the
                        // thread had been hidden, an outbound send must
                        // revive it. The filter's "max_message_ts >
                        // hidden_at_ts" check isn't sufficient on its
                        // own because both `unix_now()` calls (the one
                        // captured into `hidden_at_ts` and the one
                        // stamped onto the outbound message) can land
                        // in the same second — leaving the thread
                        // stuck-hidden right after the user sent a
                        // message. Explicit unhide is deterministic
                        // and idempotent (no-op when no entry exists).
                        unhide_dm_thread(room, peer);
                        draft.set(String::new());
                    }
                    ApplyOutcome::CapHit => {
                        send_error.set(Some(format!(
                                "This thread is full ({}/{} messages). Ask them to delete some of the older messages you've sent.",
                                MAX_DM_MESSAGES_PER_PAIR, MAX_DM_MESSAGES_PER_PAIR
                            )));
                    }
                    ApplyOutcome::RoomGone => {
                        send_error.set(Some("This room is no longer loaded.".into()));
                    }
                    ApplyOutcome::DeltaFailed => {
                        // No "please try again" — `apply_delta` failures
                        // are deterministic (signature / membership /
                        // tombstone / cap), so retrying byte-identical
                        // input gives the same result. See the
                        // `warn!`/`error!` log for the diagnostic.
                        send_error.set(Some(
                            "Couldn't send this message — something went wrong.".into(),
                        ));
                    }
                    ApplyOutcome::SilentDrop => {
                        // The contract accepted the delta but dropped
                        // the DM. The most likely cause is that the
                        // sender or recipient is not in members and
                        // the rejoin bundle was insufficient (or
                        // empty). Don't clear the composer — the user
                        // can adjust and retry.
                        send_error.set(Some(
                            "This message couldn't be added to the room \
                             (your member entry may be missing). Try \
                             posting a message in the room first, then \
                             retry the DM."
                                .into(),
                        ));
                    }
                }
            });
        });
    };

    let purge_thread = {
        move |_| {
            // Close the confirmation modal first — if the user clicks
            // Delete and then the apply path fails, the surfaced error
            // toast is on the main composer, NOT on the dismissed
            // confirm modal. Closing immediately also matches the
            // "destructive primary action commits before result" UX
            // convention used elsewhere in the codebase.
            confirm_delete_open.set(false);

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
                send_error.set(Some("No messages from them to delete.".into()));
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
                        send_error.set(Some(
                            "Couldn't delete those messages — something went wrong.".into(),
                        ));
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
                        // #310: preserve private edits/reactions across the
                        // optimistic apply_delta. No-op on public rooms.
                        rd.rebuild_private_actions_state();
                        true
                    }
                });
                if applied {
                    mark_needs_sync(room);
                } else {
                    send_error.set(Some(
                        "Couldn't delete those messages — something went wrong.".into(),
                    ));
                }
            });
        }
    };

    rsx! {
        div {
            class: "fixed inset-0 z-50 flex items-center justify-center",
            // Escape handler at the outer-modal scope so the confirm
            // dialog can be dismissed via Escape regardless of which
            // child element currently has focus (Codex round-2 review
            // finding on #275). Without this, a Tab cycle that moved
            // focus out of the dialog's two buttons stranded Escape
            // — the dialog-scoped `onkeydown` only fires for events
            // bubbling up through the dialog's subtree, not from the
            // sibling composer textarea.
            //
            // Cheap because keydown events on the outer modal also
            // bubble up the dialog's div (when it has focus) so the
            // dialog-scoped handler still runs first — this is a
            // backstop, not a replacement.
            onkeydown: move |e: KeyboardEvent| {
                // Use try_read per AGENTS.md "Dioxus WASM Signal Safety Rules":
                // a plain .read() during a concurrent write Drop can hit a
                // RefCell re-entrancy panic on Firefox/mobile. confirm_delete_open
                // is a local use_signal (not a GlobalSignal) so practical risk is
                // low, but consistency with the round-1 ARCHIVE_TOAST fix matters.
                let open = confirm_delete_open
                    .try_read()
                    .map(|v| *v)
                    .unwrap_or(false);
                if e.key() == Key::Escape && open {
                    e.prevent_default();
                    e.stop_propagation();
                    confirm_delete_open.set(false);
                }
            },
            // Backdrop
            div {
                class: "absolute inset-0 bg-black/50",
                onclick: close,
            }
            // Modal body
            div {
                class: "relative z-10 w-full max-w-lg mx-4 bg-panel rounded-xl shadow-xl border border-border flex flex-col max-h-[80vh]",
                // Header. After #266 the Hide/Archive affordance moved
                // out of this header onto the per-row rollover ✕ in
                // `DmRailSection`, so the header is just the title and
                // the close ✕ — no visual collision with the per-row
                // archive control.
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

                // Thread body. Stable id so the auto-scroll effect can
                // reach the same DOM node across re-renders without
                // having to walk the tree.
                div {
                    id: "dm-scroll-container",
                    class: "flex-1 overflow-y-auto px-5 py-4 space-y-2",
                    if view_data.messages.is_empty() {
                        p { class: "text-sm text-text-muted italic",
                            "No messages yet. Say hello!"
                        }
                    } else {
                        {
                            let messages_len = view_data.messages.len();
                            view_data.messages.iter().enumerate().map(move |(idx, m)| {
                                let is_last = idx + 1 == messages_len;
                                let on_mount = if is_last {
                                    Some(last_dm_bubble)
                                } else {
                                    None
                                };
                                rsx! {
                                    DmBubble {
                                        key: "{idx}_{m.timestamp}",
                                        message: m.clone(),
                                        last_bubble_sink: on_mount,
                                    }
                                }
                            })
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
                        // The "you" half of the disclaimer is backed by
                        // the OUTBOUND_DMS local plaintext cache (#256).
                        // On-wire encryption is to `{peer}` only —
                        // the sender cannot decrypt the network copy.
                        // If you delete or break that cache, this
                        // disclaimer becomes inaccurate ("you" can no
                        // longer read your own outbound bubbles) and
                        // should be revisited.
                        span { class: "text-[10px] text-text-muted",
                            "Only you and "
                            span { class: "text-accent", "{peer_label}" }
                            " can read these messages."
                        }
                        button {
                            class: "text-xs text-text-muted hover:text-red-400 transition-colors",
                            onclick: move |_| confirm_delete_open.set(true),
                            title: "Removes messages they sent you from the network. Your own sent messages stay. Cannot be undone.",
                            "Delete their messages"
                        }
                    }
                }
            }

            // Confirmation modal for "Delete their messages" (#266). Cancel
            // is the primary / default-focused action and Escape closes
            // the modal without mutating. The Delete action calls into
            // `purge_thread`, which also closes this modal as its first
            // step so a failure surfaces on the underlying composer.
            //
            // Focus + Escape handling (Codex P2 review finding on #275):
            //
            // * The wrapping div has `tabindex: "0"` and grabs focus on
            //   mount, so `onkeydown` fires regardless of which child
            //   element (if any) currently has focus — including after
            //   the user clicks the backdrop and unfocuses every button.
            //   Without this, Escape only worked while Cancel/Delete
            //   were focused, which broke after the very first backdrop
            //   click.
            // * `prevent_default()` on Escape stops the browser from
            //   also closing the outer DM modal (which is what `Escape`
            //   would do if the underlying modal had an Escape handler;
            //   it currently doesn't, but defensive).
            // * Mirrors `member_info_modal.rs`'s established pattern.
            if *confirm_delete_open.read() {
                div {
                    class: "absolute inset-0 z-20 flex items-center justify-center",
                    role: "dialog",
                    "aria-modal": "true",
                    "aria-label": "Confirm delete their messages",
                    tabindex: "0",
                    onmounted: move |cx| {
                        let element = cx.data();
                        wasm_bindgen_futures::spawn_local(async move {
                            let _ = element.set_focus(true).await;
                        });
                    },
                    onkeydown: move |e: KeyboardEvent| {
                        if e.key() == Key::Escape {
                            e.prevent_default();
                            confirm_delete_open.set(false);
                        }
                    },
                    // Inner backdrop — clicking it cancels.
                    div {
                        class: "absolute inset-0 bg-black/60",
                        onclick: move |_| confirm_delete_open.set(false),
                    }
                    div {
                        class: "relative z-10 w-full max-w-sm mx-4 bg-panel rounded-xl shadow-xl border border-border p-5 space-y-4",
                        h3 { class: "text-base font-semibold text-text",
                            "Delete messages from "
                            span { class: "text-accent", "{peer_label}" }
                            "?"
                        }
                        p { class: "text-sm text-text-muted",
                            "This removes their messages to you from the network. Your own sent messages stay. Cannot be undone."
                        }
                        div { class: "flex justify-end gap-2 pt-2",
                            button {
                                // `autofocus` doesn't actually fire inside a
                                // dynamically-mounted Dioxus subtree (the
                                // browser only honours it on initial page
                                // load), so the wrapping div's
                                // `onmounted -> set_focus` is what gets
                                // keyboard users a safe initial focus —
                                // Escape closes, Enter on the focused div
                                // does nothing destructive. The Tab order
                                // then runs div → Cancel → Delete, so a
                                // first Tab lands on Cancel.
                                class: "px-3 py-1.5 text-sm rounded-lg bg-surface hover:bg-surface/80 text-text transition-colors",
                                onclick: move |_| confirm_delete_open.set(false),
                                "Cancel"
                            }
                            button {
                                class: "px-3 py-1.5 text-sm rounded-lg bg-red-600 hover:bg-red-500 text-white transition-colors",
                                onclick: purge_thread,
                                "Delete"
                            }
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
    /// Structured invite delivered via DM. Rendered as an inset card with
    /// the target room name, an optional personal message, and an Accept
    /// button that hands off to the URL-bar invite-accept flow via
    /// [`present_invitation`]. Only ever set on inbound DMs — outbound
    /// invites the local user sent are rendered through the
    /// `[Invitation] …` summary cached in `OUTBOUND_DMS` because the
    /// outbound cache doesn't carry the structured body bytes today.
    ///
    /// Boxed because [`InviteCardData`] is ~240 bytes and would otherwise
    /// blow up `BodyKind`'s stack size (clippy::large_enum_variant). Wire
    /// format is unaffected — `BodyKind` is a render-time enum, not a
    /// wire type.
    Invite(Box<InviteCardData>),
}

/// Pre-resolved metadata for an inbound invite-DM card. Built during the
/// memo pass (under the existing ROOMS read) so the bubble render path
/// itself does no extra signal reads.
#[derive(Clone, PartialEq)]
struct InviteCardData {
    /// The decoded structured payload. Includes the room owner key and
    /// the CBOR-encoded `Invitation` bytes that the Accept button hands
    /// off to [`present_invitation`].
    payload: InvitePayload,
    /// Friendly target-room label resolved at memo time:
    /// configured display name if the local user is already a member of
    /// that room, otherwise a short owner-key prefix.
    room_label: String,
    /// State of the local user with respect to the target room. Drives
    /// the Accept button label / disabled state on the card. Replaces an
    /// earlier `already_member: bool` that conflated "banned" with
    /// "joinable" — banned users saw an enabled "Accept invitation"
    /// button that would silently fail on submit (#280).
    card_state: InviteCardState,
    /// Optional sender-typed message rendered inside the card above the
    /// Accept button. `None` collapses the message slot entirely so we
    /// don't render an empty line.
    personal_message: Option<String>,
}

/// Three-way classification of the local user vs the invitation's
/// target room, computed at memo time from `RoomData::can_participate`.
///
/// * `Joinable` — room not loaded OR loaded but the user is neither a
///   member nor banned. Accept proceeds normally.
/// * `AlreadyMember` — `can_participate() == Ok(())`. Accept button is
///   disabled and re-labelled to avoid duplicate-join attempts.
/// * `Banned` — `can_participate() == Err(UserBanned)`. Accept button
///   is disabled with destructive styling and a clear message; without
///   this, the prior `already_member = can_participate().is_ok()`
///   collapse left the Accept button enabled for banned users, which
///   would silently fail on submit (#280).
#[derive(Clone, Copy, PartialEq, Debug)]
enum InviteCardState {
    Joinable,
    AlreadyMember,
    Banned,
}

/// Classify `can_participate()` (or "room not loaded") into the
/// three-way card state used by [`DmInviteCard`]. Extracted as a pure
/// function so the regression test
/// `invite_card_state_distinguishes_banned_from_not_a_member` doesn't
/// have to stand up a full `RoomData`.
fn classify_invite_card_state(
    can_participate: Option<Result<(), SendMessageError>>,
) -> InviteCardState {
    match can_participate {
        Some(Ok(())) => InviteCardState::AlreadyMember,
        Some(Err(SendMessageError::UserBanned)) => InviteCardState::Banned,
        // `UserNotMember` and "room not loaded" both fall through to
        // joinable — the Accept flow itself handles "not yet a member"
        // via the normal invitation path.
        Some(Err(SendMessageError::UserNotMember)) | None => InviteCardState::Joinable,
    }
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
fn DmBubble(
    message: RenderedDm,
    /// When `Some`, this bubble is the LAST one in the rendered list
    /// and should report its mount via the supplied signal so the
    /// auto-scroll effect in [`DmThreadModalBody`] can re-fire (issue
    /// freenet/river#283). Only the last bubble carries this; earlier
    /// bubbles' mounts are irrelevant to scroll-to-bottom.
    last_bubble_sink: Option<Signal<Option<Rc<MountedData>>>>,
) -> Element {
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
        BodyKind::Invite(card) => {
            // Inset card rendered inside the bubble column. The Accept
            // button decodes the CBOR-encoded `Invitation` payload and
            // hands off to `present_invitation`, which is the same entry
            // point the URL-bar accept flow uses. Already-member rooms
            // disable the button and relabel it to reduce confusion.
            //
            // Deref the box at the boundary so `DmInviteCard`'s prop
            // type stays the unboxed `InviteCardData` — the box exists
            // purely to satisfy `clippy::large_enum_variant` on
            // `BodyKind`.
            let card: InviteCardData = *card;
            rsx! {
                DmInviteCard { card: card }
            }
        }
    };
    rsx! {
        div {
            class: "flex flex-col",
            onmounted: move |cx| {
                if let Some(mut sink) = last_bubble_sink {
                    sink.set(Some(cx.data()));
                }
            },
            {bubble_body}
            span {
                class: if message.outgoing { "self-end text-[10px] text-text-muted mt-0.5" } else { "self-start text-[10px] text-text-muted mt-0.5" },
                "{ts_label}"
            }
        }
    }
}

/// Inbound invite-DM card. Decodes the CBOR `Invitation` payload on
/// Accept and routes through [`present_invitation`] — the same entry
/// point the URL-bar accept flow uses, so there's exactly one
/// invitation-accept code path. If the local user is already a member
/// of the target room, the Accept button is disabled and relabelled.
///
/// Pure-presentational: takes `InviteCardData` snapshotted by the
/// memo, so re-renders triggered by `decode_invitation_from_payload`
/// errors don't churn ROOMS subscriptions. All cross-component state
/// goes through `present_invitation` → `PRESENT_INVITATION_REQUEST` →
/// the `App` bridge effect, which sets `receive_invitation` and pops
/// the modal.
#[component]
fn DmInviteCard(card: InviteCardData) -> Element {
    let room_label = card.room_label.clone();
    let card_state = card.card_state;
    let invitation_payload_bytes = card.payload.invitation_payload.clone();
    let expected_room = card.payload.room_owner_vk;
    let personal_message = card.personal_message.clone();

    // Local error string for "couldn't decode" — surfaced inline rather
    // than dropping into a toast so the user has context (the card is
    // right above the message that caused the failure). Empty by default.
    let mut accept_error: Signal<Option<String>> = use_signal(|| None);

    let accept_label = match card_state {
        InviteCardState::AlreadyMember => "Already a member",
        InviteCardState::Banned => "You're banned from this room",
        InviteCardState::Joinable => "Accept invitation",
    };
    let button_class = match card_state {
        InviteCardState::AlreadyMember => {
            "px-3 py-1.5 text-sm rounded-lg bg-surface text-text-muted cursor-not-allowed"
        }
        InviteCardState::Banned => {
            // Destructive-tinted disabled button. `cursor-not-allowed`
            // mirrors the AlreadyMember styling so the affordance is
            // identical (the button doesn't act); the red tint
            // communicates the failure mode.
            "px-3 py-1.5 text-sm rounded-lg bg-red-500/20 text-red-300 border border-red-500/40 cursor-not-allowed"
        }
        InviteCardState::Joinable => {
            "px-3 py-1.5 text-sm rounded-lg bg-accent hover:bg-accent-hover text-white transition-colors"
        }
    };
    let is_disabled = !matches!(card_state, InviteCardState::Joinable);

    let on_accept = move |_| {
        if is_disabled {
            return;
        }
        accept_error.set(None);
        match decode_invitation_from_payload(&invitation_payload_bytes, &expected_room) {
            Ok(invitation) => {
                info!("DM invite card: accept → present_invitation");
                present_invitation(invitation);
            }
            Err(e) => {
                warn!("DM invite card: decode failed: {}", e);
                accept_error.set(Some(format!("Couldn't open invitation: {}", e)));
            }
        }
    };

    rsx! {
        div {
            class: "self-start max-w-[85%] rounded-lg border border-accent/40 bg-accent/10 p-3 space-y-2",
            // Subtle banner label so the recipient knows at a glance
            // this is structured (vs prose).
            div { class: "flex items-center gap-2 text-[10px] uppercase tracking-wide text-accent",
                span { "Invitation" }
            }
            div { class: "text-sm text-text font-medium",
                "Invitation to "
                span { class: "text-accent", "{room_label}" }
            }
            if let Some(msg) = personal_message.as_ref() {
                // Render the personal message as plain text — no markdown
                // pass, matching the conservative Placeholder path. Inside
                // a card we keep typography minimal so the Accept button
                // remains the primary visual anchor.
                div { class: "text-xs text-text-muted whitespace-pre-wrap break-words",
                    "{msg}"
                }
            }
            if let Some(err) = accept_error.read().as_ref() {
                div { class: "text-xs text-red-400", "{err}" }
            }
            div { class: "flex justify-end pt-1",
                button {
                    class: "{button_class}",
                    disabled: is_disabled,
                    onclick: on_accept,
                    "{accept_label}"
                }
            }
        }
    }
}

/// Decode the CBOR-encoded `Invitation` carried inside an `InvitePayload`
/// and validate that its inner `invitation.room` matches the payload's
/// outer `room_owner_vk` (per `dm_body.rs`'s documented invariant).
/// Mismatches are rejected so a malicious sender can't construct a card
/// labelled with one room and an Accept payload that joins a different
/// room.
///
/// Pulled out as a pure function so the regression test
/// `decode_invitation_rejects_room_mismatch` doesn't have to touch any
/// signal wiring.
fn decode_invitation_from_payload(
    bytes: &[u8],
    expected_room: &VerifyingKey,
) -> Result<Invitation, String> {
    let invitation: Invitation = ciborium::de::from_reader(bytes)
        .map_err(|e| format!("invitation payload not valid CBOR: {}", e))?;
    if &invitation.room != expected_room {
        return Err("invitation's room key doesn't match the card's room key".into());
    }
    Ok(invitation)
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

/// Short base58 prefix for a room owner verifying key, used when the
/// local user isn't already a member of the target room (and so has no
/// configured display name to surface). Matches the CLI's
/// `format_dm_body_for_cli` convention so UI / CLI users see the same
/// short identifier for the same room.
fn short_vk_prefix(vk: &VerifyingKey) -> String {
    bs58::encode(vk.as_bytes())
        .into_string()
        .chars()
        .take(8)
        .collect()
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

/// Returns `true` if the DM scroll container is currently scrolled to
/// within `tolerance_px` of its bottom edge. Returns `false` if the
/// container is missing (e.g. modal not mounted) so callers default to
/// "don't yank the viewport" — safer than the opposite.
#[cfg(target_arch = "wasm32")]
fn is_near_bottom(container_id: &str, tolerance_px: f64) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let Some(document) = window.document() else {
        return false;
    };
    let Some(container) = document.get_element_by_id(container_id) else {
        return false;
    };
    let scroll_top = container.scroll_top() as f64;
    let client_height = container.client_height() as f64;
    let scroll_height = container.scroll_height() as f64;
    // Distance from current bottom-edge of the viewport to the
    // content's bottom. Zero when fully scrolled down.
    let distance_from_bottom = scroll_height - (scroll_top + client_height);
    distance_from_bottom <= tolerance_px
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
fn is_near_bottom(_container_id: &str, _tolerance_px: f64) -> bool {
    false
}

/// Scroll the DM thread container to its bottom edge using the given
/// behavior (smooth on subsequent triggers, instant on initial mount).
/// No-op when the container isn't in the DOM yet — safe to call from
/// effect bodies.
#[cfg(target_arch = "wasm32")]
fn scroll_dm_container_to_bottom(behavior: web_sys::ScrollBehavior) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };
    let Some(container) = document.get_element_by_id("dm-scroll-container") else {
        return;
    };
    let opts = web_sys::ScrollToOptions::new();
    opts.set_top(container.scroll_height() as f64);
    opts.set_behavior(behavior);
    container.scroll_to_with_scroll_to_options(&opts);
}

/// Pure helper: merge an incoming DM_DRAFT body into whatever the user
/// has already typed into the composer. If the existing draft is empty
/// (or whitespace-only), the new body replaces it entirely; otherwise
/// the body is appended after a blank line so the user's text is
/// preserved.
///
/// Pinned by `merge_invite_into_draft_*` tests below. Pulled out as a
/// pure function so the regression test for issue freenet/river#267
/// ("Generating…" tab lockup) can verify the result *without* touching
/// the Dioxus signal subscription wiring — the bug was in the effect,
/// the merge logic itself is fine and worth keeping testable.
fn merge_invite_into_draft(existing: &str, body: &str) -> String {
    if existing.trim().is_empty() {
        body.to_string()
    } else {
        format!("{}\n\n{}", existing.trim_end(), body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_invite_into_draft_replaces_empty() {
        assert_eq!(merge_invite_into_draft("", "INVITE_URL"), "INVITE_URL");
    }

    #[test]
    fn merge_invite_into_draft_replaces_whitespace_only() {
        assert_eq!(
            merge_invite_into_draft("   \n  ", "INVITE_URL"),
            "INVITE_URL"
        );
    }

    #[test]
    fn merge_invite_into_draft_appends_after_user_text() {
        assert_eq!(
            merge_invite_into_draft("hello there", "INVITE_URL"),
            "hello there\n\nINVITE_URL"
        );
    }

    #[test]
    fn merge_invite_into_draft_trims_trailing_whitespace_before_appending() {
        assert_eq!(
            merge_invite_into_draft("hello there   \n", "INVITE_URL"),
            "hello there\n\nINVITE_URL"
        );
    }

    /// Issue freenet/river#267 regression: the merge is idempotent
    /// only when DM_DRAFT is cleared between effect runs. The effect
    /// itself enforces that, but if the clear ever regressed to a
    /// `defer()` and the effect re-fired before the clear ran, the
    /// merge would produce `"INVITE_URL\n\nINVITE_URL\n\nINVITE_URL"`
    /// instead. This test pins the math the effect's loop-guard
    /// depends on: re-applying the merge with the previous output as
    /// the existing draft keeps growing the string — i.e. the merge
    /// is NOT self-stable, so the synchronous clear in the effect is
    /// load-bearing.
    #[test]
    fn merge_invite_into_draft_is_not_self_stable_without_external_clear() {
        let body = "INVITE_URL";
        let first = merge_invite_into_draft("", body);
        let second = merge_invite_into_draft(&first, body);
        assert_ne!(
            first, second,
            "merge must NOT be idempotent on its own — \
            the effect's synchronous DM_DRAFT clear is the only thing preventing the loop"
        );
        let third = merge_invite_into_draft(&second, body);
        assert!(
            third.len() > second.len(),
            "string grows on each re-merge — \
            confirms #267's growth pattern"
        );
    }

    use ed25519_dalek::SigningKey;
    use river_core::room_state::member::{AuthorizedMember, Member, MemberId as MemberIdInner};

    fn fixed_signing(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn make_invitation(room_owner_sk: &SigningKey) -> Invitation {
        let room_owner_vk = room_owner_sk.verifying_key();
        let owner_id: MemberIdInner = (&room_owner_vk).into();
        let invitee_signing_key = fixed_signing(99);
        let invitee_vk = invitee_signing_key.verifying_key();
        let member = Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: invitee_vk,
        };
        let authorized = AuthorizedMember::new(member, room_owner_sk);
        Invitation {
            room: room_owner_vk,
            invitee_signing_key,
            invitee: authorized,
            room_secrets: Vec::new(),
        }
    }

    fn encode_invitation_bytes(inv: &Invitation) -> Vec<u8> {
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(inv, &mut bytes).expect("encode invitation");
        bytes
    }

    /// Happy path: a well-formed CBOR `Invitation` whose inner room key
    /// matches the outer payload room key decodes into the same value
    /// it was encoded from.
    #[test]
    fn decode_invitation_accepts_well_formed_matching_card() {
        let room_owner_sk = fixed_signing(1);
        let invitation = make_invitation(&room_owner_sk);
        let bytes = encode_invitation_bytes(&invitation);
        let decoded =
            decode_invitation_from_payload(&bytes, &room_owner_sk.verifying_key()).expect("ok");
        assert_eq!(decoded.room, invitation.room);
        assert_eq!(
            decoded.invitee.member.member_vk,
            invitation.invitee.member.member_vk
        );
    }

    /// Security-relevant: the card's outer `room_owner_vk` MUST match
    /// the embedded invitation's `room` field. A sender that lies about
    /// the destination (card says "Room A" but the embedded Invitation
    /// joins Room B) is rejected. Pinned because the only thing
    /// stopping that attack from confusing users is this check.
    #[test]
    fn decode_invitation_rejects_room_mismatch() {
        let room_owner_sk = fixed_signing(1);
        let attacker_sk = fixed_signing(2);
        let invitation = make_invitation(&room_owner_sk);
        let bytes = encode_invitation_bytes(&invitation);
        // Caller passes `attacker_sk.verifying_key()` as the "expected"
        // room — i.e. the card claimed it was for Room B but the
        // payload is actually for Room A. Must reject.
        let result = decode_invitation_from_payload(&bytes, &attacker_sk.verifying_key());
        assert!(result.is_err(), "mismatched room must be rejected");
    }

    /// Garbage bytes that don't even parse as CBOR fail cleanly with
    /// an error string the UI can surface, not a panic. Pinned because
    /// the Accept button's error-surfacing depends on this returning
    /// `Err`, not unwinding.
    #[test]
    fn decode_invitation_rejects_invalid_cbor() {
        let room_owner_sk = fixed_signing(1);
        let bytes = vec![0xff, 0xff, 0xff, 0xff];
        let result = decode_invitation_from_payload(&bytes, &room_owner_sk.verifying_key());
        assert!(result.is_err(), "invalid CBOR must be rejected");
    }

    /// Empty bytes — defensive guard. CBOR allows the empty sequence
    /// for some shapes but not for our struct.
    #[test]
    fn decode_invitation_rejects_empty_bytes() {
        let room_owner_sk = fixed_signing(1);
        let result = decode_invitation_from_payload(&[], &room_owner_sk.verifying_key());
        assert!(result.is_err(), "empty payload must be rejected");
    }

    /// Pin the prefix length for [`short_vk_prefix`] — the card's label
    /// for "not-yet-a-member" rooms is "Room <8-char prefix>", and
    /// the CLI uses the same convention so users see the same string
    /// for the same room.
    #[test]
    fn short_vk_prefix_is_eight_chars() {
        let vk = fixed_signing(7).verifying_key();
        let prefix = short_vk_prefix(&vk);
        assert_eq!(prefix.chars().count(), 8);
    }

    // -----------------------------------------------------------------
    // Issue freenet/river#280 regression coverage:
    // `classify_invite_card_state` must distinguish "banned" from
    // "not-a-member" / "already-member" so the Accept button is
    // appropriately disabled with destructive copy when the local user
    // is banned. The previous `already_member = can_participate().is_ok()`
    // collapse rendered an ENABLED "Accept invitation" button for
    // banned users that would silently fail on submit.
    // -----------------------------------------------------------------

    /// `Some(Ok(()))` — local user IS a member of the target room.
    /// Card must read "Already a member".
    #[test]
    fn invite_card_state_classifies_already_member() {
        assert_eq!(
            classify_invite_card_state(Some(Ok(()))),
            InviteCardState::AlreadyMember,
        );
    }

    /// `Some(Err(UserBanned))` — local user is BANNED. Must NOT collapse
    /// to "Joinable" (the #280 bug); must distinguish from the
    /// not-a-member case so the card can render a clear disabled-with-
    /// reason state instead of an enabled-but-doomed-to-fail button.
    #[test]
    fn invite_card_state_classifies_banned() {
        assert_eq!(
            classify_invite_card_state(Some(Err(SendMessageError::UserBanned))),
            InviteCardState::Banned,
        );
    }

    /// `Some(Err(UserNotMember))` — room is loaded (observer-only) but
    /// the user can still join via the invite. Card stays joinable.
    #[test]
    fn invite_card_state_classifies_not_member_as_joinable() {
        assert_eq!(
            classify_invite_card_state(Some(Err(SendMessageError::UserNotMember))),
            InviteCardState::Joinable,
        );
    }

    /// `None` — room not loaded at all. Card stays joinable — the Accept
    /// flow will resolve the room on demand.
    #[test]
    fn invite_card_state_classifies_room_not_loaded_as_joinable() {
        assert_eq!(classify_invite_card_state(None), InviteCardState::Joinable,);
    }

    /// Explicit "banned must not collide with not-a-member" sanity
    /// check. Mirrors the issue title verbatim so a future refactor
    /// that re-merges the two states is caught by a test name match.
    #[test]
    fn invite_card_state_distinguishes_banned_from_not_a_member() {
        let banned = classify_invite_card_state(Some(Err(SendMessageError::UserBanned)));
        let not_member = classify_invite_card_state(Some(Err(SendMessageError::UserNotMember)));
        assert_ne!(
            banned, not_member,
            "Banned and UserNotMember must classify to distinct InviteCardStates (#280)"
        );
    }

    // -----------------------------------------------------------------
    // Issue freenet/river#283 regression guard:
    //
    // PR #278's Codex round-1 fix removed a `.read()` on
    // `OUTBOUND_SEND_COUNTER` from inside the auto-scroll
    // `use_effect`, but didn't replace it with any other subscribed
    // read. The captured `message_count: usize` is a plain value, not
    // a signal — Dioxus has nothing to watch, so the effect runs once
    // on mount and never re-fires. New messages don't scroll the
    // viewport. The fix mirrors `conversation.rs`'s last-bubble
    // pattern: the last bubble's `onmounted` updates a `Signal<Option
    // <Rc<MountedData>>>` and the effect reads that signal as a
    // subscribing read.
    //
    // We can't easily drive the full effect through a unit test
    // (Dioxus runtime + WASM rendering), so this pin is a source-text
    // grep that fails if the wiring regresses: the effect MUST
    // contain `last_dm_bubble()` and the `DmBubble` component MUST
    // accept a `last_bubble_sink` prop. A future agent that "cleans
    // up" the unused-looking signal would otherwise silently re-break
    // the auto-scroll on the same lines as #283.
    // -----------------------------------------------------------------

    /// The auto-scroll effect MUST read `last_dm_bubble()` as a
    /// subscribing call inside its closure body. Without that read
    /// Dioxus has no signal subscription and the effect runs exactly
    /// once on mount — re-breaking #283.
    #[test]
    fn auto_scroll_effect_subscribes_to_last_dm_bubble_signal() {
        let src = include_str!("dm_thread_modal.rs");
        assert!(
            src.contains("let trigger = last_dm_bubble();"),
            "the auto-scroll use_effect must read last_dm_bubble() as a \
             subscribing call so it re-fires on new bubble mounts (#283). \
             If you change the read shape, update this pin to match."
        );
    }

    /// The `DmBubble` component MUST carry an optional last-bubble sink
    /// prop and attach `onmounted` to its wrapper. Without that prop
    /// the last bubble can't notify the auto-scroll effect — the
    /// signal stays at `None` forever and #283 silently re-regresses.
    #[test]
    fn dm_bubble_exposes_last_bubble_sink_prop() {
        let src = include_str!("dm_thread_modal.rs");
        assert!(
            src.contains("last_bubble_sink: Option<Signal<Option<Rc<MountedData>>>>"),
            "DmBubble must accept last_bubble_sink so the LAST bubble can \
             notify the auto-scroll effect's signal on mount (#283)."
        );
        assert!(
            src.contains("if let Some(mut sink) = last_bubble_sink"),
            "DmBubble's onmounted handler must write through last_bubble_sink \
             when it's Some (i.e. the bubble is the last one). Without this \
             write the auto-scroll effect's signal stays at None and #283 \
             silently re-regresses."
        );
    }
}
