//! Building, showing and settling what the user sends, against rooms built the way River builds
//! them (`room_tests::Fixture`).

use std::time::{Duration, Instant, SystemTime};

use ed25519_dalek::SigningKey;
use river_core::ecies::decrypt_with_symmetric_key;
use river_core::room_state::ChatRoomStateV1;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::content::{
    ActionContentV1, CONTENT_TYPE_ACTION, CONTENT_TYPE_REPLY, CONTENT_TYPE_TEXT, ReplyContentV1,
    TextContentV1,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::message::{AuthorizedMessageV1, RoomMessageBody};
use river_core::room_state::privacy::{RoomDisplayMetadata, SealedBytes};

use super::*;
use crate::rooms::room::{LiveRoom, message_key};
use crate::rooms::room_tests::{Fixture, SECRET, add_secret};

fn minutes(t: SystemTime) -> u32 {
    (t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() / 60) as u32
}

fn private_room(f: &Fixture) -> LiveRoom {
    LiveRoom::new(f.stored(f.private_state()))
}

/// A public room with `me` a member, and one message from the owner and one from `me`.
fn public_state(f: &Fixture) -> ChatRoomStateV1 {
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
    state.members.members.push(admitted(f));
    let now = SystemTime::now();
    state.recent_messages.messages = vec![
        f.message(
            &f.owner,
            secs(now) - 100,
            RoomMessageBody::public("from the owner".into()),
        ),
        f.message(
            &f.me,
            secs(now) - 50,
            RoomMessageBody::public("from me".into()),
        ),
    ];
    state
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
}

fn admitted(f: &Fixture) -> AuthorizedMember {
    AuthorizedMember::new(
        Member {
            owner_member_id: f.owner_id(),
            invited_by: f.owner_id(),
            member_vk: f.me.verifying_key(),
        },
        &f.owner,
    )
}

fn public_room(f: &Fixture) -> LiveRoom {
    LiveRoom::new(f.stored(public_state(f)))
}

fn key_of(room: &LiveRoom, author: &SigningKey) -> String {
    let id = MemberId::from(&author.verifying_key());
    room.stored
        .room_state
        .recent_messages
        .messages
        .iter()
        .find(|m| m.message.author == id && !m.message.content.is_action())
        .map(|m| message_key(&m.id()))
        .unwrap()
}

/// Send `action`, then take the update it makes.
fn sent(outbox: &mut Outbox, room: &LiveRoom, action: Action) -> Vec<AuthorizedMessageV1> {
    assert!(outbox.command(room, action));
    let mut changed = false;
    let delta = outbox
        .next_update(room, Instant::now(), &mut changed)
        .expect("an update goes out");
    delta.recent_messages.unwrap()
}

fn open(body: &RoomMessageBody, secret: &[u8; 32]) -> (u32, u32, Vec<u8>) {
    match body {
        RoomMessageBody::Private {
            content_type,
            ciphertext,
            nonce,
            secret_version,
            ..
        } => (
            *content_type,
            *secret_version,
            decrypt_with_symmetric_key(secret, ciphertext, nonce).unwrap(),
        ),
        RoomMessageBody::Public { .. } => panic!("a private room's body must be sealed"),
    }
}

#[test]
fn a_public_message_is_the_web_uis_body_signed_with_the_rooms_key() {
    let f = Fixture::new();
    let room = public_room(&f);
    let mut outbox = Outbox::default();
    let messages = sent(
        &mut outbox,
        &room,
        Action::Send {
            text: "hello".into(),
            reply_to: None,
        },
    );
    let [m] = messages.as_slice() else {
        panic!("one message");
    };
    assert_eq!(m.message.content, RoomMessageBody::public("hello".into()));
    assert_eq!(m.message.author, f.me_id());
    assert_eq!(m.message.room_owner, f.owner_id());
    assert!(m.validate(&f.me.verifying_key()).is_ok());
}

#[test]
fn a_private_message_is_sealed_under_the_current_secret() {
    let f = Fixture::new();
    let room = private_room(&f);
    let mut outbox = Outbox::default();
    let messages = sent(
        &mut outbox,
        &room,
        Action::Send {
            text: "secret hello".into(),
            reply_to: None,
        },
    );
    let (ty, version, plain) = open(&messages[0].message.content, &SECRET);
    assert_eq!((ty, version), (CONTENT_TYPE_TEXT, 0));
    assert_eq!(TextContentV1::decode(&plain).unwrap().text, "secret hello");
}

#[test]
fn what_is_sent_shows_at_once_marked_until_the_node_takes_it() {
    let f = Fixture::new();
    let room = public_room(&f);
    let mut outbox = Outbox::default();
    let messages = sent(
        &mut outbox,
        &room,
        Action::Send {
            text: "on its way".into(),
            reply_to: None,
        },
    );
    let view = outbox.view(&room, &minutes);
    let shown = view.messages.last().unwrap();
    assert_eq!(shown.text, "on its way");
    assert_eq!(shown.author, view.me);
    assert_eq!(shown.pending.as_ref().map(|p| p.failed.clone()), Some(None));
    // The room's own copy is not touched until then.
    assert!(
        room.message_by_key(&message_key(&messages[0].id()))
            .is_none()
    );

    // Taken: the entry is done and its messages are handed back to apply.
    let taken = outbox.answered(&room.stored.owner_vk, Ok(())).unwrap();
    assert_eq!(taken, messages);
    assert!(outbox.is_empty());
}

#[test]
fn a_refused_send_stays_on_screen_with_the_reason_and_can_be_retried() {
    let f = Fixture::new();
    let room = public_room(&f);
    let mut outbox = Outbox::default();
    let first = sent(
        &mut outbox,
        &room,
        Action::Send {
            text: "try me".into(),
            reply_to: None,
        },
    );
    assert!(
        outbox
            .answered(&room.stored.owner_vk, Err("no room".into()))
            .is_none()
    );
    let view = outbox.view(&room, &minutes);
    let pending = view.messages.last().unwrap().pending.clone().unwrap();
    assert_eq!(
        pending.failed.as_deref(),
        Some("Freenet refused it: no room")
    );
    // Nothing goes out again by itself.
    let mut changed = false;
    assert!(
        outbox
            .next_update(&room, Instant::now(), &mut changed)
            .is_none()
    );

    std::thread::sleep(Duration::from_millis(5));
    assert!(outbox.command(
        &room,
        Action::Retry {
            entry: pending.entry
        }
    ));
    let again = outbox
        .next_update(&room, Instant::now(), &mut changed)
        .unwrap()
        .recent_messages
        .unwrap();
    assert_eq!(again.len(), 1);
    // A retry is a new send, stamped again.
    assert_ne!(again[0].id(), first[0].id());

    assert!(outbox.command(
        &room,
        Action::Discard {
            entry: pending.entry
        }
    ));
    assert!(outbox.is_empty());
}

#[test]
fn one_update_per_room_is_out_at_a_time_and_a_lost_one_goes_again_unchanged() {
    let f = Fixture::new();
    let room = public_room(&f);
    let mut outbox = Outbox::default();
    let first = sent(
        &mut outbox,
        &room,
        Action::Send {
            text: "one".into(),
            reply_to: None,
        },
    );
    assert!(outbox.command(
        &room,
        Action::Send {
            text: "two".into(),
            reply_to: None
        }
    ));
    let mut changed = false;
    assert!(
        outbox
            .next_update(&room, Instant::now(), &mut changed)
            .is_none(),
        "the second waits for the first's answer"
    );
    // Both show, in order.
    let texts: Vec<String> = outbox
        .view(&room, &minutes)
        .messages
        .iter()
        .map(|m| m.text.clone())
        .collect();
    assert!(texts.ends_with(&["one".to_owned(), "two".to_owned()]));

    // The connection drops: the same signed message goes again.
    outbox.requeue();
    let again = outbox
        .next_update(&room, Instant::now(), &mut changed)
        .unwrap()
        .recent_messages
        .unwrap();
    assert_eq!(again, first);
}

#[test]
fn an_unanswered_update_fails_and_the_network_bringing_it_in_settles_it() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = Outbox::default();
    let messages = sent(
        &mut outbox,
        &room,
        Action::Send {
            text: "late".into(),
            reply_to: None,
        },
    );
    assert!(!outbox.expire(Duration::from_secs(60)));
    assert!(outbox.expire(Duration::ZERO));
    let view = outbox.view(&room, &minutes);
    assert!(
        view.messages
            .last()
            .unwrap()
            .pending
            .as_ref()
            .unwrap()
            .failed
            .as_deref()
            .unwrap()
            .starts_with("Freenet didn't confirm it in time")
    );
    // The network brings it in after all.
    room.apply_delta(river_core::room_state::ChatRoomStateV1Delta {
        recent_messages: Some(messages),
        ..Default::default()
    })
    .unwrap();
    assert!(outbox.reconcile(&room));
    assert!(outbox.is_empty());
    assert!(
        outbox
            .view(&room, &minutes)
            .messages
            .iter()
            .all(|m| m.pending.is_none())
    );
}

