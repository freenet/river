//! Deleting someone's messages: what is refused, the envelope that goes out (the same bytes
//! river-core's `advance_recipient_purges` gives, which the web UI and `riverctl` sign with), what
//! the room keeps, and how the outbox queues, retries and repairs it. Rooms are built the way
//! River builds them (`room_tests::Fixture`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{
    AuthorizedDirectMessage, AuthorizedRecipientPurges, compose_direct_message,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::privacy::RoomDisplayMetadata;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};

use super::*;
use crate::rooms::outbox::{Action, Blocked, Outbox};
use crate::rooms::room_tests::Fixture;

const BASE: u64 = 1_700_000_000;

fn them() -> SigningKey {
    SigningKey::from_bytes(&[3; 32])
}

fn them_id() -> MemberId {
    MemberId::from(&them().verifying_key())
}

fn third() -> SigningKey {
    SigningKey::from_bytes(&[4; 32])
}

fn third_id() -> MemberId {
    MemberId::from(&third().verifying_key())
}

fn outbox() -> Outbox {
    let ticks = Arc::new(AtomicU64::new(0));
    Outbox::with_clock(Box::new(move || {
        SystemTime::UNIX_EPOCH + Duration::from_secs(BASE + ticks.fetch_add(1, Ordering::Relaxed))
    }))
}

fn member(f: &Fixture, who: &SigningKey) -> AuthorizedMember {
    AuthorizedMember::new(
        Member {
            owner_member_id: f.owner_id(),
            invited_by: f.owner_id(),
            member_vk: who.verifying_key(),
        },
        &f.owner,
    )
}

/// A public room with the user, `them()` and `third()` as members, each with a room message
/// (a member with none is pruned by the room's own cleanup).
fn state(f: &Fixture) -> ChatRoomStateV1 {
    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: f.owner_id(),
                display: RoomDisplayMetadata::public("Town".into(), None),
                ..Configuration::default()
            },
            &f.owner,
        ),
        ..Default::default()
    };
    for who in [&f.me, &them(), &third()] {
        state.members.members.push(member(f, who));
    }
    state.recent_messages.messages = [(&f.me, 50), (&them(), 40), (&third(), 30)]
        .into_iter()
        .map(|(who, ago)| {
            f.message(
                who,
                BASE - ago,
                river_core::room_state::message::RoomMessageBody::public("hi".into()),
            )
        })
        .collect();
    state
}

fn room(f: &Fixture) -> LiveRoom {
    LiveRoom::new(f.stored(state(f)))
}

/// `from` sends `to` a message at `ts`, taken by `room`.
fn deliver(
    f: &Fixture,
    room: &mut LiveRoom,
    from: &SigningKey,
    to: &SigningKey,
    ts: u64,
) -> AuthorizedDirectMessage {
    let m = compose_direct_message(
        from,
        &to.verifying_key(),
        &f.owner.verifying_key(),
        ts,
        ts,
        format!("secret words {ts}").as_bytes(),
    )
    .unwrap();
    room.apply_delta(ChatRoomStateV1Delta {
        direct_messages: Some(
            river_core::room_state::direct_messages::DirectMessagesDelta {
                new_messages: vec![m.clone()],
                advanced_purges: Vec::new(),
            },
        ),
        ..Default::default()
    })
    .unwrap();
    m
}

fn held(room: &LiveRoom, id: MemberId) -> Option<AuthorizedRecipientPurges> {
    held_envelope(room, id).cloned()
}

fn token(n: u32) -> PurgeToken {
    let mut t = [0u8; 16];
    t[..4].copy_from_slice(&n.to_le_bytes());
    PurgeToken(t)
}

fn next(outbox: &mut Outbox, room: &LiveRoom) -> Option<ChatRoomStateV1Delta> {
    let mut changed = false;
    outbox.next_update(room, Instant::now(), &mut changed)
}

fn envelope_of(delta: &ChatRoomStateV1Delta) -> AuthorizedRecipientPurges {
    let dms = delta.direct_messages.as_ref().expect("a deletion");
    assert!(dms.new_messages.is_empty());
    assert_eq!(dms.advanced_purges.len(), 1);
    dms.advanced_purges[0].clone()
}

fn bytes(delta: &ChatRoomStateV1Delta) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(delta, &mut out).unwrap();
    out
}

// The envelope ---------------------------------------------------------------------------------

#[test]
fn the_envelope_is_what_river_cores_helper_signs_and_it_verifies() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let b = deliver(&f, &mut r, &them(), &f.me, BASE - 10);
    let tokens = vec![a.purge_token(), b.purge_token()];

    let work = prepare(&r, them_id(), &tokens).unwrap();
    let mut sorted = tokens.clone();
    sorted.sort();
    // Signing is deterministic: the same inputs give the web UI's and riverctl's bytes.
    let reference =
        advance_recipient_purges(&f.me, &f.owner.verifying_key(), None, tokens).unwrap();
    assert_eq!(work.envelope, reference);
    assert_eq!(work.envelope.state.version, 1);
    assert_eq!(work.envelope.state.purged, sorted);
    assert_eq!(work.tokens, sorted);
    assert_eq!(work.peer, them_id());
    work.envelope
        .verify_signature(&f.me.verifying_key(), &f.owner.verifying_key())
        .unwrap();
}

