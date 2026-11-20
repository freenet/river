//! The invitation card and the "Share an invite" dialog, driven as a person drives them. The
//! card is drawn from what the background work sends (a room name and a note, never an
//! invitation) and Accept is only a request the background work answers.

use std::time::Instant;

use rune::{Env, Runtime};

use super::{ANSWER_WAIT, CardState};
use crate::chat::Source;
use crate::chat::dm_model::{DmBody, DmMessage, DmSnapshot, DmThread};
use crate::rooms::LinkStatus;
use crate::rooms::outbox::Action;
use crate::rooms::work::{WorkState, WorkView};
use crate::{RiverApp, Setup, WINDOW_SIZE, view};

const ROOM: &str = "town-square";
/// The room an invitation in these tests is to: one the example does not list.
const INVITED: &str = "chess-club";
const T0: u64 = 1_700_000_000;

fn app() -> Runtime<RiverApp> {
    let mut state = RiverApp::detached();
    state.setup = Setup::Connected {
        version: "0.2.155".into(),
    };
    let mut rt = Runtime::new(
        state,
        view,
        WINDOW_SIZE,
        Env {
            system_dark: true,
            scale: 1.0,
        },
    );
    rt.on_key = crate::spec().on_key;
    rt.poll = crate::spec().poll;
    rt.frame(0.0);
    rt.frame(1.0 / 60.0);
    rt.update(|s, _| s.chat.source = Source::Node(LinkStatus::Live));
    rt
}

fn press(rt: &mut Runtime<RiverApp>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no element {key}"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.1);
    rt.frame(0.2);
}

fn texts(rt: &Runtime<RiverApp>) -> Vec<String> {
    rt.elements().into_iter().filter_map(|e| e.text).collect()
}

fn shows(rt: &Runtime<RiverApp>, needle: &str) -> bool {
    texts(rt).iter().any(|t| t.contains(needle))
}

fn uid_of(rt: &Runtime<RiverApp>, name: &str) -> String {
    rt.state
        .chat
        .room()
        .unwrap()
        .members
        .iter()
        .find(|m| m.nickname == name)
        .unwrap()
        .full_id
        .to_string()
}

fn sent(rt: &Runtime<RiverApp>) -> Vec<Action> {
    rt.state
        .chat
        .queued_commands()
        .iter()
        .map(|c| c.action.clone())
        .collect()
}

fn message(key: &str, outgoing: bool, body: DmBody) -> DmMessage {
    DmMessage {
        key: key.to_owned(),
        outgoing,
        timestamp: T0,
        body,
        pending: None,
    }
}

/// The thread with Jonas open, holding a received invitation (message `in-key`, 32 hex digits
/// are needed to name a token) and one the user sent.
const IN_KEY: &str = "0123456789abcdef0123456789abcdef";
const OUT_KEY: &str = "fedcba9876543210fedcba9876543210";

fn with_cards(note: Option<&str>) -> (Runtime<RiverApp>, String) {
    let mut rt = app();
    let uid = uid_of(&rt, "Jonas");
    let thread = DmThread {
        room: ROOM.into(),
        room_name: "Town Square".into(),
        peer_key: uid.clone(),
        peer: crate::rooms::dm::peer_from_key(&uid).unwrap(),
        peer_name: "Jonas".into(),
        peer_is_member: true,
        messages: vec![
            message(
                IN_KEY,
                false,
                DmBody::Invite {
                    room: INVITED.into(),
                    room_label: "Chess Club".into(),
                    note: note.map(str::to_owned),
                },
            ),
            message(
                OUT_KEY,
                true,
                DmBody::SentInvite {
                    note: Some("my own".into()),
                },
            ),
        ],
        last_ts: T0,
        last_inbound_ts: T0,
    };
    let snapshot = DmSnapshot {
        threads: vec![thread.into()],
        room_keys: vec![ROOM.to_owned()],
        hydrated: true,
        ..Default::default()
    };
    rt.update(move |s, _| s.chat.apply_dms(snapshot));
    let u = uid.clone();
    rt.update(move |s, _| assert!(s.chat.open_dm(ROOM, &u)));
    rt.frame(0.1);
    rt.frame(0.2);
    (rt, uid)
}

