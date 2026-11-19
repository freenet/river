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
    assert!(outbox.reconcile(&mut room));
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
fn a_chip_names_a_member_the_dialog_can_open() {
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
    let url =
        m.md.rsplit_once('(')
            .and_then(|(_, u)| u.strip_suffix(')'))
            .expect("a chip link");
    let uid = crate::chat::markdown::mention_member(url).expect("the chip carries a member id");
    assert_eq!(uid, f.owner_id().to_string());
    assert!(
        view.members.iter().any(|x| x.uid == uid),
        "the chip's id is a member's uid in the room view"
    );
}

#[test]
fn a_mention_becomes_a_chip_link_with_a_safe_name() {
    let f = Fixture::new();
    let mut state = public_state(&f);
    let known = river_core::mention::encode_mention(f.owner_id(), "Olive");
    state.recent_messages.messages.push(f.message(
        &f.me,
        BASE - 5,
        RoomMessageBody::public(format!("hi {known} [x](rvm-guess:self)")),
    ));
    let room = LiveRoom::new(f.stored(state));
    let view = Outbox::default().view(&room, &minutes);
    let m = view.messages.last().unwrap();
    let scheme = crate::chat::model::mention_scheme();
    assert!(m.md.contains(&format!("]({scheme}:")), "{}", m.md);
    // The name is escaped so it cannot end its own link, and a link the author wrote is kept
    // as written (and so is not a chip: it does not carry this process's scheme).
    assert!(m.md.starts_with("hi [@"), "{}", m.md);
    assert!(m.md.ends_with("[x](rvm-guess:self)"), "{}", m.md);
    assert!(!m.md.contains("(rv:"), "{}", m.md);
    assert!(!m.md.contains(known.as_str()), "{}", m.md);
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
    assert!(outbox.reconcile(&mut room));
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
    assert!(!outbox.wants_reconnect(Duration::from_secs(600), Instant::now()));
    assert!(outbox.command(&room, Action::Retry { entry }));
    // Nothing goes out on this connection: not A again, not B. A new one is asked for.
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    assert!(outbox.wants_reconnect(Duration::from_secs(600), Instant::now()));
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

// Round 3 --------------------------------------------------------------------------------------

#[test]
fn a_waiting_message_in_a_room_that_moved_past_the_key_held_here_is_not_sent_under_the_old_one() {
    let f = Fixture::new();
    let mut room = private_room(&f);
    let mut outbox = outbox();
    a_then_b(&mut outbox, &room);
    // The owner rotates to v1 and seals it to themselves only: this member has no v1.
    let mut state = room.stored.room_state.clone();
    add_secret(&f, &mut state, 1, &[7; 32], &f.owner);
    room.merge_state(&state).unwrap();
    assert_eq!(room.current_secret(), None);
    assert!(outbox.answered(&mut room, Ok(())));
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    assert_eq!(
        pending_of(&outbox, &room, "B")
            .unwrap()
            .unwrap()
            .failed
            .as_deref(),
        Some(Blocked::NoSecret.reason())
    );
}

#[test]
fn the_cap_and_missing_key_reasons_are_said_in_full() {
    let f = Fixture::new();
    let capped = LiveRoom::new(f.stored(with_config(&f, public_state(&f), |c| {
        c.max_recent_messages = 0
    })));
    let mut outbox = outbox();
    assert!(outbox.command(&capped, say("into nothing")));
    assert_eq!(
        outbox.view(&capped, &minutes).notices[0].text,
        format!("Your message couldn't be sent. {CAPPED}")
    );

    let older = private_room(&f);
    let mut state = older.stored.room_state.clone();
    add_secret(&f, &mut state, 1, &[7; 32], &f.me);
    let mut newer = older.clone();
    newer.merge_state(&state).unwrap();
    let mut outbox = self::outbox();
    sent(&mut outbox, &newer, say("under v1"));
    assert!(outbox.answered(&mut older.clone(), Ok(())));
    assert_eq!(
        pending_of(&outbox, &newer, "under v1")
            .unwrap()
            .unwrap()
            .failed,
        Some(format!(
            "Freenet took it, but the room here doesn't show it. {KEY_NOT_HERE}"
        ))
    );
}

#[test]
fn an_unconfirmed_message_is_settled_when_the_network_brings_it_in() {
    let f = Fixture::new();
    let older = private_room(&f);
    let mut state = older.stored.room_state.clone();
    add_secret(&f, &mut state, 1, &[7; 32], &f.me);
    let mut newer = older.clone();
    newer.merge_state(&state).unwrap();
    let mut outbox = outbox();
    let messages = sent(&mut outbox, &newer, say("under v1"));
    assert!(outbox.answered(&mut older.clone(), Ok(())));
    assert!(!outbox.is_empty());
    newer
        .apply_delta(ChatRoomStateV1Delta {
            recent_messages: Some(messages),
            ..Default::default()
        })
        .unwrap();
    assert!(outbox.reconcile(&mut newer));
    assert!(outbox.is_empty());
}

#[test]
fn an_update_never_answered_costs_a_bounded_number_of_reconnects() {
    let f = Fixture::new();
    let room = public_room(&f);
    let mut outbox = outbox();
    let bound = Duration::from_secs(180);
    let mut t = Instant::now();
    assert!(outbox.command(&room, say("into the void")));
    let mut sends = 0;
    // Bounded here too, so a missing bound fails instead of looping.
    for _ in 0..10 {
        if next(&mut outbox, &room, t).is_none() {
            break;
        }
        sends += 1;
        t += bound;
        assert!(outbox.wants_reconnect(bound, t));
        outbox.requeue();
    }
    assert_eq!(sends, MAX_FORCED_RECONNECTS);
    let pending = pending_of(&outbox, &room, "into the void")
        .unwrap()
        .unwrap();
    assert_eq!(pending.failed.as_deref(), Some(NEVER_ANSWERED));
    assert!(!pending.retry, "Discard only");
    assert!(!outbox.wants_reconnect(bound, t + bound));
}

#[test]
fn the_view_says_who_invited_whom_and_gives_each_members_keys() {
    let f = Fixture::new();
    let room = LiveRoom::new(f.stored(public_state(&f)));
    let view = Outbox::default().view(&room, &minutes);
    let owner = view
        .members
        .iter()
        .find(|m| m.role == crate::chat::model::Role::Owner)
        .unwrap();
    let me = view.member(view.me).unwrap();
    assert_eq!(owner.invited_by, None);
    assert_eq!(me.invited_by, Some(owner.id));
    assert_eq!(owner.uid, f.owner_id().to_string());
    assert_eq!(me.uid, f.me_id().to_string());
    assert_eq!(
        owner.key,
        bs58::encode(f.owner.verifying_key().as_bytes()).into_string()
    );
    assert_eq!(
        me.key,
        bs58::encode(f.me.verifying_key().as_bytes()).into_string()
    );
}

// Your own nickname ----------------------------------------------------------------------------

fn nick(name: &str) -> Action {
    Action::SetNickname {
        nickname: name.into(),
    }
}

/// The record the next update carries, which must be the only one.
fn next_info(outbox: &mut Outbox, room: &LiveRoom) -> (ChatRoomStateV1Delta, AuthorizedMemberInfo) {
    let delta = next(outbox, room, Instant::now()).expect("an update goes out");
    assert!(delta.recent_messages.is_none(), "no message rides along");
    let records = delta.member_info.clone().expect("a member_info record");
    assert_eq!(records.len(), 1);
    let info = records[0].clone();
    (delta, info)
}

/// What the room contract itself makes of `delta` on a copy of `room`: its apply and then its
/// whole-state verify.
fn contract_takes(room: &LiveRoom, delta: ChatRoomStateV1Delta) -> ChatRoomStateV1 {
    use freenet_scaffold::ComposableState;
    let mut copy = room.clone();
    copy.apply_delta(delta).expect("the room takes the delta");
    let state = copy.stored.room_state.clone();
    state
        .verify(&state, &room.params())
        .expect("the resulting state verifies");
    state
}

fn shown_name(outbox: &Outbox, room: &LiveRoom) -> String {
    let view = outbox.view(room, &minutes);
    view.member(view.me).unwrap().nickname.clone()
}

#[test]
fn a_public_nickname_is_my_signed_record_at_the_next_version_and_the_room_takes_it() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    assert!(outbox.command(&room, nick("  Mia Two  ")));
    let (delta, info) = next_info(&mut outbox, &room);
    assert!(delta.members.is_none(), "a current member is not re-added");
    assert_eq!(info.member_info.member_id, f.me_id());
    // The room holds no record of mine (only the cached nickname), so this is the first.
    assert_eq!(info.member_info.version, 1);
    assert_eq!(
        info.member_info.preferred_nickname,
        SealedBytes::public(b"Mia Two".to_vec()),
        "trimmed, and plain in a public room"
    );
    assert!(
        info.verify_signature_with_key(&f.me.verifying_key())
            .is_ok()
    );
    assert!(
        info.verify_signature_with_key(&f.owner.verifying_key())
            .is_err(),
        "signed by me, not the owner"
    );
    let state = contract_takes(&room, delta);
    assert_eq!(state.member_info.canonical(f.me_id()), Some(&info));

    // On its way: shown at once, marked sending.
    assert_eq!(shown_name(&outbox, &room), "Mia Two");
    let view = outbox.view(&room, &minutes);
    let own = view.own_nickname.clone().expect("marked");
    assert_eq!(own.failed, None);
    assert!(view.notices.is_empty());
    // Not yet in the room itself.
    assert_eq!(
        room.view(&minutes)
            .member(room.view(&minutes).me)
            .unwrap()
            .nickname,
        "Mia"
    );

    // Taken: the room shows it, nothing is owed, and the record cached for rejoining follows.
    assert!(outbox.answered(&mut room, Ok(())));
    assert!(outbox.is_empty());
    assert_eq!(shown_name(&outbox, &room), "Mia Two");
    assert!(outbox.view(&room, &minutes).own_nickname.is_none());
    assert_eq!(room.stored.self_nickname.as_deref(), Some("Mia Two"));
    assert_eq!(room.stored.self_member_info.as_ref(), Some(&info));
}

