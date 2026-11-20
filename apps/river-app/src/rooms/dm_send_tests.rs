//! Sending a direct message: what is refused, the bytes that go out, what the room keeps, and how
//! the outbox queues, retries and settles it. Rooms are built the way River builds them
//! (`room_tests::Fixture`) and messages are checked against river-core's own helpers, which the
//! web UI and `riverctl` send with.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use ed25519_dalek::SigningKey;
use freenet_scaffold::ComposableState;
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{
    AuthorizedDirectMessage, compose_direct_message, open_direct_message,
};
use river_core::room_state::dm_body::{DirectMessageBody, decode_body};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::privacy::RoomDisplayMetadata;
use river_core::room_state::{ChatRoomParametersV1, ChatRoomStateV1, ChatRoomStateV1Delta};

use super::*;
use crate::chat::model::Pending;
use crate::rooms::outbox::{Action, Blocked, Outbox};
use crate::rooms::room::LiveRoom;
use crate::rooms::room_tests::Fixture;

const BASE: u64 = 1_700_000_000;

fn them() -> SigningKey {
    SigningKey::from_bytes(&[3; 32])
}

fn them_id() -> MemberId {
    MemberId::from(&them().verifying_key())
}

fn clock_at(start: u64) -> Outbox {
    let ticks = Arc::new(AtomicU64::new(0));
    Outbox::with_clock(Box::new(move || {
        SystemTime::UNIX_EPOCH + Duration::from_secs(start + ticks.fetch_add(1, Ordering::Relaxed))
    }))
}

fn outbox() -> Outbox {
    clock_at(BASE)
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

/// A public room with the user and `them()` as members, and each with a message in the room
/// (a member with no recent message is pruned by the room's own cleanup).
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
    state.members.members.push(member(f, &f.me));
    state.members.members.push(member(f, &them()));
    state.recent_messages.messages = vec![
        f.message(
            &f.me,
            BASE - 50,
            river_core::room_state::message::RoomMessageBody::public("from me".into()),
        ),
        f.message(
            &them(),
            BASE - 40,
            river_core::room_state::message::RoomMessageBody::public("from them".into()),
        ),
    ];
    state
}

fn room(f: &Fixture) -> LiveRoom {
    LiveRoom::new(f.stored(state(f)))
}

fn secs(now: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(now)
}

/// The update `outbox` sends next for `room`.
fn next(outbox: &mut Outbox, room: &LiveRoom) -> Option<ChatRoomStateV1Delta> {
    let mut changed = false;
    outbox.next_update(room, Instant::now(), &mut changed)
}

fn the_message(delta: &ChatRoomStateV1Delta) -> AuthorizedDirectMessage {
    let dms = delta.direct_messages.as_ref().expect("a direct message");
    assert_eq!(dms.new_messages.len(), 1);
    assert!(dms.advanced_purges.is_empty());
    dms.new_messages[0].clone()
}

// What is refused --------------------------------------------------------------------------------

#[test]
fn every_refusal_is_said_in_fixed_words_and_nothing_is_queued() {
    let f = Fixture::new();
    let secret = "SECRET-WORDS";
    let ok = room(&f);
    let cap = BODY_CAP;

    // No key on this computer.
    let mut no_key = ok.clone();
    no_key.stored.self_sk = None;
    assert_eq!(
        preflight(&no_key, them_id(), secret),
        Err(Refusal::Blocked(Blocked::NoKey))
    );

    // Banned.
    let mut banned = state(&f);
    banned.bans.0.push(AuthorizedUserBan::new(
        UserBan {
            owner_member_id: f.owner_id(),
            banned_at: secs(BASE),
            banned_user: f.me_id(),
        },
        f.owner_id(),
        &f.owner,
    ));
    assert_eq!(
        preflight(&LiveRoom::new(f.stored(banned)), them_id(), secret),
        Err(Refusal::Blocked(Blocked::Banned))
    );

    // Not a member, with nothing to be re-added with.
    let mut left = state(&f);
    left.members
        .members
        .retain(|m| m.member.member_vk != f.me.verifying_key());
    assert_eq!(
        preflight(&LiveRoom::new(f.stored(left)), them_id(), secret),
        Err(Refusal::Blocked(Blocked::NotMember))
    );

    // Nothing to say, and too much to say (counted in bytes, after trimming).
    assert_eq!(preflight(&ok, them_id(), " \n\t "), Err(Refusal::Empty));
    assert!(preflight(&ok, them_id(), &"a".repeat(cap)).is_ok());
    // The text is sent as written, so the cap counts every byte of it, whitespace too.
    assert_eq!(
        preflight(&ok, them_id(), &format!("  {}\n", "a".repeat(cap))),
        Err(Refusal::TooLong { bytes: cap + 3 })
    );
    assert_eq!(
        preflight(&ok, them_id(), &"a".repeat(cap + 1)),
        Err(Refusal::TooLong { bytes: cap + 1 })
    );
    // A multi-byte character counts as its bytes: 3 bytes each.
    let wide = "\u{20ac}".repeat(cap / 3 + 1);
    assert_eq!(
        preflight(&ok, them_id(), &wide),
        Err(Refusal::TooLong { bytes: wide.len() })
    );

    // Yourself, and someone who is not in the room.
    assert_eq!(preflight(&ok, f.me_id(), secret), Err(Refusal::SelfMessage));
    let stranger = MemberId::from(&SigningKey::from_bytes(&[8; 32]).verifying_key());
    assert_eq!(preflight(&ok, stranger, secret), Err(Refusal::NotMember));
    // The owner is a person to write to.
    assert!(preflight(&ok, f.owner_id(), secret).is_ok());

    // Through the outbox: the words are the refusal's, the message is not in them, and nothing
    // is left behind to send.
    let mut outbox = outbox();
    let owner = ok.stored.owner_vk;
    for (to, text, said) in [
        (f.me_id(), secret.to_owned(), Refusal::SelfMessage.text()),
        (stranger, secret.to_owned(), Refusal::NotMember.text()),
        (them_id(), " ".to_owned(), Refusal::Empty.text()),
        (
            them_id(),
            "a".repeat(cap + 1),
            Refusal::TooLong { bytes: cap + 1 }.text(),
        ),
    ] {
        assert_eq!(outbox.send_dm(&ok, to, &text).unwrap_err(), said);
        assert!(!said.contains(secret));
    }
    assert!(outbox.send_dm(&no_key, them_id(), secret).is_err());
    assert!(outbox.dm_rows(&owner).is_empty());
    assert!(next(&mut outbox, &ok).is_none(), "nothing was queued");
    assert!(outbox.is_empty());
}

