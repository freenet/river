//! Moving outbox entries (`outbox.rs`) through signing into `ROOMS`.
//!
//! Every function here that writes a signal must run in a clean execution
//! context (inside `crate::util::defer`), per `.claude/rules/dioxus-signal-safety.md`;
//! they are named `*_now` to say so.

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
        if let Some((author_vk, work)) = OUTBOX.with_mut(|o| o.retry(&room, id)) {
            start_signing(room, id, author_vk, work, fallback_key);
        }
    });
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
    let ready = OUTBOX.with_mut(|o| o.take_ready(&room));
    if ready.is_empty() {
        return;
    }
    let outcomes: Option<Vec<Result<(), String>>> = ROOMS.with_mut(|rooms| {
        let room_data = rooms.map.get_mut(&room)?;
        Some(ready.iter().map(|e| apply_entry(room_data, e)).collect())
    });
    let Some(outcomes) = outcomes else {
        // The room was left while these were signing: there is nowhere to
        // commit them or show them, so they go with the room.
        let rest = OUTBOX.with_mut(|o| o.clear_room(&room));
        warn!(
            "Room left with {} unsent message(s); dropped",
            ready.len() + rest
        );
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
    let (members, member_info) = room_data.build_rejoin_delta();
    let delta = ChatRoomStateV1Delta {
        recent_messages: Some(entry.authorized()),
        members,
        member_info,
        ..Default::default()
    };
    let parent = room_data.room_state.clone();
    room_data
        .room_state
        .apply_delta(
            &parent,
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
    fn a_message_the_room_does_not_admit_is_reported_not_dropped_silently() {
        let room = owner_sk().verifying_key();
        let mut room_data = crate::room_data::test_minimal_room_data(room);
        let mut outbox = Outbox::default();
        let id = queue_send(&mut outbox, room, "forged", 1);
        // A signature by some other key: what a stale delegate key would give
        // if the fallback check were bypassed.
        let other = SigningKey::from_bytes(&[7u8; 32]);
        outbox.record_failure(&room, id, String::new());
        let (_, work) = outbox.retry(&room, id).unwrap();
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
