//! Building, showing and settling what the user sends, against rooms built the way River builds
//! them (`room_tests::Fixture`). Times come from an injected clock and explicit instants, so
//! nothing here waits on the machine's clock.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use ed25519_dalek::SigningKey;
use river_core::ecies::decrypt_with_symmetric_key;
use river_core::room_state::ChatRoomStateV1;
use river_core::room_state::ChatRoomStateV1Delta;
use river_core::room_state::ban::{AuthorizedUserBan, UserBan};
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::content::{
    ACTION_TYPE_REACTION, ACTION_TYPE_REMOVE_REACTION, ActionContentV1, CONTENT_TYPE_ACTION,
    CONTENT_TYPE_REPLY, CONTENT_TYPE_TEXT, ReplyContentV1, TextContentV1,
};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
use river_core::room_state::message::{AuthorizedMessageV1, MessageId, RoomMessageBody};
use river_core::room_state::privacy::{RoomDisplayMetadata, SealedBytes};

use super::*;
use crate::rooms::room::{LiveRoom, message_key};
use crate::rooms::room_tests::{Fixture, SECRET, add_secret};

/// A fixed time long ago (2023-11-14), so nothing depends on today.
const BASE: u64 = 1_700_000_000;

fn minutes(t: SystemTime) -> u32 {
    (t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() / 60) as u32
}

/// An outbox whose clock starts at `BASE` and moves one second each time it is read.
fn outbox() -> Outbox {
    let ticks = Arc::new(AtomicU64::new(0));
    Outbox::with_clock(Box::new(move || {
        SystemTime::UNIX_EPOCH + Duration::from_secs(BASE + ticks.fetch_add(1, Ordering::Relaxed))
    }))
}

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

fn private_room(f: &Fixture) -> LiveRoom {
    LiveRoom::new(f.stored(f.private_state()))
}

fn admitted_by(f: &Fixture, inviter: &SigningKey, who: &SigningKey) -> AuthorizedMember {
    AuthorizedMember::new(
        Member {
            owner_member_id: f.owner_id(),
            invited_by: MemberId::from(&inviter.verifying_key()),
            member_vk: who.verifying_key(),
        },
        inviter,
    )
}

fn admitted(f: &Fixture) -> AuthorizedMember {
    admitted_by(f, &f.owner, &f.me)
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
    state.recent_messages.messages = vec![
        f.message(
            &f.owner,
            BASE - 100,
            RoomMessageBody::public("from the owner".into()),
        ),
        f.message(&f.me, BASE - 50, RoomMessageBody::public("from me".into())),
    ];
    state
}

fn public_room(f: &Fixture) -> LiveRoom {
    LiveRoom::new(f.stored(public_state(f)))
}