// The bytes that go out ----------------------------------------------------------------------------

#[test]
fn the_message_carries_the_text_as_written_in_the_web_uis_format() {
    let f = Fixture::new();
    let room = room(&f);
    let text = "  Hello \u{1f44b} wörld\nsecond line  ";
    let mut outbox = outbox();
    let queued = outbox.send_dm(&room, them_id(), text).expect("queued");
    let delta = next(&mut outbox, &room).expect("goes out");
    let sent = the_message(&delta);

    // The same fields a web UI send of the same text has: it signs with the room's key over the
    // sender, recipient, room owner and time, and seals the raw UTF-8 text to the recipient.
    let reference = compose_direct_message(
        &f.me,
        &them().verifying_key(),
        &f.owner.verifying_key(),
        sent.message.timestamp,
        sent.message.timestamp,
        text.as_bytes(),
    )
    .unwrap();
    assert_eq!(sent.message.sender, reference.message.sender);
    assert_eq!(sent.message.recipient, reference.message.recipient);
    assert_eq!(sent.message.sender, f.me_id());
    assert_eq!(sent.message.recipient, them_id());
    assert_eq!(sent.message.timestamp, BASE);
    // The same envelope layout and size (ephemeral key, nonce, body, tag); the contents differ
    // only by the sealing's randomness.
    assert_eq!(
        sent.message.ciphertext.len(),
        reference.message.ciphertext.len()
    );
    // Independently of the helper: ephemeral key (32) + nonce (12) + the text + the AES-GCM tag
    // (16). (The comment above `seal_dm_for_recipient` says 44 bytes of overhead: that leaves out
    // the tag, and the constant is 60.)
    assert_eq!(sent.message.ciphertext.len(), 32 + 12 + text.len() + 16);
    assert_eq!(
        sent.message.ciphertext.len(),
        text.len() + river_core::ecies::DM_ENVELOPE_OVERHEAD_BYTES
    );
    assert_ne!(sent.message.ciphertext, reference.message.ciphertext);

    // It verifies as the contract verifies it.
    sent.verify_signature(&f.me.verifying_key(), &f.owner.verifying_key())
        .unwrap();

    // The recipient opens it, and finds the text as written, as plain UTF-8: no structured-body
    // prefix, which only an invitation carries.
    let opened = open_direct_message(&them(), &sent).unwrap();
    assert_eq!(opened, text.as_bytes());
    assert_ne!(opened[0], river_core::room_state::dm_body::DM_BODY_MAGIC);
    assert_eq!(
        decode_body(&opened).unwrap(),
        DirectMessageBody::Text {
            text: text.to_owned()
        }
    );
    // The sender cannot open their own: it is sealed to the recipient.
    assert!(open_direct_message(&f.me, &sent).is_err());

    // What names it in its thread.
    assert_eq!(queued.recipient, them_id());
    assert_eq!(queued.sender, Some(f.me_id()));
    assert_eq!(queued.token, sent.purge_token());
    assert_eq!(queued.timestamp, sent.message.timestamp);
}