#[test]
fn a_private_nickname_is_sealed_under_the_current_secret_and_never_plain() {
    let f = Fixture::new();
    let room = private_room(&f);
    // The room already holds my record at version 0, so the next is 1.
    let mut outbox = outbox();
    assert!(outbox.command(&room, nick("Quiet Mia")));
    let (delta, info) = next_info(&mut outbox, &room);
    assert_eq!(info.member_info.version, 1);
    let SealedBytes::Private { secret_version, .. } = &info.member_info.preferred_nickname else {
        panic!("a private room's nickname must be sealed");
    };
    assert_eq!(*secret_version, 0);
    assert_eq!(
        river_core::ecies::unseal_bytes(&info.member_info.preferred_nickname, Some(&SECRET))
            .unwrap(),
        b"Quiet Mia"
    );
    let state = contract_takes(&room, delta);
    assert_eq!(state.member_info.canonical(f.me_id()), Some(&info));
    assert_eq!(shown_name(&outbox, &room), "Quiet Mia");

    // Under a newer current secret it is sealed under that one.
    let mut newer = room.clone();
    let v1 = [7u8; 32];
    let mut state = newer.stored.room_state.clone();
    add_secret(&f, &mut state, 1, &v1, &f.me);
    newer.merge_state(&state).unwrap();
    let mut fresh = Outbox::default();
    assert!(fresh.command(&newer, nick("Newer Mia")));
    let (_, info) = next_info(&mut fresh, &newer);
    assert_eq!(
        info.member_info.preferred_nickname.secret_version(),
        Some(1)
    );
    assert_eq!(
        river_core::ecies::unseal_bytes(&info.member_info.preferred_nickname, Some(&v1)).unwrap(),
        b"Newer Mia"
    );
}

#[test]
fn a_waiting_nickname_is_sealed_again_under_a_newer_secret_keeping_its_version() {
    let f = Fixture::new();
    let mut room = private_room(&f);
    let mut outbox = outbox();
    sent(&mut outbox, &room, say("first"));
    assert!(outbox.command(&room, nick("Waiting Mia")));
    assert!(outbox.answered(&mut room, Ok(())));
    let v1 = [7u8; 32];
    let mut state = room.stored.room_state.clone();
    add_secret(&f, &mut state, 1, &v1, &f.me);
    room.merge_state(&state).unwrap();
    let (_, info) = next_info(&mut outbox, &room);
    assert_eq!(
        info.member_info.preferred_nickname.secret_version(),
        Some(1)
    );
    assert_eq!(info.member_info.version, 1);
}

#[test]
fn a_private_room_without_its_secret_refuses_rather_than_send_a_plain_nickname() {
    let f = Fixture::new();
    let mut state = f.private_state();
    state.secrets.encrypted_secrets.clear();
    let room = LiveRoom::new(f.stored(state));
    let mut outbox = outbox();
    outbox.command(&room, nick("Leaky"));
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    let view = outbox.view(&room, &minutes);
    assert!(view.own_nickname.unwrap().failed.is_some());
    assert_eq!(view.notices.len(), 1);
    assert!(
        view.notices[0]
            .text
            .starts_with("Your nickname couldn't be sent.")
    );
}

#[test]
fn a_nickname_the_room_would_not_take_is_refused_here_and_nothing_goes_out() {
    let f = Fixture::new();
    let state = with_config(&f, public_state(&f), |c| c.max_nickname_size = 8);
    let room = LiveRoom::new(f.stored(state));
    for (name, why) in [
        ("", "can't be empty"),
        ("   ", "can't be empty"),
        ("123456789", "at most 8"),
        ("eve\u{202e}", "can't contain emoji"),
        ("zero\u{200b}width", "can't contain emoji"),
        ("shield \u{1f6e1}", "can't contain emoji"),
    ] {
        let mut outbox = outbox();
        outbox.command(&room, nick(name));
        assert!(
            next(&mut outbox, &room, Instant::now()).is_none(),
            "{name:?} must not go out"
        );
        let view = outbox.view(&room, &minutes);
        assert!(
            view.notices[0].text.contains(why),
            "{name:?}: {}",
            view.notices[0].text
        );
        assert_eq!(
            shown_name(&outbox, &room),
            "Mia",
            "{name:?} changes nothing"
        );
    }
    // At the limit is fine, and the contract's own ceiling holds whatever the room says.
    let mut outbox = outbox();
    outbox.command(&room, nick("12345678"));
    assert!(next(&mut outbox, &room, Instant::now()).is_some());
    let state = with_config(&f, public_state(&f), |c| {
        c.max_nickname_size = river_core::room_state::member_info::NICKNAME_CEILING + 500
    });
    let room = LiveRoom::new(f.stored(state));
    let mut outbox = Outbox::default();
    outbox.command(
        &room,
        nick(&"n".repeat(river_core::room_state::member_info::NICKNAME_CEILING + 1)),
    );
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
}

#[test]
fn a_name_that_imitates_nobody_in_particular_is_the_users_to_choose() {
    // The web UI's nickname field does not compare against other members (it warns readers
    // instead), so neither does this: taking another member's name is allowed.
    let f = Fixture::new();
    let room = private_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Olive"));
    let (delta, _) = next_info(&mut outbox, &room);
    contract_takes(&room, delta);
}

#[test]
fn only_a_member_can_set_a_nickname_and_is_told_why_if_not() {
    let f = Fixture::new();
    let mut state = public_state(&f);
    state.members.members.clear();
    state.recent_messages.messages.truncate(1);
    let not_member = LiveRoom::new(f.stored(state));
    let mut stored = f.stored(public_state(&f));
    stored.self_sk = None;
    stored.self_vk = Some(f.me.verifying_key());
    let no_key = LiveRoom::new(stored);
    for room in [&not_member, &no_key] {
        let mut outbox = outbox();
        outbox.command(room, nick("Intruder"));
        assert!(next(&mut outbox, room, Instant::now()).is_none());
        let view = outbox.view(room, &minutes);
        assert!(view.send_block.is_some());
        assert!(view.own_nickname.unwrap().failed.is_some());
        assert_eq!(view.notices.len(), 1);
    }

    // The room itself refuses a record from a key that is no member's, whoever sends it.
    let outsider = SigningKey::from_bytes(&[9; 32]);
    let forged = AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: f.me_id(),
            version: 9,
            preferred_nickname: SealedBytes::public(b"Forged".to_vec()),
            deputies: Vec::new(),
        },
        &outsider,
    );
    let mut room = public_room(&f);
    assert!(
        room.apply_delta(ChatRoomStateV1Delta {
            member_info: Some(vec![forged]),
            ..Default::default()
        })
        .is_err()
    );
}

#[test]
fn a_banned_member_cannot_set_a_nickname() {
    let f = Fixture::new();
    let mut state = public_state(&f);
    state.bans.0.push(ban(&f, f.me_id()));
    let room = LiveRoom::new(f.stored(state));
    let mut outbox = outbox();
    outbox.command(&room, nick("Banned Mia"));
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    assert_eq!(
        outbox.view(&room, &minutes).notices[0].text,
        format!(
            "Your nickname couldn't be sent. {}",
            Blocked::Banned.reason()
        )
    );
}

#[test]
fn a_member_the_room_removed_for_inactivity_is_told_to_send_a_message_first() {
    // The room drops a member with no recent message as soon as it applies anything, taking
    // a record sent with their re-adding along: the contract's own apply shows it.
    let f = Fixture::new();
    let mut stored = f.stored(pruned(&f, public_state(&f)));
    stored.self_authorized_member = Some(admitted(&f));
    let room = LiveRoom::new(stored);
    let info = f.nickname(&f.me, "Lost");
    let mut copy = room.clone();
    copy.apply_delta(info_delta(&room, &info)).unwrap();
    assert!(copy.stored.room_state.member_info.member_info.is_empty());
    assert!(
        copy.stored.room_state.members.members.is_empty(),
        "the room drops the member again in the same apply"
    );
    assert!(
        outbox_view_blocks(&room),
        "and Save is refused before it is pressed"
    );

    let mut outbox = outbox();
    outbox.command(&room, nick("Back As"));
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    assert_eq!(
        outbox.view(&room, &minutes).notices[0].text,
        format!("Your nickname couldn't be sent. {PRUNED}")
    );
    // After a message re-adds them, the nickname goes.
    let mut room = room;
    let messages = sent(&mut outbox, &room, say("back"));
    assert_eq!(messages.len(), 1);
    assert!(outbox.answered(&mut room, Ok(())));
    let mut outbox = Outbox::default();
    outbox.command(&room, nick("Back As"));
    let (delta, _) = next_info(&mut outbox, &room);
    contract_takes(&room, delta);
}

#[test]
fn my_deputy_grants_survive_a_nickname_change() {
    let f = Fixture::new();
    let mut state = public_state(&f);
    state
        .member_info
        .member_info
        .push(AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: f.me_id(),
                version: 3,
                preferred_nickname: SealedBytes::public(b"Mia".to_vec()),
                deputies: vec![f.owner_id()],
            },
            &f.me,
        ));
    let room = LiveRoom::new(f.stored(state));
    let mut outbox = outbox();
    outbox.command(&room, nick("Mia With Deputy"));
    let (delta, info) = next_info(&mut outbox, &room);
    assert_eq!(info.member_info.version, 4);
    assert_eq!(info.member_info.deputies, [f.owner_id()]);
    contract_takes(&room, delta);
}