#[test]
fn only_what_they_sent_you_is_ever_named() {
    let f = Fixture::new();
    let mut r = room(&f);
    let inbound = deliver(&f, &mut r, &them(), &f.me, BASE - 30);
    let sent = deliver(&f, &mut r, &f.me, &them(), BASE - 20);
    // Between two other people, and from another person to the user.
    let between = deliver(&f, &mut r, &them(), &third(), BASE - 15);
    let from_third = deliver(&f, &mut r, &third(), &f.me, BASE - 10);

    assert_eq!(
        inbound_tokens(&r, f.me_id(), them_id()),
        vec![inbound.purge_token()]
    );
    // Asked for every token in the room, only the one they sent the user is deleted.
    let everything = [&inbound, &sent, &between, &from_third].map(|m| m.purge_token());
    let work = prepare(&r, them_id(), &everything).unwrap();
    assert_eq!(work.tokens, vec![inbound.purge_token()]);
    // Their messages to the user are not named by a deletion of someone else's, nor the
    // reverse.
    let work = prepare(&r, third_id(), &everything).unwrap();
    assert_eq!(work.tokens, vec![from_third.purge_token()]);
    // What the user sent cannot be deleted: nothing is left to do.
    assert_eq!(
        prepare(&r, them_id(), &[sent.purge_token()]).err(),
        Some(Refusal::Nothing)
    );
    assert_eq!(
        prepare(&r, them_id(), &[between.purge_token()]).err(),
        Some(Refusal::Nothing)
    );
    // And the user is not their own peer.
    assert_eq!(
        prepare(&r, f.me_id(), &everything).err(),
        Some(Refusal::Nothing)
    );
}

#[test]
fn a_later_deletion_carries_the_earlier_tombstones_forward() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 30);
    let first = prepare(&r, them_id(), &[a.purge_token()]).unwrap();
    r.apply_delta(delta(&first.envelope)).unwrap();
    assert!(inbound_tokens(&r, f.me_id(), them_id()).is_empty());
    assert_eq!(held(&r, f.me_id()).unwrap().state.version, 1);

    // The room holds another from a different person, deleted too.
    let c = deliver(&f, &mut r, &third(), &f.me, BASE - 25);
    let b = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let second = prepare(&r, them_id(), &[b.purge_token()]).unwrap();
    assert_eq!(second.envelope.state.version, 2);
    let mut both = vec![a.purge_token(), b.purge_token()];
    both.sort();
    assert_eq!(second.envelope.state.purged, both);
    // It adds only its own token to the work, and the whole list to the envelope.
    assert_eq!(second.tokens, vec![b.purge_token()]);
    r.apply_delta(delta(&second.envelope)).unwrap();
    // The first message does not come back when it is offered again.
    r.apply_delta(ChatRoomStateV1Delta {
        direct_messages: Some(
            river_core::room_state::direct_messages::DirectMessagesDelta {
                new_messages: vec![a.clone()],
                advanced_purges: Vec::new(),
            },
        ),
        ..Default::default()
    })
    .unwrap();
    assert!(inbound_tokens(&r, f.me_id(), them_id()).is_empty());
    // The other person's message is untouched.
    assert_eq!(
        inbound_tokens(&r, f.me_id(), third_id()),
        vec![c.purge_token()]
    );
}

#[test]
fn an_envelope_that_omits_a_held_tombstone_is_never_current() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 30);
    let b = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let held_v1 = prepare(&r, them_id(), &[a.purge_token()]).unwrap().envelope;
    r.apply_delta(delta(&held_v1)).unwrap();

    // A wrongly built v2 that does not carry the first token forward lifts it: the contract
    // would keep it by rank.
    let previous_without_a =
        advance_recipient_purges(&f.me, &f.owner.verifying_key(), None, [token(99)]).unwrap();
    let rogue = advance_recipient_purges(
        &f.me,
        &f.owner.verifying_key(),
        Some(&previous_without_a),
        [b.purge_token()],
    )
    .unwrap();
    assert_eq!(rogue.state.version, 2);
    assert!(!rogue.state.purged.contains(&a.purge_token()));
    let mut probe = r.clone();
    probe.apply_delta(delta(&rogue)).unwrap();
    assert!(
        !held(&probe, f.me_id())
            .unwrap()
            .state
            .purged
            .contains(&a.purge_token()),
        "the room keeps the higher-ranked envelope whatever it omits"
    );
    // So this app never sends one: it is not current, and applying it is refused.
    assert!(!is_current(&r, &rogue));
    assert_eq!(
        applied(&r, &rogue, &[b.purge_token()]).err(),
        Some(Refusal::Stale)
    );
    // One at a version the room already has, or below, is refused as older too.
    let old =
        advance_recipient_purges(&f.me, &f.owner.verifying_key(), None, [b.purge_token()]).unwrap();
    assert_eq!(old.state.version, 1);
    assert!(!is_current(&r, &old));
    // The honest one is current.
    let honest = build(&r, &[b.purge_token()]).unwrap();
    assert!(is_current(&r, &honest));
    assert!(honest.state.purged.contains(&a.purge_token()));
}

