//! Moving outbox entries (`outbox.rs`) through signing into `ROOMS`.
//!
//! Every signal write here runs in a clean execution context (inside
//! `crate::util::defer`), per `.claude/rules/dioxus-signal-safety.md`. The
//! `*_now` functions write directly and must be called from a deferred
//! closure; `retry` and `discard` defer themselves.

use super::outbox::{PendingEntry, PendingKind, SigningWork, OUTBOX};
use crate::components::app::ROOMS;
use crate::room_data::RoomData;
use dioxus::logger::tracing::{error, info, warn};
use dioxus::prelude::*;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_scaffold::ComposableState;
use river_core::room_state::message::MessageV1;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta};

/// Add an entry to `room`'s outbox and start signing it. Returns the entry's
/// id. From a deferred closure only: a pending send renders from here, so
/// this is the write the user sees on Enter.
pub(crate) fn submit_now(
    room: VerifyingKey,
    kind: PendingKind,
    author_vk: VerifyingKey,
    messages: Vec<(MessageV1, Vec<u8>)>,
    fallback_key: Option<SigningKey>,
) -> u64 {
    let (id, work) = OUTBOX.with_mut(|o| o.enqueue(room, kind, author_vk, messages));
    start_signing(room, id, author_vk, work, fallback_key);
    id
}

/// Sign a failed entry again. The fallback key is read afresh from `ROOMS`.
pub(crate) fn retry(room: VerifyingKey, id: u64) {
    crate::util::defer(move || {
        let fallback_key = ROOMS
            .peek()
            .map
            .get(&room)
            .and_then(|r| r.signing_key().cloned());
        // The fallback key is the current identity's; `sign_message_with_fallback`
        // uses it only if it matches `author_vk`, so a row from a replaced
        // identity cannot be signed with the new key.
        if let Some((author_vk, work)) =
            OUTBOX.with_mut(|o| o.retry(&room, id, crate::util::get_current_system_time()))
        {
            start_signing(room, id, author_vk, work, fallback_key);
        }
    });
}

/// Whether any message is waiting in the outbox (signing, or failed and kept
/// for a retry). The outbox is memory only, so a page reload loses these:
/// the generation reload treats this as the reader being busy, and waits for
/// it before reloading. Unreadable means "cannot tell": busy.
#[cfg(target_arch = "wasm32")]
pub(crate) fn has_unsent() -> bool {
    OUTBOX.try_read().map_or(true, |o| !o.is_empty())
}

/// Drop everything waiting in `room`: the user left it. From a deferred
/// closure only.
pub(crate) fn clear_room_now(room: VerifyingKey) {
    if OUTBOX.peek().has_room(&room) {
        OUTBOX.with_mut(|o| o.clear_room(&room));
    }
}

/// Drop a failed entry at the user's request.
pub(crate) fn discard(room: VerifyingKey, id: u64) {
    crate::util::defer(move || {
        OUTBOX.with_mut(|o| o.discard(&room, id));
    });
}