#[test]
fn nicknames_in_a_row_go_out_in_order_at_rising_versions_and_a_newer_one_replaces_one_unsent() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("One"));
    // One is out; two and three wait. Three replaces two, which was never sent.
    let (_, one) = next_info(&mut outbox, &room);
    outbox.command(&room, nick("Two"));
    outbox.command(&room, nick("Three"));
    assert!(
        next(&mut outbox, &room, Instant::now()).is_none(),
        "one at a time"
    );
    assert_eq!(shown_name(&outbox, &room), "Three");
    assert!(outbox.answered(&mut room, Ok(())));
    let (delta, three) = next_info(&mut outbox, &room);
    assert!(three.member_info.version > one.member_info.version);
    assert_eq!(
        three.member_info.preferred_nickname,
        SealedBytes::public(b"Three".to_vec())
    );
    let state = contract_takes(&room, delta);
    assert_eq!(state.member_info.canonical(f.me_id()), Some(&three));
    assert!(outbox.answered(&mut room, Ok(())));
    assert!(outbox.is_empty());
    assert_eq!(shown_name(&outbox, &room), "Three");
}

#[test]
fn a_nickname_goes_in_order_with_messages_written_around_it() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, say("before"));
    outbox.command(&room, nick("Between"));
    outbox.command(&room, say("after"));
    let mut order = Vec::new();
    while let Some(delta) = next(&mut outbox, &room, Instant::now()) {
        order.push(match (&delta.recent_messages, &delta.member_info) {
            (Some(m), None) => m[0].message.content.to_string_lossy(),
            (None, Some(i)) => format!("nick v{}", i[0].member_info.version),
            other => panic!("{other:?}"),
        });
        assert!(outbox.answered(&mut room, Ok(())));
    }
    assert_eq!(order.len(), 3);
    assert!(order[0].contains("before"));
    assert_eq!(order[1], "nick v1");
    assert!(order[2].contains("after"));
    assert!(outbox.is_empty());
}

#[test]
fn a_refused_nickname_stays_with_the_reason_and_retry_signs_it_again_discard_drops_it() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Try Me"));
    let (_, first) = next_info(&mut outbox, &room);
    assert!(outbox.answered(&mut room, Err("no room".into())));
    let view = outbox.view(&room, &minutes);
    let own = view.own_nickname.clone().unwrap();
    assert_eq!(
        own.failed.as_deref(),
        Some("Not saved. Freenet refused it: no room")
    );
    assert!(own.retry);
    assert_eq!(
        view.notices[0].text,
        "Your nickname couldn't be sent. Freenet refused it: no room"
    );
    // A failed change has no effect to show.
    assert_eq!(shown_name(&outbox, &room), "Mia");
    assert!(next(&mut outbox, &room, Instant::now()).is_none());

    assert!(outbox.command(&room, Action::Retry { entry: own.entry }));
    let (_, again) = next_info(&mut outbox, &room);
    assert_eq!(
        again.member_info.preferred_nickname,
        first.member_info.preferred_nickname
    );
    assert!(outbox.answered(&mut room, Err("still no".into())));
    let own = outbox.view(&room, &minutes).own_nickname.unwrap();
    assert!(outbox.command(&room, Action::Discard { entry: own.entry }));
    assert!(outbox.is_empty());
    assert!(outbox.view(&room, &minutes).own_nickname.is_none());
}

#[test]
fn a_nickname_the_network_brings_in_is_settled_and_an_unanswered_one_goes_again_unchanged() {
    let f = Fixture::new();
    let room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Net"));
    let (delta, info) = next_info(&mut outbox, &room);
    // The connection drops: it goes again, as the same signed record.
    outbox.requeue();
    let (_, again) = next_info(&mut outbox, &room);
    assert_eq!(again, info);
    // The network brings it in (another connection of mine, or the node's earlier copy).
    let mut other = room.clone();
    other.apply_delta(delta).unwrap();
    assert!(outbox.reconcile(&mut other));
    assert!(outbox.is_empty());
}

#[test]
fn a_newer_record_from_elsewhere_settles_mine_instead_of_being_overwritten() {
    // The web UI on another device published a later record while this was waiting.
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Mine"));
    let elsewhere = AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: f.me_id(),
            version: 5,
            preferred_nickname: SealedBytes::public(b"Elsewhere".to_vec()),
            deputies: Vec::new(),
        },
        &f.me,
    );
    room.apply_delta(ChatRoomStateV1Delta {
        member_info: Some(vec![elsewhere]),
        ..Default::default()
    })
    .unwrap();
    assert!(outbox.reconcile(&mut room));
    // Not dropped silently: said, and nothing more is reported by looking again.
    let view = outbox.view(&room, &minutes);
    assert!(view.notices[0].text.contains("replaced this one"));
    assert!(!outbox.reconcile(&mut room));
    assert_eq!(shown_name(&outbox, &room), "Elsewhere");
}

fn outbox_view_blocks(room: &LiveRoom) -> bool {
    Outbox::default()
        .view(room, &minutes)
        .nickname_block
        .as_deref()
        == Some(PRUNED)
}

/// A record of mine at `version` naming `name`, as another device would sign it.
fn record(f: &Fixture, version: u32, name: &str) -> AuthorizedMemberInfo {
    AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: f.me_id(),
            version,
            preferred_nickname: SealedBytes::public(name.as_bytes().to_vec()),
            deputies: Vec::new(),
        },
        &f.me,
    )
}

fn merge_info(room: &mut LiveRoom, info: AuthorizedMemberInfo) {
    room.apply_delta(ChatRoomStateV1Delta {
        member_info: Some(vec![info]),
        ..Default::default()
    })
    .unwrap();
}

// Review round: what a waiting nickname must never become ----------------------------------

#[test]
fn a_public_nickname_queued_before_the_room_turned_private_is_sealed_not_sent_plain() {
    let f = Fixture::new();
    let public = public_room(&f);
    let private = private_room(&f);
    // Queued, never sent.
    let mut outbox = outbox();
    outbox.command(&public, nick("Was Public"));
    let (delta, info) = next_info(&mut outbox, &private);
    assert!(matches!(
        info.member_info.preferred_nickname,
        SealedBytes::Private { .. }
    ));
    assert_eq!(
        river_core::ecies::unseal_bytes(&info.member_info.preferred_nickname, Some(&SECRET))
            .unwrap(),
        b"Was Public"
    );
    contract_takes(&private, delta);

    // Already sent once in the public room, sent again after the room turned private: a new
    // sealed record at a higher version, not the plain one again.
    let mut outbox = self::outbox();
    outbox.command(&public, nick("Was Sent"));
    let (_, plain) = next_info(&mut outbox, &public);
    outbox.requeue();
    let (_, again) = next_info(&mut outbox, &private);
    assert!(matches!(
        again.member_info.preferred_nickname,
        SealedBytes::Private { .. }
    ));
    assert!(again.member_info.version > plain.member_info.version);
}

#[test]
fn a_private_nickname_queued_before_the_room_turned_public_is_not_sent_sealed() {
    let f = Fixture::new();
    let private = private_room(&f);
    let public = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&private, nick("Was Private"));
    let (_, info) = next_info(&mut outbox, &public);
    assert_eq!(
        info.member_info.preferred_nickname,
        SealedBytes::public(b"Was Private".to_vec())
    );
}

#[test]
fn an_equal_version_record_that_wins_on_the_digest_is_superseded_and_never_cached() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Mine"));
    let (delta, mine) = next_info(&mut outbox, &room);
    let version = mine.member_info.version;
    // Another device signed a different record at the same version; find one the room ranks
    // above mine (Ed25519 is deterministic, so this is fixed).
    let other = (0..200)
        .map(|i| record(&f, version, &format!("Other {i}")))
        .find(|o| {
            let mut t = room.clone();
            merge_info(&mut t, o.clone());
            t.apply_delta(delta.clone()).unwrap();
            t.stored.room_state.member_info.canonical(f.me_id()) == Some(o)
        })
        .expect("some record outranks mine");
    merge_info(&mut room, other.clone());
    assert!(outbox.answered(&mut room, Ok(())));
    // Not "saved": said, and the cache holds nothing of mine.
    let view = outbox.view(&room, &minutes);
    assert!(view.notices[0].text.contains("replaced this one"));
    assert_eq!(room.stored.self_nickname.as_deref(), Some("Mia"));
    assert_eq!(room.stored.self_member_info, None);
    assert_eq!(
        shown_name(&outbox, &room),
        "Other 0".replace('0', &{
            let i = (0..200)
                .position(|i| record(&f, version, &format!("Other {i}")) == other)
                .unwrap();
            i.to_string()
        })
    );
    // An Unconfirmed nickname can be dropped.
    let own = view.own_nickname.unwrap();
    assert!(outbox.command(&room, Action::Discard { entry: own.entry }));
    assert!(outbox.is_empty());
}

#[test]
fn a_newer_record_that_arrives_before_sending_fails_it_as_superseded_not_as_banned() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Mine"));
    merge_info(&mut room, record(&f, 5, "Newer"));
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    let text = &outbox.view(&room, &minutes).notices[0].text;
    assert!(text.contains("replaced this one"), "{text}");
    assert!(!text.contains("banned"));
}

#[test]
fn an_equal_version_record_that_won_settles_mine_on_reconcile() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Mine"));
    let (delta, mine) = next_info(&mut outbox, &room);
    let other = (0..200)
        .map(|i| record(&f, mine.member_info.version, &format!("Race {i}")))
        .find(|o| {
            let mut t = room.clone();
            merge_info(&mut t, o.clone());
            t.apply_delta(delta.clone()).unwrap();
            t.stored.room_state.member_info.canonical(f.me_id()) == Some(o)
        })
        .unwrap();
    merge_info(&mut room, other);
    assert!(outbox.reconcile(&mut room));
    let view = outbox.view(&room, &minutes);
    assert!(view.notices[0].text.contains("replaced this one"));
    // Mine was sent, so it is "took it, replaced", with no dialog "Not saved".
    assert!(
        !view
            .own_nickname
            .unwrap()
            .failed
            .unwrap()
            .starts_with("Not saved")
    );
}

