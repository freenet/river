use super::*;
use ed25519_dalek::{Signer, SigningKey};
use river_core::room_state::member::MemberId;
use river_core::room_state::message::RoomMessageBody;
use std::time::Duration;

fn room() -> VerifyingKey {
    SigningKey::from_bytes(&[1u8; 32]).verifying_key()
}

/// When a retry happens in these tests.
fn later() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(3600)
}

fn author() -> SigningKey {
    SigningKey::from_bytes(&[2u8; 32])
}

/// A message stamped `ms` after the epoch, and its CBOR bytes.
fn message(text: &str, ms: u64) -> (MessageV1, Vec<u8>) {
    let message = MessageV1 {
        room_owner: MemberId::from(&room()),
        author: MemberId::from(&author().verifying_key()),
        content: RoomMessageBody::public(text.to_string()),
        time: SystemTime::UNIX_EPOCH + Duration::from_millis(ms),
    };
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&message, &mut bytes).unwrap();
    (message, bytes)
}

fn send(outbox: &mut Outbox, text: &str, ms: u64) -> (u64, SigningWork) {
    outbox.enqueue(
        room(),
        PendingKind::Send {
            text: text.to_string(),
            author_name: "Me".to_string(),
        },
        author().verifying_key(),
        vec![message(text, ms)],
    )
}

fn sign_all(outbox: &mut Outbox, id: u64, work: &SigningWork) {
    for (i, bytes) in work {
        outbox.record_signature(&room(), id, *i, author().sign(bytes));
    }
}

fn texts(entries: &[PendingEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|e| match &e.kind {
            PendingKind::Send { text, .. } => text.clone(),
            PendingKind::Action { label, .. } => label.to_string(),
        })
        .collect()
}

#[test]
fn a_send_shows_as_pending_until_its_signature_arrives_then_commits_once() {
    let mut outbox = Outbox::default();
    let (id, work) = send(&mut outbox, "hello", 10);

    let rows = outbox.pending_rows(&room(), &author().verifying_key());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text, "hello");
    assert_eq!(rows[0].author_name, "Me");
    assert_eq!(rows[0].failed, None);
    assert_eq!(
        rows[0].time,
        SystemTime::UNIX_EPOCH + Duration::from_millis(10),
        "the row keeps the time the user sent it"
    );
    assert!(
        outbox.take_ready(&room()).is_empty(),
        "nothing commits before it is signed"
    );

    sign_all(&mut outbox, id, &work);
    let ready = outbox.take_ready(&room());
    assert_eq!(texts(&ready), ["hello"]);
    let signed = ready[0].authorized();
    signed[0]
        .validate(&author().verifying_key())
        .expect("the committed message verifies under the author's key");
    assert_eq!(
        signed[0].message.time,
        SystemTime::UNIX_EPOCH + Duration::from_millis(10),
        "signing does not restamp the message"
    );
    assert!(
        outbox
            .pending_rows(&room(), &author().verifying_key())
            .is_empty(),
        "the pending row is gone once the entry is taken to commit"
    );
    assert!(outbox.take_ready(&room()).is_empty(), "it commits once");
}

#[test]
fn messages_commit_in_the_order_they_were_sent_whatever_order_they_are_signed_in() {
    let mut outbox = Outbox::default();
    let (a, wa) = send(&mut outbox, "a", 1);
    let (b, wb) = send(&mut outbox, "b", 2);
    let (c, wc) = send(&mut outbox, "c", 3);

    sign_all(&mut outbox, c, &wc);
    sign_all(&mut outbox, b, &wb);
    assert!(
        !outbox.has_ready(&room()),
        "has_ready agrees with take_ready"
    );
    assert!(
        outbox.take_ready(&room()).is_empty(),
        "b and c are signed but must wait for a"
    );
    assert_eq!(
        outbox
            .pending_rows(&room(), &author().verifying_key())
            .len(),
        3
    );

    sign_all(&mut outbox, a, &wa);
    assert!(outbox.has_ready(&room()));
    assert_eq!(texts(&outbox.take_ready(&room())), ["a", "b", "c"]);
    assert!(!outbox.has_ready(&room()));
}