#[test]
fn a_reply_carries_the_target_its_author_and_a_preview() {
    let f = Fixture::new();
    let room = private_room(&f);
    let target = key_of(&room, &f.owner);
    let mut outbox = Outbox::default();
    let messages = sent(
        &mut outbox,
        &room,
        Action::Send {
            text: "answering".into(),
            reply_to: Some(target.clone()),
        },
    );
    let (ty, _, plain) = open(&messages[0].message.content, &SECRET);
    assert_eq!(ty, CONTENT_TYPE_REPLY);
    let reply = ReplyContentV1::decode(&plain).unwrap();
    assert_eq!(message_key(&reply.target_message_id), target);
    assert_eq!(reply.text, "answering");
    assert_eq!(reply.target_author_name, "Olive");
    // The target as shown: edited, with the mention as a name.
    assert_eq!(reply.target_content_preview, "Hello everyone");
    let view = outbox.view(&room, &minutes);
    let shown = view.messages.last().unwrap();
    assert!(shown.reply_to.is_some());
}

#[test]
fn edits_and_deletions_are_sealed_actions_on_your_own_messages_only() {
    let f = Fixture::new();
    let room = private_room(&f);
    let mine = key_of(&room, &f.me);
    let theirs = key_of(&room, &f.owner);
    let mut outbox = Outbox::default();
    let edit = sent(
        &mut outbox,
        &room,
        Action::Edit {
            target: mine.clone(),
            text: "Hi again".into(),
        },
    );
    let (ty, _, plain) = open(&edit[0].message.content, &SECRET);
    assert_eq!(ty, CONTENT_TYPE_ACTION);
    let action = ActionContentV1::decode(&plain).unwrap();
    assert_eq!(message_key(&action.target), mine);
    assert_eq!(action.edit_payload().unwrap().new_text, "Hi again");
    // Shown edited at once.
    let view = outbox.view(&room, &minutes);
    let shown = view.messages.iter().find(|m| m.key == mine).unwrap();
    assert_eq!(shown.text, "Hi again");
    assert!(shown.edited);
    outbox.answered(&room.stored.owner_vk, Ok(())).unwrap();

    // Someone else's message: refused here, said above the composer, nothing sent.
    for action in [
        Action::Edit {
            target: theirs.clone(),
            text: "mine now".into(),
        },
        Action::Delete {
            target: theirs.clone(),
        },
    ] {
        assert!(outbox.command(&room, action));
    }
    let mut changed = false;
    assert!(
        outbox
            .next_update(&room, Instant::now(), &mut changed)
            .is_none()
    );
    let view = outbox.view(&room, &minutes);
    assert_eq!(view.notices.len(), 2);
    assert!(view.notices[0].text.contains("only change your own"));
    assert!(!view.notices[0].retry);
    let shown = view.messages.iter().find(|m| m.key == theirs).unwrap();
    assert_eq!(shown.text, "Hello everyone");

    let mut outbox = Outbox::default();
    let delete = sent(
        &mut outbox,
        &room,
        Action::Delete {
            target: mine.clone(),
        },
    );
    let (_, _, plain) = open(&delete[0].message.content, &SECRET);
    assert_eq!(
        ActionContentV1::decode(&plain).unwrap(),
        ActionContentV1::delete(room.message_by_key(&mine).unwrap().id())
    );
    assert!(
        outbox
            .view(&room, &minutes)
            .messages
            .iter()
            .all(|m| m.key != mine),
        "a deletion shows at once"
    );
}