/// One delegate request per message, each in its own task, so a stalled one
/// does not delay the others' requests (their commits still wait their turn).
fn start_signing(
    room: VerifyingKey,
    id: u64,
    author_vk: VerifyingKey,
    work: SigningWork,
    fallback_key: Option<SigningKey>,
) {
    for (index, bytes) in work {
        let fallback_key = fallback_key.clone();
        crate::util::safe_spawn_local(async move {
            #[cfg(all(target_arch = "wasm32", feature = "example-data", feature = "no-sync"))]
            let injected_failure = {
                crate::test_hooks::message_sign_gate().await;
                crate::test_hooks::take_message_sign_failure()
            };
            #[cfg(not(all(
                target_arch = "wasm32",
                feature = "example-data",
                feature = "no-sync"
            )))]
            let injected_failure = false;

            let result = if injected_failure {
                Err("signing failed (test hook)".to_string())
            } else {
                crate::signing::sign_message_with_fallback(
                    room.to_bytes(),
                    bytes,
                    author_vk,
                    fallback_key,
                )
                .await
            };
            match &result {
                Ok((_, signer)) => {
                    crate::util::debug_log(&format!("[send] signed via {signer:?}"));
                    info!("Message {id} signed via {signer:?}");
                }
                Err(e) => {
                    crate::util::debug_log(&format!("[send] signing FAILED: {e}"));
                    warn!("Message {id} could not be signed: {e}");
                }
            }
            crate::util::defer(move || {
                #[cfg(all(target_arch = "wasm32", feature = "example-data", feature = "no-sync"))]
                crate::test_hooks::note_message_sign_recorded();
                OUTBOX.with_mut(|o| match result {
                    Ok((sig, _)) => {
                        #[cfg(all(
                            target_arch = "wasm32",
                            feature = "example-data",
                            feature = "no-sync"
                        ))]
                        let sig = crate::test_hooks::forge_if_requested(sig);
                        o.record_signature(&room, id, index, sig)
                    }
                    Err(e) => o.record_failure(&room, id, e),
                });
                report_failed_actions_now(room);
                commit_ready_now(room);
            });
        });
    }
}

/// Commit, in order, every entry of `room` that is ready, in the same tick
/// that removes it from the outbox: the pending row and the committed message
/// are swapped in one render, so neither a gap nor a duplicate shows.
pub(crate) fn commit_ready_now(room: VerifyingKey) {
    if !ROOMS.peek().map.contains_key(&room) {
        // Left (on any path) while these were signing: there is nowhere to
        // commit them or show them, so they go with the room.
        if OUTBOX.peek().has_room(&room) {
            let dropped = OUTBOX.with_mut(|o| o.clear_room(&room));
            warn!("Room left with {dropped} unsent message(s); dropped");
        }
        return;
    }
    // An identity import replaced the room's identity while these waited:
    // they are never committed (`apply_entry`) or shown under the new one.
    // The key is bound first so no `ROOMS` borrow is live during the write.
    let current = ROOMS
        .peek()
        .map
        .get(&room)
        .and_then(|r| r.self_verifying_key());
    if let Some(current) = current {
        if OUTBOX.peek().has_other_identities(&room, &current) {
            let dropped = OUTBOX.with_mut(|o| o.drop_other_identities(&room, &current));
            warn!(
                "Room identity replaced with {dropped} unsent message(s) from the old one; dropped"
            );
            crate::components::toast::show_toast(
                format!("{dropped} unsent message(s) from your previous identity in this room were discarded"),
                None,
            );
        }
    }
    // Checked first so a result that commits nothing writes no signal.
    if !OUTBOX.peek().has_ready(&room) {
        return;
    }
    let ready = OUTBOX.with_mut(|o| o.take_ready(&room));
    let outcomes: Option<Vec<Result<(), String>>> = ROOMS.with_mut(|rooms| {
        let room_data = rooms.map.get_mut(&room)?;
        Some(ready.iter().map(|e| apply_entry(room_data, e)).collect())
    });
    let Some(outcomes) = outcomes else {
        // Unreachable: checked above, in the same synchronous call.
        warn!("Room left with {} unsent message(s); dropped", ready.len());
        return;
    };
    let mut any_applied = false;
    for (entry, outcome) in ready.into_iter().zip(outcomes) {
        match outcome {
            Ok(()) => any_applied = true,
            Err(reason) => {
                error!("Failed to apply message delta: {reason}");
                crate::util::debug_log(&format!("[send] delta FAILED: {reason}"));
                match entry.kind {
                    PendingKind::Send { .. } => {
                        OUTBOX.with_mut(|o| o.restore_failed(room, entry, reason))
                    }
                    PendingKind::Action { label, .. } => action_failed_toast(label),
                }
            }
        }
    }
    if any_applied {
        crate::util::debug_log("[send] delta applied OK, marking NEEDS_SYNC");
        crate::components::app::mark_needs_sync(room);
    }
}