fn accept_key() -> String {
    format!("dm-invite-accept-{IN_KEY}")
}

fn work(room: &str, state: WorkState) -> Vec<WorkView> {
    vec![WorkView {
        id: 1,
        what: "Joining a room".into(),
        room: Some(room.to_owned()),
        state,
    }]
}

fn state_of(rt: &Runtime<RiverApp>, uid: &str) -> CardState {
    rt.state.chat.dm_card_state(ROOM, uid, IN_KEY, INVITED)
}

// The card ---------------------------------------------------------------------------------------

#[test]
fn a_received_card_names_the_room_the_note_and_who_sent_it_and_a_sent_one_has_no_button() {
    let (rt, uid) = with_cards(Some("come and play"));
    assert!(shows(&rt, "Chess Club"));
    assert!(shows(&rt, "come and play"));
    // The sender is named, from the message's signature; nothing says who invited.
    assert!(shows(&rt, "Sent by Jonas"));
    assert!(!texts(&rt).iter().any(|t| t.contains("invited you")));
    assert!(rt.find(&accept_key()).is_some());
    // The user's own card has the note and no Accept.
    assert!(shows(&rt, "my own"));
    assert!(rt.find(&format!("dm-invite-accept-{OUT_KEY}")).is_none());
    assert_eq!(state_of(&rt, &uid), CardState::New);
}

#[test]
fn a_hostile_note_is_drawn_as_plain_text() {
    let (rt, _) = with_cards(Some("**bold** [link](http://x) <b>tag</b>"));
    // Shown as written, not as markdown.
    assert!(shows(&rt, "**bold** [link](http://x) <b>tag</b>"));
}

// Accept ----------------------------------------------------------------------------------------

#[test]
fn accept_asks_the_background_work_by_token_once_and_says_joining() {
    let (mut rt, uid) = with_cards(None);
    press(&mut rt, &accept_key());
    let commands = sent(&rt);
    assert_eq!(commands.len(), 1);
    let Action::AcceptInviteDm {
        peer,
        token,
        ticket,
    } = commands[0].clone()
    else {
        panic!("an accept, not {:?}", commands[0])
    };
    assert_eq!(crate::rooms::dm::peer_key(peer), uid);
    assert_eq!(crate::rooms::dm::token_hex(&token), IN_KEY);
    // The command is addressed to the thread's room, and prints nothing of the token.
    assert_eq!(rt.state.chat.queued_commands()[0].room, ROOM);
    assert!(!format!("{:?}", commands[0]).contains(IN_KEY));
    assert!(shows(&rt, "Joining"));
    assert!(rt.find(&accept_key()).is_none(), "no button while it runs");

    // A second press does nothing, whether by the button or by the call.
    assert!(!rt.state.chat.dm_accept(IN_KEY));
    assert_eq!(sent(&rt).len(), 1);

    // The answer: started. The card keeps saying so.
    rt.update(move |s, _| {
        s.chat.dm_invite_answer(ROOM, &uid, ticket, Ok(()));
    });
    rt.frame(0.2);
    assert!(shows(&rt, "Joining"));
}

#[test]
fn a_refusal_is_said_on_the_card_and_accept_can_be_pressed_again() {
    let (mut rt, uid) = with_cards(None);
    press(&mut rt, &accept_key());
    let Action::AcceptInviteDm { ticket, .. } = sent(&rt)[0].clone() else {
        panic!()
    };
    let u = uid.clone();
    rt.update(move |s, _| {
        s.chat
            .dm_invite_answer(ROOM, &u, ticket, Err("This invitation is damaged.".into()));
    });
    rt.frame(0.2);
    assert!(shows(&rt, "This invitation is damaged."));
    assert!(rt.find(&accept_key()).is_some());
    press(&mut rt, &accept_key());
    assert_eq!(sent(&rt).len(), 2);
}