fn with_config(
    f: &Fixture,
    mut state: ChatRoomStateV1,
    edit: impl FnOnce(&mut Configuration),
) -> ChatRoomStateV1 {
    let mut config = state.configuration.configuration.clone();
    edit(&mut config);
    config.configuration_version += 1;
    state.configuration = AuthorizedConfigurationV1::new(config, &f.owner);
    state
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

fn say(text: &str) -> Action {
    Action::Send {
        text: text.into(),
        reply_to: None,
    }
}

/// The next update for `room`, sent at `now`.
fn next(outbox: &mut Outbox, room: &LiveRoom, now: Instant) -> Option<ChatRoomStateV1Delta> {
    let mut changed = false;
    outbox.next_update(room, now, &mut changed)
}

/// Send `action`, then take the messages of the update it makes.
fn sent(outbox: &mut Outbox, room: &LiveRoom, action: Action) -> Vec<AuthorizedMessageV1> {
    assert!(outbox.command(room, action));
    next(outbox, room, Instant::now())
        .expect("an update goes out")
        .recent_messages
        .unwrap()
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

fn pending_of(outbox: &Outbox, room: &LiveRoom, text: &str) -> Option<Option<Pending>> {
    outbox
        .view(room, &minutes)
        .messages
        .iter()
        .find(|m| m.text == text)
        .map(|m| m.pending.clone())
}

// What is built ---------------------------------------------------------------------------------

#[test]
fn public_bodies_are_the_web_uis_bodies_signed_with_the_rooms_key() {
    let f = Fixture::new();
    let room = public_room(&f);
    let theirs = key_of(&room, &f.owner);
    let mine = key_of(&room, &f.me);
    let target = room.message_by_key(&theirs).unwrap().id();
    let own_target = room.message_by_key(&mine).unwrap().id();
    let one = |mut outbox: Outbox, action| {
        let m = sent(&mut outbox, &room, action);
        assert_eq!(m.len(), 1);
        assert!(m[0].validate(&f.me.verifying_key()).is_ok());
        assert_eq!(m[0].message.author, f.me_id());
        assert_eq!(m[0].message.room_owner, f.owner_id());
        m[0].message.content.clone()
    };
    assert_eq!(
        one(outbox(), say("hello")),
        RoomMessageBody::public("hello".into())
    );
    assert_eq!(
        one(
            outbox(),
            Action::Send {
                text: "re".into(),
                reply_to: Some(theirs.clone())
            }
        ),
        RoomMessageBody::reply(
            "re".into(),
            target.clone(),
            "Member ".to_owned() + &f.owner_id().to_string(),
            "from the owner".into()
        )
    );
    assert_eq!(
        one(
            outbox(),
            Action::Edit {
                target: mine.clone(),
                text: "fixed".into()
            }
        ),
        RoomMessageBody::edit(own_target.clone(), "fixed".into())
    );
    assert_eq!(
        one(outbox(), Action::Delete { target: mine }),
        RoomMessageBody::delete(own_target)
    );
    assert_eq!(
        one(
            outbox(),
            Action::React {
                target: theirs,
                emoji: "👍".into()
            }
        ),
        RoomMessageBody::reaction(target, "👍".into())
    );
}

#[test]
fn a_private_message_and_reaction_are_sealed_under_the_current_secret() {
    let f = Fixture::new();
    let room = private_room(&f);
    let messages = sent(&mut outbox(), &room, say("secret hello"));
    let (ty, version, plain) = open(&messages[0].message.content, &SECRET);
    assert_eq!((ty, version), (CONTENT_TYPE_TEXT, 0));
    assert_eq!(TextContentV1::decode(&plain).unwrap().text, "secret hello");

    // The fixture's room already holds my 🌱 on the owner's message: a 🎉 replaces it.
    let theirs = key_of(&room, &f.owner);
    let messages = sent(
        &mut outbox(),
        &room,
        Action::React {
            target: theirs.clone(),
            emoji: "🎉".into(),
        },
    );
    let actions: Vec<ActionContentV1> = messages
        .iter()
        .map(|m| {
            let (ty, _, plain) = open(&m.message.content, &SECRET);
            assert_eq!(ty, CONTENT_TYPE_ACTION);
            ActionContentV1::decode(&plain).unwrap()
        })
        .collect();
    let id = room.message_by_key(&theirs).unwrap().id();
    assert_eq!(
        actions,
        [
            ActionContentV1::remove_reaction(id.clone(), "🌱".into()),
            ActionContentV1::reaction(id, "🎉".into())
        ]
    );
}

#[test]
fn a_reply_carries_the_target_its_author_and_a_preview() {
    let f = Fixture::new();
    let room = private_room(&f);
    let target = key_of(&room, &f.owner);
    let mut outbox = outbox();
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
    // The target as shown: its edited text.
    assert_eq!(reply.target_content_preview, "Hello everyone");
    assert!(
        outbox
            .view(&room, &minutes)
            .messages
            .last()
            .unwrap()
            .reply_to
            .is_some()
    );
}

/// The emoji of `me`'s reactions on `target` in the view.
fn reactions_by_me(outbox: &Outbox, room: &LiveRoom, target: &str) -> Vec<String> {
    let view = outbox.view(room, &minutes);
    let m = view.messages.iter().find(|m| m.key == target).unwrap();
    m.reactions
        .iter()
        .filter(|r| r.by.contains(&view.me))
        .map(|r| r.emoji.clone())
        .collect()
}

/// Each update's reaction actions, as "+emoji" / "-emoji", sending until none is left.
fn reaction_updates(outbox: &mut Outbox, room: &mut LiveRoom) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    while let Some(delta) = next(outbox, room, Instant::now()) {
        let actions: Vec<String> = delta
            .recent_messages
            .unwrap()
            .iter()
            .map(|m| {
                let RoomMessageBody::Public { data, .. } = &m.message.content else {
                    panic!("a public room's action is public");
                };
                let a = ActionContentV1::decode(data).unwrap();
                let sign = match a.action_type {
                    ACTION_TYPE_REACTION => "+",
                    ACTION_TYPE_REMOVE_REACTION => "-",
                    other => panic!("not a reaction action: {other}"),
                };
                format!("{sign}{}", a.reaction_payload().unwrap().emoji)
            })
            .collect();
        out.push(actions);
        assert!(outbox.answered(room, Ok(())));
    }
    out
}

#[test]
fn reacting_toggles_and_replaces_counting_reactions_still_on_their_way() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let target = key_of(&room, &f.owner);
    let mut outbox = outbox();
    let react = |emoji: &str| Action::React {
        target: target.clone(),
        emoji: emoji.into(),
    };
    assert!(outbox.command(&room, react("👍")));
    assert_eq!(reactions_by_me(&outbox, &room, &target), ["👍"]);
    assert!(outbox.command(&room, react("😂")));
    assert_eq!(reactions_by_me(&outbox, &room, &target), ["😂"]);
    assert!(outbox.command(&room, react("😂")));
    assert!(reactions_by_me(&outbox, &room, &target).is_empty());
    assert_eq!(
        reaction_updates(&mut outbox, &mut room),
        [vec!["+👍"], vec!["-👍", "+😂"], vec!["-😂"]]
    );
}