#[test]
fn deputy_grants_revoked_elsewhere_are_not_brought_back_by_a_waiting_nickname() {
    let f = Fixture::new();
    let mut state = public_state(&f);
    state
        .member_info
        .member_info
        .push(AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: f.me_id(),
                version: 3,
                preferred_nickname: SealedBytes::public(b"Mia".to_vec()),
                deputies: vec![f.owner_id()],
            },
            &f.me,
        ));
    let mut room = LiveRoom::new(f.stored(state));
    let mut outbox = outbox();
    outbox.command(&room, nick("Waiting"));
    // Another device revokes the grant, at a higher version, before this is sent.
    merge_info(&mut room, record(&f, 6, "Mia"));
    let (delta, info) = next_info(&mut outbox, &room);
    assert!(
        info.member_info.deputies.is_empty(),
        "the revoked grant is not re-granted"
    );
    assert!(info.member_info.version > 6);
    let state = contract_takes(&room, delta);
    assert!(state.member_info.deputies_of(f.me_id()).is_empty());
}

#[test]
fn no_version_left_fails_the_nickname_instead_of_wrapping() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    merge_info(&mut room, record(&f, u32::MAX, "Last"));
    let mut outbox = outbox();
    outbox.command(&room, nick("One More"));
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    assert_eq!(
        outbox.view(&room, &minutes).notices[0].text,
        format!("Your nickname couldn't be sent. {NO_VERSION_LEFT}")
    );
}

#[test]
fn an_intent_never_prints_the_nickname() {
    let shown = format!(
        "{:?}",
        Intent::Nickname {
            nickname: "hunter2-secret".into()
        }
    );
    assert!(!shown.contains("hunter2"));
}

#[test]
fn the_cached_record_decides_the_version_when_the_room_is_behind() {
    // Another connection of mine already published version 7; the room copy here has older.
    let f = Fixture::new();
    let mut stored = f.stored(public_state(&f));
    stored.self_member_info = Some(record(&f, 7, "Cached"));
    let room = LiveRoom::new(stored);
    let mut outbox = outbox();
    outbox.command(&room, nick("Next"));
    let (_, info) = next_info(&mut outbox, &room);
    assert_eq!(info.member_info.version, 8);
}

#[test]
fn a_sent_private_nickname_is_resent_as_is_after_a_rotation_and_a_retry_signs_higher() {
    let f = Fixture::new();
    let mut room = private_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Rotating"));
    let (_, first) = next_info(&mut outbox, &room);
    assert_eq!(
        first.member_info.preferred_nickname.secret_version(),
        Some(0)
    );
    // The secret rotates; the connection drops. The record the node may hold goes again
    // unchanged, not sealed under the new secret.
    let v1 = [7u8; 32];
    let mut state = room.stored.room_state.clone();
    add_secret(&f, &mut state, 1, &v1, &f.me);
    room.merge_state(&state).unwrap();
    outbox.requeue();
    let (_, again) = next_info(&mut outbox, &room);
    assert_eq!(again, first);
    // Refused: a retry signs it again, higher, under the current secret.
    assert!(outbox.answered(&mut room, Err("no".into())));
    let own = outbox.view(&room, &minutes).own_nickname.unwrap();
    assert!(outbox.command(&room, Action::Retry { entry: own.entry }));
    let (_, retried) = next_info(&mut outbox, &room);
    assert!(retried.member_info.version > first.member_info.version);
    assert_eq!(
        retried.member_info.preferred_nickname.secret_version(),
        Some(1)
    );
}

#[test]
fn a_private_nickname_at_the_limit_is_sealed_and_the_room_takes_it() {
    let f = Fixture::new();
    for limit in [
        16usize,
        river_core::room_state::member_info::NICKNAME_CEILING,
    ] {
        let state = with_config(&f, f.private_state(), |c| c.max_nickname_size = limit);
        let room = LiveRoom::new(f.stored(state));
        let mut outbox = Outbox::default();
        outbox.command(&room, nick(&"n".repeat(limit)));
        let (delta, info) = next_info(&mut outbox, &room);
        assert_eq!(info.member_info.preferred_nickname.declared_len(), limit);
        let state = contract_takes(&room, delta);
        assert_eq!(
            state.member_info.canonical(f.me_id()),
            Some(&info),
            "limit {limit}"
        );
        // One more byte is refused here.
        let mut outbox = Outbox::default();
        outbox.command(&room, nick(&"n".repeat(limit + 1)));
        assert!(next(&mut outbox, &room, Instant::now()).is_none());
    }
}

#[test]
fn the_dialog_follows_a_real_outbox_view() {
    use crate::chat::{Chat, LinkStatus, Source};
    let f = Fixture::new();
    let room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Via Outbox"));
    let view = outbox.view(&room, &minutes);
    let uid = view.member(view.me).unwrap().uid.clone();
    let mut chat = Chat::new(Source::Node(LinkStatus::Live), vec![view], 0, 0, 0);
    chat.open_member(&uid);
    // Sending: the dialog's state is the outbox's, and a second change waits.
    assert_eq!(
        chat.room().unwrap().own_nickname.as_ref().unwrap().failed,
        None
    );
    chat.nickname.replace_text("Another");
    chat.submit_nickname();
    assert!(chat.outgoing.is_empty());
    // Refused: the same view says why and a new change goes.
    let mut room = room;
    next(&mut outbox, &room, Instant::now()).unwrap();
    assert!(outbox.answered(&mut room, Err("nope".into())));
    let view = outbox.view(&room, &minutes);
    chat.set_rooms(vec![view]);
    let own = chat.room().unwrap().own_nickname.clone().unwrap();
    assert_eq!(
        own.failed.as_deref(),
        Some("Not saved. Freenet refused it: nope")
    );
    chat.submit_nickname();
    assert_eq!(chat.outgoing.len(), 1);
}

#[test]
fn naming_yourself_what_you_already_are_is_the_dialogs_no_op_not_the_outboxs() {
    // The dialog closes without sending when the name is unchanged (the web UI's own rule);
    // the outbox below it takes whatever it is given and publishes a new version.
    let f = Fixture::new();
    let room = private_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Mia"));
    let (_, info) = next_info(&mut outbox, &room);
    assert_eq!(info.member_info.version, 1);
}

#[test]
fn names_read_the_same_here_as_in_the_browser() {
    use crate::chat::visual_fold::{contains_hidden_chars, sanitize_display_name};
    // (name, the web's input check refuses it, what every client shows)
    let cases: &[(&str, bool, &str)] = &[
        ("Bo\u{200D}b", false, "Bob"),
        ("\u{3164}", true, "Unnamed"),
        ("\u{2800}", true, "Unnamed"),
        ("a\u{2028}b", true, "a b"),
        ("\u{1f6e1}", true, "Unnamed"),
        ("Ian\u{E0100}", true, "Ian"),
        ("\u{8fbb}\u{E0100}", false, "\u{8fbb}\u{E0100}"),
        (
            "\u{5c71}\u{7530}\u{3000}\u{592a}\u{90ce}",
            false,
            "\u{5c71}\u{7530}\u{3000}\u{592a}\u{90ce}",
        ),
        ("Plain Name", false, "Plain Name"),
    ];
    for (name, refused, shown) in cases {
        assert_eq!(contains_hidden_chars(name), *refused, "{name:?}");
        assert_eq!(sanitize_display_name(name), *shown, "{name:?}");
    }
    // A name set by another client shows sanitised in a room read here.
    let f = Fixture::new();
    let mut room = public_room(&f);
    merge_info(&mut room, record(&f, 2, "Bo\u{200D}b \u{1f6e1}"));
    assert_eq!(shown_name(&Outbox::default(), &room), "Bob");
    merge_info(&mut room, record(&f, 3, "\u{1f6e1}"));
    assert_eq!(shown_name(&Outbox::default(), &room), "Unnamed");
}

#[test]
fn a_nickname_that_failed_while_being_built_is_replaced_by_the_next_and_cannot_be_retried() {
    let f = Fixture::new();
    // A private room whose secret has not reached this computer.
    let mut state = f.private_state();
    state.secrets.encrypted_secrets.clear();
    let mut room = LiveRoom::new(f.stored(state));
    let mut outbox = outbox();
    outbox.command(&room, nick("A"));
    assert_eq!(outbox.view(&room, &minutes).notices.len(), 1);
    // The secret arrives; "B" is written, sent and held.
    let full = private_room(&f);
    room.merge_state(&full.stored.room_state).unwrap();
    let mut room = LiveRoom::new({
        let mut s = full.stored.clone();
        s.room_state = room.stored.room_state.clone();
        s
    });
    outbox.command(&room, nick("B"));
    let view = outbox.view(&room, &minutes);
    assert!(
        view.notices.is_empty(),
        "A's notice is gone: {:?}",
        view.notices
    );
    let (_, info) = next_info(&mut outbox, &room);
    assert_eq!(
        river_core::ecies::unseal_bytes(&info.member_info.preferred_nickname, Some(&SECRET))
            .unwrap(),
        b"B"
    );
    assert!(outbox.answered(&mut room, Ok(())));
    assert!(
        outbox.is_empty(),
        "nothing is left to retry and bring A back"
    );
    assert_eq!(shown_name(&outbox, &room), "B");
}

#[test]
fn a_record_the_network_brings_in_is_cached_for_rejoining_like_one_the_node_answered() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Echoed"));
    let (delta, info) = next_info(&mut outbox, &room);
    room.apply_delta(delta).unwrap();
    assert!(outbox.reconcile(&mut room));
    assert!(outbox.is_empty());
    assert_eq!(room.stored.self_member_info.as_ref(), Some(&info));
    assert_eq!(room.stored.self_nickname.as_deref(), Some("Echoed"));
}

#[test]
fn the_dialog_words_each_outcome_in_its_own_way() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Words"));
    let why = |o: &Outbox, r: &LiveRoom| o.view(r, &minutes).own_nickname.unwrap().failed;
    assert_eq!(why(&outbox, &room), None, "sending says nothing yet");
    next(&mut outbox, &room, Instant::now()).unwrap();
    // Not answered in time.
    assert!(outbox.expire(
        Duration::from_secs(1),
        Instant::now() + Duration::from_secs(5)
    ));
    assert!(why(&outbox, &room).unwrap().contains("didn't confirm"));
    // Refused.
    outbox.requeue();
    next(&mut outbox, &room, Instant::now()).unwrap();
    assert!(outbox.answered(&mut room, Err("full".into())));
    assert_eq!(
        why(&outbox, &room).unwrap(),
        "Not saved. Freenet refused it: full"
    );
}