#[test]
fn the_update_applies_through_the_contract_and_survives_its_cleanup() {
    let f = Fixture::new();
    let room = room(&f);
    let mut outbox = outbox();
    outbox.send_dm(&room, them_id(), "kept").unwrap();
    let delta = next(&mut outbox, &room).expect("goes out");
    let sent = the_message(&delta);

    // Through the room contract's own apply, which runs the cleanup after it.
    let params = ChatRoomParametersV1 {
        owner: f.owner.verifying_key(),
    };
    let mut raw = room.stored.room_state.clone();
    let parent = raw.clone();
    raw.apply_delta(&parent, &params, &Some(delta.clone()))
        .unwrap();
    assert!(
        raw.direct_messages
            .messages
            .iter()
            .any(|m| m.sender_signature == sent.sender_signature)
    );
    // And once more, the cleanup alone: it changes nothing.
    let before = raw.direct_messages.messages.clone();
    raw.post_apply_cleanup(&params).unwrap();
    assert_eq!(raw.direct_messages.messages, before);
    // The room's other state is as it was.
    assert_eq!(
        raw.members.members.len(),
        room.stored.room_state.members.members.len()
    );

    // The app's own room does the same.
    assert!(landed(&applied(&room, &sent).unwrap(), &sent));
}

#[test]
fn a_pruned_sender_is_re_added_with_the_message_and_one_with_no_way_back_is_refused() {
    let f = Fixture::new();
    // Pruned: not listed, no message in the room, but still holding what it was admitted with.
    let mut pruned = state(&f);
    pruned
        .members
        .members
        .retain(|m| m.member.member_vk != f.me.verifying_key());
    pruned
        .recent_messages
        .messages
        .retain(|m| m.message.author != f.me_id());
    let mut stored = f.stored(pruned.clone());
    stored.self_authorized_member = Some(member(&f, &f.me));
    let room = LiveRoom::new(stored);

    let mut outbox = outbox();
    outbox.send_dm(&room, them_id(), "back again").unwrap();
    let delta = next(&mut outbox, &room).expect("goes out");
    let sent = the_message(&delta);
    assert!(delta.members.is_some(), "the rejoin goes with it");
    let after = applied(&room, &sent).unwrap();
    assert!(landed(&after, &sent));
    assert!(
        after
            .stored
            .room_state
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == f.me.verifying_key()),
        "and the sender is a member again"
    );

    // Without the rejoin the room drops the message without a word: the landing check says so.
    let stranded = LiveRoom::new(f.stored(pruned));
    let message = compose_direct_message(
        &f.me,
        &them().verifying_key(),
        &f.owner.verifying_key(),
        BASE,
        BASE,
        b"lost",
    )
    .unwrap();
    assert_eq!(applied(&stranded, &message).err(), Some(Refusal::NotKept));
    // And the app does not queue it in the first place.
    assert_eq!(
        preflight(&stranded, them_id(), "lost"),
        Err(Refusal::Blocked(Blocked::NotMember))
    );
    assert!(
        clock_at(BASE)
            .send_dm(&stranded, them_id(), "lost")
            .is_err()
    );
}

#[test]
fn a_message_the_room_does_not_keep_is_not_queued() {
    let f = Fixture::new();
    // The recipient leaves between the check and the apply: the room sweeps messages to a
    // member it no longer lists. Here the recipient is gone from the copy the update is tried on.
    let room = room(&f);
    let mut left = room.clone();
    left.stored
        .room_state
        .members
        .members
        .retain(|m| m.member.member_vk != them().verifying_key());
    let message = compose_direct_message(
        &f.me,
        &them().verifying_key(),
        &f.owner.verifying_key(),
        BASE,
        BASE,
        b"hello",
    )
    .unwrap();
    // The recipient is gone: the one refusal that cannot heal, and typed as such.
    let refusal = applied(&left, &message).err();
    assert_eq!(refusal, Some(Refusal::NotMember));
    assert!(is_permanent(&Refusal::NotMember));
    // The others can heal, so they are not permanent.
    for other in [
        Refusal::NotKept,
        Refusal::Refused("x".into()),
        Refusal::Empty,
        Refusal::Blocked(Blocked::NoSecret),
    ] {
        assert!(!is_permanent(&other), "{other:?}");
    }
    assert!(Refusal::NotKept.text().contains("didn't keep"));
}

// Queueing, retrying and settling --------------------------------------------------------------------

fn rows(outbox: &Outbox, room: &LiveRoom) -> Vec<crate::rooms::outbox::DmRow> {
    outbox.dm_rows(&room.stored.owner_vk)
}

