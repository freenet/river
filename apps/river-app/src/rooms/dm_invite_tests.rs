//! Invitations in direct messages: the bytes that go out, what is saved for the sender's row,
//! what accepting refuses, and where the invitation can and cannot end up. Rooms are built the
//! way River builds them (`room_tests::Fixture`) and messages are checked against river-core's own
//! helpers, which the web UI and `riverctl` send and read with.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use ed25519_dalek::SigningKey;
use river_core::invitation::Invitation;
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{
    AuthorizedDirectMessage, MAX_DM_CIPHERTEXT_BYTES, compose_direct_message, open_direct_message,
};
use river_core::room_state::dm_body::{
    DM_BODY_MAGIC, DirectMessageBody, InvitePayload, decode_body, encode_body,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::privacy::RoomDisplayMetadata;
use river_core::room_state::{ChatRoomStateV1, ChatRoomStateV1Delta};

use super::*;
use crate::rooms::outbox::{Action, Outbox};
use crate::rooms::room_tests::Fixture;

const BASE: u64 = 1_700_000_000;

fn them() -> SigningKey {
    SigningKey::from_bytes(&[3; 32])
}

fn them_id() -> MemberId {
    MemberId::from(&them().verifying_key())
}

fn clock() -> Outbox {
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

/// A public room named `name` with the user and `them()` as members, each with a message in it
/// (a member with no recent message is pruned by the room's own cleanup).
fn state(f: &Fixture, name: &str) -> ChatRoomStateV1 {
    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: f.owner_id(),
                display: RoomDisplayMetadata::public(name.into(), None),
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

/// The room a message is sent in (the owner is seed 1, the user seed 2).
fn carrier() -> (Fixture, LiveRoom) {
    let f = Fixture::new();
    let room = LiveRoom::new(f.stored(state(&f, "Town")));
    (f, room)
}

/// A second room, owned by seed 11, where the user is a member.
fn target() -> (Fixture, LiveRoom) {
    let f = Fixture {
        owner: SigningKey::from_bytes(&[11; 32]),
        me: SigningKey::from_bytes(&[2; 32]),
    };
    let room = LiveRoom::new(f.stored(state(&f, "Book club")));
    (f, room)
}

/// The fixture's private room ("Secret garden") as a target, owned by seed 12, where the user is
/// a member holding its secret.
fn private_target() -> (Fixture, LiveRoom) {
    let f = Fixture {
        owner: SigningKey::from_bytes(&[12; 32]),
        me: SigningKey::from_bytes(&[2; 32]),
    };
    let room = LiveRoom::new(f.stored(f.private_state()));
    (f, room)
}

fn secs(now: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(now)
}

fn the_message(delta: &ChatRoomStateV1Delta) -> AuthorizedDirectMessage {
    let dms = delta.direct_messages.as_ref().expect("a direct message");
    assert_eq!(dms.new_messages.len(), 1);
    dms.new_messages[0].clone()
}

/// The invitation the message carries, opened as the recipient does.
fn invitation_in(message: &AuthorizedDirectMessage) -> (InvitePayload, Invitation) {
    let opened = open_direct_message(&them(), message).expect("sealed to them");
    let DirectMessageBody::Invite(payload) = decode_body(&opened).expect("a body") else {
        panic!("an invitation");
    };
    let invitation: Invitation =
        ciborium::de::from_reader(&payload.invitation_payload[..]).expect("an invitation");
    (*payload, invitation)
}

// The bytes that go out --------------------------------------------------------------------------

#[test]
fn the_message_is_the_web_uis_structured_body_sealed_to_the_recipient() {
    let (_, carrier) = carrier();
    let (tf, target) = target();
    let (message, _) =
        prepare(&carrier, &target, them_id(), Some("  come along  "), BASE).expect("prepared");

    // The recipient opens it with their key and finds `0x80` + CBOR, as `encode_body` writes it.
    let opened = open_direct_message(&them(), &message).unwrap();
    assert_eq!(opened[0], DM_BODY_MAGIC);
    let (payload, invitation) = invitation_in(&message);
    assert_eq!(payload.room_owner_vk, tf.owner.verifying_key());
    assert_eq!(payload.personal_message.as_deref(), Some("come along"));
    // Writing the body again gives exactly the bytes that were sealed.
    assert_eq!(
        encode_body(&DirectMessageBody::Invite(Box::new(payload))).unwrap(),
        opened
    );

    // The invitation is the user's, to the second room, for a fresh key, signed by the user, and
    // agrees with itself.
    assert_eq!(invitation.room, tf.owner.verifying_key());
    assert_eq!(create::invitation_refusal(&invitation), None);
    assert_eq!(
        invitation.invitee.member.invited_by,
        MemberId::from(&tf.me.verifying_key())
    );
    invitation
        .invitee
        .verify_signature(&tf.me.verifying_key())
        .expect("the room's contract takes it");
    assert!(invitation.room_secrets.is_empty(), "a public room");
    assert_ne!(
        invitation.invitee_signing_key.verifying_key(),
        tf.me.verifying_key()
    );

    // As any direct message: signed by the user for the room it is sent in, to the recipient.
    assert_eq!(
        message.message.sender,
        MemberId::from(&SigningKey::from_bytes(&[2; 32]).verifying_key())
    );
    assert_eq!(message.message.recipient, them_id());
    message
        .verify_signature(
            &SigningKey::from_bytes(&[2; 32]).verifying_key(),
            &carrier.stored.owner_vk,
        )
        .unwrap();
    // The sender cannot open their own.
    assert!(open_direct_message(&SigningKey::from_bytes(&[2; 32]), &message).is_err());
}

#[test]
fn a_private_rooms_invitation_carries_its_secrets_as_the_link_does() {
    let (_, carrier) = carrier();
    let (_, target) = private_target();
    let (message, _) = prepare(&carrier, &target, them_id(), None, BASE).expect("prepared");
    let (payload, invitation) = invitation_in(&message);
    assert_eq!(payload.personal_message, None);
    assert_eq!(invitation.room_secrets, target.secrets_by_version());
    assert!(!invitation.room_secrets.is_empty());
    // The link made for the same room holds the same secrets.
    let (link_invitation, _) = create::invitation(&target, "http://x/").unwrap();
    assert_eq!(link_invitation.room_secrets, invitation.room_secrets);
}

#[test]
fn each_invitation_is_a_new_one_and_the_cache_summary_is_the_web_uis_string() {
    assert_eq!(summary(None), "[Invitation]");
    assert_eq!(summary(Some("")), "[Invitation]");
    assert_eq!(summary(Some("   \n ")), "[Invitation]");
    assert_eq!(summary(Some("  hello  ")), "[Invitation] hello");
    assert_eq!(summary(Some("a\nb")), "[Invitation] a\nb");

    let (_, carrier) = carrier();
    let (_, target) = target();
    let (a, sa) = prepare(&carrier, &target, them_id(), Some("hi"), BASE).unwrap();
    let (b, sb) = prepare(&carrier, &target, them_id(), Some("hi"), BASE).unwrap();
    assert_eq!(sa, "[Invitation] hi");
    assert_eq!(sa, sb);
    assert_ne!(
        invitation_in(&a).1.invitee_signing_key.verifying_key(),
        invitation_in(&b).1.invitee_signing_key.verifying_key(),
        "every invitation has its own key"
    );
}

#[test]
fn the_sent_row_holds_only_the_summary_and_never_the_invitation() {
    let (_, carrier) = carrier();
    let (_, target) = target();
    let mut outbox = clock();
    let (queued, summary) = outbox
        .send_invite_dm(&carrier, &target, them_id(), Some("see you there"))
        .expect("queued");
    assert_eq!(summary, "[Invitation] see you there");
    let rows = outbox.dm_rows(&carrier.stored.owner_vk);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].text.as_ref().map(|t| t.as_str()),
        Some("[Invitation] see you there")
    );
    assert_eq!(rows[0].token, queued.token);

    // The invitation's key and the room's name appear in none of what the row holds or prints.
    let mut changed = false;
    let delta = outbox
        .next_update(&carrier, Instant::now(), &mut changed)
        .expect("goes out");
    let (_, invitation) = invitation_in(&the_message(&delta));
    let key = invitation.invitee_signing_key.to_bytes();
    let printed = format!("{rows:?} {queued:?} {summary:?}");
    assert!(!printed.contains("Book club"));
    for needle in [
        bs58::encode(key).into_string(),
        key.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        format!("{key:?}"),
    ] {
        assert!(!printed.contains(&needle));
    }
    // The row reads as an invitation sent, as the web UI's cache entry does.
    assert!(matches!(
        super::super::dm::sent_body(rows[0].text.as_ref().unwrap().as_str()),
        crate::chat::dm_model::DmBody::SentInvite { note: Some(n) } if n == "see you there"
    ));
}

#[test]
fn a_retry_sends_the_very_same_signed_message() {
    let (_, mut carrier) = carrier();
    let (_, target) = target();
    let mut outbox = clock();
    outbox
        .send_invite_dm(&carrier, &target, them_id(), None)
        .unwrap();
    let mut changed = false;
    let first = the_message(
        &outbox
            .next_update(&carrier, Instant::now(), &mut changed)
            .unwrap(),
    );
    assert!(outbox.answered(&mut carrier, Err("nope".into())));
    let entry = outbox.dm_rows(&carrier.stored.owner_vk)[0].pending.entry;
    assert!(outbox.command(&carrier, Action::Retry { entry }));
    let again = the_message(
        &outbox
            .next_update(&carrier, Instant::now(), &mut changed)
            .unwrap(),
    );
    assert_eq!(
        again, first,
        "never composed again: a second invitation would be a new key"
    );
    outbox.requeue();
    let once_more = the_message(
        &outbox
            .next_update(&carrier, Instant::now(), &mut changed)
            .unwrap(),
    );
    assert_eq!(once_more, first);
}

#[test]
fn the_update_applies_through_the_contract_and_survives_its_cleanup() {
    let (_, carrier) = carrier();
    let (_, target) = target();
    let (message, _) = prepare(&carrier, &target, them_id(), None, BASE).unwrap();
    let mut next = carrier.clone();
    next.apply_delta(dm_send::delta(&carrier, &message))
        .unwrap();
    assert!(dm_send::landed(&next, &message));
}

// What is refused when sending ---------------------------------------------------------------------

#[test]
fn a_note_is_bounded_and_may_not_hide_or_reorder_text() {
    assert_eq!(note_refusal("plain\nwith a line break"), None);
    assert_eq!(note_refusal(&"x".repeat(MAX_NOTE_CHARS)), None);
    assert!(note_refusal(&"x".repeat(MAX_NOTE_CHARS + 1)).is_some());
    for hostile in [
        "evil\u{202E}txt",
        "bell\u{7}",
        "zero\u{200B}width",
        "isolate\u{2066}x",
        "tag\u{E0041}",
        "nul\0",
    ] {
        assert!(note_refusal(hostile).is_some(), "{hostile:?}");
    }
    let (_, carrier) = carrier();
    let (_, target) = target();
    let why = prepare(&carrier, &target, them_id(), Some("a\u{202E}b"), BASE).unwrap_err();
    assert!(matches!(why, Refusal::Invite(_)));
    // The longest allowed note, of the widest characters, still fits one message.
    let wide = "\u{1F600}".repeat(MAX_NOTE_CHARS);
    prepare(&carrier, &target, them_id(), Some(&wide), BASE).expect("fits the cap");
}

#[test]
fn every_refusal_is_said_in_fixed_words_and_mints_nothing() {
    let (f, carrier) = carrier();
    let (_, target) = target();
    let secret = "SECRET-NOTE";

    // The same room.
    let why = prepare(&carrier, &carrier, them_id(), Some(secret), BASE).unwrap_err();
    assert!(matches!(&why, Refusal::Invite(w) if !w.contains(secret)));

    // No key for the room the invitation is to.
    let mut no_key = target.clone();
    no_key.stored.self_sk = None;
    assert!(can_invite_to(&carrier, &no_key).is_err());

    // Banned from the room the invitation is to.
    let mut banned = target.clone();
    banned.stored.room_state.bans.0.push(AuthorizedUserBan::new(
        UserBan {
            owner_member_id: MemberId::from(&banned.stored.owner_vk),
            banned_at: secs(BASE),
            banned_user: MemberId::from(&SigningKey::from_bytes(&[2; 32]).verifying_key()),
        },
        MemberId::from(&banned.stored.owner_vk),
        &SigningKey::from_bytes(&[11; 32]),
    ));
    assert!(can_invite_to(&carrier, &banned).is_err());

    // Not listed in it: an invitation from a member the room does not list is not accepted.
    let mut unlisted = target.clone();
    unlisted.stored.room_state.members.members.clear();
    unlisted.stored.self_authorized_member = Some(member(
        &Fixture {
            owner: SigningKey::from_bytes(&[11; 32]),
            me: SigningKey::from_bytes(&[2; 32]),
        },
        &SigningKey::from_bytes(&[2; 32]),
    ));
    assert!(can_invite_to(&carrier, &unlisted).is_err());

    // The person is not a member of the room it is sent in, or is the user.
    let stranger = MemberId::from(&SigningKey::from_bytes(&[77; 32]).verifying_key());
    assert_eq!(
        prepare(&carrier, &target, stranger, None, BASE).unwrap_err(),
        Refusal::NotMember
    );
    assert_eq!(
        prepare(&carrier, &target, f.me_id(), None, BASE).unwrap_err(),
        Refusal::SelfMessage
    );
    // No key for the room it is sent in.
    let mut keyless = carrier.clone();
    keyless.stored.self_sk = None;
    assert!(matches!(
        prepare(&keyless, &target, them_id(), None, BASE),
        Err(Refusal::Blocked(_))
    ));
}

#[test]
fn a_body_over_the_cap_is_refused_before_it_is_sealed() {
    let (_, carrier) = carrier();
    let big = vec![b'x'; dm_send::BODY_CAP + 1];
    assert!(matches!(
        dm_send::prepare_body(&carrier, them_id(), &big, BASE),
        Err(Refusal::TooLong { .. })
    ));
}

// Accepting ---------------------------------------------------------------------------------------

/// A message to the user in `carrier` from `them()`, carrying `body`, as the room holds it.
fn deliver(
    carrier: &mut LiveRoom,
    body: &[u8],
    ts: u64,
) -> river_core::room_state::direct_messages::PurgeToken {
    let m = compose_direct_message(
        &them(),
        &SigningKey::from_bytes(&[2; 32]).verifying_key(),
        &carrier.stored.owner_vk,
        ts,
        ts,
        body,
    )
    .unwrap();
    let token = m.purge_token();
    carrier.stored.room_state.direct_messages.messages.push(m);
    token
}

/// The invitation `them()` would send for `target`: made by the user's own code from another
/// room's owner, since any invitation is only bytes to the recipient.
fn invitation_for(target: &LiveRoom) -> Invitation {
    create::mint_invitation(target).expect("a key for the room")
}

fn invite_body(
    room: &ed25519_dalek::VerifyingKey,
    invitation: &Invitation,
    note: Option<&str>,
) -> Vec<u8> {
    let mut cbor = Vec::new();
    ciborium::ser::into_writer(invitation, &mut cbor).unwrap();
    encode_body(&DirectMessageBody::Invite(Box::new(InvitePayload {
        room_owner_vk: *room,
        invitation_payload: cbor,
        personal_message: note.map(str::to_owned),
    })))
    .unwrap()
}

/// A room owned by seed 11 where seed 11 is the user: the owner mints in it.
fn owners_room() -> LiveRoom {
    let f = Fixture {
        owner: SigningKey::from_bytes(&[11; 32]),
        me: SigningKey::from_bytes(&[11; 32]),
    };
    LiveRoom::new(f.stored(state(&f, "Book club")))
}

#[test]
fn a_received_invitation_is_read_again_from_the_message_and_agrees_with_what_was_sent() {
    let (_, mut carrier) = carrier();
    let (_, target) = target();
    // The user sends one to `them()`; `them()` is the one who reads it, so build the same message
    // the other way: from them to the user.
    let invitation = invitation_for(&target);
    let body = invite_body(&target.stored.owner_vk, &invitation, Some("hello"));
    let token = deliver(&mut carrier, &body, BASE);
    let opened = open(&carrier, them_id(), &token).expect("accepted for joining");
    assert_eq!(opened.room, invitation.room);
    assert_eq!(
        opened.invitee_signing_key.to_bytes(),
        invitation.invitee_signing_key.to_bytes()
    );
    assert_eq!(opened.invitee, invitation.invitee);
    assert_eq!(opened.room_secrets, invitation.room_secrets);
}

#[test]
fn a_forged_room_id_a_wrong_room_and_a_damaged_invitation_are_refused() {
    let (_, mut carrier) = carrier();
    let (_, target) = target();
    let (_, other) = private_target();
    let invitation = invitation_for(&target);

    // The message names another room than the invitation is for.
    let forged = invite_body(&other.stored.owner_vk, &invitation, None);
    let token = deliver(&mut carrier, &forged, BASE);
    let why = open(&carrier, them_id(), &token).unwrap_err();
    assert!(why.contains("different room"), "{why}");

    // An invitation whose membership is for a room other than the one it names.
    let mut wrong = invitation.clone();
    wrong.room = other.stored.owner_vk;
    let body = invite_body(&other.stored.owner_vk, &wrong, None);
    let token = deliver(&mut carrier, &body, BASE + 1);
    let why = open(&carrier, them_id(), &token).unwrap_err();
    assert!(why.contains("damaged"), "{why}");

    // The key it gives is not the one its membership names.
    let mut mismatched = invitation.clone();
    mismatched.invitee_signing_key = SigningKey::from_bytes(&[99; 32]);
    let body = invite_body(&target.stored.owner_vk, &mismatched, None);
    let token = deliver(&mut carrier, &body, BASE + 2);
    assert!(
        open(&carrier, them_id(), &token)
            .unwrap_err()
            .contains("damaged")
    );

    // Bytes that are not an invitation, and one over the bound that is not even decoded.
    for junk in [
        vec![0u8; 10],
        vec![0xff; 50],
        vec![0u8; MAX_INVITATION_BYTES + 1],
    ] {
        let body = encode_body(&DirectMessageBody::Invite(Box::new(InvitePayload {
            room_owner_vk: target.stored.owner_vk,
            invitation_payload: junk,
            personal_message: None,
        })))
        .unwrap();
        let token = deliver(&mut carrier, &body, BASE + 3);
        let why = open(&carrier, them_id(), &token).unwrap_err();
        assert!(why.contains("damaged"), "{why}");
    }
}

#[test]
fn only_a_verified_invitation_to_the_user_from_the_named_sender_opens() {
    let (_, mut carrier) = carrier();
    let (_, target) = target();
    let invitation = invitation_for(&target);
    let body = invite_body(&target.stored.owner_vk, &invitation, None);
    let token = deliver(&mut carrier, &body, BASE);

    // A different sender named: the message is not theirs.
    let someone = MemberId::from(&carrier.stored.owner_vk);
    assert!(
        open(&carrier, someone, &token)
            .unwrap_err()
            .contains("no longer")
    );
    // A message that is not there.
    let none = river_core::room_state::direct_messages::PurgeToken([9; 16]);
    assert!(open(&carrier, them_id(), &none).is_err());
    // Text is not an invitation.
    let text = deliver(&mut carrier, b"just words", BASE + 1);
    assert!(
        open(&carrier, them_id(), &text)
            .unwrap_err()
            .contains("not an invitation")
    );
    // A sender the room no longer lists cannot be verified.
    let mut gone = carrier.clone();
    gone.stored
        .room_state
        .members
        .members
        .retain(|m| m.member.id() != them_id());
    assert!(open(&gone, them_id(), &token).is_err());
    // A tampered message fails its signature.
    let mut tampered = carrier.clone();
    tampered.stored.room_state.direct_messages.messages[0]
        .message
        .timestamp += 1;
    assert!(open(&tampered, them_id(), &token).is_err());
    // This computer holds no key for the room.
    let mut keyless = carrier.clone();
    keyless.stored.self_sk = None;
    assert!(open(&keyless, them_id(), &token).is_err());
    // A message to someone else cannot be opened by the user.
    let mut elsewhere = carrier.clone();
    let m = compose_direct_message(
        &them(),
        &carrier.stored.owner_vk,
        &carrier.stored.owner_vk,
        BASE + 5,
        BASE + 5,
        &body,
    )
    .unwrap();
    let to_owner = m.purge_token();
    elsewhere.stored.room_state.direct_messages.messages.push(m);
    assert!(open(&elsewhere, them_id(), &to_owner).is_err());
}

#[test]
fn an_envelope_over_the_contracts_size_is_refused() {
    let (_, mut carrier) = carrier();
    let (_, target) = target();
    let invitation = invitation_for(&target);
    let body = invite_body(&target.stored.owner_vk, &invitation, None);
    let token = deliver(&mut carrier, &body, BASE);
    carrier.stored.room_state.direct_messages.messages[0]
        .message
        .ciphertext
        .resize(MAX_DM_CIPHERTEXT_BYTES + 1, 0);
    assert!(open(&carrier, them_id(), &token).is_err());
}

#[test]
fn an_invitation_someone_else_made_opens_and_the_card_claims_nothing_about_who_made_it() {
    // Any member can pass on an invitation they were given; it opens, and what is joined is the
    // room it names. (A key a member of that room already holds is refused when joining.)
    let (_, mut carrier) = carrier();
    let other_owner = owners_room();
    let theirs = create::mint_invitation(&other_owner).unwrap();
    let body = invite_body(&other_owner.stored.owner_vk, &theirs, Some("forwarded"));
    let token = deliver(&mut carrier, &body, BASE);
    let opened = open(&carrier, them_id(), &token).expect("opens");
    assert_eq!(opened.room, other_owner.stored.owner_vk);
}

// Where the invitation can end up ------------------------------------------------------------------

#[test]
fn no_action_printed_names_a_note_a_token_or_an_invitation() {
    let secret_note = "NOTE-NEVER-PRINTED";
    let send = Action::SendInviteDm {
        peer: them_id(),
        target: "room-key".into(),
        note: Some(secret_note.into()),
        ticket: 3,
    };
    let accept = Action::AcceptInviteDm {
        peer: them_id(),
        token: river_core::room_state::direct_messages::PurgeToken([0xab; 16]),
        ticket: 4,
    };
    let printed = format!("{send:?} {accept:?}");
    assert!(!printed.contains(secret_note));
    assert!(!printed.contains("abab"), "{printed}");
    assert!(!printed.contains("171"), "{printed}");
    let pair = format!(
        "{:?}",
        (
            crate::chat::dm_model::DmBody::Invite {
                room: "R".into(),
                room_label: "L".into(),
                note: Some(secret_note.into()),
            },
            crate::chat::dm_model::DmBody::SentInvite {
                note: Some(secret_note.into())
            }
        )
    );
    assert!(!pair.contains(secret_note));
}

/// The part of a source file before its tests, with line endings as `\n`.
fn production(src: &str) -> String {
    let src = src.replace("\r\n", "\n");
    match src.find("#[cfg(test)]\n#[path") {
        Some(i) => src[..i].to_owned(),
        None => src,
    }
}

/// The body of `fn name` in `src`: from its signature to the closing brace at its own indent.
fn function<'a>(src: &'a str, name: &str) -> &'a str {
    let from = src
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("no fn {name}"));
    let rest = &src[from..];
    let indent = src[..from]
        .rsplit('\n')
        .next()
        .map_or(0, |l| l.len() - l.trim_start().len());
    let close = format!("\n{}}}\n", " ".repeat(indent));
    &rest[..rest.find(&close).unwrap_or(rest.len())]
}