#[test]
fn reacting_toggles_and_replaces_a_reaction_the_room_already_holds() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let target = key_of(&room, &f.owner);
    let id = room.message_by_key(&target).unwrap().id();
    let held = f.message(&f.me, BASE - 10, RoomMessageBody::reaction(id, "👍".into()));
    room.apply_delta(ChatRoomStateV1Delta {
        recent_messages: Some(vec![held]),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(reactions_by_me(&Outbox::default(), &room, &target), ["👍"]);
    let react = |emoji: &str| Action::React {
        target: target.clone(),
        emoji: emoji.into(),
    };

    let mut toggle_off = outbox();
    assert!(toggle_off.command(&room, react("👍")));
    assert!(reactions_by_me(&toggle_off, &room, &target).is_empty());
    assert_eq!(
        reaction_updates(&mut toggle_off, &mut room.clone()),
        [vec!["-👍"]]
    );

    let mut replace = outbox();
    assert!(replace.command(&room, react("🎉")));
    assert_eq!(reactions_by_me(&replace, &room, &target), ["🎉"]);
    assert_eq!(
        reaction_updates(&mut replace, &mut room),
        [vec!["-👍", "+🎉"]]
    );
}

#[test]
fn edits_and_deletions_are_sealed_actions_on_your_own_messages_only() {
    let f = Fixture::new();
    let mut room = private_room(&f);
    let mine = key_of(&room, &f.me);
    let theirs = key_of(&room, &f.owner);
    let mut outbox = outbox();
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
    let view = outbox.view(&room, &minutes);
    let shown = view.messages.iter().find(|m| m.key == mine).unwrap();
    assert_eq!(shown.text, "Hi again");
    assert!(shown.edited);
    assert!(outbox.answered(&mut room, Ok(())));
    assert!(outbox.is_empty());

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
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    let view = outbox.view(&room, &minutes);
    assert_eq!(view.notices.len(), 2);
    assert!(view.notices[0].text.contains("only change your own"));
    assert!(!view.notices[0].retry);

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

#[test]
fn actions_on_what_is_gone_or_empty_are_said_not_sent() {
    let f = Fixture::new();
    let room = public_room(&f);
    let mine = key_of(&room, &f.me);
    let cases = [
        (say("   "), "Your message couldn't be sent. It is empty."),
        (
            Action::Edit {
                target: mine,
                text: " ".into(),
            },
            "Your edit couldn't be sent. It is empty.",
        ),
        (
            Action::Send {
                text: "re".into(),
                reply_to: Some("12345".into()),
            },
            "Your reply couldn't be sent. The message you replied to is gone.",
        ),
        (
            Action::React {
                target: "12345".into(),
                emoji: "👍".into(),
            },
            "Your reaction couldn't be sent. That message is gone.",
        ),
        (
            Action::Delete {
                target: "12345".into(),
            },
            "Your deletion couldn't be sent. That message is gone.",
        ),
    ];
    for (action, said) in cases {
        let mut outbox = outbox();
        assert!(outbox.command(&room, action));
        assert!(next(&mut outbox, &room, Instant::now()).is_none());
        let view = outbox.view(&room, &minutes);
        assert_eq!(view.notices.len(), 1);
        assert_eq!(view.notices[0].text, said);
        assert!(!view.notices[0].retry, "nothing to send again");
    }
}

#[test]
fn a_message_over_the_rooms_size_limit_is_refused_here_public_or_private() {
    let f = Fixture::new();
    let small = |c: &mut Configuration| c.max_message_size = 20;
    for state in [
        with_config(&f, public_state(&f), small),
        with_config(&f, f.private_state(), small),
    ] {
        let room = LiveRoom::new(f.stored(state));
        let mut outbox = outbox();
        assert!(outbox.command(&room, say("this is far too long for twenty bytes")));
        assert!(next(&mut outbox, &room, Instant::now()).is_none());
        let view = outbox.view(&room, &minutes);
        assert!(
            view.notices[0].text.contains("too long"),
            "{:?}",
            view.notices
        );
    }
}

#[test]
fn a_message_the_room_would_not_keep_is_refused_here() {
    let f = Fixture::new();
    // A room that keeps no messages admits the delta but holds nothing.
    let state = with_config(&f, public_state(&f), |c| c.max_recent_messages = 0);
    let room = LiveRoom::new(f.stored(state));
    let mut outbox = outbox();
    assert!(outbox.command(&room, say("into nothing")));
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    let view = outbox.view(&room, &minutes);
    assert!(
        view.notices[0]
            .text
            .contains("keeps only its newest messages"),
        "{:?}",
        view.notices
    );
}

// Who can send -----------------------------------------------------------------------------------

#[test]
fn who_cannot_send_is_told_why_and_nothing_goes_out() {
    let f = Fixture::new();

    let mut stored = f.stored(public_state(&f));
    stored.self_sk = None;
    stored.self_vk = Some(f.me.verifying_key());
    let no_key = LiveRoom::new(stored);
    assert_eq!(participation(&no_key), Err(Blocked::NoKey));

    let mut state = public_state(&f);
    state.members.members.clear();
    state.recent_messages.messages.truncate(1);
    let not_member = LiveRoom::new(f.stored(state));
    assert_eq!(participation(&not_member), Err(Blocked::NotMember));

    let mut state = f.private_state();
    state.secrets.encrypted_secrets.clear();
    let no_secret = LiveRoom::new(f.stored(state));
    assert_eq!(participation(&no_secret), Err(Blocked::NoSecret));

    for room in [&no_key, &not_member, &no_secret] {
        let mut outbox = outbox();
        outbox.command(room, say("hi"));
        assert!(next(&mut outbox, room, Instant::now()).is_none());
        let view = outbox.view(room, &minutes);
        assert!(view.send_block.is_some());
        assert_eq!(view.notices.len(), 1, "the refusal is said, not dropped");
    }
    assert!(
        Outbox::default()
            .view(&public_room(&f), &minutes)
            .send_block
            .is_none()
    );
}

#[test]
fn the_owner_can_always_send() {
    let f = Fixture::new();
    let mut stored = f.stored(public_state(&f));
    stored.self_sk = Some(f.owner.clone());
    let room = LiveRoom::new(stored);
    assert_eq!(participation(&room), Ok(()));
    let m = sent(&mut outbox(), &room, say("from the top"));
    assert!(m[0].validate(&f.owner.verifying_key()).is_ok());
}

fn ban(f: &Fixture, who: MemberId) -> AuthorizedUserBan {
    AuthorizedUserBan::new(
        UserBan {
            owner_member_id: f.owner_id(),
            banned_at: at(BASE),
            banned_user: who,
        },
        f.owner_id(),
        &f.owner,
    )
}

#[test]
fn a_banned_member_cannot_send() {
    let f = Fixture::new();
    let mut state = public_state(&f);
    state.bans.0.push(ban(&f, f.me_id()));
    let room = LiveRoom::new(f.stored(state));
    assert_eq!(participation(&room), Err(Blocked::Banned));
}

#[test]
fn a_ban_on_whoever_invited_you_bans_you_too_even_once_you_are_pruned() {
    let f = Fixture::new();
    let inviter = SigningKey::from_bytes(&[7; 32]);
    let inviter_record = admitted_by(&f, &f.owner, &inviter);
    let mine = admitted_by(&f, &inviter, &f.me);
    let mut state = public_state(&f);
    state.members.members = vec![inviter_record.clone()];
    state.recent_messages.messages.truncate(1);
    state
        .bans
        .0
        .push(ban(&f, MemberId::from(&inviter.verifying_key())));
    let mut stored = f.stored(state);
    stored.self_authorized_member = Some(mine);
    stored.invite_chain = vec![inviter_record];
    let room = LiveRoom::new(stored);
    assert_eq!(participation(&room), Err(Blocked::Banned));
}

// Rejoining -------------------------------------------------------------------------------------

/// `state` with `me` pruned: not a member, and none of my messages left.
fn pruned(f: &Fixture, mut state: ChatRoomStateV1) -> ChatRoomStateV1 {
    state.members.members.clear();
    let me = f.me_id();
    state
        .recent_messages
        .messages
        .retain(|m| m.message.author != me);
    state
}

fn rejoin_delta(room: &LiveRoom) -> ChatRoomStateV1Delta {
    let mut outbox = outbox();
    assert!(outbox.command(room, say("back again")));
    let delta = next(&mut outbox, room, Instant::now()).expect("goes out");
    // The room takes the message with the member back.
    assert_eq!(
        outbox.view(room, &minutes).messages.last().unwrap().text,
        "back again"
    );
    delta
}

#[test]
fn a_pruned_member_rejoins_in_a_private_room_with_a_sealed_nickname() {
    let f = Fixture::new();
    let mut stored = f.stored(pruned(&f, f.private_state()));
    stored.self_authorized_member = Some(admitted(&f));
    let room = LiveRoom::new(stored);
    assert_eq!(participation(&room), Ok(()));
    let delta = rejoin_delta(&room);
    assert_eq!(delta.members.as_ref().unwrap().added(), [admitted(&f)]);
    let info = &delta.member_info.as_ref().unwrap()[0];
    let SealedBytes::Private { .. } = &info.member_info.preferred_nickname else {
        panic!("a private room's nickname must be sealed");
    };
    assert_eq!(
        river_core::ecies::unseal_bytes(&info.member_info.preferred_nickname, Some(&SECRET))
            .unwrap(),
        b"Mia"
    );
}

#[test]
fn a_pruned_member_rejoins_a_public_room_with_ancestors_and_their_stored_nickname() {
    let f = Fixture::new();
    let inviter = SigningKey::from_bytes(&[7; 32]);
    let inviter_record = admitted_by(&f, &f.owner, &inviter);
    let mine = admitted_by(&f, &inviter, &f.me);
    let stored_info = AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: f.me_id(),
            version: 4,
            preferred_nickname: SealedBytes::public(b"Stored Me".to_vec()),
            deputies: Vec::new(),
        },
        &f.me,
    );
    let mut stored = f.stored(pruned(&f, public_state(&f)));
    stored.self_authorized_member = Some(mine.clone());
    stored.invite_chain = vec![inviter_record.clone()];
    stored.self_member_info = Some(stored_info.clone());
    let room = LiveRoom::new(stored.clone());
    let delta = rejoin_delta(&room);
    assert_eq!(
        delta.members.as_ref().unwrap().added(),
        [mine, inviter_record]
    );
    assert_eq!(delta.member_info.unwrap(), [stored_info]);

    // With no stored record, the chosen nickname goes out in plain text in a public room.
    stored.self_member_info = None;
    let delta = rejoin_delta(&LiveRoom::new(stored));
    assert_eq!(
        delta.member_info.unwrap()[0].member_info.preferred_nickname,
        SealedBytes::public(b"Mia".to_vec())
    );
}