#[test]
fn a_send_waits_its_turn_behind_the_update_that_is_out_and_keeps_its_order() {
    let f = Fixture::new();
    let room = room(&f);
    let mut outbox = outbox();
    outbox.send_dm(&room, them_id(), "first").unwrap();
    outbox.send_dm(&room, them_id(), "second").unwrap();
    // A message to the room between them.
    assert!(outbox.command(
        &room,
        Action::Send {
            text: "in the room".into(),
            reply_to: None
        }
    ));
    let first = the_message(&next(&mut outbox, &room).expect("the first goes"));
    // One update per room is out at a time: nothing more goes until it is answered.
    assert!(next(&mut outbox, &room).is_none());
    assert!(next(&mut outbox, &room).is_none());

    let mut room = room;
    assert!(outbox.answered(&mut room, Ok(())));
    assert!(landed(&room, &first), "taken: it joins the room at once");
    let second = the_message(&next(&mut outbox, &room).expect("then the second"));
    assert!(outbox.answered(&mut room, Ok(())));
    let third = next(&mut outbox, &room).expect("then the room message");
    assert!(third.direct_messages.is_none() && third.recent_messages.is_some());
    assert_ne!(first.sender_signature, second.sender_signature);
    // A message to a room member and the room's own message are different entries: the answers
    // were matched to them in order, and every one of the direct messages is in the room.
    assert!(landed(&room, &second));
    assert!(rows(&outbox, &room).is_empty(), "taken ones are done");
}

#[test]
fn the_row_says_sending_then_not_sent_and_retry_sends_the_same_bytes() {
    let f = Fixture::new();
    let mut room = room(&f);
    let mut outbox = outbox();
    outbox.send_dm(&room, them_id(), "try me").unwrap();
    let r = rows(&outbox, &room);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].text.as_ref().map(|t| t.as_str()), Some("try me"));
    assert_eq!(r[0].pending.failed, None, "still on its way");

    let first = the_message(&next(&mut outbox, &room).unwrap());
    // The node refused it.
    assert!(outbox.answered(&mut room, Err("nope".into())));
    let r = rows(&outbox, &room);
    assert_eq!(r.len(), 1);
    let Pending {
        entry,
        failed,
        retry,
    } = r[0].pending.clone();
    assert!(failed.unwrap().contains("nope"));
    assert!(retry);
    assert!(
        !landed(&room, &first),
        "a refused message is not in the room"
    );

    // Retry sends the very same signed message: the same signature and ciphertext, never a new
    // composition (a second one would be sealed with new randomness and show twice).
    assert!(outbox.command(&room, Action::Retry { entry }));
    let again = the_message(&next(&mut outbox, &room).expect("goes again"));
    assert_eq!(again, first);
    assert_eq!(again.message.ciphertext, first.message.ciphertext);
    // And again after the connection was lost with it in flight.
    outbox.requeue();
    let once_more = the_message(&next(&mut outbox, &room).expect("goes on the new connection"));
    assert_eq!(once_more, first);

    assert!(outbox.answered(&mut room, Ok(())));
    assert!(landed(&room, &first));
    assert!(rows(&outbox, &room).is_empty());
}

#[test]
fn an_unanswered_send_is_resent_unchanged_and_discard_forgets_it() {
    let f = Fixture::new();
    let room = room(&f);
    let mut outbox = outbox();
    outbox.send_dm(&room, them_id(), "no answer").unwrap();
    let first = the_message(&next(&mut outbox, &room).unwrap());
    assert!(outbox.expire(
        Duration::from_secs(1),
        Instant::now() + Duration::from_secs(5)
    ));
    let r = rows(&outbox, &room);
    let entry = r[0].pending.entry;
    assert!(r[0].pending.failed.is_some(), "shown as not confirmed");
    assert!(r[0].pending.retry);
    // Retry of an unanswered one asks for a new connection and sends it unchanged there.
    assert!(outbox.command(&room, Action::Retry { entry }));
    assert!(outbox.wants_reconnect(Duration::from_secs(3600), Instant::now()));
    outbox.requeue();
    assert_eq!(the_message(&next(&mut outbox, &room).unwrap()), first);

    // Discard: only a message shown as not sent can be discarded.
    assert!(!outbox.command(&room, Action::Discard { entry }));
    assert!(outbox.expire(
        Duration::from_secs(1),
        Instant::now() + Duration::from_secs(5)
    ));
    assert!(outbox.command(&room, Action::Discard { entry }));
    assert!(rows(&outbox, &room).is_empty());
    assert_eq!(
        outbox.dm_identity(&room.stored.owner_vk, entry),
        None,
        "and it is gone"
    );
}

