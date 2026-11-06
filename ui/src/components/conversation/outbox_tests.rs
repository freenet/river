use super::*;
use ed25519_dalek::{Signer, SigningKey};
use river_core::room_state::member::MemberId;
use river_core::room_state::message::RoomMessageBody;
use std::time::Duration;

fn room() -> VerifyingKey {
    SigningKey::from_bytes(&[1u8; 32]).verifying_key()
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
        outbox.retry(&room(), a).is_none(),
        "only a failed entry can be retried"
    );
    outbox.record_failure(&room(), a, "timed out".to_string());

    let (vk, work) = outbox.retry(&room(), a).expect("a failed entry retries");
    assert_eq!(vk, author().verifying_key());
    assert_eq!(work, wa, "a retry signs the same bytes");
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
}

/// By design: a message that failed and is retried commits after the ones
/// sent after it (keeping its own timestamp, so it still shows in typed order).
#[test]
fn a_retried_message_commits_after_the_ones_that_went_ahead() {
    let mut outbox = Outbox::default();
    let (a, _) = send(&mut outbox, "a", 1);
    let (b, wb) = send(&mut outbox, "b", 2);
    outbox.record_failure(&room(), a, "timed out".to_string());
    sign_all(&mut outbox, b, &wb);
    let (_, wa) = outbox.retry(&room(), a).unwrap();
    sign_all(&mut outbox, a, &wa);
    let first = outbox.take_ready(&room());
    assert_eq!(texts(&first), ["a", "b"], "both ready: committed in order");

    let mut outbox = Outbox::default();
    let (a, _) = send(&mut outbox, "a", 1);
    let (b, wb) = send(&mut outbox, "b", 2);
    outbox.record_failure(&room(), a, "timed out".to_string());
    sign_all(&mut outbox, b, &wb);
    assert_eq!(texts(&outbox.take_ready(&room())), ["b"]);
    let (_, wa) = outbox.retry(&room(), a).unwrap();
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
    let (_, work) = outbox.retry(&room(), a).expect("a refused message retries");
    assert_eq!(work, wa);
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