// Settling --------------------------------------------------------------------------------------

#[test]
fn what_is_sent_shows_at_once_marked_until_the_node_takes_it() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    let messages = sent(&mut outbox, &room, say("on its way"));
    let view = outbox.view(&room, &minutes);
    let shown = view.messages.last().unwrap();
    assert_eq!(shown.author, view.me);
    assert_eq!(
        pending_of(&outbox, &room, "on its way")
            .unwrap()
            .unwrap()
            .failed,
        None
    );
    assert!(
        room.message_by_key(&message_key(&messages[0].id()))
            .is_none()
    );

    assert!(outbox.answered(&mut room, Ok(())));
    assert!(outbox.is_empty());
    assert!(
        room.message_by_key(&message_key(&messages[0].id()))
            .is_some()
    );
    assert_eq!(pending_of(&outbox, &room, "on its way"), Some(None));
}

#[test]
fn an_answer_the_room_does_not_show_stays_unconfirmed_and_is_never_sent_again() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    sent(&mut outbox, &room, say("dropped"));
    // Removed from the room while the update was out: the node's room drops the message.
    let mut state = room.stored.room_state.clone();
    state.members.members.clear();
    room.stored.room_state = state;
    assert!(outbox.answered(&mut room, Ok(())));
    let view = outbox.view(&room, &minutes);
    assert!(
        view.notices[0]
            .text
            .starts_with("Your message: Freenet took it, but the room here doesn't show it.")
            && view.notices[0].text.contains("removed or banned"),
        "{:?}",
        view.notices
    );
    assert!(!view.notices[0].retry);
    // Taken by the node: Retry does nothing, and it never goes out again from here.
    let entry = view.notices[0].entry;
    assert!(!outbox.command(&room, Action::Retry { entry }));
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    assert!(outbox.command(&room, Action::Discard { entry }));
    assert!(outbox.is_empty());
}