#[test]
fn a_failed_message_stays_and_does_not_hold_back_later_ones() {
    let mut outbox = Outbox::default();
    let (a, _) = send(&mut outbox, "a", 1);
    let (b, wb) = send(&mut outbox, "b", 2);

    outbox.record_failure(&room(), a, "no signing key".to_string());
    sign_all(&mut outbox, b, &wb);
    assert!(
        outbox.has_ready(&room()),
        "has_ready skips a failed head as take_ready does"
    );
    assert_eq!(texts(&outbox.take_ready(&room())), ["b"]);

    let rows = outbox.pending_rows(&room(), &author().verifying_key());
    assert_eq!(rows.len(), 1, "the failed text is kept, never dropped");
    assert_eq!(rows[0].text, "a");
    assert_eq!(rows[0].failed.as_deref(), Some("no signing key"));
}

#[test]
fn a_late_signature_for_a_failed_attempt_is_ignored() {
    let mut outbox = Outbox::default();
    let (a, wa) = send(&mut outbox, "a", 1);
    outbox.record_failure(&room(), a, "timed out".to_string());
    sign_all(&mut outbox, a, &wa);
    assert!(outbox.take_ready(&room()).is_empty());
    assert!(outbox.pending_rows(&room(), &author().verifying_key())[0]
        .failed
        .is_some());
}

#[test]
fn retry_signs_again_and_commits() {
    let mut outbox = Outbox::default();
    let (a, wa) = send(&mut outbox, "a", 1);
    assert!(
        outbox.retry(&room(), a, later(), None).is_none(),
        "only a failed entry can be retried"
    );
    outbox.record_failure(&room(), a, "timed out".to_string());

    let (vk, work) = outbox
        .retry(&room(), a, later(), None)
        .expect("a failed entry retries");
    assert_eq!(vk, author().verifying_key());
    assert_ne!(work, wa, "a retry is restamped, so it signs new bytes");
    assert_eq!(
        outbox.pending_rows(&room(), &author().verifying_key())[0].failed,
        None,
        "sending again"
    );
    sign_all(&mut outbox, a, &work);
    assert_eq!(texts(&outbox.take_ready(&room())), ["a"]);
}

#[test]
fn a_message_the_room_refused_goes_back_failed_in_its_place() {
    let mut outbox = Outbox::default();
    let (a, wa) = send(&mut outbox, "a", 1);
    let (b, wb) = send(&mut outbox, "b", 2);
    let (c, _) = send(&mut outbox, "c", 3);
    outbox.record_failure(&room(), c, "x".to_string());
    sign_all(&mut outbox, a, &wa);
    sign_all(&mut outbox, b, &wb);
    let mut ready = outbox.take_ready(&room());
    assert_eq!(texts(&ready), ["a", "b"]);

    let b_entry = ready.pop().unwrap();
    outbox.restore_failed(room(), b_entry, "not admitted".to_string());
    let rows = outbox.pending_rows(&room(), &author().verifying_key());
    assert_eq!(
        rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
        ["b", "c"],
        "restored in send order, before later entries"
    );
    assert_eq!(rows[0].failed.as_deref(), Some("not admitted"));
}

#[test]
fn a_reaction_swap_commits_only_when_both_halves_are_signed() {
    let mut outbox = Outbox::default();
    let (id, work) = outbox.enqueue(
        room(),
        PendingKind::Action {
            label: "reaction",
            effects: vec![],
        },
        author().verifying_key(),
        vec![message("remove", 1), message("add", 1)],
    );
    assert!(
        outbox
            .pending_rows(&room(), &author().verifying_key())
            .is_empty(),
        "an action is not shown as a pending row"
    );
    outbox.record_signature(&room(), id, 1, author().sign(&work[1].1));
    assert!(outbox.take_ready(&room()).is_empty());
    outbox.record_signature(&room(), id, 0, author().sign(&work[0].1));
    let ready = outbox.take_ready(&room());
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].authorized().len(), 2);
}

#[test]
fn discard_removes_the_entry() {
    let mut outbox = Outbox::default();
    let (a, _) = send(&mut outbox, "a", 1);
    outbox.record_failure(&room(), a, "x".to_string());
    assert!(outbox.discard(&room(), a).is_some());
    assert!(outbox
        .pending_rows(&room(), &author().verifying_key())
        .is_empty());
    assert!(outbox.discard(&room(), a).is_none());
}