#[test]
fn the_cap_refuses_the_whole_deletion_and_drops_no_older_tombstone() {
    let f = Fixture::new();
    let mut r = room(&f);
    // 999 tombstones are held already.
    let held_tokens: Vec<PurgeToken> = (0..999).map(token).collect();
    let v1 = advance_recipient_purges(&f.me, &f.owner.verifying_key(), None, held_tokens.clone())
        .unwrap();
    r.apply_delta(delta(&v1)).unwrap();
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 30);
    let b = deliver(&f, &mut r, &them(), &f.me, BASE - 20);

    // Two more do not fit; nothing is built or queued, and the words say why.
    let refusal = prepare(&r, them_id(), &[a.purge_token(), b.purge_token()])
        .err()
        .unwrap();
    assert_eq!(
        refusal,
        Refusal::Cap {
            held: 999,
            adding: 2
        }
    );
    let said = refusal.text();
    assert!(said.contains("1000") && said.contains("999") && said.contains("Nothing was deleted"));
    let mut outbox = outbox();
    assert_eq!(
        outbox
            .purge_dm(&r, them_id(), &[a.purge_token(), b.purge_token()])
            .unwrap_err(),
        said
    );
    assert!(outbox.purge_rows(&r.stored.owner_vk).is_empty());
    assert!(next(&mut outbox, &r).is_none());
    // One fits, exactly at the cap, and the 999 are all still there.
    let work = prepare(&r, them_id(), &[a.purge_token()]).unwrap();
    assert_eq!(work.envelope.state.purged.len(), 1000);
    for t in &held_tokens {
        assert!(work.envelope.state.purged.contains(t));
    }
    r.apply_delta(delta(&work.envelope)).unwrap();
    // At the cap, nothing more fits, not even one.
    assert_eq!(
        prepare(&r, them_id(), &[b.purge_token()]).err(),
        Some(Refusal::Cap {
            held: 1000,
            adding: 1
        })
    );
    // A token the envelope lists already does not count twice.
    assert!(build(&r, &[a.purge_token()]).is_ok());
}

#[test]
fn what_is_refused_before_anything_is_queued() {
    let f = Fixture::new();
    let mut ok = room(&f);
    let m = deliver(&f, &mut ok, &them(), &f.me, BASE - 20);
    let tokens = [m.purge_token()];

    // No key on this computer.
    let mut no_key = ok.clone();
    no_key.stored.self_sk = None;
    assert_eq!(
        prepare(&no_key, them_id(), &tokens).err(),
        Some(Refusal::Blocked(Blocked::NoKey))
    );

    // Banned.
    let mut banned = ok.clone();
    banned.stored.room_state.bans.0.push(AuthorizedUserBan::new(
        UserBan {
            owner_member_id: f.owner_id(),
            banned_at: (SystemTime::UNIX_EPOCH + Duration::from_secs(BASE)).into(),
            banned_user: f.me_id(),
        },
        f.owner_id(),
        &f.owner,
    ));
    assert_eq!(
        prepare(&banned, them_id(), &tokens).err(),
        Some(Refusal::Blocked(Blocked::Banned))
    );

    // Not a member.
    let mut left = ok.clone();
    left.stored
        .room_state
        .members
        .members
        .retain(|m| m.member.member_vk != f.me.verifying_key());
    assert_eq!(
        prepare(&left, them_id(), &tokens).err(),
        Some(Refusal::Blocked(Blocked::NotMember))
    );

    // Pruned, with a stored admission that would let a message be sent: a deletion is not
    // signed for someone the room does not list (the contract drops it without a word).
    let mut pruned = left.clone();
    pruned.stored.self_authorized_member = Some(member(&f, &f.me));
    assert_eq!(
        prepare(&pruned, them_id(), &tokens).err(),
        Some(Refusal::Blocked(Blocked::NotMember))
    );

    // Nothing to delete: a token the room does not hold, and no tokens.
    assert_eq!(
        prepare(&ok, them_id(), &[token(5)]).err(),
        Some(Refusal::Nothing)
    );
    assert_eq!(prepare(&ok, them_id(), &[]).err(), Some(Refusal::Nothing));

    // Through the outbox the words are the refusal's, no message is in them, and nothing waits.
    let mut outbox = outbox();
    for (r, says) in [
        (&no_key, Blocked::NoKey.reason().to_owned()),
        (&banned, Blocked::Banned.reason().to_owned()),
        (&left, Blocked::NotMember.reason().to_owned()),
    ] {
        assert_eq!(outbox.purge_dm(r, them_id(), &tokens).unwrap_err(), says);
    }
    assert!(!Refusal::Nothing.text().contains("secret"));
    assert!(next(&mut outbox, &ok).is_none());
}