/// A failed edit, delete or reaction has no row to show it on: say so once and
/// drop it.
fn report_failed_actions_now(room: VerifyingKey) {
    if !OUTBOX.peek().has_failed_actions(&room) {
        return;
    }
    let failed: Vec<PendingEntry> = OUTBOX.with_mut(|o| o.take_failed_actions(&room));
    for entry in failed {
        if let PendingKind::Action { label, .. } = entry.kind {
            action_failed_toast(label);
        }
    }
}

fn action_failed_toast(label: &'static str) {
    crate::components::toast::show_toast(format!("Your {label} could not be sent"), None);
}

/// Apply one entry's signed messages to the room, with a rejoin if this member
/// was pruned for inactivity.
fn apply_entry(room_data: &mut RoomData, entry: &PendingEntry) -> Result<(), String> {
    // Written as an identity this room no longer uses (an identity import
    // replaced it while this was signing): never publish it under the old one.
    if room_data.self_verifying_key() != Some(entry.author_vk) {
        return Err("this room's identity changed after the message was written".to_string());
    }
    let parent = room_data.room_state.clone();
    let result = apply_entry_to(room_data, entry, &parent);
    if result.is_err() {
        // All of the entry or none of it: a reaction swap whose second half
        // the room skipped must not leave the first half applied locally.
        room_data.room_state = parent;
    }
    result
}

