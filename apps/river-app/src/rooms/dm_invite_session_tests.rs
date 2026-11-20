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

/// A real room (owner seed 60, "Open") and its state, which an invitation made by its owner is for.
fn real_room() -> (LiveRoom, ChatRoomStateV1) {
    let owner = SigningKey::from_bytes(&[60; 32]);
    let plan = crate::rooms::create::new_room(owner.clone(), "Open", "Olive", false).unwrap();
    let f = Fixture {
        owner: owner.clone(),
        me: owner,
    };
    (
        LiveRoom::new(f.stored(plan.room.room_state.clone())),
        plan.room.room_state,
    )
}

/// An invitation made by `invited`'s owner for `invited`, sent to the user by the carrier room's
/// owner (seed 1) and held by the carrier room. Returns the message's token.
fn receive_for(rooms: &mut Rooms, f: &Fixture, invited: &LiveRoom) -> PurgeToken {
    let invitation = crate::rooms::create::mint_invitation(invited).unwrap();
    let mut cbor = Vec::new();
    ciborium::ser::into_writer(&invitation, &mut cbor).unwrap();
    let body = encode_body(&DirectMessageBody::Invite(Box::new(InvitePayload {
        room_owner_vk: invited.stored.owner_vk,
        invitation_payload: cbor,
        personal_message: None,
    })))
    .unwrap();
    let carrier = &mut rooms.list[0];
    let m = compose_direct_message(
        &f.owner,
        &f.me.verifying_key(),
        &carrier.stored.owner_vk,
        1_700_000_200,
        1_700_000_200,
        &body,
    )
    .unwrap();
    let token = m.purge_token();
    carrier.stored.room_state.direct_messages.messages.push(m);
    token
}