#[test]
fn the_room_drops_the_messages_when_it_applies_the_envelope_and_when_it_cleans_up() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let kept = deliver(&f, &mut r, &f.me, &them(), BASE - 10);
    let work = prepare(&r, them_id(), &[a.purge_token()]).unwrap();

    // The contract's own path, from a state that still holds the message: apply, then clean up.
    let mut contract = r.stored.room_state.clone();
    let params = ChatRoomParametersV1 {
        owner: f.owner.verifying_key(),
    };
    contract
        .apply_delta(
            &ChatRoomStateV1::default(),
            &params,
            &Some(delta(&work.envelope)),
        )
        .unwrap();
    contract.post_apply_cleanup(&params).unwrap();
    let dms = &contract.direct_messages;
    assert!(
        dms.messages
            .iter()
            .all(|m| m.purge_token() != a.purge_token())
    );
    // What the user sent them is not deleted.
    assert!(
        dms.messages
            .iter()
            .any(|m| m.purge_token() == kept.purge_token())
    );
    assert_eq!(dms.purges.len(), 1);
    assert_eq!(dms.purges[0].state.purged, vec![a.purge_token()]);
    // The app's own copy agrees, and lands.
    let next = applied(&r, &work.envelope, &work.tokens).unwrap();
    assert!(landed(&next, &work.envelope));
    assert!(covers(&next, f.me_id(), &work.tokens));
}

#[test]
fn an_unsent_deletion_is_built_again_from_the_room_as_it_is_and_a_current_one_is_kept() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 30);
    let b = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let work = prepare(&r, them_id(), &[a.purge_token()]).unwrap();
    // Still current: kept byte for byte.
    assert_eq!(refresh(&r, &work).unwrap(), None);

    // Another device deleted `b` at version 1 in the meantime (and `a` not).
    let other =
        advance_recipient_purges(&f.me, &f.owner.verifying_key(), None, [b.purge_token()]).unwrap();
    r.apply_delta(delta(&other)).unwrap();
    assert_eq!(
        held(&r, f.me_id()).unwrap().state.purged,
        vec![b.purge_token()]
    );
    // This one would not outrank (same version) or would lift `b`: it is built again.
    assert_eq!(
        applied(&r, &work.envelope, &work.tokens).err(),
        Some(Refusal::Stale)
    );
    let fresh = refresh(&r, &work).unwrap().expect("built again");
    assert_eq!(fresh.envelope.state.version, 2);
    let mut both = vec![a.purge_token(), b.purge_token()];
    both.sort();
    assert_eq!(fresh.envelope.state.purged, both);
    assert_eq!(fresh.tokens, vec![a.purge_token()]);
    assert!(is_current(&r, &fresh.envelope));

    // If the message went meanwhile, nothing is left to delete.
    let mut gone = r.clone();
    gone.stored.room_state.direct_messages.messages.clear();
    assert_eq!(refresh(&gone, &work).err(), Some(Refusal::Nothing));
}

// The queue -------------------------------------------------------------------------------------

#[test]
fn a_deletion_is_queued_sent_and_settled_like_any_other_update() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let owner = r.stored.owner_vk;
    let mut outbox = outbox();
    outbox.purge_dm(&r, them_id(), &[a.purge_token()]).unwrap();

    // It shows in the thread as under way, and not in the conversation.
    let rows = outbox.purge_rows(&owner);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].peer, rows[0].count), (them_id(), 1));
    assert_eq!(rows[0].pending.failed, None);
    let view = outbox.view(&r, &|_| 0);
    assert!(view.messages.iter().all(|m| m.pending.is_none()));
    assert!(view.own_nickname.is_none());

    let sent = next(&mut outbox, &r).expect("it goes out");
    let envelope = envelope_of(&sent);
    assert_eq!(envelope.state.purged, vec![a.purge_token()]);
    // One update per room: nothing more goes out while its answer is owed.
    assert!(next(&mut outbox, &r).is_none());
    // The node took it: the room holds it, the messages are gone, the entry is done.
    assert!(outbox.answered(&mut r, Ok(())));
    assert!(landed(&r, &envelope));
    assert!(inbound_tokens(&r, f.me_id(), them_id()).is_empty());
    assert!(outbox.purge_rows(&owner).is_empty());
    assert!(outbox.is_empty());
}