#[test]
fn nothing_prints_a_nickname() {
    let secret = "hunter2-secret";
    let all = [
        format!(
            "{:?}",
            Action::SetNickname {
                nickname: secret.into()
            }
        ),
        format!(
            "{:?}",
            Action::CreateRoom {
                name: "r".into(),
                nickname: secret.into(),
                private: false
            }
        ),
        format!(
            "{:?}",
            Command {
                room: "r".into(),
                action: Action::SetNickname {
                    nickname: secret.into()
                }
            }
        ),
    ];
    for shown in all {
        assert!(!shown.contains("hunter2"), "{shown}");
    }
    let f = Fixture::new();
    let room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick(secret));
    assert!(!format!("{:?}", outbox.entries).contains("hunter2"));
}

#[test]
fn a_member_just_joined_or_with_a_removed_inviter_is_told_neutrally() {
    // Not listed as a member, for whatever reason: the message does not claim inactivity.
    assert!(!PRUNED.contains("removed you"));
    assert!(PRUNED.contains("doesn't list you as a member"));
}

#[test]
fn my_own_newer_nickname_is_not_reported_as_made_elsewhere() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Older"));
    let (_, older) = next_info(&mut outbox, &room);
    // Unanswered for a long time, so still out and still owed.
    assert!(outbox.expire(
        Duration::from_secs(1),
        Instant::now() + Duration::from_secs(5)
    ));
    outbox.command(&room, nick("Newer"));
    // The newer one is the room's canonical record (it went out on another connection, or the
    // network echoed it).
    let newer = AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: f.me_id(),
            version: older.member_info.version + 1,
            preferred_nickname: SealedBytes::public(b"Newer".to_vec()),
            deputies: Vec::new(),
        },
        &f.me,
    );
    merge_info(&mut room, newer);
    outbox.requeue();
    // Older is sent again first; before it is answered the network brings the newer in.
    assert!(outbox.reconcile(&mut room));
    let view = outbox.view(&room, &minutes);
    assert!(view.notices.is_empty(), "{:?}", view.notices);
    assert!(view.own_nickname.is_none());
    assert!(outbox.is_empty());
}

#[test]
fn a_newer_nickname_settling_by_the_nodes_answer_drops_an_older_one_still_owed() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Older"));
    next_info(&mut outbox, &room);
    assert!(outbox.expire(
        Duration::from_secs(1),
        Instant::now() + Duration::from_secs(5)
    ));
    outbox.command(&room, nick("Newer"));
    // A new connection owes nothing: the newer goes first only if the older is not queued, so
    // the older is answered (taken), then the newer.
    outbox.requeue();
    next_info(&mut outbox, &room);
    assert!(outbox.answered(&mut room, Ok(())));
    next_info(&mut outbox, &room);
    assert!(outbox.answered(&mut room, Ok(())));
    assert!(outbox.is_empty());
    assert!(outbox.view(&room, &minutes).notices.is_empty());
    assert_eq!(shown_name(&outbox, &room), "Newer");
}

#[test]
fn a_refused_nickname_is_replaced_by_a_newer_one_and_cannot_come_back() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Refused"));
    next_info(&mut outbox, &room);
    assert!(outbox.answered(&mut room, Err("no".into())));
    let refused = outbox.view(&room, &minutes).own_nickname.unwrap();
    assert!(refused.retry, "it was sent, and refused");
    outbox.command(&room, nick("Newer"));
    let view = outbox.view(&room, &minutes);
    assert!(view.notices.is_empty());
    assert!(!outbox.command(
        &room,
        Action::Retry {
            entry: refused.entry
        }
    ));
    let (_, info) = next_info(&mut outbox, &room);
    assert_eq!(
        info.member_info.preferred_nickname,
        SealedBytes::public(b"Newer".to_vec())
    );
}

#[test]
fn an_older_nickname_refused_after_a_newer_one_was_written_is_gone_and_cannot_come_back() {
    let f = Fixture::new();
    let mut room = public_room(&f);
    let mut outbox = outbox();
    outbox.command(&room, nick("Older"));
    next_info(&mut outbox, &room);
    // Not late: still out, so it is kept when the newer is written.
    outbox.command(&room, nick("Newer"));
    assert!(outbox.answered(&mut room, Err("no".into())));
    let view = outbox.view(&room, &minutes);
    assert!(view.notices.is_empty(), "{:?}", view.notices);
    assert!(view.own_nickname.is_none_or(|p| !p.retry));
    // Only the newer goes out, and is held.
    let (_, info) = next_info(&mut outbox, &room);
    assert_eq!(
        info.member_info.preferred_nickname,
        SealedBytes::public(b"Newer".to_vec())
    );
    assert!(outbox.answered(&mut room, Ok(())));
    assert!(outbox.is_empty());
    assert_eq!(shown_name(&outbox, &room), "Newer");
}

// The room's details ----------------------------------------------------------------------------

/// A public room the local identity OWNS, with the owner's key.
fn owner_public(f: &Fixture) -> LiveRoom {
    let mut stored = f.stored(public_state(f));
    stored.self_sk = Some(f.owner.clone());
    LiveRoom::new(stored)
}

/// A private room the local identity owns, holding the room's secret.
fn owner_private(f: &Fixture) -> LiveRoom {
    let mut state = f.private_state();
    add_secret(f, &mut state, 0, &SECRET, &f.owner);
    let mut stored = f.stored(state);
    stored.self_sk = Some(f.owner.clone());
    LiveRoom::new(stored)
}

fn rename(name: &str) -> Action {
    Action::SetRoomDetails {
        edit: ConfigEdit {
            name: Some(name.into()),
            ..ConfigEdit::default()
        },
    }
}

fn the_config(delta: &ChatRoomStateV1Delta) -> AuthorizedConfigurationV1 {
    delta
        .configuration
        .clone()
        .expect("a configuration goes out")
}

/// What the room CONTRACT does with `delta`: its own `apply_delta`, then its own `verify`.
fn through_the_contract(room: &LiveRoom, delta: &ChatRoomStateV1Delta) -> ChatRoomStateV1 {
    use freenet_scaffold::ComposableState;
    let params = room.params();
    let mut state = room.stored.room_state.clone();
    state
        .apply_delta(&ChatRoomStateV1::default(), &params, &Some(delta.clone()))
        .expect("the contract takes it");
    state
        .verify(&state, &params)
        .expect("and the state still verifies");
    state
}

#[test]
fn a_public_rename_is_signed_by_the_owner_and_the_contract_takes_it() {
    let f = Fixture::new();
    let mut room = owner_public(&f);
    let held = room.stored.room_state.configuration.configuration.clone();
    let mut outbox = outbox();
    let edit = ConfigEdit {
        name: Some("  New Town  ".into()),
        description: Some("About *us*".into()),
        max_members: Some(7),
        ..ConfigEdit::default()
    };
    assert!(outbox.command(&room, Action::SetRoomDetails { edit }));
    // Shown at once, with the edit on its way.
    let view = outbox.view(&room, &minutes);
    assert_eq!(view.name, "New Town");
    assert_eq!(view.description, "About *us*");
    assert_eq!(view.details.limits.max_members, 7);
    assert!(
        view.details
            .pending
            .as_ref()
            .is_some_and(|p| p.failed.is_none())
    );

    let delta = next(&mut outbox, &room, Instant::now()).expect("an update goes out");
    let config = the_config(&delta);
    assert!(config.verify_signature(&f.owner.verifying_key()).is_ok());
    let c = &config.configuration;
    assert_eq!(c.configuration_version, held.configuration_version + 1);
    assert_eq!(c.max_members, 7);
    // Nothing else moved.
    assert_eq!(c.max_message_size, held.max_message_size);
    assert_eq!(c.privacy_mode, held.privacy_mode);
    assert_eq!(c.owner_member_id, f.owner_id());
    assert_eq!(c.display.name, SealedBytes::public(b"New Town".to_vec()));
    assert_eq!(
        c.display.description,
        Some(SealedBytes::public(b"About *us*".to_vec()))
    );
    let state = through_the_contract(&room, &delta);
    assert_eq!(state.configuration, config);

    // Answered: done, and the room itself shows it.
    assert!(outbox.answered(&mut room, Ok(())));
    assert!(outbox.is_empty());
    let view = outbox.view(&room, &minutes);
    assert_eq!(
        (view.name.as_str(), view.details.pending),
        ("New Town", None)
    );
}

#[test]
fn a_private_rename_is_sealed_under_the_current_secret() {
    let f = Fixture::new();
    let mut room = owner_private(&f);
    let mut outbox = outbox();
    let edit = ConfigEdit {
        name: Some("Winter garden".into()),
        description: Some("Bulbs only".into()),
        ..ConfigEdit::default()
    };
    assert!(outbox.command(&room, Action::SetRoomDetails { edit }));
    let delta = next(&mut outbox, &room, Instant::now()).expect("an update goes out");
    let config = the_config(&delta);
    let display = &config.configuration.display;
    for (sealed, plain) in [
        (&display.name, "Winter garden"),
        (display.description.as_ref().unwrap(), "Bulbs only"),
    ] {
        let SealedBytes::Private {
            ciphertext,
            nonce,
            secret_version,
            declared_len_bytes,
        } = sealed
        else {
            panic!("a private room's name and description must be sealed, never plain");
        };
        assert_eq!(*secret_version, 0);
        assert_eq!(*declared_len_bytes as usize, plain.len());
        assert_eq!(
            decrypt_with_symmetric_key(&SECRET, ciphertext, nonce).unwrap(),
            plain.as_bytes()
        );
    }
    assert!(config.verify_signature(&f.owner.verifying_key()).is_ok());
    through_the_contract(&room, &delta);
    assert!(outbox.answered(&mut room, Ok(())));
    let view = outbox.view(&room, &minutes);
    assert_eq!(view.name, "Winter garden");
    assert_eq!(view.description, "Bulbs only");
    assert!(view.private);
}