#[test]
fn an_answer_for_a_message_sealed_with_a_key_the_room_here_lacks_says_so() {
    let f = Fixture::new();
    let older = private_room(&f);
    let mut state = older.stored.room_state.clone();
    add_secret(&f, &mut state, 1, &[7; 32], &f.me);
    let mut newer = older.clone();
    newer.merge_state(&state).unwrap();
    let mut outbox = outbox();
    sent(&mut outbox, &newer, say("under v1"));
    // The answer lands while the room here is a copy from before the rotation.
    let mut here = older.clone();
    assert!(outbox.answered(&mut here, Ok(())));
    let pending = pending_of(&outbox, &newer, "under v1").unwrap().unwrap();
    assert!(
        pending
            .failed
            .unwrap()
            .contains("doesn't have the key it was sealed with"),
    );
    assert!(!pending.retry);
}

#[test]
fn a_refused_send_stays_with_the_reason_and_a_retry_builds_it_again() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    let first = sent(&mut outbox, &room, say("try me"));
    assert!(outbox.answered(&mut room, Err("no room".into())));
    let pending = pending_of(&outbox, &room, "try me").unwrap().unwrap();
    assert_eq!(
        pending.failed.as_deref(),
        Some("Freenet refused it: no room")
    );
    assert!(
        next(&mut outbox, &room, Instant::now()).is_none(),
        "not again by itself"
    );

    assert!(outbox.command(
        &room,
        Action::Retry {
            entry: pending.entry
        }
    ));
    let again = next(&mut outbox, &room, Instant::now())
        .unwrap()
        .recent_messages
        .unwrap();
    // Refused, so not held: a new send, stamped again (the clock moved on).
    assert_ne!(again[0].id(), first[0].id());
    assert!(again[0].message.time > first[0].message.time);
}

#[test]
fn a_late_update_keeps_its_rooms_turn_so_its_answer_is_not_taken_for_the_next() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    let t0 = Instant::now();
    let wait = Duration::from_secs(60);
    assert!(outbox.command(&room, say("A")));
    next(&mut outbox, &room, t0).unwrap();
    assert!(!outbox.expire(wait, t0 + wait / 2));
    assert!(outbox.expire(wait, t0 + wait));
    assert_eq!(
        pending_of(&outbox, &room, "A")
            .unwrap()
            .unwrap()
            .failed
            .as_deref(),
        Some(LATE)
    );
    assert!(outbox.command(&room, say("B")));
    assert!(
        next(&mut outbox, &room, t0 + wait).is_none(),
        "B waits for A's answer"
    );

    // A's late answer is A's.
    assert!(outbox.answered(&mut room, Ok(())));
    assert_eq!(pending_of(&outbox, &room, "A"), Some(None));
    assert_eq!(
        pending_of(&outbox, &room, "B").unwrap().unwrap().failed,
        None
    );
    assert!(next(&mut outbox, &room, t0 + wait).is_some(), "now B goes");

    // A late refusal fails the late one, not the one after it.
    let mut outbox = Outbox::default();
    assert!(outbox.command(&room, say("C")));
    next(&mut outbox, &room, t0).unwrap();
    outbox.expire(wait, t0 + wait);
    assert!(outbox.command(&room, say("D")));
    assert!(outbox.answered(&mut room, Err("no".into())));
    assert!(
        pending_of(&outbox, &room, "C")
            .unwrap()
            .unwrap()
            .failed
            .unwrap()
            .contains("refused")
    );
    assert_eq!(
        pending_of(&outbox, &room, "D").unwrap().unwrap().failed,
        None
    );
}