#[test]
fn retry_after_a_refusal_sends_the_identical_envelope() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let owner = r.stored.owner_vk;
    let mut outbox = outbox();
    outbox.purge_dm(&r, them_id(), &[a.purge_token()]).unwrap();
    let first = next(&mut outbox, &r).unwrap();
    assert!(outbox.answered(&mut r, Err("busy".into())));

    let rows = outbox.purge_rows(&owner);
    assert!(
        rows[0]
            .pending
            .failed
            .as_deref()
            .is_some_and(|w| w.contains("busy"))
    );
    assert!(rows[0].pending.retry);
    let entry = rows[0].pending.entry;
    assert!(outbox.command(&r, Action::Retry { entry }));
    let again = next(&mut outbox, &r).expect("sent again");
    // The same signed envelope, byte for byte: a deletion is not signed twice.
    assert_eq!(bytes(&again), bytes(&first));
    assert_eq!(envelope_of(&again), envelope_of(&first));
}

#[test]
fn an_unanswered_deletion_is_sent_again_unchanged() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let owner = r.stored.owner_vk;
    let mut outbox = outbox();
    outbox.purge_dm(&r, them_id(), &[a.purge_token()]).unwrap();
    let first = next(&mut outbox, &r).unwrap();
    let t0 = Instant::now();
    assert!(outbox.expire(Duration::from_secs(1), t0 + Duration::from_secs(2)));
    let rows = outbox.purge_rows(&owner);
    assert!(rows[0].pending.retry);
    let entry = rows[0].pending.entry;
    // Retry of an unanswered update goes out on a new connection: it asks for one, and the
    // very same envelope goes out there.
    assert!(outbox.command(&r, Action::Retry { entry }));
    assert!(outbox.wants_reconnect(Duration::from_secs(3600), Instant::now()));
    outbox.requeue();
    let again = next(&mut outbox, &r).expect("sent again");
    assert_eq!(bytes(&again), bytes(&first));
}

#[test]
fn a_deletion_waiting_is_built_again_if_the_rooms_envelope_moved_before_it_went_out() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 30);
    let b = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let mut outbox = outbox();
    outbox.purge_dm(&r, them_id(), &[a.purge_token()]).unwrap();
    // Another device deletes `b` before this goes out.
    let other =
        advance_recipient_purges(&f.me, &f.owner.verifying_key(), None, [b.purge_token()]).unwrap();
    r.apply_delta(delta(&other)).unwrap();
    let sent = next(&mut outbox, &r).expect("goes out, built from the room as it is");
    let envelope = envelope_of(&sent);
    assert_eq!(envelope.state.version, 2);
    assert!(envelope.state.purged.contains(&b.purge_token()));
    assert!(envelope.state.purged.contains(&a.purge_token()));
    // And it does not lift what the room held.
    assert!(is_current(&r, &envelope));
}

#[test]
fn a_deletion_whose_messages_went_meanwhile_fails_with_discard_only() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let owner = r.stored.owner_vk;
    let mut outbox = outbox();
    outbox.purge_dm(&r, them_id(), &[a.purge_token()]).unwrap();
    r.stored.room_state.direct_messages.messages.clear();
    assert!(next(&mut outbox, &r).is_none());
    let rows = outbox.purge_rows(&owner);
    assert_eq!(
        rows[0].pending.failed.as_deref(),
        Some(Refusal::Nothing.text().as_str())
    );
    assert!(!rows[0].pending.retry, "there is nothing to send again");
    let entry = rows[0].pending.entry;
    assert!(outbox.command(&r, Action::Discard { entry }));
    assert!(outbox.purge_rows(&owner).is_empty());
}

#[test]
fn what_another_device_made_settles_it_without_sending() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let mut outbox = outbox();
    outbox.purge_dm(&r, them_id(), &[a.purge_token()]).unwrap();
    // The room takes the same deletion from the network first.
    let other =
        advance_recipient_purges(&f.me, &f.owner.verifying_key(), None, [a.purge_token()]).unwrap();
    r.apply_delta(delta(&other)).unwrap();
    assert!(outbox.reconcile(&mut r));
    assert!(outbox.is_empty());
    assert!(next(&mut outbox, &r).is_none());
}

#[test]
fn the_debug_of_everything_here_shows_counts_and_no_token_or_text() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let work = prepare(&r, them_id(), &[a.purge_token()]).unwrap();
    let hex = crate::rooms::dm::token_hex(&a.purge_token());
    let mut shown = format!("{work:?}");
    let action = Action::PurgeDm {
        peer: them_id(),
        tokens: vec![a.purge_token()],
        ticket: 3,
    };
    shown.push_str(&format!("{action:?}"));
    let mut outbox = outbox();
    outbox.purge_dm(&r, them_id(), &[a.purge_token()]).unwrap();
    shown.push_str(&format!("{:?}", outbox.purge_rows(&r.stored.owner_vk)));
    assert!(!shown.contains(&hex), "{shown}");
    assert!(!shown.contains("secret words"));
    assert!(shown.contains("tokens: 1"));
    // Nor does the wording of a refusal.
    for refusal in [
        Refusal::Nothing,
        Refusal::NotKept,
        Refusal::Stale,
        Refusal::Cap { held: 1, adding: 2 },
    ] {
        assert!(!refusal.text().contains(&hex));
    }
}