fn apply_entry_to(
    room_data: &mut RoomData,
    entry: &PendingEntry,
    parent: &river_core::ChatRoomStateV1,
) -> Result<(), String> {
    let (members, member_info) = room_data.build_rejoin_delta();
    let delta = ChatRoomStateV1Delta {
        recent_messages: Some(entry.authorized()),
        members,
        member_info,
        ..Default::default()
    };
    room_data
        .room_state
        .apply_delta(
            parent,
            &ChatRoomParametersV1 {
                owner: room_data.owner_vk,
            },
            &Some(delta),
        )
        .map_err(|e| format!("{e:?}"))?;
    // apply_delta's built-in rebuild only handles public actions, so without
    // this the apply wipes private edits/deletes/reactions until the network
    // echo re-applies them (#310).
    room_data.rebuild_private_actions_state();
    // The room skips (does not reject) a message it does not admit (a banned
    // or pruned author, a cap), so an apply without error may still have
    // added nothing. Actions are messages too, so this holds for every kind.
    let held = &room_data.room_state.recent_messages.messages;
    if !entry
        .message_ids()
        .iter()
        .all(|id| held.iter().any(|m| m.id() == *id))
    {
        return Err("the room did not admit the message".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::outbox::Outbox;
    use super::*;
    use ed25519_dalek::Signer;
    use river_core::room_state::member::MemberId;
    use river_core::room_state::message::RoomMessageBody;
    use std::time::{Duration, SystemTime};

    fn owner_sk() -> SigningKey {
        // The key `test_minimal_room_data` stores as `self_sk`.
        SigningKey::from_bytes(&[1u8; 32])
    }

    fn queue_send(outbox: &mut Outbox, room: VerifyingKey, text: &str, ms: u64) -> u64 {
        let message = MessageV1 {
            room_owner: MemberId::from(&room),
            author: MemberId::from(&owner_sk().verifying_key()),
            content: RoomMessageBody::public(text.to_string()),
            time: SystemTime::now() - Duration::from_secs(60) + Duration::from_millis(ms),
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&message, &mut bytes).unwrap();
        let (id, work) = outbox.enqueue(
            room,
            PendingKind::Send {
                text: text.to_string(),
                author_name: "Owner".to_string(),
            },
            owner_sk().verifying_key(),
            vec![(message, bytes)],
        );
        for (i, bytes) in work {
            outbox.record_signature(&room, id, i, owner_sk().sign(&bytes));
        }
        id
    }

    fn texts_in(room_data: &RoomData) -> Vec<String> {
        room_data
            .room_state
            .recent_messages
            .messages
            .iter()
            .filter_map(|m| m.message.content.as_public_string())
            .collect()
    }

    #[test]
    fn signed_messages_commit_to_the_room_in_sent_order() {
        let room = owner_sk().verifying_key();
        let mut room_data = crate::room_data::test_minimal_room_data(room);
        let mut outbox = Outbox::default();
        queue_send(&mut outbox, room, "first", 1);
        queue_send(&mut outbox, room, "second", 2);
        for entry in outbox.take_ready(&room) {
            apply_entry(&mut room_data, &entry).expect("a signed message commits");
        }
        assert_eq!(texts_in(&room_data), ["first", "second"]);
    }

    #[test]
    fn a_wrongly_signed_message_is_refused() {
        let room = owner_sk().verifying_key();
        let mut room_data = crate::room_data::test_minimal_room_data(room);
        let mut outbox = Outbox::default();
        let id = queue_send(&mut outbox, room, "forged", 1);
        // A signature by some other key: what a stale delegate key would give
        // if the fallback check were bypassed.
        let other = SigningKey::from_bytes(&[7u8; 32]);
        outbox.record_failure(&room, id, String::new());
        let (_, work) = outbox.retry(&room, id, SystemTime::now()).unwrap();
        outbox.record_signature(&room, id, 0, other.sign(&work[0].1));
        let entry = outbox.take_ready(&room).pop().unwrap();
        assert!(apply_entry(&mut room_data, &entry).is_err());
        assert!(texts_in(&room_data).is_empty());
    }

    #[test]
    fn an_edit_and_a_reaction_commit_and_change_what_the_room_shows() {
        let room = owner_sk().verifying_key();
        let mut room_data = crate::room_data::test_minimal_room_data(room);
        let mut outbox = Outbox::default();
        queue_send(&mut outbox, room, "original", 1);
        let sent = outbox.take_ready(&room).pop().unwrap();
        apply_entry(&mut room_data, &sent).unwrap();
        let target = sent.message_ids()[0].clone();

        let action = |body: RoomMessageBody, ms: u64| {
            let message = MessageV1 {
                room_owner: MemberId::from(&room),
                author: MemberId::from(&owner_sk().verifying_key()),
                content: body,
                time: SystemTime::now() - Duration::from_secs(30) + Duration::from_millis(ms),
            };
            let mut bytes = Vec::new();
            ciborium::ser::into_writer(&message, &mut bytes).unwrap();
            (message, bytes)
        };
        let (id, work) = outbox.enqueue(
            room,
            PendingKind::Action {
                label: "edit",
                effects: vec![],
            },
            owner_sk().verifying_key(),
            vec![
                action(
                    RoomMessageBody::edit(target.clone(), "changed".to_string()),
                    1,
                ),
                action(
                    RoomMessageBody::reaction(target.clone(), "👍".to_string()),
                    2,
                ),
            ],
        );
        for (i, bytes) in work {
            outbox.record_signature(&room, id, i, owner_sk().sign(&bytes));
        }
        let entry = outbox.take_ready(&room).pop().unwrap();
        apply_entry(&mut room_data, &entry).expect("signed actions are admitted");
        let messages = &room_data.room_state.recent_messages;
        assert!(messages.is_edited(&target));
        assert!(messages
            .reactions(&target)
            .is_some_and(|r| r.contains_key("👍")));

        // The other two kinds: removing the reaction, then deleting.
        let (id, work) = outbox.enqueue(
            room,
            PendingKind::Action {
                label: "delete",
                effects: vec![],
            },
            owner_sk().verifying_key(),
            vec![
                action(
                    RoomMessageBody::remove_reaction(target.clone(), "👍".to_string()),
                    3,
                ),
                action(RoomMessageBody::delete(target.clone()), 4),
            ],
        );
        for (i, bytes) in work {
            outbox.record_signature(&room, id, i, owner_sk().sign(&bytes));
        }
        let entry = outbox.take_ready(&room).pop().unwrap();
        apply_entry(&mut room_data, &entry)
            .expect("a reaction removal and a delete are admitted, not reported as refused");
        let messages = &room_data.room_state.recent_messages;
        assert!(messages
            .reactions(&target)
            .is_none_or(|r| !r.contains_key("👍")));
        assert!(messages.display_messages().all(|m| m.id() != target));
    }

    /// An action the room does not admit is reported, so the user is told
    /// (`commit_ready_now` toasts it) instead of the change silently undoing.
    #[test]
    fn an_action_the_room_does_not_admit_is_reported() {
        let room = owner_sk().verifying_key();
        let mut room_data = crate::room_data::test_minimal_room_data(room);
        let mut outbox = Outbox::default();
        queue_send(&mut outbox, room, "original", 1);
        let sent = outbox.take_ready(&room).pop().unwrap();
        apply_entry(&mut room_data, &sent).unwrap();
        let target = sent.message_ids()[0].clone();

        // Validly signed by someone who is not a member of the room: the room
        // skips it without an error. (This device holds that identity, so it
        // is the admission check that refuses it, not the identity check.)
        let stranger = SigningKey::from_bytes(&[9u8; 32]);
        room_data.self_sk = Some(stranger.clone());
        let message = MessageV1 {
            room_owner: MemberId::from(&room),
            author: MemberId::from(&stranger.verifying_key()),
            content: RoomMessageBody::edit(target.clone(), "hijacked".to_string()),
            time: SystemTime::now(),
        };
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&message, &mut bytes).unwrap();
        let (id, work) = outbox.enqueue(
            room,
            PendingKind::Action {
                label: "edit",
                effects: vec![],
            },
            stranger.verifying_key(),
            vec![(message, bytes)],
        );
        outbox.record_signature(&room, id, 0, stranger.sign(&work[0].1));
        let entry = outbox.take_ready(&room).pop().unwrap();
        let before = room_data.room_state.clone();
        assert_eq!(
            apply_entry(&mut room_data, &entry),
            Err("the room did not admit the message".to_string())
        );
        assert!(!room_data.room_state.recent_messages.is_edited(&target));
        assert!(
            room_data.room_state == before,
            "a refused entry leaves no trace"
        );
    }

    /// A refused entry is undone whole: a two-message entry whose second
    /// message the room skips must not leave the first applied locally.
    #[test]
    fn a_refused_entry_leaves_no_half_applied() {
        let room = owner_sk().verifying_key();
        let mut room_data = crate::room_data::test_minimal_room_data(room);
        let mut outbox = Outbox::default();
        queue_send(&mut outbox, room, "original", 1);
        let sent = outbox.take_ready(&room).pop().unwrap();
        apply_entry(&mut room_data, &sent).unwrap();
        let target = sent.message_ids()[0].clone();

        let by = |sk: &SigningKey, body: RoomMessageBody| {
            let message = MessageV1 {
                room_owner: MemberId::from(&room),
                author: MemberId::from(&sk.verifying_key()),
                content: body,
                time: SystemTime::now(),
            };
            let mut bytes = Vec::new();
            ciborium::ser::into_writer(&message, &mut bytes).unwrap();
            (message, bytes)
        };
        // The first half is the owner's and is admitted; the second is a
        // validly signed message by a non-member, which the room skips.
        let stranger = SigningKey::from_bytes(&[9u8; 32]);
        let (id, work) = outbox.enqueue(
            room,
            PendingKind::Action {
                label: "reaction",
                effects: vec![],
            },
            owner_sk().verifying_key(),
            vec![
                by(
                    &owner_sk(),
                    RoomMessageBody::reaction(target.clone(), "👍".to_string()),
                ),
                by(
                    &stranger,
                    RoomMessageBody::reaction(target.clone(), "🎉".to_string()),
                ),
            ],
        );
        outbox.record_signature(&room, id, 0, owner_sk().sign(&work[0].1));
        outbox.record_signature(&room, id, 1, stranger.sign(&work[1].1));
        let entry = outbox.take_ready(&room).pop().unwrap();
        let before = room_data.room_state.clone();
        assert!(apply_entry(&mut room_data, &entry).is_err());
        assert!(
            room_data.room_state == before,
            "the admitted half was rolled back with the refused one"
        );
        assert!(room_data
            .room_state
            .recent_messages
            .reactions(&target)
            .is_none());
    }

    /// Guards that have no runtime to test them in: leaving a room clears its
    /// outbox, and a signing result that commits or reports nothing writes no
    /// signal (each write re-renders the conversation).
    #[test]
    fn leave_clears_and_empty_results_write_nothing() {
        let squash = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
        let leave = squash(include_str!("../room_list/edit_room_modal.rs"));
        let left = leave
            .find("ROOMS.write().leave_room(room_vk);")
            .expect("leave site");
        assert!(
            leave[left..].contains("clear_outbox_for_room_now(room_vk);"),
            "leaving a room must clear its outbox"
        );
        let source = include_str!("outbox_commit.rs");
        let prod = squash(&source[..source.find("#[cfg(test)]\nmod tests").unwrap()]);
        for (guard, write) in [
            (
                "OUTBOX.peek().has_other_identities(&room,&current)",
                "o.drop_other_identities(&room,&current)",
            ),
            ("OUTBOX.peek().has_ready(&room)", "o.take_ready(&room)"),
            (
                "OUTBOX.peek().has_failed_actions(&room)",
                "o.take_failed_actions(&room)",
            ),
        ] {
            let g = prod
                .find(guard)
                .unwrap_or_else(|| panic!("missing `{guard}`"));
            let w = prod
                .find(write)
                .unwrap_or_else(|| panic!("missing `{write}`"));
            assert!(g < w, "`{guard}` must come before `{write}`");
        }
    }

    /// A message written as an identity the room has since replaced (an
    /// identity import while it was signing) is never committed under it.
    #[test]
    fn a_message_from_a_replaced_identity_is_not_committed() {
        let room = owner_sk().verifying_key();
        let mut room_data = crate::room_data::test_minimal_room_data(room);
        let mut outbox = Outbox::default();
        queue_send(&mut outbox, room, "from the old identity", 1);
        let entry = outbox.take_ready(&room).pop().unwrap();
        room_data.self_sk = Some(SigningKey::from_bytes(&[3u8; 32]));
        assert!(apply_entry(&mut room_data, &entry).is_err());
        assert!(texts_in(&room_data).is_empty());
    }

    /// The commit takes the entry out of the outbox and writes it to `ROOMS`
    /// in one synchronous function, so the pending row and the committed
    /// message swap in one render: no gap and no duplicate. And messages are
    /// signed through the delegate, never with a local key directly.
    #[test]
    fn a_commit_is_one_tick_and_signing_goes_through_the_delegate() {
        let source = include_str!("outbox_commit.rs");
        let prod = &source[..source.find("#[cfg(test)]\nmod tests").unwrap()];
        let squashed: String = prod.chars().filter(|c| !c.is_whitespace()).collect();
        let start = squashed
            .find("pub(crate)fncommit_ready_now(")
            .expect("commit_ready_now exists");
        let body = &squashed[start..];
        let body = &body[..body.find("fnreport_failed_actions_now(").expect("next fn")];
        let take = body
            .find("o.take_ready(&room)")
            .expect("takes from the outbox");
        let write = body.find("ROOMS.with_mut(").expect("writes ROOMS");
        assert!(take < write);
        let between = &body[take..write];
        for forbidden in [".await", "defer(", "spawn_local("] {
            assert!(
                !between.contains(forbidden),
                "`{forbidden}` between taking an entry and committing it would \
                 render a frame with the message in neither place"
            );
        }
        assert!(squashed.contains("crate::signing::sign_message_with_fallback("));
        assert!(
            !squashed.contains(".sign("),
            "outbox_commit must not sign with a local key; the fallback lives in \
             `signing::sign_message_with_fallback`"
        );
    }
}