#[test]
fn what_the_network_brings_in_settles_the_entry_and_a_taken_but_dropped_one_says_so() {
    let f = Fixture::new();
    let mut room = room(&f);
    let mut outbox = outbox();
    outbox.send_dm(&room, them_id(), "arrives").unwrap();
    let sent = the_message(&next(&mut outbox, &room).unwrap());
    // The room brings it in by itself, before the answer.
    room.apply_delta(super::delta(&room, &sent)).unwrap();
    assert!(outbox.reconcile(&mut room));
    assert!(rows(&outbox, &room).is_empty());
    // The answer still owed is consumed and changes nothing: the entry is already gone.
    assert!(!outbox.answered(&mut room, Ok(())));
    assert!(outbox.is_empty());

    // Taken by the node but not held by the room here: unconfirmed, with why, and not sent again.
    let mut room = self::room(&f);
    let mut outbox = self::outbox();
    outbox.send_dm(&room, them_id(), "dropped").unwrap();
    let message = the_message(&next(&mut outbox, &room).unwrap());
    // Meanwhile the recipient left the room here.
    room.stored
        .room_state
        .members
        .members
        .retain(|m| m.member.member_vk != them().verifying_key());
    assert!(outbox.answered(&mut room, Ok(())));
    let r = rows(&outbox, &room);
    assert_eq!(r.len(), 1);
    assert!(
        r[0].pending
            .failed
            .as_deref()
            .unwrap()
            .contains("Freenet took it")
    );
    assert!(!landed(&room, &message));
    assert!(
        next(&mut outbox, &room).is_none(),
        "never sent again from here"
    );
}

#[test]
fn a_send_is_failed_not_sent_when_you_can_no_longer_send_or_the_identity_changed() {
    let f = Fixture::new();
    let room = room(&f);
    // Banned after it was queued: it fails with the reason instead of going out.
    let mut outbox = outbox();
    outbox.send_dm(&room, them_id(), "too late").unwrap();
    let mut banned_state = state(&f);
    banned_state.bans.0.push(AuthorizedUserBan::new(
        UserBan {
            owner_member_id: f.owner_id(),
            banned_at: secs(BASE),
            banned_user: f.me_id(),
        },
        f.owner_id(),
        &f.owner,
    ));
    let banned = LiveRoom::new(f.stored(banned_state));
    assert!(next(&mut outbox, &banned).is_none());
    let r = rows(&outbox, &banned);
    assert!(r[0].pending.failed.as_deref().unwrap().contains("banned"));

    // A different identity for the room: the message was signed as another member.
    let mut outbox = self::outbox();
    outbox.send_dm(&room, them_id(), "mine").unwrap();
    let mut other = room.clone();
    other.stored.self_sk = Some(SigningKey::from_bytes(&[77; 32]));
    assert!(next(&mut outbox, &other).is_none());
    assert!(
        rows(&outbox, &other)[0]
            .pending
            .failed
            .as_deref()
            .unwrap()
            .contains("identity")
    );
}

#[test]
fn a_newer_river_fails_what_is_queued_and_nothing_goes_out() {
    let f = Fixture::new();
    let room = room(&f);
    let mut outbox = outbox();
    outbox.send_dm(&room, them_id(), "held back").unwrap();
    assert!(outbox.fail_queued(crate::rooms::generation::UPDATE_REQUIRED));
    assert!(next(&mut outbox, &room).is_none());
    let r = rows(&outbox, &room);
    assert!(
        r[0].pending
            .failed
            .as_deref()
            .unwrap()
            .contains("newer version of River")
    );
}

#[test]
fn nothing_prints_the_text_a_key_or_the_ciphertext() {
    let f = Fixture::new();
    let room = room(&f);
    let mut outbox = outbox();
    outbox
        .send_dm(&room, them_id(), "PLAINTEXT-SECRET")
        .unwrap();
    let action = Action::SendDm {
        peer: them_id(),
        text: "PLAINTEXT-SECRET".into(),
        ticket: 3,
    };
    assert!(next(&mut outbox, &room).unwrap().direct_messages.is_some());
    let rows = rows(&outbox, &room);
    for shown in [
        format!("{action:?}"),
        format!("{action:#?}"),
        format!("{rows:?}"),
        format!("{rows:#?}"),
    ] {
        assert!(!shown.contains("PLAINTEXT"), "{shown}");
    }
    // The refusals never carry the message either, and a compose error names sizes only.
    for refusal in [
        Refusal::Empty,
        Refusal::SelfMessage,
        Refusal::NotMember,
        Refusal::NotKept,
        Refusal::TooLong { bytes: 5 },
        Refusal::Blocked(Blocked::NoKey),
    ] {
        assert!(!refusal.text().contains("PLAINTEXT"));
    }
    // The source neither logs nor writes a delegate key.
    let code = include_str!("dm_send.rs").replace("\r\n", "\n");
    let code = code.split("#[cfg(test)]").next().unwrap();
    for banned in [
        "println!",
        "eprintln!",
        "dbg!",
        "tracing::",
        "log::",
        "ClientRequest",
        "StoreRequest",
        "CasStore",
        "super::delegate",
        "NodeSocket",
        "sign_direct_message",
        "SigningKey::from",
    ] {
        assert!(!code.contains(banned), "dm_send.rs contains {banned}");
    }
}

// The recipient is a full member id --------------------------------------------------------------------

/// An id with the label (`Display`, the first 40 bits) of `id` and a different full value, as
/// one made from a key ground to match `id`'s label would be.
fn same_label(id: MemberId) -> MemberId {
    let twin = MemberId(freenet_scaffold::util::FastHash(id.0.0 ^ (0x5a << 48)));
    assert_eq!(twin.to_string(), id.to_string(), "the same label");
    assert_ne!(twin, id);
    twin
}