// The limit, the golden vector and the negatives -----------------------------------------------

#[test]
fn a_fixed_signer_owner_and_tokens_give_a_fixed_envelope() {
    // A regression pin, not an independent oracle: the expected signatures were taken from this
    // code's own output for a signer, an owner and tokens fixed here (Ed25519 is deterministic).
    // A change to what is signed shows here before it reaches a room. What is independent is the
    // signed bytes laid out by hand below, which must verify under the signer's key.
    let signer = SigningKey::from_bytes(&[7; 32]);
    let owner = SigningKey::from_bytes(&[1; 32]).verifying_key();
    let tokens = [PurgeToken([2; 16]), PurgeToken([1; 16])];
    let first = advance_recipient_purges(&signer, &owner, None, tokens).unwrap();
    assert_eq!(first.state.version, 1);
    assert_eq!(
        first.state.purged,
        vec![PurgeToken([1; 16]), PurgeToken([2; 16])]
    );
    assert_eq!(first.recipient_id, MemberId::from(&signer.verifying_key()));
    let second =
        advance_recipient_purges(&signer, &owner, Some(&first), [PurgeToken([3; 16])]).unwrap();
    assert_eq!(second.state.version, 2);
    assert_eq!(
        second.state.purged,
        vec![
            PurgeToken([1; 16]),
            PurgeToken([2; 16]),
            PurgeToken([3; 16])
        ]
    );
    let hex = |e: &AuthorizedRecipientPurges| {
        e.recipient_signature
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    assert_eq!(hex(&first), GOLDEN_V1, "version 1");
    assert_eq!(hex(&second), GOLDEN_V2, "version 2");
    // The signed bytes, laid out by hand from the documented format (tag 'P', recipient id,
    // owner key, version, count, tokens), verify under the signer's key: the helper signs what
    // the contract checks and nothing else.
    {
        use ed25519_dalek::Verifier;
        let mut bytes = vec![b'P'];
        bytes.extend_from_slice(&MemberId::from(&signer.verifying_key()).0.0.to_le_bytes());
        bytes.extend_from_slice(owner.as_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[1; 16]);
        bytes.extend_from_slice(&[2; 16]);
        signer
            .verifying_key()
            .verify(&bytes, &first.recipient_signature)
            .unwrap();
    }
    {
        use ed25519_dalek::Verifier;
        let mut bytes = vec![b'P'];
        bytes.extend_from_slice(&MemberId::from(&signer.verifying_key()).0.0.to_le_bytes());
        bytes.extend_from_slice(owner.as_bytes());
        bytes.extend_from_slice(&2u64.to_le_bytes());
        bytes.extend_from_slice(&3u32.to_le_bytes());
        for t in [1u8, 2, 3] {
            bytes.extend_from_slice(&[t; 16]);
        }
        signer
            .verifying_key()
            .verify(&bytes, &second.recipient_signature)
            .unwrap();
    }
    // Verified by the contract's own path, with the key the room resolves for the recipient.
    first
        .verify_signature(&signer.verifying_key(), &owner)
        .unwrap();
    second
        .verify_signature(&signer.verifying_key(), &owner)
        .unwrap();
    // And by nobody else's key, or for another owner.
    assert!(first.verify_signature(&owner, &owner).is_err());
    let other_owner = SigningKey::from_bytes(&[9; 32]).verifying_key();
    assert!(
        first
            .verify_signature(&signer.verifying_key(), &other_owner)
            .is_err()
    );
}

const GOLDEN_V1: &str = "8fcd1c58bc0af9798309419ef17c9e26da703c25ce9012cb3aa43cfdf3f25ff97523274d00d887ee6d4056c55123d3b78197ee81009daa1a7e27292c94592106";
const GOLDEN_V2: &str = "f00fea0df63d31a6a8ac3cd510683872046f21147dbe04dc6fedbfe5043260bbd3c94d60ffe50cf8c37c9558638cdff9caf0b29ca27bd85b7e6ec8eab0bf6708";

#[test]
fn the_contract_ignores_an_envelope_naming_what_the_user_sent_or_what_is_between_others() {
    let f = Fixture::new();
    let mut r = room(&f);
    let sent = deliver(&f, &mut r, &f.me, &them(), BASE - 30);
    let between = deliver(&f, &mut r, &them(), &third(), BASE - 20);
    let theirs = deliver(&f, &mut r, &them(), &f.me, BASE - 10);
    // The user signs an envelope that names all three tokens, which is not what this app builds.
    let rogue = advance_recipient_purges(
        &f.me,
        &f.owner.verifying_key(),
        None,
        [
            sent.purge_token(),
            between.purge_token(),
            theirs.purge_token(),
        ],
    )
    .unwrap();
    let mut contract = r.stored.room_state.clone();
    let params = ChatRoomParametersV1 {
        owner: f.owner.verifying_key(),
    };
    contract
        .apply_delta(&ChatRoomStateV1::default(), &params, &Some(delta(&rogue)))
        .unwrap();
    contract.post_apply_cleanup(&params).unwrap();
    let alive = |t: PurgeToken| {
        contract
            .direct_messages
            .messages
            .iter()
            .any(|m| m.purge_token() == t)
    };
    assert!(
        alive(sent.purge_token()),
        "a message the user sent survives"
    );
    assert!(
        alive(between.purge_token()),
        "a message between others survives"
    );
    assert!(
        !alive(theirs.purge_token()),
        "only the one addressed to the user goes"
    );
}

#[test]
fn the_cap_boundary_counts_a_token_the_envelope_holds_once() {
    let f = Fixture::new();
    let mut r = room(&f);
    let held_tokens: Vec<PurgeToken> = (0..999).map(token).collect();
    let v1 = advance_recipient_purges(&f.me, &f.owner.verifying_key(), None, held_tokens).unwrap();
    r.apply_delta(delta(&v1)).unwrap();
    // One already in the envelope and one new: 999 + 1 fits exactly.
    let ok = build(&r, &[token(5), token(5000)]).unwrap();
    assert_eq!(ok.state.purged.len(), 1000);
    // Two new ones do not, and the numbers say 999 held and 2 added.
    assert_eq!(
        build(&r, &[token(5000), token(5001), token(5)]).err(),
        Some(Refusal::Cap {
            held: 999,
            adding: 2
        })
    );
}

#[test]
fn an_envelope_that_lifts_a_tombstone_lets_the_message_come_back() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 30);
    let b = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let held_v1 = prepare(&r, them_id(), &[a.purge_token()]).unwrap().envelope;
    r.apply_delta(delta(&held_v1)).unwrap();
    assert_eq!(
        inbound_tokens(&r, f.me_id(), them_id()),
        vec![b.purge_token()]
    );
    // The rogue v2 omits `a`: the room keeps it by rank, and when a peer offers `a` again, the
    // message is back. This is why this app never sends such an envelope.
    let seen =
        advance_recipient_purges(&f.me, &f.owner.verifying_key(), None, [token(99)]).unwrap();
    let rogue = advance_recipient_purges(
        &f.me,
        &f.owner.verifying_key(),
        Some(&seen),
        [b.purge_token()],
    )
    .unwrap();
    let mut harmed = r.clone();
    harmed.apply_delta(delta(&rogue)).unwrap();
    harmed
        .apply_delta(ChatRoomStateV1Delta {
            direct_messages: Some(
                river_core::room_state::direct_messages::DirectMessagesDelta {
                    new_messages: vec![a.clone()],
                    advanced_purges: Vec::new(),
                },
            ),
            ..Default::default()
        })
        .unwrap();
    assert!(inbound_tokens(&harmed, f.me_id(), them_id()).contains(&a.purge_token()));
    // With the honest envelope, offering it again changes nothing.
    let mut safe = r.clone();
    let honest = build(&safe, &[b.purge_token()]).unwrap();
    safe.apply_delta(delta(&honest)).unwrap();
    safe.apply_delta(ChatRoomStateV1Delta {
        direct_messages: Some(
            river_core::room_state::direct_messages::DirectMessagesDelta {
                new_messages: vec![a.clone()],
                advanced_purges: Vec::new(),
            },
        ),
        ..Default::default()
    })
    .unwrap();
    assert!(inbound_tokens(&safe, f.me_id(), them_id()).is_empty());
}