#[test]
fn a_failed_join_is_said_and_a_dismissed_one_offers_accept_again() {
    let (mut rt, uid) = with_cards(None);
    press(&mut rt, &accept_key());
    let Action::AcceptInviteDm { ticket, .. } = sent(&rt)[0].clone() else {
        panic!()
    };
    let u = uid.clone();
    rt.update(move |s, _| {
        s.chat.dm_invite_answer(ROOM, &u, ticket, Ok(()));
        s.chat.show_work(work(INVITED, WorkState::Working));
    });
    rt.frame(0.2);
    assert_eq!(state_of(&rt, &uid), CardState::Accepting);
    rt.update(|s, _| {
        s.chat.show_work(work(
            INVITED,
            WorkState::Failed {
                why: "This room is full.".into(),
                retry: true,
            },
        ))
    });
    rt.frame(0.2);
    assert!(matches!(state_of(&rt, &uid), CardState::Failed(w) if w.contains("full")));
    assert!(shows(&rt, "This room is full."));
    // Put away: the card offers Accept again.
    rt.update(|s, _| s.chat.show_work(Vec::new()));
    rt.frame(0.2);
    assert_eq!(state_of(&rt, &uid), CardState::New);
    assert!(rt.find(&accept_key()).is_some());
}

#[test]
fn a_join_started_elsewhere_is_the_same_join() {
    let (mut rt, uid) = with_cards(None);
    rt.update(|s, _| s.chat.show_work(work(INVITED, WorkState::Working)));
    rt.frame(0.2);
    assert_eq!(state_of(&rt, &uid), CardState::Accepting);
    assert!(!rt.state.chat.dm_accept(IN_KEY), "one join at a time");
    assert!(sent(&rt).is_empty());
}

#[test]
fn an_unanswered_accept_says_so_and_may_be_pressed_again() {
    let (mut rt, uid) = with_cards(None);
    let now = Instant::now();
    rt.update(move |s, _| assert!(s.chat.dm_accept_at(IN_KEY, now)));
    assert_eq!(state_of(&rt, &uid), CardState::Accepting);
    rt.update(move |s, _| assert!(s.chat.dm_tick(now + ANSWER_WAIT)));
    rt.frame(0.2);
    assert_eq!(state_of(&rt, &uid), CardState::Silent);
    assert!(shows(&rt, "River has not answered yet"));
    press(&mut rt, &accept_key());
    assert_eq!(sent(&rt).len(), 2);
}

#[test]
fn a_room_you_have_is_already_a_member_and_one_that_dropped_you_is_not_used() {
    let (mut rt, uid) = with_cards(None);
    let mut rooms = rt.state.chat.rooms().to_vec();
    let mut have = rooms[0].clone();
    have.key = INVITED.to_owned();
    have.name = "Chess Club".to_owned();
    rooms.push(have);
    rt.update(move |s, _| s.chat.set_rooms(rooms));
    rt.frame(0.2);
    assert!(matches!(state_of(&rt, &uid), CardState::Member { .. }));
    assert!(shows(&rt, "You are already a member of Chess Club."));
    assert!(rt.find(&accept_key()).is_none());
    assert!(!rt.state.chat.dm_accept(IN_KEY));
    // The room opens from the card.
    press(&mut rt, &format!("dm-invite-open-{IN_KEY}"));
    assert_eq!(rt.state.chat.room().unwrap().key, INVITED);

    // The room is listed and no longer has the user: the invitation is not used.
    let (mut rt, uid) = with_cards(None);
    let mut rooms = rt.state.chat.rooms().to_vec();
    let mut gone = rooms[0].clone();
    gone.key = INVITED.to_owned();
    gone.name = "Chess Club".to_owned();
    gone.not_member = true;
    rooms.push(gone);
    rt.update(move |s, _| s.chat.set_rooms(rooms));
    rt.frame(0.2);
    assert!(matches!(state_of(&rt, &uid), CardState::Have { .. }));
    assert!(shows(&rt, "so this invitation is not used"));
    assert!(rt.find(&accept_key()).is_none());
}