#[test]
fn the_recipient_is_found_by_the_full_id_and_never_by_its_label() {
    let f = Fixture::new();
    let room = room(&f);
    let twin = same_label(them_id());
    // A twin of a member is not a member: refused, whatever its label says.
    let mut outbox = outbox();
    assert_eq!(
        outbox.send_dm(&room, twin, "to the twin").unwrap_err(),
        Refusal::NotMember.text()
    );
    assert_eq!(preflight(&room, twin, "x").err(), Some(Refusal::NotMember));
    // And the owner's twin the same, though the owner is looked at first.
    assert!(preflight(&room, same_label(f.owner_id()), "x").is_err());
    assert!(outbox.is_empty());
    // The real member gets it: the message is addressed to that id and sealed to that key.
    let queued = outbox.send_dm(&room, them_id(), "to them").unwrap();
    assert_eq!(queued.recipient, them_id());
    let message = the_message(&next(&mut outbox, &room).unwrap());
    assert_eq!(message.message.recipient, them_id());
    assert!(open_direct_message(&them(), &message).is_ok());
    // The command and the update name the full id.
    let action = Action::SendDm {
        peer: them_id(),
        text: "x".into(),
        ticket: 1,
    };
    assert!(format!("{action:?}").contains(&crate::rooms::dm::peer_key(them_id())));
    assert_eq!(
        crate::rooms::dm::peer_from_key(&crate::rooms::dm::peer_key(twin)),
        Some(twin),
        "the key round-trips every bit"
    );
    assert_ne!(
        crate::rooms::dm::peer_key(twin),
        crate::rooms::dm::peer_key(them_id()),
        "twins have two keys"
    );
}

// The cap ----------------------------------------------------------------------------------------------

#[test]
fn a_body_at_the_cap_is_composed_applied_and_kept_ascii_or_with_a_multibyte_tail() {
    use river_core::room_state::direct_messages::MAX_DM_CIPHERTEXT_BYTES;
    let f = Fixture::new();
    let room = room(&f);
    let at_cap = "a".repeat(BODY_CAP);
    // 3 bytes at the end, ending exactly on the cap.
    let tail = format!("{}\u{20ac}", "a".repeat(BODY_CAP - 3));
    assert_eq!(tail.len(), BODY_CAP);
    for text in [at_cap, tail] {
        let message = prepare(&room, them_id(), &text, BASE).expect("the cap itself is sent");
        assert!(message.message.ciphertext.len() <= MAX_DM_CIPHERTEXT_BYTES);
        assert_eq!(
            message.message.ciphertext.len(),
            text.len() + river_core::ecies::DM_ENVELOPE_OVERHEAD_BYTES
        );
        assert_eq!(
            open_direct_message(&them(), &message).unwrap(),
            text.as_bytes()
        );
        // Through the app's room, and through the contract's own apply and cleanup.
        assert!(landed(&applied(&room, &message).unwrap(), &message));
        let params = ChatRoomParametersV1 {
            owner: f.owner.verifying_key(),
        };
        let mut raw = room.stored.room_state.clone();
        let parent = raw.clone();
        raw.apply_delta(&parent, &params, &Some(delta(&room, &message)))
            .unwrap();
        raw.post_apply_cleanup(&params).unwrap();
        assert!(
            raw.direct_messages
                .messages
                .iter()
                .any(|m| m.sender_signature == message.sender_signature),
            "kept by the contract"
        );
        // And one more byte is refused before anything is composed.
        assert!(prepare(&room, them_id(), &format!("{text}b"), BASE).is_err());
    }
}

// What changed in the room between queueing and sending ---------------------------------------------------

fn without_member(f: &Fixture, who: &SigningKey) -> LiveRoom {
    let mut s = state(f);
    s.members
        .members
        .retain(|m| m.member.member_vk != who.verifying_key());
    s.recent_messages
        .messages
        .retain(|m| m.message.author != MemberId::from(&who.verifying_key()));
    LiveRoom::new(f.stored(s))
}

#[test]
fn a_recipient_who_left_makes_the_row_failed_and_not_retryable_with_why() {
    let f = Fixture::new();
    let room = room(&f);
    let mut outbox = outbox();
    outbox.send_dm(&room, them_id(), "too slow").unwrap();
    let left = without_member(&f, &them());
    assert!(next(&mut outbox, &left).is_none(), "nothing goes out");
    let r = rows(&outbox, &left);
    assert_eq!(r.len(), 1);
    let why = r[0].pending.failed.clone().expect("failed");
    assert_eq!(why, Refusal::NotMember.text());
    assert!(!r[0].pending.retry, "sending it again can only fail again");
    let entry = r[0].pending.entry;
    assert!(!outbox.command(&left, Action::Retry { entry }));
    assert!(next(&mut outbox, &left).is_none());
    // Discard is there.
    assert!(outbox.command(&left, Action::Discard { entry }));
    assert!(rows(&outbox, &left).is_empty());
}