#[test]
fn a_taken_deletion_the_room_does_not_hold_is_unconfirmed_and_retry_builds_it_again() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 30);
    let b = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let owner = r.stored.owner_vk;
    let mut outbox = outbox();
    outbox.purge_dm(&r, them_id(), &[a.purge_token()]).unwrap();
    let first = envelope_of(&next(&mut outbox, &r).unwrap());
    // Another device's envelope, which outranks this one and omits its token, is in the room
    // when the node's answer comes: the node took it, the room does not hold it.
    let seen = advance_recipient_purges(&f.me, &f.owner.verifying_key(), None, [token(7)]).unwrap();
    let theirs = advance_recipient_purges(
        &f.me,
        &f.owner.verifying_key(),
        Some(&seen),
        [b.purge_token()],
    )
    .unwrap();
    assert!(theirs.rank() > first.rank());
    r.apply_delta(delta(&theirs)).unwrap();
    assert!(outbox.answered(&mut r, Ok(())));
    let rows = outbox.purge_rows(&owner);
    assert_eq!(rows.len(), 1);
    let said = rows[0].pending.failed.clone().expect("said");
    assert!(said.contains(NOT_KEPT) && said.contains("Retry"));
    assert!(
        rows[0].pending.retry,
        "an unconfirmed deletion can be retried"
    );
    // Retry builds it again from the room as it is: the next version, carrying forward the other
    // device's tokens.
    let entry = rows[0].pending.entry;
    assert!(outbox.command(&r, Action::Retry { entry }));
    let again = envelope_of(&next(&mut outbox, &r).expect("sent again"));
    assert_eq!(again.state.version, theirs.state.version + 1);
    for t in &theirs.state.purged {
        assert!(again.state.purged.contains(t), "carried forward");
    }
    assert!(again.state.purged.contains(&a.purge_token()));
    assert!(is_current(&r, &again));
    assert!(outbox.answered(&mut r, Ok(())));
    assert!(outbox.is_empty(), "now the room holds it");
}