#[test]
fn rooms_are_independent() {
    let mut outbox = Outbox::default();
    let other = SigningKey::from_bytes(&[3u8; 32]).verifying_key();
    let (_a, _) = send(&mut outbox, "held", 1);
    let (b, wb) = outbox.enqueue(
        other,
        PendingKind::Send {
            text: "other room".to_string(),
            author_name: "Me".to_string(),
        },
        author().verifying_key(),
        vec![message("other room", 2)],
    );
    for (i, bytes) in &wb {
        outbox.record_signature(&other, b, *i, author().sign(bytes));
    }
    assert_eq!(
        texts(&outbox.take_ready(&other)),
        ["other room"],
        "an unsigned message in one room does not hold back another room"
    );
}

#[test]
fn a_failed_action_is_taken_out_and_a_failed_send_is_not() {
    let mut outbox = Outbox::default();
    let (a, _) = send(&mut outbox, "a", 1);
    let (e, _) = outbox.enqueue(
        room(),
        PendingKind::Action {
            label: "edit",
            effects: vec![],
        },
        author().verifying_key(),
        vec![message("edit", 2)],
    );
    let (_pending_edit, _) = outbox.enqueue(
        room(),
        PendingKind::Action {
            label: "delete",
            effects: vec![],
        },
        author().verifying_key(),
        vec![message("delete", 3)],
    );
    outbox.record_failure(&room(), a, "x".to_string());
    assert!(
        !outbox.has_failed_actions(&room()),
        "a failed send is not one"
    );
    outbox.record_failure(&room(), e, "x".to_string());
    assert!(outbox.has_failed_actions(&room()));
    assert_eq!(texts(&outbox.take_failed_actions(&room())), ["edit"]);
    assert!(!outbox.has_failed_actions(&room()));
    assert_eq!(
        outbox
            .pending_rows(&room(), &author().verifying_key())
            .len(),
        1,
        "the failed send stays"
    );
    assert!(
        outbox.take_ready(&room()).is_empty(),
        "the still-signing delete keeps its place"
    );
}

#[test]
fn pending_effects_are_those_of_actions_still_signing() {
    let mut outbox = Outbox::default();
    let effect = |n: &str| PendingEffect::Edit {
        target: river_core::room_state::message::MessageId(freenet_scaffold::util::FastHash(1)),
        text: n.to_string(),
    };
    send(&mut outbox, "a send has no effect", 1);
    let (failed, _) = outbox.enqueue(
        room(),
        PendingKind::Action {
            label: "edit",
            effects: vec![effect("failed")],
        },
        author().verifying_key(),
        vec![message("x", 2)],
    );
    outbox.enqueue(
        room(),
        PendingKind::Action {
            label: "edit",
            effects: vec![effect("signing")],
        },
        author().verifying_key(),
        vec![message("y", 3)],
    );
    outbox.record_failure(&room(), failed, "x".to_string());
    assert_eq!(
        outbox.pending_effects(&room(), &author().verifying_key()),
        vec![effect("signing")]
    );
    let other = SigningKey::from_bytes(&[5u8; 32]).verifying_key();
    assert!(
        outbox.pending_effects(&room(), &other).is_empty(),
        "another identity's actions are not shown"
    );
}

/// By design: a message that failed and is retried commits after the ones
/// sent after it, and is restamped at the retry, so it shows after them too.
#[test]
fn a_retried_message_commits_after_the_ones_that_went_ahead() {
    let mut outbox = Outbox::default();
    let (a, _) = send(&mut outbox, "a", 1);
    let (b, wb) = send(&mut outbox, "b", 2);
    outbox.record_failure(&room(), a, "timed out".to_string());
    sign_all(&mut outbox, b, &wb);
    let (_, wa) = outbox.retry(&room(), a, later(), None).unwrap();
    sign_all(&mut outbox, a, &wa);
    let first = outbox.take_ready(&room());
    assert_eq!(texts(&first), ["a", "b"], "both ready: committed in order");

    let mut outbox = Outbox::default();
    let (a, _) = send(&mut outbox, "a", 1);
    let (b, wb) = send(&mut outbox, "b", 2);
    outbox.record_failure(&room(), a, "timed out".to_string());
    sign_all(&mut outbox, b, &wb);
    assert_eq!(texts(&outbox.take_ready(&room())), ["b"]);
    let (_, wa) = outbox.retry(&room(), a, later(), None).unwrap();
    sign_all(&mut outbox, a, &wa);
    assert_eq!(texts(&outbox.take_ready(&room())), ["a"]);
}