#[test]
fn a_late_update_retried_or_resent_after_a_drop_goes_again_unchanged() {
    let f = Fixture::new();
    let mut room = private_room(&f);
    let mut outbox = outbox();
    let wait = Duration::from_secs(60);
    let first = sent(&mut outbox, &room, say("again and again"));
    let t0 = Instant::now();
    assert!(outbox.expire(wait, t0 + wait));
    let entry = pending_of(&outbox, &room, "again and again")
        .unwrap()
        .unwrap()
        .entry;
    // Retry of an unanswered update: not on this connection, which still owes its answer.
    assert!(!outbox.wants_reconnect(wait * 3, t0 + wait));
    assert!(outbox.command(&room, Action::Retry { entry }));
    assert!(next(&mut outbox, &room, t0 + wait).is_none());
    assert!(outbox.wants_reconnect(wait * 3, t0 + wait));

    // On the new connection (with the room's secret rotated meanwhile): the same signed
    // message, since the node may already hold it.
    outbox.requeue();
    assert!(!outbox.wants_reconnect(wait * 3, t0 + wait));
    let mut state = room.stored.room_state.clone();
    add_secret(&f, &mut state, 1, &[7; 32], &f.me);
    room.merge_state(&state).unwrap();
    let resent = next(&mut outbox, &room, t0 + wait)
        .unwrap()
        .recent_messages
        .unwrap();
    assert_eq!(resent, first);
}

#[test]
fn an_unanswered_update_is_settled_when_the_network_brings_it_in() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    let t0 = Instant::now();
    let messages = sent(&mut outbox, &room, say("late"));
    assert!(outbox.expire(Duration::ZERO, t0));
    room.apply_delta(ChatRoomStateV1Delta {
        recent_messages: Some(messages),
        ..Default::default()
    })
    .unwrap();
    assert!(outbox.reconcile(&room));
    assert!(outbox.is_empty());
    assert_eq!(pending_of(&outbox, &room, "late"), Some(None));
}

#[test]
fn one_update_per_room_is_out_at_a_time_and_a_lost_one_goes_again_unchanged() {
    let f = Fixture::new();
    let room = public_room(&f);
    let mut outbox = outbox();
    let first = sent(&mut outbox, &room, say("one"));
    assert!(outbox.command(&room, say("two")));
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    let texts: Vec<String> = outbox
        .view(&room, &minutes)
        .messages
        .iter()
        .map(|m| m.text.clone())
        .collect();
    assert!(texts.ends_with(&["one".to_owned(), "two".to_owned()]));
    outbox.requeue();
    let again = next(&mut outbox, &room, Instant::now())
        .unwrap()
        .recent_messages
        .unwrap();
    assert_eq!(again, first);
}

#[test]
fn a_waiting_message_is_sealed_again_under_a_newer_secret_keeping_its_time() {
    let f = Fixture::new();
    let mut room = private_room(&f);
    let mut outbox = outbox();
    sent(&mut outbox, &room, say("first"));
    assert!(outbox.command(&room, say("second")));
    assert!(outbox.answered(&mut room, Ok(())));

    let v1 = [7u8; 32];
    let mut state = room.stored.room_state.clone();
    add_secret(&f, &mut state, 1, &v1, &f.me);
    room.merge_state(&state).unwrap();
    assert_eq!(room.current_secret().map(|(_, v)| v), Some(1));

    let messages = next(&mut outbox, &room, Instant::now())
        .unwrap()
        .recent_messages
        .unwrap();
    let (_, version, plain) = open(&messages[0].message.content, &v1);
    assert_eq!(version, 1);
    assert_eq!(TextContentV1::decode(&plain).unwrap().text, "second");
    // Stamped when it was written (the clock's second reading), not now.
    assert_eq!(messages[0].message.time, at(BASE + 1));
}

/// A public room where `other` is a member too, and the room's key on this computer switches
/// from `me` to `other` (an identity import).
fn identity_replaced(f: &Fixture, other: &SigningKey) -> (LiveRoom, LiveRoom) {
    let mut state = public_state(f);
    state.members.members.push(admitted_by(f, &f.owner, other));
    let before = LiveRoom::new(f.stored(state));
    let mut stored = before.stored.clone();
    stored.self_sk = Some(other.clone());
    let after = LiveRoom::new(stored);
    assert_eq!(participation(&after), Ok(()), "the new identity can send");
    (before, after)
}

#[test]
fn a_message_written_as_another_identity_is_never_sent_as_this_one() {
    let f = Fixture::new();
    let other = SigningKey::from_bytes(&[9; 32]);
    let (before, after) = identity_replaced(&f, &other);
    let mut outbox = outbox();
    assert!(outbox.command(&before, say("as me")));
    let mut changed = false;
    assert!(
        outbox
            .next_update(&after, Instant::now(), &mut changed)
            .is_none()
    );
    assert!(changed);
    // Shown as not sent, with why.
    assert_eq!(
        pending_of(&outbox, &after, "as me")
            .unwrap()
            .unwrap()
            .failed
            .as_deref(),
        Some(IDENTITY_CHANGED)
    );
}