#[test]
fn a_refused_deletion_is_built_again_if_the_room_moved_and_kept_if_it_did_not() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 30);
    let b = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let owner = r.stored.owner_vk;
    let mut outbox = outbox();
    outbox.purge_dm(&r, them_id(), &[a.purge_token()]).unwrap();
    let first = next(&mut outbox, &r).unwrap();
    assert!(outbox.answered(&mut r, Err("busy".into())));
    let entry = outbox.purge_rows(&owner)[0].pending.entry;
    // Nothing moved: Retry sends the identical bytes.
    assert!(outbox.command(&r, Action::Retry { entry }));
    assert_eq!(bytes(&next(&mut outbox, &r).unwrap()), bytes(&first));
    assert!(outbox.answered(&mut r, Err("busy again".into())));
    // Another device's envelope arrives: Retry is version N+1 with its tokens carried forward.
    let other =
        advance_recipient_purges(&f.me, &f.owner.verifying_key(), None, [b.purge_token()]).unwrap();
    r.apply_delta(delta(&other)).unwrap();
    assert!(outbox.command(&r, Action::Retry { entry }));
    let again = envelope_of(&next(&mut outbox, &r).unwrap());
    assert_eq!(again.state.version, other.state.version + 1);
    assert!(again.state.purged.contains(&a.purge_token()));
    assert!(again.state.purged.contains(&b.purge_token()));
}

#[test]
fn asking_again_for_a_deletion_that_is_queued_or_out_queues_nothing_new() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 30);
    let b = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let owner = r.stored.owner_vk;
    let mut outbox = outbox();
    outbox
        .purge_dm(&r, them_id(), &[a.purge_token(), b.purge_token()])
        .unwrap();
    // Queued: the same again, and a subset, are that deletion.
    outbox
        .purge_dm(&r, them_id(), &[a.purge_token(), b.purge_token()])
        .unwrap();
    outbox.purge_dm(&r, them_id(), &[a.purge_token()]).unwrap();
    assert_eq!(outbox.purge_rows(&owner).len(), 1);
    // Out, unanswered: the same.
    next(&mut outbox, &r).unwrap();
    outbox
        .purge_dm(&r, them_id(), &[a.purge_token(), b.purge_token()])
        .unwrap();
    assert_eq!(outbox.purge_rows(&owner).len(), 1);
    // A deletion of a message that arrived since is another one.
    let c = deliver(&f, &mut r, &them(), &f.me, BASE - 10);
    outbox.purge_dm(&r, them_id(), &[c.purge_token()]).unwrap();
    assert_eq!(outbox.purge_rows(&owner).len(), 2);
}

#[test]
fn our_own_envelope_reaching_the_room_from_the_network_settles_a_queued_deletion_as_done() {
    let f = Fixture::new();
    let mut r = room(&f);
    let a = deliver(&f, &mut r, &them(), &f.me, BASE - 20);
    let owner = r.stored.owner_vk;
    let mut outbox = outbox();
    outbox.purge_dm(&r, them_id(), &[a.purge_token()]).unwrap();
    let ours = {
        let rows = outbox.purge_rows(&owner);
        assert_eq!(rows.len(), 1);
        build(&r, &[a.purge_token()]).unwrap()
    };
    // The same deletion reaches the room from the network while the entry is still queued.
    r.apply_delta(delta(&ours)).unwrap();
    // The tick: the next update is asked first (and sends nothing), then the room is reconciled.
    assert!(next(&mut outbox, &r).is_none(), "nothing is sent");
    let rows = outbox.purge_rows(&owner);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].pending.failed, None, "not failed: it is done");
    assert!(outbox.reconcile(&mut r));
    assert!(outbox.purge_rows(&owner).is_empty());
    assert!(outbox.is_empty());
}