#[test]
fn a_sender_pruned_after_the_first_send_resends_the_same_message_with_the_rejoin() {
    let f = Fixture::new();
    let mut room = room(&f);
    let mut outbox = outbox();
    outbox.send_dm(&room, them_id(), "again").unwrap();
    let first = the_message(&next(&mut outbox, &room).unwrap());
    assert!(outbox.answered(&mut room, Err("refused".into())));
    let entry = rows(&outbox, &room)[0].pending.entry;
    // The room then prunes the sender for inactivity, as far as this copy knows.
    let mut stored = without_member(&f, &f.me).stored;
    stored.self_authorized_member = Some(member(&f, &f.me));
    let pruned = LiveRoom::new(stored);
    assert!(outbox.command(&pruned, Action::Retry { entry }));
    let delta = next(&mut outbox, &pruned).expect("goes again");
    assert!(delta.members.is_some(), "the rejoin goes with it");
    let again = the_message(&delta);
    assert_eq!(
        again, first,
        "the same signed message, not a new composition"
    );
    assert!(landed(&applied(&pruned, &again).unwrap(), &again));
}

#[test]
fn room_messages_and_direct_messages_queue_in_order_and_retry_between_them() {
    let f = Fixture::new();
    let mut room = room(&f);
    let mut outbox = outbox();
    let say = |text: &str| Action::Send {
        text: text.into(),
        reply_to: None,
    };
    assert!(outbox.command(&room, say("room one")));
    outbox.send_dm(&room, them_id(), "dm one").unwrap();
    outbox.send_dm(&room, them_id(), "dm two").unwrap();
    // The room message goes first and is refused; the direct messages are not held by it.
    let first = next(&mut outbox, &room).expect("room one");
    assert!(first.recent_messages.is_some() && first.direct_messages.is_none());
    assert!(outbox.answered(&mut room, Err("busy".into())));
    let one = the_message(&next(&mut outbox, &room).expect("dm one goes"));
    // Retry of the room message while a direct message is out: it waits its turn.
    let view = outbox.view(&room, &|t| {
        (t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() / 60) as u32
    });
    let entry = view.messages.last().unwrap().pending.clone().unwrap().entry;
    assert!(outbox.command(&room, Action::Retry { entry }));
    assert!(
        next(&mut outbox, &room).is_none(),
        "the update that is out comes first"
    );
    assert!(outbox.answered(&mut room, Ok(())));
    assert!(landed(&room, &one));
    // Entries go in the order they were made: the retried room message, which was first, then
    // the second direct message.
    let again = next(&mut outbox, &room).expect("the retried room message");
    assert!(again.recent_messages.is_some() && again.direct_messages.is_none());
    assert!(outbox.answered(&mut room, Ok(())));
    let last = the_message(&next(&mut outbox, &room).expect("then dm two"));
    assert_ne!(last.sender_signature, one.sender_signature);
}

// The thread rows of messages still in the outbox ----------------------------------------------------------

fn dm_row(
    room: &LiveRoom,
    to: MemberId,
    text: &str,
    ts: u64,
    pending: Pending,
) -> crate::rooms::outbox::DmRow {
    let sk = room.stored.self_sk.as_ref().unwrap();
    let vk = if to == them_id() {
        them().verifying_key()
    } else {
        room.stored.owner_vk
    };
    let m =
        compose_direct_message(sk, &vk, &room.stored.owner_vk, ts, ts, text.as_bytes()).unwrap();
    crate::rooms::outbox::DmRow {
        recipient: to,
        sender: m.message.sender,
        token: m.purge_token(),
        timestamp: ts,
        text: Some(crate::rooms::dm_cache::Plaintext::new(text.to_owned())),
        pending,
    }
}

fn pending(entry: u64) -> Pending {
    Pending {
        entry,
        failed: None,
        retry: false,
    }
}

fn thread_of(
    room: &LiveRoom,
    to: MemberId,
    messages: Vec<crate::chat::dm_model::DmMessage>,
) -> Arc<crate::chat::dm_model::DmThread> {
    Arc::new(crate::chat::dm_model::DmThread {
        room: room.key(),
        room_name: room.name(),
        peer_key: crate::rooms::dm::peer_key(to),
        peer: to,
        peer_name: room.member_name(to),
        peer_is_member: true,
        last_ts: messages.last().map_or(0, |m| m.timestamp),
        last_inbound_ts: 0,
        messages,
    })
}