/// An invitation to a room owned by seed 21, from that room's owner, sent to the user by the
/// carrier room's owner (seed 1), and held by the carrier room. Returns the message's token and
/// the invited room's key.
fn receive(rooms: &mut Rooms, f: &Fixture) -> (PurgeToken, String) {
    let g = Fixture {
        owner: SigningKey::from_bytes(&[21; 32]),
        me: SigningKey::from_bytes(&[21; 32]),
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

// The new-member rule, through the session -------------------------------------------------------

fn answer_get(rooms: &mut Rooms, owner: &ed25519_dalek::VerifyingKey, state: &ChatRoomStateV1) {
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(state, &mut bytes).unwrap();
    assert!(rooms.outbox.work.on_contract(
        &freenet_stdlib::client_api::ContractResponse::GetResponse {
            key: crate::rooms::create::contract_key(owner),
            contract: None,
            state: freenet_stdlib::prelude::WrappedState::new(bytes),
        }
    ));
}

#[test]
fn accepting_an_invitation_for_a_key_the_room_holds_fails_for_good_and_wipes_it() {
    for (holds, failed) in [(true, true), (false, false)] {
        let (mut rooms, f, carrier, _target, peer) = two_rooms();
        let (invited, mut state) = real_room();
        let token = receive_for(&mut rooms, &f, &invited);
        assert_eq!(
            take(&mut rooms, accept(&carrier, &peer, token, 1)),
            vec![(1, Ok(()))],
            "queued either way: the room is not read yet"
        );
        let invitation = crate::rooms::dm_invite::open(
            &rooms.list[0],
            crate::rooms::dm::peer_from_key(&peer).unwrap(),
            &token,
        )
        .unwrap();
        if holds {
            // The room already has the invitee as a member.
            state.members.members.push(invitation.invitee.clone());
        }
        let now = std::time::Instant::now();
        let get = rooms.outbox.work.next_request(false, now).expect("a GET");
        assert!(matches!(
            get,
            freenet_stdlib::client_api::ClientRequest::ContractOp(
                freenet_stdlib::client_api::ContractRequest::Get { .. }
            )
        ));
        answer_get(&mut rooms, &invited.stored.owner_vk, &state);
        let views = rooms.outbox.work.views();
        let refused = views.iter().any(|v| {
            matches!(&v.state, crate::rooms::work::WorkState::Failed { retry: false, why } if why.contains("member of the room already"))
        });
        assert_eq!(refused, failed, "{views:?}");
        if holds {
            // Nothing was written for it, and the credential is gone from the task.
            assert!(rooms.outbox.work.join_credentials_wiped());
            while let Some(req) = rooms.outbox.work.next_request(false, now) {
                assert!(
                    !matches!(
                        req,
                        freenet_stdlib::client_api::ClientRequest::ContractOp(
                            freenet_stdlib::client_api::ContractRequest::Update { .. }
                        )
                    ),
                    "no join was written"
                );
            }
        }
    }
}

#[test]
fn a_new_accept_replaces_a_join_that_failed_for_good_and_does_not_pile_up_credentials() {
    let (mut rooms, f, carrier, _target, peer) = two_rooms();
    let (invited, mut state) = real_room();
    let token = receive_for(&mut rooms, &f, &invited);
    assert_eq!(
        take(&mut rooms, accept(&carrier, &peer, token, 1)),
        vec![(1, Ok(()))]
    );
    let invitation = crate::rooms::dm_invite::open(
        &rooms.list[0],
        crate::rooms::dm::peer_from_key(&peer).unwrap(),
        &token,
    )
    .unwrap();
    state.members.members.push(invitation.invitee.clone());
    rooms
        .outbox
        .work
        .next_request(false, std::time::Instant::now())
        .unwrap();
    answer_get(&mut rooms, &invited.stored.owner_vk, &state);
    assert_eq!(rooms.outbox.work.views().len(), 1);
    // Pressing Accept again starts a new join and the failed one goes.
    assert_eq!(
        take(&mut rooms, accept(&carrier, &peer, token, 2)),
        vec![(2, Ok(()))]
    );
    let views = rooms.outbox.work.views();
    assert_eq!(views.len(), 1, "{views:?}");
    assert!(matches!(
        views[0].state,
        crate::rooms::work::WorkState::Working
    ));
}

// Where the invitation can end up ------------------------------------------------------------------

/// A private room (secret `SECRET`) owned by seed 12 where the user is a member, so its invitation
/// carries room secrets.
fn private_room() -> LiveRoom {
    let g = Fixture {
        owner: SigningKey::from_bytes(&[12; 32]),
        me: SigningKey::from_bytes(&[2; 32]),
    };
    LiveRoom::new(g.stored(g.private_state()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn the_invitation_is_in_nothing_the_session_keeps_prints_or_saves() {
    const NOTE: &str = "SENTINEL-NOTE-7";
    let (mut rooms, f, carrier, _target, peer) = two_rooms();
    let private = private_room();
    let target = private.key();
    rooms.network_seen.insert(private.stored.owner_vk);
    rooms.list[1] = private;
    let command = send_invite(&carrier, &peer, &target, Some(NOTE), 9);
    let command_debug = format!("{command:?}");
    let answers = take(&mut rooms, command);
    assert_eq!(answers, vec![(9, Ok(()))]);

    // What went out: the invitation, found by opening the message as its recipient.
    let mut changed = false;
    let delta = rooms
        .outbox
        .next_update(&rooms.list[0], std::time::Instant::now(), &mut changed)
        .expect("the invitation goes out");
    let message = delta.direct_messages.unwrap().new_messages.remove(0);
    let opened =
        river_core::room_state::direct_messages::open_direct_message(&f.owner, &message).unwrap();
    let Ok(DirectMessageBody::Invite(payload)) =
        river_core::room_state::dm_body::decode_body(&opened)
    else {
        panic!("an invitation")
    };
    let invitation: river_core::invitation::Invitation =
        ciborium::de::from_reader(&payload.invitation_payload[..]).unwrap();
    let key = invitation.invitee_signing_key.to_bytes();
    let secret = crate::rooms::room_tests::SECRET;
    assert!(
        invitation.room_secrets.iter().any(|(_, s)| *s == secret),
        "a private room's invitation carries its secret"
    );

    // Everything the session can print.
    let owner = rooms.list[0].stored.owner_vk;
    let printed = [
        command_debug,
        format!("{:?}", rooms.outbox.dm_rows(&owner)),
        format!("{:?}", rooms.dm_cache),
        format!("{:?}", rooms.dm_snapshot()),
        format!("{:?}", rooms.outbox.work.views()),
        format!("{answers:?}"),
        format!(
            "{:?}",
            RoomsUpdate::DmInvite {
                room: carrier.clone(),
                peer: peer.clone(),
                ticket: 9,
                result: Ok(()),
            }
        ),
    ]
    .join("\n");
    let list = |b: &[u8]| {
        b.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    for needle in [
        bs58::encode(key).into_string(),
        hex(&key),
        list(&key),
        hex(&secret),
        list(&secret),
        bs58::encode(secret).into_string(),
    ] {
        assert!(!printed.contains(&needle), "printed: {needle}");
    }
    assert!(!printed.contains(NOTE), "no note in anything printed");

    // What would be saved to the chat delegate: the summary and no invitation.
    let ops: Vec<_> = rooms
        .dm_cache
        .unsaved()
        .into_iter()
        .map(|(key, sent, _)| crate::rooms::dm_store::DmOp::Add {
            key,
            sender: sent.sender,
            timestamp: sent.timestamp,
            text: crate::rooms::dm_cache::Plaintext::new(sent.plaintext.as_str().to_owned()),
        })
        .collect();
    assert_eq!(ops.len(), 1);
    let stored = crate::rooms::dm_store::merge(None, false, &ops)
        .unwrap()
        .bytes
        .expect("a value to store");
    assert!(contains(&stored, format!("[Invitation] {NOTE}").as_bytes()));
    for needle in [&key[..], &secret[..], &payload.invitation_payload[..]] {
        assert!(!contains(&stored, needle), "the store holds the invitation");
    }
    // And the entry reads back as the summary.
    let entry = rooms
        .dm_cache
        .unsaved()
        .remove(0)
        .1
        .plaintext
        .as_str()
        .to_owned();
    assert_eq!(entry, format!("[Invitation] {NOTE}"));
}