#[test]
fn a_private_room_without_its_secret_refuses_and_sends_nothing() {
    let f = Fixture::new();
    // The secret is sealed to `me`, not the owner, so this computer has none.
    let mut stored = f.stored(f.private_state());
    stored.self_sk = Some(f.owner.clone());
    let room = LiveRoom::new(stored);
    assert!(room.current_secret().is_none());
    let mut outbox = outbox();
    assert!(outbox.command(&room, rename("Plain leak")));
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    let view = outbox.view(&room, &minutes);
    let pending = view.details.pending.expect("said");
    assert_eq!(
        pending.failed.as_deref(),
        Some(format!("Not saved. {}", Blocked::NoSecret.reason()).as_str())
    );
    assert_eq!(
        view.details.editable,
        crate::chat::model::Editable::Blocked(crate::rooms::room::NO_SECRET_TO_EDIT.into())
    );
}

#[test]
fn only_the_owner_changes_the_details() {
    let f = Fixture::new();
    let room = public_room(&f);
    assert_eq!(
        room.details().editable,
        crate::chat::model::Editable::No,
        "a member is only shown the details"
    );
    let mut outbox = outbox();
    assert!(outbox.command(&room, rename("Mine now")));
    assert!(next(&mut outbox, &room, Instant::now()).is_none());
    let pending = outbox.view(&room, &minutes).details.pending.unwrap();
    assert_eq!(
        pending.failed.as_deref(),
        Some(format!("Not saved. {NOT_OWNER}").as_str())
    );
    assert!(pending.retry);
    // The owner on a computer with only the PUBLIC half of the key cannot sign either.
    let mut stored = f.stored(public_state(&f));
    stored.self_sk = None;
    stored.self_vk = Some(f.owner.verifying_key());
    let keyless = LiveRoom::new(stored);
    assert_eq!(
        keyless.details().editable,
        crate::chat::model::Editable::Blocked(crate::rooms::room::NO_KEY_TO_EDIT.into())
    );
    let mut outbox = self::outbox();
    assert!(outbox.command(&keyless, rename("Nope")));
    assert!(next(&mut outbox, &keyless, Instant::now()).is_none());
}

#[test]
fn values_the_room_would_refuse_are_refused_before_anything_is_signed() {
    let f = Fixture::new();
    let room = owner_public(&f);
    let held = room.details();
    let long = "x".repeat(held.limits.max_room_name + 1);
    let cases: [(ConfigEdit, &str); 7] = [
        (
            ConfigEdit {
                name: Some("   ".into()),
                ..Default::default()
            },
            "Give the room a name.",
        ),
        (
            ConfigEdit {
                name: Some("a\u{202e}b".into()),
                ..Default::default()
            },
            "characters that don't show",
        ),
        (
            ConfigEdit {
                name: Some(long),
                ..Default::default()
            },
            "Max Room Name Size",
        ),
        (
            ConfigEdit {
                description: Some("d".repeat(held.limits.max_room_description + 1)),
                ..Default::default()
            },
            "Max Room Description Size",
        ),
        (
            ConfigEdit {
                max_members: Some(0),
                ..Default::default()
            },
            "Members must be at least 1.",
        ),
        (
            ConfigEdit {
                max_nickname_size: Some(river_core::room_state::member_info::NICKNAME_CEILING + 1),
                ..Default::default()
            },
            "at most",
        ),
        (ConfigEdit::default(), "nothing to change"),
    ];
    for (edit, expect) in cases {
        let mut outbox = outbox();
        assert!(outbox.command(&room, Action::SetRoomDetails { edit: edit.clone() }));
        assert!(
            next(&mut outbox, &room, Instant::now()).is_none(),
            "{edit:?} must not be sent"
        );
        let view = outbox.view(&room, &minutes);
        let said = view
            .notices
            .iter()
            .map(|n| n.text.clone())
            .collect::<Vec<_>>()
            .join(" / ");
        assert!(said.contains(expect), "{edit:?}: {said}");
    }
    // A name over the limit is also refused with the room's limit LOWERED by the same edit.
    let edit = ConfigEdit {
        max_room_name: Some(3),
        ..Default::default()
    };
    assert!(details_refusal(&held.limits, "Town", "", &edit).is_some());
    assert!(details_refusal(&held.limits, "Tow", "", &edit).is_none());
}

#[test]
fn a_held_nickname_limit_over_the_ceiling_does_not_block_an_unrelated_edit() {
    let f = Fixture::new();
    let ceiling = river_core::room_state::member_info::NICKNAME_CEILING;
    let state = with_config(&f, public_state(&f), |c| c.max_nickname_size = ceiling + 5);
    let mut stored = f.stored(state);
    stored.self_sk = Some(f.owner.clone());
    let room = LiveRoom::new(stored);
    let mut outbox = outbox();
    assert!(outbox.command(&room, rename("Still works")));
    let delta = next(&mut outbox, &room, Instant::now()).expect("sent");
    // Lowered to what the room enforces anyway, as riverctl does.
    assert_eq!(the_config(&delta).configuration.max_nickname_size, ceiling);
    through_the_contract(&room, &delta);
}

#[test]
fn edits_made_in_a_row_become_one_change_at_the_next_version() {
    let f = Fixture::new();
    let room = owner_public(&f);
    let held = room
        .stored
        .room_state
        .configuration
        .configuration
        .configuration_version;
    let mut outbox = outbox();
    assert!(outbox.command(&room, rename("First")));
    assert!(outbox.command(
        &room,
        Action::SetRoomDetails {
            edit: ConfigEdit {
                max_user_bans: Some(3),
                ..Default::default()
            }
        }
    ));
    let delta = next(&mut outbox, &room, Instant::now()).expect("sent");
    let c = the_config(&delta).configuration;
    assert_eq!(c.configuration_version, held + 1);
    assert_eq!(c.display.name, SealedBytes::public(b"First".to_vec()));
    assert_eq!(c.max_user_bans, 3);
    // And one on its way keeps its turn, with the next one built on top of it.
    assert!(outbox.command(
        &room,
        Action::SetRoomDetails {
            edit: ConfigEdit {
                max_members: Some(9),
                ..Default::default()
            }
        }
    ));
    assert!(
        next(&mut outbox, &room, Instant::now()).is_none(),
        "one at a time"
    );
}

#[test]
fn a_change_made_while_another_is_on_its_way_keeps_the_one_on_its_way() {
    let f = Fixture::new();
    let mut room = owner_public(&f);
    let mut outbox = outbox();
    assert!(outbox.command(&room, rename("Renamed")));
    let first = the_config(&next(&mut outbox, &room, Instant::now()).unwrap());
    // Out, unanswered. A second change, to another field only.
    assert!(outbox.command(
        &room,
        Action::SetRoomDetails {
            edit: ConfigEdit {
                max_user_bans: Some(4),
                ..Default::default()
            }
        }
    ));
    assert!(outbox.answered(&mut room, Ok(())));
    let second = the_config(&next(&mut outbox, &room, Instant::now()).unwrap());
    assert!(second.configuration.configuration_version > first.configuration.configuration_version);
    assert_eq!(second.configuration.max_user_bans, 4);
    assert_eq!(
        second.configuration.display.name,
        SealedBytes::public(b"Renamed".to_vec()),
        "the rename the first change made is not undone by the second"
    );
}

/// The owner's OTHER device signs `edit` at `version` on top of `room`'s configuration.
fn other_device(
    f: &Fixture,
    room: &LiveRoom,
    version: u32,
    edit: impl FnOnce(&mut Configuration),
) -> ChatRoomStateV1Delta {
    let mut c = room.stored.room_state.configuration.configuration.clone();
    edit(&mut c);
    c.configuration_version = version;
    ChatRoomStateV1Delta {
        configuration: Some(AuthorizedConfigurationV1::new(c, &f.owner)),
        ..Default::default()
    }
}

#[test]
fn a_change_waiting_when_the_owners_other_device_changed_the_room_goes_on_top_of_it() {
    let f = Fixture::new();
    let mut room = owner_public(&f);
    let mut outbox = outbox();
    assert!(outbox.command(&room, rename("Mine")));
    // Before it is sent, the other device sets the member cap at the version this was built at.
    let version = the_config(&outbox.entries[0].delta(&room))
        .configuration
        .configuration_version;
    let theirs = other_device(&f, &room, version, |c| c.max_members = 12);
    room.apply_delta(theirs).unwrap();
    assert!(!outbox.reconcile(&mut room), "not ours, so not done");
    let delta = next(&mut outbox, &room, Instant::now()).expect("still goes out");
    let c = the_config(&delta).configuration;
    assert!(
        c.configuration_version > version,
        "signed again above theirs"
    );
    assert_eq!(c.max_members, 12, "their change is kept");
    assert_eq!(c.display.name, SealedBytes::public(b"Mine".to_vec()));
    through_the_contract(&room, &delta);
}

#[test]
fn a_change_sent_and_outranked_by_the_other_device_is_said_to_be_replaced() {
    let f = Fixture::new();
    let mut room = owner_public(&f);
    let mut outbox = outbox();
    assert!(outbox.command(&room, rename("Mine")));
    let mine = the_config(&next(&mut outbox, &room, Instant::now()).unwrap());
    let version = mine.configuration.configuration_version;
    // The other device gets in first, at a HIGHER version.
    let theirs = other_device(&f, &room, version + 1, |c| c.max_members = 12);
    room.apply_delta(theirs).unwrap();
    assert!(outbox.reconcile(&mut room));
    let view = outbox.view(&room, &minutes);
    let pending = view.details.pending.expect("not dropped silently");
    assert_eq!(pending.failed.as_deref(), Some(CONFIG_SUPERSEDED));
    // The node's answer (taken) changes nothing more: the room still holds theirs.
    assert!(!outbox.answered(&mut room, Ok(())));
    assert_eq!(
        room.stored
            .room_state
            .configuration
            .configuration
            .max_members,
        12
    );
    assert_eq!(room.name(), "Town");
    // Discard clears it.
    let id = outbox.view(&room, &minutes).details.pending.unwrap().entry;
    assert!(outbox.command(&room, Action::Discard { entry: id }));
    assert!(outbox.is_empty());
}