/// `river_core::invitation::Invitation` has a hand-written `Debug` that redacts its room secrets
/// and prints no key (`SigningKey`'s own `Debug` does not either), so a stray `{:?}` of one would
/// not print the credential. The scrape below is the second guard: none of the code that holds
/// an invitation prints anything.
#[test]
fn nothing_in_the_invitation_paths_logs_or_prints() {
    let comments_out = |code: &str| -> String {
        code.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let banned = [
        "println!",
        "eprintln!",
        "diag!(",
        "diag::line(",
        "dbg!",
        "tracing::",
        " log::",
        ":?}",
        "{:#?}",
    ];
    for (name, src) in [
        ("rooms/dm_invite.rs", include_str!("dm_invite.rs")),
        ("chat/dm_invite.rs", include_str!("../chat/dm_invite.rs")),
        ("chat/dm_picker.rs", include_str!("../chat/dm_picker.rs")),
    ] {
        let code = comments_out(&production(src));
        for b in banned {
            assert!(!code.contains(b), "{name} has {b}");
        }
    }
    // The functions of shared files that hold or move an invitation.
    for (file, src, names) in [
        (
            "rooms/create.rs",
            include_str!("create.rs"),
            &["mint_invitation"][..],
        ),
        (
            "rooms/work.rs",
            include_str!("work.rs"),
            &["join_invitation", "wipe_credential"][..],
        ),
        (
            "rooms/outbox.rs",
            include_str!("outbox.rs"),
            &["send_invite_dm"][..],
        ),
        (
            "rooms/dm_send.rs",
            include_str!("dm_send.rs"),
            &["prepare_body", "seal"][..],
        ),
    ] {
        let src = production(src);
        for name in names {
            let body = comments_out(function(&src, name));
            for b in banned {
                assert!(!body.contains(b), "{file} fn {name} has {b}");
            }
        }
    }
    // The session's handler has one print: the existing notice about unsaved sent texts.
    let session = production(include_str!("session.rs"));
    let handler = comments_out(function(&session, "invite_dm"));
    assert_eq!(handler.matches("diag!(").count(), 1);
    assert!(handler.contains("too many sent direct messages"));
    assert!(!handler.contains(":?}"));
    // The window's handler of the answers prints nothing either.
    let lib = production(include_str!("../lib.rs"));
    let arm = &lib[lib.find("RoomsUpdate::DmInvite {").expect("the handler")..];
    let arm = comments_out(&arm[..arm.find("RoomsUpdate::Status").unwrap_or(600).min(900)]);
    for b in banned {
        assert!(!arm.contains(b), "lib.rs DmInvite handler has {b}");
    }
}

// The web UI's literals, as an oracle ----------------------------------------------------------------

/// The summary strings the web UI's own tests pin (`ui/src/components/direct_messages.rs`:
/// `a_sent_invite_is_recognisable_in_the_senders_own_thread`, `a_sent_invite_summary_stays_one_line`
/// and `a_multi_line_note_survives_the_round_trip_intact`), copied as literals.
#[test]
fn the_summaries_and_their_reading_are_the_web_uis_literals() {
    use crate::chat::dm_model::DmBody;
    assert_eq!(summary(None), "[Invitation]");
    assert_eq!(summary(Some("come say hi")), "[Invitation] come say hi");
    assert!(!summary(Some("come say hi")).contains('\n'));
    assert_eq!(
        summary(Some("Hey!\n\nThing on Friday.")),
        "[Invitation] Hey!\n\nThing on Friday."
    );
    assert_eq!(
        crate::rooms::dm::sent_body("[Invitation]"),
        DmBody::SentInvite { note: None }
    );
    assert_eq!(
        crate::rooms::dm::sent_body("[Invitation] come say hi"),
        DmBody::SentInvite {
            note: Some("come say hi".into())
        }
    );
    assert_eq!(
        crate::rooms::dm::sent_body("[Invitation] Hey!\n\nThing on Friday."),
        DmBody::SentInvite {
            note: Some("Hey!\n\nThing on Friday.".into())
        }
    );
}

/// `encode_body` of a fixed invitation body, as bytes: `0x80`, then the CBOR of the enum (a map
/// with the variant's name) holding the room's key (32 bytes), the payload bytes and the note.
/// Fixed here so a change in river-core's encoding, which the web UI and `riverctl` read, fails.
#[test]
fn the_invite_body_has_one_fixed_wire_form() {
    let body = DirectMessageBody::Invite(Box::new(InvitePayload {
        room_owner_vk: SigningKey::from_bytes(&[7; 32]).verifying_key(),
        invitation_payload: vec![1, 2, 3],
        personal_message: Some("hi".into()),
    }));
    // 80 | a1 "Invite" | a3 "room_owner_vk" 5820<key> "invitation_payload" 83 01 02 03 (an array of
    // integers: how serde writes a `Vec<u8>` here) "personal_message" "hi".
    let expected = concat!(
        "80a166496e76697465a36d726f6f6d5f6f776e65725f766b5820",
        "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c",
        "72696e7669746174696f6e5f7061796c6f6164830102037",
        "0706572736f6e616c5f6d657373616765626869",
    );
    let bytes = encode_body(&body).unwrap();
    let got: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(got, expected);
    assert_eq!(decode_body(&bytes).unwrap(), body);
}
