//! Invitations in direct messages as the session takes them: asked for by command, answered by
//! ticket, held back while River has moved on, and joined through the same join work a pasted
//! invite code uses.

use ed25519_dalek::SigningKey;
use river_core::room_state::ChatRoomStateV1;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::direct_messages::{PurgeToken, compose_direct_message};
use river_core::room_state::dm_body::{DirectMessageBody, InvitePayload, encode_body};
use river_core::room_state::member::MemberId;
use river_core::room_state::privacy::RoomDisplayMetadata;

use super::*;
use crate::chat::dm_model::DmBody;
use crate::rooms::generation::{UPDATE_REQUIRED_SEND, UPDATE_REQUIRED_WORK};
use crate::rooms::room_tests::Fixture;

/// The session's rooms: the fixture's private room "Secret garden" (the room messages go in,
/// where the owner, seed 1, is the one to write to), and a second room owned by seed 11, where
/// the user is the owner's guest.
fn two_rooms() -> (Rooms, Fixture, String, String, String) {
    let f = Fixture::new();
    let mut rooms = Rooms::default();
    let carrier = LiveRoom::new(f.stored(f.private_state()));
    let carrier_key = carrier.key();
    rooms.network_seen.insert(carrier.stored.owner_vk);
    rooms.list.push(carrier);

    let g = Fixture {
        owner: SigningKey::from_bytes(&[11; 32]),
        me: SigningKey::from_bytes(&[2; 32]),
    };
    let mut state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: g.owner_id(),
                display: RoomDisplayMetadata::public("Book club".into(), None),
                ..Configuration::default()
            },
            &g.owner,
        ),
        ..Default::default()
    };
    state
        .members
        .members
        .push(river_core::room_state::member::AuthorizedMember::new(
            river_core::room_state::member::Member {
                owner_member_id: g.owner_id(),
                invited_by: g.owner_id(),
                member_vk: g.me.verifying_key(),
            },
            &g.owner,
        ));
    state.recent_messages.messages = vec![g.message(
        &g.me,
        1_700_000_000,
        river_core::room_state::message::RoomMessageBody::public("hi".into()),
    )];
    let target = LiveRoom::new(g.stored(state));
    let target_key = target.key();
    rooms.network_seen.insert(target.stored.owner_vk);
    rooms.list.push(target);
    rooms.loaded = true;
    rooms.complete = true;
    let peer = crate::rooms::dm::peer_key(f.owner_id());
    (rooms, f, carrier_key, target_key, peer)
}

fn take(rooms: &mut Rooms, command: Command) -> Vec<(u64, Result<(), String>)> {
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(command).unwrap();
    rooms.take_commands(&rx);
    std::mem::take(&mut rooms.dm_invite_results)
        .into_iter()
        .map(|r| (r.ticket, r.result))
        .collect()
}

fn send_invite(room: &str, peer: &str, target: &str, note: Option<&str>, ticket: u64) -> Command {
    Command {
        room: room.to_owned(),
        action: Action::SendInviteDm {
            peer: crate::rooms::dm::peer_from_key(peer).expect("a member key"),
            target: target.to_owned(),
            note: note.map(str::to_owned),
            ticket,
        },
    }
}

fn accept(room: &str, peer: &str, token: PurgeToken, ticket: u64) -> Command {
    Command {
        room: room.to_owned(),
        action: Action::AcceptInviteDm {
            peer: crate::rooms::dm::peer_from_key(peer).expect("a member key"),
            token,
            ticket,
        },
    }
}

/// An invitation to a room owned by seed 21, from that room's owner, sent to the user by the
/// carrier room's owner (seed 1), and held by the carrier room. Returns the message's token and
/// the invited room's key.
fn receive(rooms: &mut Rooms, f: &Fixture) -> (PurgeToken, String) {
    // The room's owner is seed 21 and the invitation is made by seed 1, who sends it.
    let g = Fixture {
        owner: SigningKey::from_bytes(&[21; 32]),
        me: SigningKey::from_bytes(&[1; 32]),
    };
    let state = ChatRoomStateV1 {
        configuration: AuthorizedConfigurationV1::new(
            Configuration {
                owner_member_id: g.owner_id(),
                display: RoomDisplayMetadata::public("Chess".into(), None),
                ..Configuration::default()
            },
            &g.owner,
        ),
        ..Default::default()
    };
    let invited = LiveRoom::new(g.stored(state));
    let invitation = crate::rooms::create::mint_invitation(&invited).unwrap();
    let mut cbor = Vec::new();
    ciborium::ser::into_writer(&invitation, &mut cbor).unwrap();
    let body = encode_body(&DirectMessageBody::Invite(Box::new(InvitePayload {
        room_owner_vk: invited.stored.owner_vk,
        invitation_payload: cbor,
        personal_message: Some("join us".into()),
    })))
    .unwrap();
    let carrier = &mut rooms.list[0];
    let m = compose_direct_message(
        &f.owner,
        &f.me.verifying_key(),
        &carrier.stored.owner_vk,
        1_700_000_100,
        1_700_000_100,
        &body,
    )
    .unwrap();
    let token = m.purge_token();
    carrier.stored.room_state.direct_messages.messages.push(m);
    (token, invited.key())
}

// Sending ---------------------------------------------------------------------------------------