#[test]
fn a_refused_message_can_be_retried() {
    let mut outbox = Outbox::default();
    let (a, wa) = send(&mut outbox, "a", 1);
    sign_all(&mut outbox, a, &wa);
    let entry = outbox.take_ready(&room()).pop().unwrap();
    outbox.restore_failed(room(), entry, "not admitted".to_string());
    let (_, work) = outbox
        .retry(&room(), a, later(), None)
        .expect("a refused message retries");
    assert_ne!(work, wa, "a retry is restamped, so it signs new bytes");
    assert_eq!(
        outbox.pending_rows(&room(), &author().verifying_key())[0].time,
        later(),
        "the retried message carries the retry's time, so a room at its cap keeps it"
    );
    assert_eq!(
        outbox.pending_rows(&room(), &author().verifying_key())[0].failed,
        None
    );
}

#[test]
fn a_reaction_swap_with_one_half_failed_is_reported_whole() {
    let mut outbox = Outbox::default();
    let (id, work) = outbox.enqueue(
        room(),
        PendingKind::Action {
            label: "reaction",
            effects: vec![],
        },
        author().verifying_key(),
        vec![message("remove", 1), message("add", 1)],
    );
    outbox.record_failure(&room(), id, "timed out".to_string());
    // The other half's signature arrives afterwards and is ignored.
    outbox.record_signature(&room(), id, 1, author().sign(&work[1].1));
    assert!(
        outbox.take_ready(&room()).is_empty(),
        "neither half commits"
    );
    assert_eq!(texts(&outbox.take_failed_actions(&room())), ["reaction"]);
}

#[test]
fn leaving_a_room_drops_what_waits_in_it() {
    let mut outbox = Outbox::default();
    send(&mut outbox, "a", 1);
    send(&mut outbox, "b", 2);
    assert!(outbox.has_room(&room()));
    assert_eq!(outbox.clear_room(&room()), 2);
    assert!(outbox
        .pending_rows(&room(), &author().verifying_key())
        .is_empty());
    assert!(!outbox.has_room(&room()));
}

#[test]
fn entries_from_a_replaced_identity_are_neither_shown_nor_kept() {
    let mut outbox = Outbox::default();
    send(&mut outbox, "old identity", 1);
    let new_identity = SigningKey::from_bytes(&[5u8; 32]).verifying_key();
    assert!(
        outbox.pending_rows(&room(), &new_identity).is_empty(),
        "the new identity does not see the old one's pending text"
    );
    assert!(outbox.has_other_identities(&room(), &new_identity));
    assert_eq!(outbox.drop_other_identities(&room(), &new_identity), 1);
    assert!(!outbox.has_room(&room()));
}

#[test]
fn a_reload_would_lose_signing_messages_and_failed_rows_on_screen_only() {
    let me = author().verifying_key();
    let shown = |_: &VerifyingKey| Some(me);
    let mut outbox = Outbox::default();
    assert!(!outbox.has_unsent(shown));
    let (a, _) = send(&mut outbox, "a", 1);
    assert!(outbox.has_unsent(|_| None), "signing counts even unseen");
    outbox.record_failure(&room(), a, "x".to_string());
    assert!(
        outbox.has_unsent(shown),
        "a failed row on screen is held like a draft"
    );
    assert!(
        !outbox.has_unsent(|_| None),
        "a failed row in a room no longer held does not hold a reload forever"
    );
    let other = SigningKey::from_bytes(&[5u8; 32]).verifying_key();
    assert!(
        !outbox.has_unsent(|_| Some(other)),
        "nor does one written as a replaced identity"
    );
    let (e, _) = outbox.enqueue(
        room(),
        PendingKind::Action {
            label: "edit",
            effects: vec![],
        },
        me,
        vec![message("edit", 2)],
    );
    outbox.record_failure(&room(), e, "x".to_string());
    outbox.discard(&room(), a);
    assert!(
        !outbox.has_unsent(shown),
        "a failed action has no row; it is reported and dropped"
    );
    let (_, work) = outbox.retry(&room(), e, later(), None).unwrap();
    assert!(outbox.has_unsent(|_| None));
    sign_all(&mut outbox, e, &work);
    let _ = outbox.take_ready(&room());
    assert!(!outbox.has_unsent(shown));
}