/// The emoji(s) of `me`'s reactions on `target` in the view.
fn reactions_by_me(outbox: &Outbox, room: &LiveRoom, target: &str) -> Vec<String> {
    let view = outbox.view(room, &minutes);
    let m = view.messages.iter().find(|m| m.key == target).unwrap();
    m.reactions
        .iter()
        .filter(|r| r.by.contains(&view.me))
        .map(|r| r.emoji.clone())
        .collect()
}

#[test]
fn reacting_toggles_and_replaces_counting_reactions_still_on_their_way() {
    let f = Fixture::new();
    let room = public_room(&f);
    let target = key_of(&room, &f.owner);
    let mut outbox = Outbox::default();
    let react = |emoji: &str| Action::React {
        target: target.clone(),
        emoji: emoji.into(),
    };
    assert!(outbox.command(&room, react("👍")));
    assert_eq!(reactions_by_me(&outbox, &room, &target), ["👍"]);
    // Still on its way, a different emoji replaces it: a removal and an addition.
    assert!(outbox.command(&room, react("😂")));
    assert_eq!(reactions_by_me(&outbox, &room, &target), ["😂"]);
    // The same again removes it.
    assert!(outbox.command(&room, react("😂")));
    assert!(reactions_by_me(&outbox, &room, &target).is_empty());

    let mut changed = false;
    let mut kinds = Vec::new();
    while let Some(delta) = outbox.next_update(&room, Instant::now(), &mut changed) {
        let actions: Vec<ActionContentV1> = delta
            .recent_messages
            .unwrap()
            .iter()
            .map(|m| match &m.message.content {
                RoomMessageBody::Public { data, .. } => ActionContentV1::decode(data).unwrap(),
                _ => panic!("a public room's action is public"),
            })
            .collect();
        kinds.push(
            actions
                .iter()
                .map(|a| {
                    let r = a.reaction_payload().unwrap();
                    format!("{}{}", if a.action_type == 3 { "+" } else { "-" }, r.emoji)
                })
                .collect::<Vec<_>>(),
        );
        outbox.answered(&room.stored.owner_vk, Ok(()));
    }
    assert_eq!(kinds, [vec!["+👍"], vec!["-👍", "+😂"], vec!["-😂"]]);
}