#[test]
fn equal_versions_are_settled_by_the_contracts_rank() {
    let f = Fixture::new();
    // The other device's change differs between runs, and with it its signature digest, so the
    // tie goes each way: both outcomes must be reached and each must be said right.
    let (mut saw_theirs, mut saw_ours) = (false, false);
    for cap in 10..200usize {
        let mut room = owner_public(&f);
        let mut outbox = outbox();
        assert!(outbox.command(&room, rename("Mine")));
        let mine = the_config(&next(&mut outbox, &room, Instant::now()).unwrap());
        let version = mine.configuration.configuration_version;
        let theirs = other_device(&f, &room, version, |c| c.max_members = cap);
        let theirs_config = theirs.configuration.clone().unwrap();
        // Whichever outranks wins in the room itself; the entry's state must say the same.
        room.apply_delta(through_the_contract_delta(&mine)).unwrap();
        room.apply_delta(theirs).unwrap();
        let params = room.params();
        let theirs_wins = theirs_config.rank(&params) > mine.rank(&params);
        outbox.reconcile(&mut room);
        let pending = outbox.view(&room, &minutes).details.pending;
        if theirs_wins {
            saw_theirs = true;
            assert_eq!(
                room.stored.room_state.configuration.signature,
                theirs_config.signature
            );
            assert_eq!(pending.unwrap().failed.as_deref(), Some(CONFIG_SUPERSEDED));
            assert_eq!(config_state(&room, &mine), ConfigState::Superseded);
        } else {
            saw_ours = true;
            assert_eq!(
                room.stored.room_state.configuration.signature,
                mine.signature
            );
            assert!(pending.is_none(), "ours holds, so it is done");
            assert!(outbox.is_empty());
            assert_eq!(config_state(&room, &mine), ConfigState::Ours);
        }
        if saw_theirs && saw_ours {
            return;
        }
    }
    panic!("the tie never went both ways (theirs: {saw_theirs}, ours: {saw_ours})");
}

fn through_the_contract_delta(config: &AuthorizedConfigurationV1) -> ChatRoomStateV1Delta {
    config_delta(config)
}

#[test]
fn a_change_waiting_when_the_room_turns_private_is_never_sent_plain() {
    let f = Fixture::new();
    let public = owner_public(&f);
    let mut outbox = outbox();
    assert!(outbox.command(&public, rename("Quiet")));
    // The room becomes private (with the owner's secret) before the change goes out.
    let private = owner_private(&f);
    let delta = next(&mut outbox, &private, Instant::now()).expect("sent, built again");
    let config = the_config(&delta);
    assert!(
        !config.configuration.display.name.is_public(),
        "a name written while the room was public must not go out plain into a private one"
    );
    assert_eq!(
        config.configuration.privacy_mode,
        private
            .stored
            .room_state
            .configuration
            .configuration
            .privacy_mode
    );
    through_the_contract(&private, &delta);
}

#[test]
fn a_refused_change_is_signed_again_at_a_new_version_on_retry() {
    let f = Fixture::new();
    let mut room = owner_public(&f);
    let mut outbox = outbox();
    assert!(outbox.command(&room, rename("Try")));
    let first = the_config(&next(&mut outbox, &room, Instant::now()).unwrap());
    assert!(outbox.answered(&mut room, Err("busy".into())));
    let view = outbox.view(&room, &minutes);
    let pending = view.details.pending.unwrap();
    assert_eq!(
        pending.failed.as_deref(),
        Some("Not saved. Freenet refused it: busy")
    );
    // The shown room does not carry a refused change.
    assert_eq!(view.name, "Town");
    assert!(outbox.command(
        &room,
        Action::Retry {
            entry: pending.entry
        }
    ));
    let second = the_config(&next(&mut outbox, &room, Instant::now()).unwrap());
    assert!(
        second.configuration.configuration_version > first.configuration.configuration_version
            || second.signature != first.signature
    );
    assert!(outbox.answered(&mut room, Ok(())));
    assert!(outbox.is_empty());
    assert_eq!(room.name(), "Try");
}

#[test]
fn the_network_bringing_the_change_in_settles_it() {
    let f = Fixture::new();
    let mut room = owner_public(&f);
    let mut outbox = outbox();
    assert!(outbox.command(&room, rename("Seen")));
    let delta = next(&mut outbox, &room, Instant::now()).unwrap();
    room.apply_delta(delta).unwrap();
    assert!(outbox.reconcile(&mut room));
    assert!(outbox.is_empty());
}

#[test]
fn a_change_for_a_clone_of_the_edit_is_merged_by_field() {
    let a = ConfigEdit {
        name: Some("A".into()),
        max_members: Some(1),
        ..Default::default()
    };
    let b = ConfigEdit {
        name: Some("B".into()),
        max_user_bans: Some(2),
        ..Default::default()
    };
    let merged = b.over(&a);
    assert_eq!(merged.name.as_deref(), Some("B"));
    assert_eq!(merged.max_members, Some(1));
    assert_eq!(merged.max_user_bans, Some(2));
}

#[test]
fn a_refused_older_change_is_replaced_by_a_newer_one_and_cannot_come_back() {
    let f = Fixture::new();
    let mut room = owner_public(&f);
    let mut outbox = outbox();
    assert!(outbox.command(&room, rename("Old")));
    let _ = next(&mut outbox, &room, Instant::now()).unwrap();
    // A newer change while the older is out and unanswered: it waits its turn.
    assert!(outbox.command(&room, rename("New")));
    // The older is refused: nothing is left to Retry it back above the newer.
    assert!(outbox.answered(&mut room, Err("busy".into())));
    let pending = outbox
        .view(&room, &minutes)
        .details
        .pending
        .expect("the newer");
    assert_eq!(pending.failed, None, "only the newer is left, waiting");
    let delta = next(&mut outbox, &room, Instant::now()).expect("the newer goes out");
    assert_eq!(
        the_config(&delta).configuration.display.name,
        SealedBytes::public(b"New".to_vec())
    );
    assert!(outbox.answered(&mut room, Ok(())));
    assert!(outbox.is_empty());
    assert_eq!(room.name(), "New");
}

#[test]
fn my_own_newer_change_is_not_reported_as_made_elsewhere() {
    let f = Fixture::new();
    let mut room = owner_public(&f);
    let mut outbox = outbox();
    assert!(outbox.command(&room, rename("One")));
    let _ = next(&mut outbox, &room, Instant::now()).unwrap();
    assert!(outbox.command(&room, rename("Two")));
    // The older is answered, then the newer goes out and is answered.
    assert!(outbox.answered(&mut room, Ok(())));
    let _ = next(&mut outbox, &room, Instant::now()).unwrap();
    assert!(outbox.answered(&mut room, Ok(())));
    assert!(outbox.is_empty(), "nothing is left saying it was replaced");
    assert_eq!(room.name(), "Two");
    // The network bringing the newer in first settles the older without a notice too.
    let mut room = owner_public(&f);
    let mut outbox = self::outbox();
    assert!(outbox.command(&room, rename("One")));
    let first = next(&mut outbox, &room, Instant::now()).unwrap();
    assert!(outbox.command(&room, rename("Two")));
    room.apply_delta(first).unwrap();
    assert!(outbox.reconcile(&mut room));
    // The older's answer is still owed; it is consumed, and the newer goes out after it.
    assert!(!outbox.answered(&mut room, Ok(())));
    let second = next(&mut outbox, &room, Instant::now()).expect("the newer is sent");
    room.apply_delta(second).unwrap();
    outbox.reconcile(&mut room);
    assert!(outbox.view(&room, &minutes).notices.is_empty());
    assert!(outbox.view(&room, &minutes).details.pending.is_none());
}

#[test]
fn nothing_prints_a_rooms_name_or_description() {
    let f = Fixture::new();
    let room = owner_private(&f);
    let mut outbox = outbox();
    let action = Action::SetRoomDetails {
        edit: ConfigEdit {
            name: Some("Hidden Name".into()),
            description: Some("Hidden Description".into()),
            ..Default::default()
        },
    };
    assert!(!format!("{action:?}").contains("Hidden"));
    assert!(outbox.command(&room, action));
    assert!(!format!("{:?}", outbox.entries).contains("Hidden"));
}

/// Take the delta of the next update, after the owed answer of any entry before it.
fn after_answer(outbox: &mut Outbox, room: &mut LiveRoom, ok: bool) {
    let result = if ok { Ok(()) } else { Err("busy".into()) };
    outbox.answered(room, result);
}

#[test]
fn a_change_the_dialog_never_showed_does_not_come_back_in_a_newer_one() {
    let f = Fixture::new();
    // The room replaced the name this owner wrote with the other device's.
    let mut room = owner_public(&f);
    let mut outbox = outbox();
    assert!(outbox.command(&room, rename("Alpha")));
    let mine = the_config(&next(&mut outbox, &room, Instant::now()).unwrap());
    let version = mine.configuration.configuration_version;
    let theirs = other_device(&f, &room, version + 1, |c| {
        c.display.name = SealedBytes::public(b"Beta".to_vec())
    });
    room.apply_delta(theirs).unwrap();
    outbox.reconcile(&mut room);
    assert_eq!(
        outbox
            .view(&room, &minutes)
            .details
            .pending
            .unwrap()
            .failed
            .as_deref(),
        Some(CONFIG_SUPERSEDED)
    );
    // Then only the description is edited: the name Alpha is not signed back over Beta.
    assert!(outbox.command(
        &room,
        Action::SetRoomDetails {
            edit: ConfigEdit {
                description: Some("About".into()),
                ..Default::default()
            }
        }
    ));
    after_answer(&mut outbox, &mut room, true);
    let c = the_config(&next(&mut outbox, &room, Instant::now()).unwrap()).configuration;
    assert_eq!(c.display.name, SealedBytes::public(b"Beta".to_vec()));
    assert_eq!(
        c.display.description,
        Some(SealedBytes::public(b"About".to_vec()))
    );
}