/// A private message sealed under v1, signed while the room rotated to v2.
fn sealed_entry(outbox: &mut Outbox, text: &str, ms: u64, version: u32) -> (u64, SigningWork) {
    use super::super::seal::SealInput;
    let seal = SealInput::Text(text.to_string());
    let (mut message, _) = message(text, ms);
    message.content = seal.seal(&[1u8; 32], version);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&message, &mut bytes).unwrap();
    outbox.enqueue_sealed(
        room(),
        PendingKind::Send {
            text: text.to_string(),
            author_name: "Me".to_string(),
        },
        author().verifying_key(),
        vec![(message, bytes, Some(seal))],
    )
}

fn version_of(entry: &PendingEntry) -> u32 {
    match &entry.messages[0].message.content {
        RoomMessageBody::Private { secret_version, .. } => *secret_version,
        RoomMessageBody::Public { .. } => panic!("expected a sealed body"),
    }
}

/// The room's secret rotated (a join, a prune or a ban) while a private
/// message was being signed: it is not committed under the old secret, it is
/// sealed again under the new one, re-signed, and committed, keeping its
/// timestamp and its place ahead of what was sent after it.
#[test]
fn a_message_signed_across_a_rotation_is_sealed_again_and_committed_in_order() {
    let mut outbox = Outbox::default();
    let (a, wa) = sealed_entry(&mut outbox, "across the rotation", 10, 1);
    let (b, wb) = sealed_entry(&mut outbox, "after it", 20, 2);
    sign_all(&mut outbox, a, &wa);
    sign_all(&mut outbox, b, &wb);

    let (ready, stale) = outbox.take_ready_sealed_under(&room(), Some(Some(2)));
    assert!(ready.is_empty(), "nothing commits ahead of the stale entry");
    assert_eq!(stale, Some(a));

    let (vk, work) = outbox
        .reseal(&room(), a, &[2u8; 32], 2)
        .expect("sealed again under the current secret");
    assert_eq!(vk, author().verifying_key());
    assert_ne!(work, wa, "new bytes to sign");
    sign_all(&mut outbox, a, &work);

    let (ready, stale) = outbox.take_ready_sealed_under(&room(), Some(Some(2)));
    assert_eq!(stale, None);
    assert_eq!(texts(&ready), ["across the rotation", "after it"]);
    assert_eq!(version_of(&ready[0]), 2, "committed under the new secret");
    assert_eq!(
        ready[0].messages[0].message.time,
        SystemTime::UNIX_EPOCH + Duration::from_millis(10),
        "keeping its Enter-time timestamp"
    );
    ready[0].authorized()[0]
        .validate(&author().verifying_key())
        .expect("the new signature covers the resealed message");
}

#[test]
fn resealing_is_bounded_and_retry_seals_again() {
    let mut outbox = Outbox::default();
    let (a, _) = sealed_entry(&mut outbox, "keeps rotating", 10, 1);
    for v in 2..2 + MAX_RESEALS {
        assert!(outbox.reseal(&room(), a, &[3u8; 32], v).is_some());
    }
    assert!(
        outbox.reseal(&room(), a, &[3u8; 32], 99).is_none(),
        "bounded"
    );
    let rows = outbox.pending_rows(&room(), &author().verifying_key());
    assert!(rows[0].failed.is_some(), "shown as failed, text kept");

    let (_, work) = outbox
        .retry(&room(), a, later(), Some(([4u8; 32], 7)))
        .expect("Retry works on it");
    sign_all(&mut outbox, a, &work);
    let (ready, _) = outbox.take_ready_sealed_under(&room(), Some(Some(7)));
    assert_eq!(
        version_of(&ready[0]),
        7,
        "Retry sealed it under the current secret"
    );
}