#[test]
fn a_retry_after_the_identity_changed_is_still_refused() {
    let f = Fixture::new();
    let other = SigningKey::from_bytes(&[9; 32]);
    let (mut before, after) = identity_replaced(&f, &other);
    let mut outbox = outbox();
    sent(&mut outbox, &before, say("as me"));
    assert!(outbox.answered(&mut before, Err("busy".into())));
    let entry = pending_of(&outbox, &before, "as me")
        .unwrap()
        .unwrap()
        .entry;
    assert!(outbox.command(&after, Action::Retry { entry }));
    assert!(next(&mut outbox, &after, Instant::now()).is_none());
    assert_eq!(
        pending_of(&outbox, &after, "as me")
            .unwrap()
            .unwrap()
            .failed
            .as_deref(),
        Some(IDENTITY_CHANGED)
    );
}

#[test]
fn retry_and_discard_act_only_on_what_failed_in_their_own_room() {
    let f = Fixture::new();
    let room = public_room(&f);
    let mut other_room = room.clone();
    other_room.stored.owner_vk = SigningKey::from_bytes(&[8; 32]).verifying_key();
    let mut outbox = outbox();
    sent(&mut outbox, &room, say("out"));
    let entry = pending_of(&outbox, &room, "out").unwrap().unwrap().entry;
    // In flight and answered by nobody yet: neither can touch it.
    assert!(!outbox.command(&room, Action::Discard { entry }));
    assert!(!outbox.command(&room, Action::Retry { entry }));
    // Nor from another room once it failed.
    outbox.expire(Duration::ZERO, Instant::now() + Duration::from_secs(1));
    assert!(!outbox.command(&other_room, Action::Discard { entry }));
    assert!(!outbox.command(&other_room, Action::Retry { entry }));
    assert!(outbox.command(&room, Action::Discard { entry }));
    assert!(outbox.is_empty());
}

#[test]
fn the_view_keeps_mention_tokens_for_editing() {
    let f = Fixture::new();
    let mut state = public_state(&f);
    let token = river_core::mention::encode_mention(f.owner_id(), "Olive");
    state.recent_messages.messages.push(f.message(
        &f.me,
        BASE - 5,
        RoomMessageBody::public(format!("hi {token}")),
    ));
    let room = LiveRoom::new(f.stored(state));
    let view = Outbox::default().view(&room, &minutes);
    let m = view.messages.last().unwrap();
    assert_eq!(m.source, format!("hi {token}"));
    assert_ne!(m.text, m.source);
}

#[test]
fn message_ids_are_stable_keys() {
    let id = MessageId(freenet_scaffold::util::FastHash(-42));
    assert_eq!(message_key(&id), "-42");
}

#[test]
fn a_waiting_message_is_never_sealed_back_under_an_older_secret() {
    let f = Fixture::new();
    let older = private_room(&f);
    let mut state = older.stored.room_state.clone();
    add_secret(&f, &mut state, 1, &[7; 32], &f.me);
    let mut newer = older.clone();
    newer.merge_state(&state).unwrap();
    let mut outbox = outbox();
    let first = sent(&mut outbox, &newer, say("under v1"));
    assert!(outbox.answered(&mut newer.clone(), Err("busy".into())));
    // Retried against a copy of the room from before the rotation (after a restart): it is not
    // sealed back to v0, and says why.
    let entry = pending_of(&outbox, &newer, "under v1")
        .unwrap()
        .unwrap()
        .entry;
    assert!(outbox.command(&older, Action::Retry { entry }));
    assert!(next(&mut outbox, &older, Instant::now()).is_none());
    let pending = pending_of(&outbox, &newer, "under v1").unwrap().unwrap();
    assert!(pending.failed.unwrap().contains("newer key hasn't reached"));
    assert_eq!(outbox.entries[0].messages, first, "not resealed");
    // Once the room here catches up, Retry sends it.
    assert!(outbox.command(&newer, Action::Retry { entry }));
    let again = next(&mut outbox, &newer, Instant::now()).unwrap();
    for m in again.recent_messages.unwrap() {
        assert_eq!(m.message.content.secret_version(), Some(1));
    }
}

// An answer owed is never credited to the next update ----------------------------------------

/// Send "A", and queue "B" behind it.
fn a_then_b(outbox: &mut Outbox, room: &LiveRoom) -> Vec<AuthorizedMessageV1> {
    let a = sent(outbox, room, say("A"));
    assert!(outbox.command(room, say("B")));
    a
}

/// B has not gone out, has no answer, and is not in the room.
fn b_untouched(outbox: &Outbox, room: &LiveRoom) {
    assert_eq!(pending_of(outbox, room, "B").unwrap().unwrap().failed, None);
    assert!(
        room.stored
            .room_state
            .recent_messages
            .messages
            .iter()
            .all(|m| room.shown_text(m) != "B")
    );
}