#[test]
fn with_river_moved_on_accept_is_off_and_says_why() {
    let (mut rt, uid) = with_cards(None);
    rt.update(|s, _| s.chat.issues.update_required = true);
    rt.frame(0.2);
    assert!(matches!(state_of(&rt, &uid), CardState::Blocked(_)));
    assert!(shows(&rt, "A newer version of River is out"));
    assert!(!rt.state.chat.dm_accept(IN_KEY));
    assert!(sent(&rt).is_empty());
}

#[test]
fn accept_is_only_for_an_invitation_the_thread_holds_and_the_user_did_not_send() {
    let (mut rt, _) = with_cards(None);
    assert!(!rt.state.chat.dm_accept(OUT_KEY), "their own card");
    assert!(!rt.state.chat.dm_accept("not-a-message"));
    assert!(sent(&rt).is_empty());
    // No thread open: nothing to accept.
    rt.update(|s, _| s.chat.close_dm());
    assert!(!rt.state.chat.dm_accept(IN_KEY));
}

// The picker -------------------------------------------------------------------------------------

fn open_picker() -> (Runtime<RiverApp>, String) {
    let mut rt = app();
    let uid = uid_of(&rt, "Jonas");
    let u = uid.clone();
    rt.update(move |s, _| assert!(s.chat.open_invite_picker(ROOM, &u)));
    rt.frame(0.2);
    (rt, uid)
}

#[test]
fn the_member_dialog_offers_share_invite_and_it_opens_the_picker() {
    let mut rt = app();
    let uid = uid_of(&rt, "Jonas");
    let id = rt
        .state
        .chat
        .room()
        .unwrap()
        .member_by_peer_key(&uid)
        .unwrap()
        .id;
    press(&mut rt, &format!("member-{id}"));
    press(&mut rt, "member-dm-invite");
    assert!(rt.state.chat.invite_picker_open());
    assert!(rt.find("dm-invite-send").is_some());
    assert!(rt.state.chat.member_modal.is_none(), "the dialog gives way");
}

#[test]
fn the_picker_lists_your_other_rooms_and_sends_the_chosen_one_with_the_note() {
    let (mut rt, uid) = open_picker();
    let rooms = rt.state.chat.invite_picker_rooms();
    assert!(!rooms.is_empty());
    assert!(rooms.iter().all(|r| r.key != ROOM), "not the current room");
    let target = rooms[0].key.clone();
    // Nothing is sent without a room.
    assert!(!rt.state.chat.invite_send());
    press(&mut rt, &format!("dm-invite-room-{target}"));
    rt.update(|s, _| s.chat.invite_write("  see you there  "));
    rt.frame(0.2);
    press(&mut rt, "dm-invite-send");
    let commands = sent(&rt);
    assert_eq!(commands.len(), 1);
    let Action::SendInviteDm {
        peer,
        target: to,
        note,
        ticket,
    } = commands[0].clone()
    else {
        panic!("{:?}", commands[0])
    };
    assert_eq!(crate::rooms::dm::peer_key(peer), uid);
    assert_eq!(to, target);
    assert_eq!(note.as_deref(), Some("see you there"));
    assert_eq!(rt.state.chat.queued_commands()[0].room, ROOM);
    // While it waits there is no second send, and the dialog stays to say what became of it.
    assert!(!rt.state.chat.invite_send());
    rt.update(|s, _| s.chat.close_invite_picker());
    assert!(rt.state.chat.invite_picker_open());
    assert!(shows(&rt, "Sending"));
    assert_eq!(sent(&rt).len(), 1);

    // A refusal keeps the choice and the note and says why.
    let u = uid.clone();
    rt.update(move |s, _| {
        s.chat
            .dm_invite_answer(ROOM, &u, ticket, Err("The room didn't keep this.".into()));
    });
    rt.frame(0.2);
    assert!(shows(&rt, "The room didn't keep this."));
    assert!(rt.state.chat.invite_picker_open());
    assert_eq!(
        rt.state.chat.dm.invite.picker.note_text(),
        "  see you there  "
    );
    assert!(rt.state.chat.invite_send(), "and can be sent again");
    let Action::SendInviteDm { ticket: second, .. } = sent(&rt)[1].clone() else {
        panic!()
    };
    // Queued: the picker closes and the conversation opens at its end.
    let u = uid.clone();
    rt.update(move |s, _| {
        s.chat.dm_invite_answer(ROOM, &u, second, Ok(()));
    });
    rt.frame(0.2);
    assert!(!rt.state.chat.invite_picker_open());
    assert_eq!(rt.state.chat.dm.open_key(), Some((ROOM, uid.as_str())));
    assert_eq!(
        rt.state.chat.dm.invite.picker.note_text(),
        "",
        "the note is gone"
    );
}