/// A message sealed under v1, nothing to seal it from (`enqueue` keeps no
/// `SealInput`), so it cannot be sealed again.
fn sealed_without_input(
    outbox: &mut Outbox,
    text: &str,
    ms: u64,
    version: u32,
) -> (u64, SigningWork) {
    use super::super::seal::SealInput;
    let (mut message, _) = message(text, ms);
    message.content = SealInput::Text(text.to_string()).seal(&[1u8; 32], version);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&message, &mut bytes).unwrap();
    outbox.enqueue(
        room(),
        PendingKind::Send {
            text: text.to_string(),
            author_name: "Me".to_string(),
        },
        author().verifying_key(),
        vec![(message, bytes)],
    )
}

/// A stale entry that cannot be sealed again fails, and the entries behind it
/// still commit: one failure never strands the queue.
#[test]
fn a_reseal_that_fails_does_not_strand_what_follows() {
    let current = Some(([2u8; 32], 2));
    // At the bound.
    let mut outbox = Outbox::default();
    let (a, wa) = sealed_entry(&mut outbox, "keeps rotating", 1, 1);
    let (b, wb) = sealed_entry(&mut outbox, "behind it", 2, 2);
    // Sealed again at v1 each time, so at current v2 it is stale and sealed
    // at or below the current version: only the bound fails it.
    for _ in 0..MAX_RESEALS {
        outbox.reseal(&room(), a, &[9u8; 32], 1).unwrap();
    }
    let _ = wa;
    let work = outbox.entry(&room(), a).unwrap().work();
    sign_all(&mut outbox, a, &work);
    sign_all(&mut outbox, b, &wb);
    let (ready, resign) = outbox.take_committable(&room(), current);
    assert!(resign.is_empty());
    assert_eq!(texts(&ready), ["behind it"]);
    assert!(outbox.pending_rows(&room(), &author().verifying_key())[0]
        .failed
        .is_some());

    // No current secret on this device, and nothing to seal from: each fails,
    // and the public entry behind them commits.
    for (current, make) in [
        (
            None,
            sealed_entry as fn(&mut Outbox, &str, u64, u32) -> (u64, SigningWork),
        ),
        (current, sealed_without_input),
    ] {
        let mut outbox = Outbox::default();
        let (a, wa) = make(&mut outbox, "stale", 1, 1);
        let (b, wb) = send(&mut outbox, "public behind it", 2);
        sign_all(&mut outbox, a, &wa);
        sign_all(&mut outbox, b, &wb);
        let (ready, resign) = outbox.take_committable(&room(), current);
        assert!(resign.is_empty());
        assert_eq!(texts(&ready), ["public behind it"]);
    }
}

/// A body sealed under a newer version than this device holds is never sealed
/// down to the older secret.
#[test]
fn a_reseal_never_moves_to_an_older_secret() {
    let mut outbox = Outbox::default();
    let (a, wa) = sealed_entry(&mut outbox, "sealed at v3", 1, 3);
    sign_all(&mut outbox, a, &wa);
    let (ready, resign) = outbox.take_committable(&room(), Some(([2u8; 32], 2)));
    assert!(ready.is_empty() && resign.is_empty());
    assert!(outbox.pending_rows(&room(), &author().verifying_key())[0]
        .failed
        .is_some());
}

/// The everyday path: public messages, and messages sealed under the current
/// secret (v0 included), commit with no resealing.
#[test]
fn current_and_public_messages_commit_without_resealing() {
    let mut outbox = Outbox::default();
    let (a, wa) = send(&mut outbox, "public", 1);
    let (b, wb) = sealed_entry(&mut outbox, "sealed at v0", 2, 0);
    sign_all(&mut outbox, a, &wa);
    sign_all(&mut outbox, b, &wb);
    let (ready, resign) = outbox.take_committable(&room(), Some(([0u8; 32], 0)));
    assert!(resign.is_empty());
    assert_eq!(texts(&ready), ["public", "sealed at v0"]);

    let mut outbox = Outbox::default();
    let (a, wa) = send(&mut outbox, "public room", 1);
    sign_all(&mut outbox, a, &wa);
    let (ready, resign) = outbox.take_committable(&room(), None);
    assert!(resign.is_empty());
    assert_eq!(
        texts(&ready),
        ["public room"],
        "a public room has no secret"
    );
}