#[test]
fn who_cannot_send_is_told_why_and_nothing_goes_out() {
    let f = Fixture::new();
    let send = Action::Send {
        text: "hi".into(),
        reply_to: None,
    };

    // No key on this computer.
    let mut stored = f.stored(public_state(&f));
    stored.self_sk = None;
    stored.self_vk = Some(f.me.verifying_key());
    let no_key = LiveRoom::new(stored);
    assert_eq!(participation(&no_key), Err(Blocked::NoKey));

    // Not a member, and nothing to rejoin with.
    let mut state = public_state(&f);
    state.members.members.clear();
    state.recent_messages.messages.truncate(1);
    let not_member = LiveRoom::new(f.stored(state));
    assert_eq!(participation(&not_member), Err(Blocked::NotMember));

    // A private room whose current secret this member does not have.
    let mut state = f.private_state();
    state.secrets.encrypted_secrets.clear();
    let no_secret = LiveRoom::new(f.stored(state));
    assert_eq!(participation(&no_secret), Err(Blocked::NoSecret));

    for room in [&no_key, &not_member, &no_secret] {
        let mut outbox = Outbox::default();
        outbox.command(room, send.clone());
        let mut changed = false;
        assert!(
            outbox
                .next_update(room, Instant::now(), &mut changed)
                .is_none()
        );
        let view = outbox.view(room, &minutes);
        assert!(view.send_block.is_some());
        assert_eq!(view.notices.len(), 1, "the refusal is said, not dropped");
    }

    assert_eq!(participation(&public_room(&f)), Ok(()));
    assert!(outbox_view_block(&public_room(&f)).is_none());
}

fn outbox_view_block(room: &LiveRoom) -> Option<String> {
    Outbox::default().view(room, &minutes).send_block
}

#[test]
fn a_banned_member_cannot_send() {
    use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
    let f = Fixture::new();
    let mut state = public_state(&f);
    state.bans.0.push(AuthorizedUserBan::new(
        UserBan {
            owner_member_id: f.owner_id(),
            banned_at: SystemTime::now(),
            banned_user: f.me_id(),
        },
        f.owner_id(),
        &f.owner,
    ));
    let room = LiveRoom::new(f.stored(state));
    assert_eq!(participation(&room), Err(Blocked::Banned));
}