#[test]
fn a_refused_change_the_dialog_did_not_show_is_not_merged_into_a_newer_one() {
    let f = Fixture::new();
    let mut room = owner_public(&f);
    let held_cap = room
        .stored
        .room_state
        .configuration
        .configuration
        .max_members;
    let mut outbox = outbox();
    assert!(outbox.command(
        &room,
        Action::SetRoomDetails {
            edit: ConfigEdit {
                max_members: Some(3),
                ..Default::default()
            }
        }
    ));
    let _ = next(&mut outbox, &room, Instant::now()).unwrap();
    assert!(outbox.answered(&mut room, Err("busy".into())));
    assert!(outbox.command(&room, rename("Only the name")));
    let c = the_config(&next(&mut outbox, &room, Instant::now()).unwrap()).configuration;
    assert_eq!(
        c.max_members, held_cap,
        "the refused cap is not resurrected"
    );
}

#[test]
fn a_waiting_change_is_built_again_when_the_other_device_gets_in_at_its_version() {
    let f = Fixture::new();
    // Whichever way the digest tie goes, the other device's change must survive.
    let (mut saw_theirs, mut saw_ours) = (false, false);
    for cap in 10..200usize {
        let mut room = owner_public(&f);
        let mut outbox = outbox();
        assert!(outbox.command(&room, rename("Mine")));
        let mine = outbox.entries[0].config.clone().unwrap();
        let version = mine.configuration.configuration_version;
        let theirs = other_device(&f, &room, version, |c| c.max_members = cap);
        let theirs_rank = theirs.configuration.clone().unwrap().rank(&room.params());
        room.apply_delta(theirs).unwrap();
        if theirs_rank > mine.rank(&room.params()) {
            saw_theirs = true;
        } else {
            saw_ours = true;
        }
        let delta = next(&mut outbox, &room, Instant::now()).expect("goes out");
        let c = the_config(&delta).configuration;
        assert_eq!(c.max_members, cap, "their change is kept (cap {cap})");
        assert!(c.configuration_version > version);
        through_the_contract(&room, &delta);
        if saw_theirs && saw_ours {
            return;
        }
    }
    panic!("the tie never went both ways");
}

#[test]
fn a_change_built_on_one_that_is_then_refused_is_built_again() {
    let f = Fixture::new();
    let mut room = owner_public(&f);
    let held_bans = room
        .stored
        .room_state
        .configuration
        .configuration
        .max_user_bans;
    let mut outbox = outbox();
    assert!(outbox.command(
        &room,
        Action::SetRoomDetails {
            edit: ConfigEdit {
                max_user_bans: Some(3),
                ..Default::default()
            }
        }
    ));
    let _ = next(&mut outbox, &room, Instant::now()).unwrap();
    // Built on top of the one on its way, which carries bans = 3.
    assert!(outbox.command(&room, rename("B")));
    assert_eq!(
        outbox.entries[1]
            .config
            .as_ref()
            .unwrap()
            .configuration
            .max_user_bans,
        3
    );
    assert!(outbox.answered(&mut room, Err("busy".into())));
    let c = the_config(&next(&mut outbox, &room, Instant::now()).unwrap()).configuration;
    assert_eq!(c.display.name, SealedBytes::public(b"B".to_vec()));
    assert_eq!(
        c.max_user_bans, held_bans,
        "the refused values are not published"
    );
}

#[test]
fn limits_are_bounded_by_the_contracts_u32_and_a_sanity_ceiling() {
    let f = Fixture::new();
    let held = owner_public(&f).details();
    let edit = |f: fn(&mut ConfigEdit, usize), v: usize| {
        let mut e = ConfigEdit::default();
        f(&mut e, v);
        details_refusal(
            &held.limits,
            &held.name_source,
            &held.description_source,
            &e,
        )
    };
    let big = u32::MAX as usize + 1;
    assert!(
        edit(|e, v| e.max_members = Some(v), big)
            .unwrap()
            .contains("at most 4294967295")
    );
    assert!(edit(|e, v| e.max_message_size = Some(v), usize::MAX).is_some());
    // Each limit has a ceiling, and the ceiling itself is allowed.
    let setters: [fn(&mut ConfigEdit, usize); 7] = [
        |e, v| e.max_members = Some(v),
        |e, v| e.max_recent_messages = Some(v),
        |e, v| e.max_message_size = Some(v),
        |e, v| e.max_user_bans = Some(v),
        |e, v| e.max_nickname_size = Some(v),
        |e, v| e.max_room_name = Some(v),
        |e, v| e.max_room_description = Some(v),
    ];
    for (i, set) in setters.iter().enumerate() {
        assert!(LIMIT_SANITY_CEILINGS[i] <= u32::MAX as usize);
        // Above the ceiling is refused; the web UI's defaults are far below every one.
        assert!(edit(*set, LIMIT_SANITY_CEILINGS[i] + 1).is_some(), "{i}");
        let defaults = [200, 100, 1000, 10, 50, 100, 500];
        assert!(LIMIT_SANITY_CEILINGS[i] >= defaults[i] * 2, "{i}");
    }
}

#[test]
fn lowering_a_limit_below_what_the_room_uses_is_said_in_plain_words() {
    use crate::chat::model::{Details, Usage};
    let d = Details {
        usage: Usage {
            members: 7,
            bans: 4,
            messages: 90,
        },
        ..Details::default()
    };
    let w = lowering_warnings(
        &d,
        &ConfigEdit {
            max_members: Some(3),
            max_user_bans: Some(1),
            max_recent_messages: Some(50),
            ..Default::default()
        },
    );
    assert_eq!(w.len(), 3);
    assert!(w[0].contains("Lowering Members to 3 removes 4 members, starting with the longest invite chains. This cannot be undone."));
    assert!(w[1].contains("3 of the room's 4 bans"));
    assert!(w[2].contains("oldest 40 of the room's 90 messages"));
    // At or above what is used: nothing to warn about.
    assert!(
        lowering_warnings(
            &d,
            &ConfigEdit {
                max_members: Some(7),
                max_recent_messages: Some(90),
                max_user_bans: Some(4),
                ..Default::default()
            }
        )
        .is_empty()
    );
}

const SECRET1: [u8; 32] = [43; 32];

fn owner_private_rotated(f: &Fixture) -> LiveRoom {
    let mut state = f.private_state();
    add_secret(f, &mut state, 0, &SECRET, &f.owner);
    add_secret(f, &mut state, 1, &SECRET1, &f.owner);
    let mut stored = f.stored(state);
    stored.self_sk = Some(f.owner.clone());
    LiveRoom::new(stored)
}

#[test]
fn a_private_rename_after_a_rotation_is_sealed_under_the_newest_secret() {
    let f = Fixture::new();
    let room = owner_private_rotated(&f);
    assert_eq!(room.current_secret().map(|(_, v)| v), Some(1));
    let mut outbox = outbox();
    assert!(outbox.command(&room, rename("After rotation")));
    let delta = next(&mut outbox, &room, Instant::now()).unwrap();
    let c = the_config(&delta).configuration;
    let SealedBytes::Private {
        ciphertext,
        nonce,
        secret_version,
        declared_len_bytes,
    } = &c.display.name
    else {
        panic!("sealed");
    };
    assert_eq!(*secret_version, 1);
    assert_eq!(*declared_len_bytes as usize, "After rotation".len());
    assert_eq!(
        decrypt_with_symmetric_key(&SECRET1, ciphertext, nonce).unwrap(),
        b"After rotation"
    );
    through_the_contract(&room, &delta);
}

#[test]
fn a_change_waiting_when_the_secret_rotates_is_sealed_again_under_the_new_one() {
    let f = Fixture::new();
    let before = owner_private(&f);
    let mut outbox = outbox();
    assert!(outbox.command(&before, rename("Queued")));
    let after = owner_private_rotated(&f);
    let delta = next(&mut outbox, &after, Instant::now()).unwrap();
    let c = the_config(&delta).configuration;
    assert_eq!(c.display.name.secret_version(), Some(1));
    through_the_contract(&after, &delta);
}

#[test]
fn clearing_a_private_description_publishes_none_and_trimming_applies() {
    let f = Fixture::new();
    let room = owner_private(&f);
    assert!(room.details().description_source == "Plants only");
    let mut outbox = outbox();
    assert!(outbox.command(
        &room,
        Action::SetRoomDetails {
            edit: ConfigEdit {
                description: Some("   ".into()),
                ..Default::default()
            }
        }
    ));
    let delta = next(&mut outbox, &room, Instant::now()).unwrap();
    assert_eq!(the_config(&delta).configuration.display.description, None);
    through_the_contract(&room, &delta);
    // Padding is trimmed from a description and a name.
    let room = owner_public(&f);
    let mut outbox = self::outbox();
    assert!(outbox.command(
        &room,
        Action::SetRoomDetails {
            edit: ConfigEdit {
                name: Some("  Padded  ".into()),
                description: Some("\n  words \n".into()),
                ..Default::default()
            }
        }
    ));
    let c = the_config(&next(&mut outbox, &room, Instant::now()).unwrap()).configuration;
    assert_eq!(c.display.name, SealedBytes::public(b"Padded".to_vec()));
    assert_eq!(
        c.display.description,
        Some(SealedBytes::public(b"words".to_vec()))
    );
}

#[test]
fn a_private_change_waiting_when_the_room_turns_public_goes_out_public() {
    let f = Fixture::new();
    let private = owner_private(&f);
    let mut outbox = outbox();
    assert!(outbox.command(&private, rename("Open")));
    let public = owner_public(&f);
    let delta = next(&mut outbox, &public, Instant::now()).unwrap();
    let c = the_config(&delta).configuration;
    assert!(c.display.name.is_public());
    assert_eq!(
        c.privacy_mode,
        public
            .stored
            .room_state
            .configuration
            .configuration
            .privacy_mode
    );
    through_the_contract(&public, &delta);
}