#[test]
fn pending_rows_join_their_thread_or_make_one_and_the_room_s_own_row_only_gains_the_state() {
    use crate::chat::dm_model::{DmBody, DmMessage};
    let f = Fixture::new();
    let room = room(&f);
    let msg = |key: String, ts: u64, text: &str| DmMessage {
        key,
        outgoing: true,
        timestamp: ts,
        body: DmBody::Text(text.into()),
        pending: None,
    };

    // No thread yet: one is made, named for the peer, with the row on its way.
    let row = dm_row(&room, them_id(), "first", BASE + 5, pending(1));
    let made = with_pending(&room, &[], std::slice::from_ref(&row));
    assert_eq!(made.len(), 1);
    assert_eq!(made[0].peer_key, crate::rooms::dm::peer_key(them_id()));
    assert_eq!(made[0].messages.len(), 1);
    assert_eq!(made[0].messages[0].pending, Some(pending(1)));
    assert_eq!(made[0].messages[0].body, DmBody::Text("first".into()));
    assert!(made[0].messages[0].outgoing);
    assert_eq!(made[0].last_ts, BASE + 5);

    // The room already holds the row: it only gains the state, and the thread is not doubled.
    let key = crate::rooms::dm::token_hex(&row.token);
    let held = thread_of(&room, them_id(), vec![msg(key.clone(), BASE + 5, "first")]);
    let out = with_pending(
        &room,
        std::slice::from_ref(&held),
        std::slice::from_ref(&row),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].messages.len(), 1);
    assert_eq!(out[0].messages[0].pending, Some(pending(1)));
    assert!(
        held.messages[0].pending.is_none(),
        "the memo's copy is not touched"
    );

    // A thread of other peers is handed back as it was (the same Arc), and one with another
    // message gains the row in time order, stable for the same second.
    let owner_thread = thread_of(&room, f.owner_id(), vec![msg("o".into(), BASE, "owner")]);
    let a = msg("a".into(), BASE + 5, "same second, earlier");
    let b = msg("b".into(), BASE + 9, "later");
    let mine = thread_of(&room, them_id(), vec![a, b]);
    let out = with_pending(
        &room,
        &[owner_thread.clone(), mine],
        std::slice::from_ref(&row),
    );
    assert!(Arc::ptr_eq(&out[0], &owner_thread));
    let order: Vec<&str> = out[1].messages.iter().map(|m| m.key.as_str()).collect();
    // By time, and within one second by purge token (as `dm::threads` orders them).
    let mut tie = ["a", key.as_str()];
    tie.sort();
    assert_eq!(order, [tie[0], tie[1], "b"]);
    assert_eq!(out[1].last_ts, BASE + 9);
    // Two pending rows in one second are ordered by token, whatever order they were queued in.
    let second = dm_row(&room, them_id(), "second", BASE + 5, pending(2));
    let mut want = vec![key.clone(), crate::rooms::dm::token_hex(&second.token)];
    want.sort();
    for rows in [[row.clone(), second.clone()], [second.clone(), row.clone()]] {
        let out = with_pending(&room, &[], &rows);
        let keys: Vec<String> = out[0].messages.iter().map(|m| m.key.clone()).collect();
        assert_eq!(keys, want);
    }
    // A row with no text is shown as what it is.
    let mut bare = dm_row(&room, them_id(), "x", BASE, pending(3));
    bare.text = None;
    let out = with_pending(&room, &[], &[bare]);
    assert_eq!(
        out[0].messages[0].body,
        DmBody::Placeholder(crate::chat::dm_model::Placeholder::CiphertextOnly)
    );
}

#[test]
fn only_the_recipient_being_gone_stops_retry_and_other_refusals_keep_it() {
    let f = Fixture::new();
    let mut room = room(&f);
    let mut outbox = outbox();
    outbox.send_dm(&room, them_id(), "heals").unwrap();
    let first = the_message(&next(&mut outbox, &room).unwrap());
    assert!(outbox.answered(&mut room, Err("busy".into())));
    let entry = rows(&outbox, &room)[0].pending.entry;
    // The room copy loses the sender (no way back) while the recipient is still there: a
    // refusal that can heal, so Retry is still offered, and the row stays retryable after the
    // retry fails again.
    let stranded = without_member(&f, &f.me);
    assert!(outbox.command(&stranded, Action::Retry { entry }));
    let r = rows(&outbox, &stranded);
    assert!(r[0].pending.failed.is_some());
    assert!(r[0].pending.retry, "a transient refusal keeps Retry");
    // And it goes out as the same message when the room is as it was.
    assert!(outbox.command(&room, Action::Retry { entry }));
    assert_eq!(the_message(&next(&mut outbox, &room).unwrap()), first);
    // The recipient leaving is the permanent one.
    let left = without_member(&f, &them());
    assert!(outbox.answered(&mut room, Err("again".into())));
    assert!(outbox.command(&left, Action::Retry { entry }));
    assert!(
        !rows(&outbox, &left)[0].pending.retry,
        "the recipient is gone"
    );
    let mut other = self::outbox();
    other.send_dm(&room, them_id(), "gone").unwrap();
    assert!(next(&mut other, &left).is_none());
    assert!(!rows(&other, &left)[0].pending.retry);
}