#[test]
fn an_invitation_is_queued_answered_and_shown_in_its_thread_as_sent() {
    let (mut rooms, _f, carrier, target, peer) = two_rooms();
    assert_eq!(
        take(
            &mut rooms,
            send_invite(&carrier, &peer, &target, Some("come"), 7)
        ),
        vec![(7, Ok(()))]
    );
    assert!(!rooms.outbox.is_empty());
    let snap = rooms.dm_snapshot().expect("a new thread");
    let m = &snap.threads[0].messages[0];
    assert!(m.outgoing);
    assert_eq!(
        m.body,
        DmBody::SentInvite {
            note: Some("come".into())
        }
    );
    assert!(m.pending.is_some());

    // What is kept for the row is the summary and only that.
    let owner = rooms.list[0].stored.owner_vk;
    let recipient = crate::rooms::dm::peer_from_key(&peer).unwrap();
    let token = crate::rooms::dm::token_from_hex(&m.key).unwrap();
    assert_eq!(
        rooms.dm_cache.plaintext(&owner, recipient, &token),
        Some("[Invitation] come")
    );
}

#[test]
fn a_send_the_session_cannot_take_is_answered_with_why_and_leaves_nothing() {
    let (mut rooms, _f, carrier, target, peer) = two_rooms();
    rooms.update_required = true;
    assert_eq!(
        take(&mut rooms, send_invite(&carrier, &peer, &target, None, 1)),
        vec![(1, Err(UPDATE_REQUIRED_SEND.to_owned()))]
    );
    assert!(rooms.outbox.is_empty());
    rooms.update_required = false;

    // A room to invite to that is not listed.
    assert_eq!(
        take(
            &mut rooms,
            send_invite(&carrier, &peer, "no-such-room", None, 2)
        ),
        vec![(2, Err("That room is no longer here.".to_owned()))]
    );
    // The room it is sent in is gone.
    assert_eq!(
        take(&mut rooms, send_invite("gone", &peer, &target, None, 3)),
        vec![(3, Err("This room is no longer here.".to_owned()))]
    );
    // The same room; a note that hides text.
    assert!(matches!(
        &take(&mut rooms, send_invite(&carrier, &peer, &carrier, None, 4))[..],
        [(4, Err(_))]
    ));
    assert!(matches!(
        &take(
            &mut rooms,
            send_invite(&carrier, &peer, &target, Some("a\u{202E}b"), 5)
        )[..],
        [(5, Err(_))]
    ));
    assert!(rooms.outbox.is_empty());
    // One that may only be unread yet waits.
    rooms.complete = false;
    assert!(take(&mut rooms, send_invite("gone", &peer, &target, None, 6)).is_empty());
    assert_eq!(rooms.outbox.waiting.len(), 1);
}

// Accepting --------------------------------------------------------------------------------------

#[test]
fn accepting_reads_the_invitation_and_starts_the_join_once() {
    let (mut rooms, f, carrier, _target, peer) = two_rooms();
    let (token, invited) = receive(&mut rooms, &f);
    assert_eq!(
        take(&mut rooms, accept(&carrier, &peer, token, 1)),
        vec![(1, Ok(()))]
    );
    let work = rooms.outbox.work.views();
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].room.as_deref(), Some(invited.as_str()));
    assert_eq!(work[0].what, "Joining a room");
    assert!(matches!(
        work[0].state,
        crate::rooms::work::WorkState::Working
    ));

    // A second Accept while the first is under way is refused, and starts nothing.
    assert_eq!(
        take(&mut rooms, accept(&carrier, &peer, token, 2)),
        vec![(2, Err("You're already joining this room.".to_owned()))]
    );
    assert_eq!(rooms.outbox.work.views().len(), 1);
}

#[test]
fn accepting_a_room_you_are_in_or_with_river_moved_on_or_a_forgery_starts_nothing() {
    let (mut rooms, f, carrier, _target, peer) = two_rooms();
    let (token, invited) = receive(&mut rooms, &f);

    // River has moved on.
    rooms.update_required = true;
    assert_eq!(
        take(&mut rooms, accept(&carrier, &peer, token, 1)),
        vec![(1, Err(UPDATE_REQUIRED_WORK.to_owned()))]
    );
    rooms.update_required = false;

    // The message is not there.
    assert!(matches!(
        &take(&mut rooms, accept(&carrier, &peer, PurgeToken([5; 16]), 2))[..],
        [(2, Err(why))] if why.contains("no longer")
    ));
    // The room gone.
    assert_eq!(
        take(&mut rooms, accept("gone", &peer, token, 3)),
        vec![(3, Err("This room is no longer here.".to_owned()))]
    );
    assert!(rooms.outbox.work.views().is_empty());

    // Already in the invited room: the invitation is not used.
    let g = Fixture {
        owner: SigningKey::from_bytes(&[21; 32]),
        me: SigningKey::from_bytes(&[2; 32]),
    };
    let existing = LiveRoom::new(g.stored(ChatRoomStateV1::default()));
    assert_eq!(existing.key(), invited);
    rooms.list.push(existing);
    let answers = take(&mut rooms, accept(&carrier, &peer, token, 4));
    assert!(
        matches!(&answers[..], [(4, Err(why))] if why.contains("already a member")),
        "{answers:?}"
    );
    assert!(rooms.outbox.work.views().is_empty());

    // A message from a different member than the one named is not that member's.
    let other = crate::rooms::dm::peer_key(MemberId::from(&f.me.verifying_key()));
    let answers = take(&mut rooms, accept(&carrier, &other, token, 5));
    assert!(matches!(&answers[..], [(5, Err(_))]), "{answers:?}");
}