#[test]
fn an_owed_answer_after_the_network_settled_its_update_is_not_the_next_ones() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    let a = a_then_b(&mut outbox, &room);
    // The network brings A in before A's answer.
    room.apply_delta(ChatRoomStateV1Delta {
        recent_messages: Some(a),
        ..Default::default()
    })
    .unwrap();
    assert!(outbox.reconcile(&room));
    assert!(
        next(&mut outbox, &room, Instant::now()).is_none(),
        "A's answer is owed"
    );
    // A's answer: consumed, not credited to B.
    assert!(!outbox.answered(&mut room, Err("late".into())));
    b_untouched(&outbox, &room);
    assert!(
        next(&mut outbox, &room, Instant::now()).is_some(),
        "now B goes"
    );
}

#[test]
fn an_owed_answer_after_its_update_was_discarded_is_not_the_next_ones() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    a_then_b(&mut outbox, &room);
    outbox.expire(Duration::ZERO, Instant::now() + Duration::from_secs(1));
    let entry = pending_of(&outbox, &room, "A").unwrap().unwrap().entry;
    assert!(outbox.command(&room, Action::Discard { entry }));
    assert!(
        next(&mut outbox, &room, Instant::now()).is_none(),
        "A's answer is owed"
    );
    assert!(!outbox.answered(&mut room, Ok(())));
    b_untouched(&outbox, &room);
    assert!(
        next(&mut outbox, &room, Instant::now()).is_some(),
        "now B goes"
    );
}

#[test]
fn an_owed_answer_after_its_update_was_retried_is_its_own() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    a_then_b(&mut outbox, &room);
    outbox.expire(Duration::ZERO, Instant::now() + Duration::from_secs(1));
    let entry = pending_of(&outbox, &room, "A").unwrap().unwrap().entry;
    assert!(outbox.command(&room, Action::Retry { entry }));
    // Nothing goes out on this connection: not A again, not B.
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    assert!(outbox.answered(&mut room, Ok(())));
    assert_eq!(
        pending_of(&outbox, &room, "A"),
        Some(None),
        "A's answer settles A"
    );
    b_untouched(&outbox, &room);
}

#[test]
fn an_answer_owed_too_long_asks_for_a_new_connection() {
    let f = Fixture::new();
    let room = public_room(&f);
    let mut outbox = outbox();
    let t0 = Instant::now();
    assert!(outbox.command(&room, say("A")));
    next(&mut outbox, &room, t0).unwrap();
    let bound = Duration::from_secs(180);
    assert!(!outbox.wants_reconnect(bound, t0 + bound / 2));
    assert!(outbox.wants_reconnect(bound, t0 + bound));
    outbox.requeue();
    assert!(!outbox.wants_reconnect(bound, t0 + bound));
    assert!(
        next(&mut outbox, &room, t0 + bound).is_some(),
        "A goes again"
    );
}

#[test]
fn a_message_never_built_is_not_built_later_as_another_identity() {
    let f = Fixture::new();
    let other = SigningKey::from_bytes(&[9; 32]);
    // A private room where `me` has no secret, so nothing can be built as `me`, and `other`,
    // a member holding the secret, is the identity this computer uses later.
    let mut state = f.private_state();
    state.secrets.encrypted_secrets.clear();
    state
        .members
        .members
        .push(admitted_by(&f, &f.owner, &other));
    add_secret(&f, &mut state, 0, &SECRET, &other);
    let unbuildable = LiveRoom::new(f.stored(state));
    assert_eq!(participation(&unbuildable), Err(Blocked::NoSecret));
    let mut stored = unbuildable.stored.clone();
    stored.self_sk = Some(other.clone());
    let after = LiveRoom::new(stored);
    assert_eq!(participation(&after), Ok(()));
    let mut outbox = outbox();
    assert!(outbox.command(&unbuildable, say("as me")));
    let entry = outbox.view(&unbuildable, &minutes).notices[0].entry;
    assert!(outbox.command(&after, Action::Retry { entry }));
    assert!(next(&mut outbox, &after, Instant::now()).is_none());
    let view = outbox.view(&after, &minutes);
    assert!(
        view.notices
            .iter()
            .any(|n| n.text.contains(IDENTITY_CHANGED))
            || view
                .messages
                .iter()
                .any(|m| m.pending.as_ref().and_then(|p| p.failed.as_deref())
                    == Some(IDENTITY_CHANGED)),
        "{:?}",
        view.notices
    );
}

#[test]
fn a_message_this_computer_cannot_read_offers_nothing_to_edit() {
    let f = Fixture::new();
    let mut state = f.private_state();
    let (ct, nonce) = river_core::ecies::encrypt_with_symmetric_key(
        &[3; 32],
        &TextContentV1::new("unreadable".into()).encode(),
    );
    state.recent_messages.messages.push(f.message(
        &f.me,
        BASE,
        RoomMessageBody::private_text(ct, nonce, 0),
    ));
    let room = LiveRoom::new(f.stored(state));
    let view = Outbox::default().view(&room, &minutes);
    let m = view.messages.last().unwrap();
    assert!(m.text.starts_with("[Encrypted"), "{}", m.text);
    assert_eq!(m.source, "");
}