/// Both halves of a reaction swap are sealed again, and both must be signed
/// again before it commits.
#[test]
fn every_message_of_an_entry_is_sealed_again() {
    use super::super::seal::SealInput;
    use river_core::room_state::content::ActionContentV1;
    let mut outbox = Outbox::default();
    let target = river_core::room_state::message::MessageId(freenet_scaffold::util::FastHash(1));
    let half = |action: ActionContentV1, ms: u64| {
        let seal = SealInput::Action(action);
        let (mut message, _) = message("half", ms);
        message.content = seal.seal(&[1u8; 32], 1);
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&message, &mut bytes).unwrap();
        (message, bytes, Some(seal))
    };
    let (id, work) = outbox.enqueue_sealed(
        room(),
        PendingKind::Action {
            label: "reaction",
            effects: vec![],
        },
        author().verifying_key(),
        vec![
            half(
                ActionContentV1::remove_reaction(target.clone(), "a".into()),
                1,
            ),
            half(ActionContentV1::reaction(target, "b".into()), 1),
        ],
    );
    sign_all(&mut outbox, id, &work);
    let (_, resign) = outbox.take_committable(&room(), Some(([2u8; 32], 2)));
    let (_, _, work) = resign.into_iter().next().unwrap();
    assert_eq!(work.len(), 2, "both halves are signed again");
    outbox.record_signature(&room(), id, 0, author().sign(&work[0].1));
    assert!(!outbox.has_ready(&room()), "one half signed is not enough");
    outbox.record_signature(&room(), id, 1, author().sign(&work[1].1));
    let (ready, _) = outbox.take_committable(&room(), Some(([2u8; 32], 2)));
    for m in &ready[0].messages {
        assert!(matches!(
            m.message.content,
            RoomMessageBody::Private {
                secret_version: 2,
                ..
            }
        ));
    }
}

/// Retry resets the reseal count, so a retried message gets the full bound.
#[test]
fn retry_resets_the_reseal_count() {
    let mut outbox = Outbox::default();
    let (a, _) = sealed_entry(&mut outbox, "retried", 1, 1);
    for v in 0..MAX_RESEALS {
        outbox.reseal(&room(), a, &[9u8; 32], 2 + v).unwrap();
    }
    assert!(outbox.reseal(&room(), a, &[9u8; 32], 9).is_none());
    outbox
        .retry(&room(), a, later(), Some(([9u8; 32], 9)))
        .unwrap();
    for v in 0..MAX_RESEALS {
        assert!(outbox.reseal(&room(), a, &[9u8; 32], 10 + v).is_some());
    }
}

/// Retry seals forward only: it never seals a message down to an older
/// secret this device holds, and leaves one already sealed under the current
/// secret as it is.
#[test]
fn retry_never_seals_down_and_leaves_a_current_seal_alone() {
    let ciphertext_of =
        |outbox: &Outbox, id: u64| match &outbox.entry(&room(), id).unwrap().messages[0]
            .message
            .content
        {
            RoomMessageBody::Private { ciphertext, .. } => ciphertext.clone(),
            RoomMessageBody::Public { .. } => panic!("sealed"),
        };
    let mut outbox = Outbox::default();
    let (newer, _) = sealed_entry(&mut outbox, "sealed at v3", 1, 3);
    outbox.record_failure(&room(), newer, "x".to_string());
    let before = ciphertext_of(&outbox, newer);
    assert!(
        outbox
            .retry(&room(), newer, later(), Some(([2u8; 32], 2)))
            .is_none(),
        "no Retry down to v2"
    );
    assert_eq!(ciphertext_of(&outbox, newer), before);
    assert!(outbox.pending_rows(&room(), &author().verifying_key())[0]
        .failed
        .is_some());

    let (current, _) = sealed_entry(&mut outbox, "sealed at v2", 2, 2);
    outbox.record_failure(&room(), current, "x".to_string());
    let before = ciphertext_of(&outbox, current);
    outbox
        .retry(&room(), current, later(), Some(([2u8; 32], 2)))
        .unwrap();
    assert_eq!(ciphertext_of(&outbox, current), before, "not sealed again");

    let (bare, _) = sealed_without_input(&mut outbox, "nothing to seal from", 3, 1);
    outbox.record_failure(&room(), bare, "x".to_string());
    assert!(
        outbox
            .retry(&room(), bare, later(), Some(([2u8; 32], 2)))
            .is_none(),
        "cannot be sealed again"
    );
}