#[test]
fn a_member_pruned_for_inactivity_rejoins_with_the_message() {
    let f = Fixture::new();
    let mut state = f.private_state();
    // Pruned: no longer a member, and none of their messages left.
    state.members.members.clear();
    let me = f.me_id();
    state
        .recent_messages
        .messages
        .retain(|m| m.message.author != me);
    let mut stored = f.stored(state);
    stored.self_authorized_member = Some(admitted(&f));
    let room = LiveRoom::new(stored);
    assert_eq!(participation(&room), Ok(()));
    let mut outbox = Outbox::default();
    assert!(outbox.command(
        &room,
        Action::Send {
            text: "back again".into(),
            reply_to: None
        }
    ));
    let mut changed = false;
    let delta = outbox
        .next_update(&room, Instant::now(), &mut changed)
        .expect("goes out");
    assert_eq!(delta.members.as_ref().unwrap().added(), [admitted(&f)]);
    // The nickname is republished sealed, never in plain text, in a private room.
    let info = &delta.member_info.as_ref().unwrap()[0];
    let SealedBytes::Private { .. } = &info.member_info.preferred_nickname else {
        panic!("a private room's nickname must be sealed");
    };
    assert_eq!(
        river_core::ecies::unseal_bytes(&info.member_info.preferred_nickname, Some(&SECRET))
            .unwrap(),
        b"Mia"
    );
    // The room takes the message with the member back.
    let view = outbox.view(&room, &minutes);
    assert_eq!(view.messages.last().unwrap().text, "back again");
}

#[test]
fn a_rotated_secret_reseals_what_is_waiting_keeping_its_time() {
    let f = Fixture::new();
    let mut room = private_room(&f);
    let mut outbox = Outbox::default();
    // Out first, so the second waits sealed under v0.
    sent(
        &mut outbox,
        &room,
        Action::Send {
            text: "first".into(),
            reply_to: None,
        },
    );
    assert!(outbox.command(
        &room,
        Action::Send {
            text: "second".into(),
            reply_to: None
        }
    ));
    outbox.answered(&room.stored.owner_vk, Ok(())).unwrap();

    let v1 = [7u8; 32];
    let mut state = room.stored.room_state.clone();
    add_secret(&f, &mut state, 1, &v1, &f.me);
    room.merge_state(&state).unwrap();
    assert_eq!(room.current_secret().map(|(_, v)| v), Some(1));

    let before = outbox.entries[0].time;
    let mut changed = false;
    let messages = outbox
        .next_update(&room, Instant::now(), &mut changed)
        .unwrap()
        .recent_messages
        .unwrap();
    let (_, version, plain) = open(&messages[0].message.content, &v1);
    assert_eq!(version, 1);
    assert_eq!(TextContentV1::decode(&plain).unwrap().text, "second");
    assert_eq!(messages[0].message.time, before);
}

#[test]
fn a_message_written_as_another_identity_is_never_sent_as_this_one() {
    let f = Fixture::new();
    let room = public_room(&f);
    let mut outbox = Outbox::default();
    assert!(outbox.command(
        &room,
        Action::Send {
            text: "as me".into(),
            reply_to: None
        }
    ));
    let mut stored = room.stored.clone();
    stored.self_sk = Some(SigningKey::from_bytes(&[9; 32]));
    let replaced = LiveRoom::new(stored);
    let mut changed = false;
    assert!(
        outbox
            .next_update(&replaced, Instant::now(), &mut changed)
            .is_none()
    );
    assert!(changed);
}

#[test]
fn a_message_over_the_rooms_size_limit_is_refused_here() {
    let f = Fixture::new();
    let mut state = public_state(&f);
    let mut config = state.configuration.configuration.clone();
    config.max_message_size = 20;
    config.configuration_version += 1;
    state.configuration = AuthorizedConfigurationV1::new(config, &f.owner);
    let room = LiveRoom::new(f.stored(state));
    let mut outbox = Outbox::default();
    assert!(outbox.command(
        &room,
        Action::Send {
            text: "this is far too long for twenty bytes".into(),
            reply_to: None
        }
    ));
    let mut changed = false;
    assert!(
        outbox
            .next_update(&room, Instant::now(), &mut changed)
            .is_none()
    );
    let view = outbox.view(&room, &minutes);
    assert!(
        view.notices[0].text.contains("too long"),
        "{:?}",
        view.notices
    );
}