#[test]
fn the_picker_refuses_what_cannot_be_sent_and_says_why() {
    // A note that is too long or hides text.
    let (mut rt, _) = open_picker();
    let target = rt.state.chat.invite_picker_rooms()[0].key.clone();
    rt.update(move |s, _| s.chat.invite_pick(&target));
    rt.update(|s, _| s.chat.invite_write(&"x".repeat(5_000)));
    rt.frame(0.2);
    assert!(!rt.state.chat.invite_send());
    assert!(shows(&rt, "too long"));
    rt.update(|s, _| s.chat.invite_write("bad\u{202E}note"));
    assert!(!rt.state.chat.invite_send());
    assert!(sent(&rt).is_empty());

    // The room it goes in forbids sending (which includes River having moved on).
    let (mut rt, _) = open_picker();
    let target = rt.state.chat.invite_picker_rooms()[0].key.clone();
    rt.update(move |s, _| s.chat.invite_pick(&target));
    let mut rooms = rt.state.chat.rooms().to_vec();
    rooms.iter_mut().find(|r| r.key == ROOM).unwrap().send_block =
        Some("You are banned from this room.".into());
    rt.update(move |s, _| s.chat.set_rooms(rooms));
    rt.frame(0.2);
    assert!(!rt.state.chat.invite_send());
    assert!(shows(&rt, "You are banned from this room."));

    let (mut rt, _) = open_picker();
    let target = rt.state.chat.invite_picker_rooms()[0].key.clone();
    rt.update(move |s, _| s.chat.invite_pick(&target));
    rt.update(|s, _| s.chat.issues.update_required = true);
    rt.frame(0.2);
    assert!(!rt.state.chat.invite_send());
    assert!(shows(&rt, "A newer version of River is out"));
    assert!(sent(&rt).is_empty());

    // No other room to invite to.
    let (mut rt, _) = open_picker();
    let only: Vec<_> = rt
        .state
        .chat
        .rooms()
        .iter()
        .filter(|r| r.key == ROOM)
        .cloned()
        .collect();
    rt.update(move |s, _| s.chat.set_rooms(only));
    rt.frame(0.2);
    assert!(rt.state.chat.invite_picker_rooms().is_empty());
    assert!(shows(&rt, "not in another room you can invite from"));
    assert!(!rt.state.chat.invite_send());
}

#[test]
fn a_room_that_is_not_on_offer_cannot_be_picked_and_cancel_forgets_the_note() {
    let (mut rt, _) = open_picker();
    rt.update(|s, _| s.chat.invite_pick(ROOM));
    rt.update(|s, _| s.chat.invite_pick("not-a-room"));
    assert!(!rt.state.chat.invite_send());
    rt.update(|s, _| s.chat.invite_write("note"));
    press(&mut rt, "dm-invite-cancel");
    assert!(!rt.state.chat.invite_picker_open());
    assert_eq!(rt.state.chat.dm.invite.picker.note_text(), "");
    // Not for oneself, nor for a room or member that is not there.
    let me = rt.state.chat.room().unwrap().me;
    let own = rt
        .state
        .chat
        .room()
        .unwrap()
        .member(me)
        .unwrap()
        .full_id
        .to_string();
    rt.update(move |s, _| assert!(!s.chat.open_invite_picker(ROOM, &own)));
    rt.update(|s, _| assert!(!s.chat.open_invite_picker(ROOM, "12345")));
    rt.update(|s, _| assert!(!s.chat.open_invite_picker("nope", "12345")));
}
