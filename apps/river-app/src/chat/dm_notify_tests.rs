//! Notifications for new direct messages: which messages are news, what a notification says
//! (and does not), who it is held back for, and what its click, reply and mark-read do. Clocks
//! are passed in; nothing here waits.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::dm_model::{DmBody, DmMessage, DmPurge, DmSnapshot, DmThread, Placeholder};
use super::dm_view::DmUi;
use super::model::Pending;
use super::{LinkStatus, ReplyRefusal, Source, example};
use crate::notify::{self, Notification, Notifier, dm_target, parse_dm_target};
use crate::rooms::modes::NotificationMode;
use crate::rooms::outbox::Action;
use crate::{RiverApp, rooms};

const T0: u64 = 1_700_000_000;

#[derive(Clone, Default)]
struct Recorder {
    shown: Rc<RefCell<Vec<Notification>>>,
}

impl Notifier for Recorder {
    fn notify(&self, n: Notification) -> bool {
        self.shown.borrow_mut().push(n);
        true
    }
    fn set_unread(&self, _: usize) {}
}

/// A peer's name, and what they write: kept out of every generic notification.
const NAME: &str = "Quentin Secretname";
const WORDS: &str = "meet me behind the old mill";

fn text(key: &str, ts: u64, words: &str) -> DmMessage {
    DmMessage {
        key: key.to_owned(),
        outgoing: false,
        timestamp: ts,
        body: DmBody::Text(words.to_owned()),
        pending: None,
    }
}

fn mine(key: &str, ts: u64) -> DmMessage {
    DmMessage {
        outgoing: true,
        ..text(key, ts, "my own words")
    }
}

fn thread(room: &str, peer: &str, member: bool, messages: Vec<DmMessage>) -> Arc<DmThread> {
    let inbound = messages.iter().filter(|m| !m.outgoing).map(|m| m.timestamp);
    Arc::new(DmThread {
        room: room.to_owned(),
        room_name: "Some Room".to_owned(),
        peer_key: peer.to_owned(),
        peer: rooms::dm::peer_from_key(peer).unwrap(),
        peer_name: NAME.to_owned(),
        peer_is_member: member,
        last_ts: messages.iter().map(|m| m.timestamp).max().unwrap_or(0),
        last_inbound_ts: inbound.clone().max().unwrap_or(0),
        last_inbound_signed: inbound.max().unwrap_or(0),
        messages,
    })
}

fn snap(met: &[&str], threads: Vec<Arc<DmThread>>) -> DmSnapshot {
    DmSnapshot {
        threads,
        room_keys: met.iter().map(|r| (*r).to_owned()).collect(),
        hydrated: true,
        ..Default::default()
    }
}

// What is news -------------------------------------------------------------------------------

const R: &str = "room-a";
const P: &str = "1001";

fn news(ui: &mut DmUi, s: DmSnapshot) -> Vec<(usize, String)> {
    ui.apply(s);
    ui.take_arrivals()
        .into_iter()
        .map(|a| (a.count, a.key))
        .collect()
}

#[test]
fn what_a_room_held_when_met_and_what_is_delivered_again_is_not_news() {
    let mut ui = DmUi::default();
    // Seeded when the room is met.
    assert!(
        news(
            &mut ui,
            snap(&[R], vec![thread(R, P, true, vec![text("a", T0, "x")])])
        )
        .is_empty()
    );
    // A new one is news, once.
    let two = vec![text("a", T0, "x"), text("b", T0 + 5, WORDS)];
    assert_eq!(
        news(&mut ui, snap(&[R], vec![thread(R, P, true, two.clone())])),
        vec![(1, "b".to_owned())]
    );
    // The same snapshot again, a thread that leaves a snapshot and comes back, and a message
    // that comes back after it left: none is news again.
    assert!(news(&mut ui, snap(&[R], vec![thread(R, P, true, two.clone())])).is_empty());
    assert!(news(&mut ui, snap(&[R], vec![])).is_empty());
    assert!(news(&mut ui, snap(&[R], vec![thread(R, P, true, two)])).is_empty());
    let only_a = vec![text("a", T0, "x")];
    assert!(news(&mut ui, snap(&[R], vec![thread(R, P, true, only_a)])).is_empty());
    let back = vec![text("a", T0, "x"), text("b", T0 + 5, WORDS)];
    assert!(news(&mut ui, snap(&[R], vec![thread(R, P, true, back)])).is_empty());
}

#[test]
fn a_room_not_met_yet_is_seeded_when_it_is_and_never_announced() {
    let mut ui = DmUi::default();
    let t = thread(R, P, true, vec![text("a", T0, "x")]);
    // Listed with a stub: its threads are not news and not remembered as read either.
    assert!(news(&mut ui, snap(&[], vec![t.clone()])).is_empty());
    assert!(news(&mut ui, snap(&[R], vec![t])).is_empty(), "seeded");
    let more = vec![text("a", T0, "x"), text("b", T0 + 1, "y")];
    assert_eq!(
        news(&mut ui, snap(&[R], vec![thread(R, P, true, more)])).len(),
        1
    );
}

#[test]
fn own_messages_unreadable_rows_and_departed_peers_are_not_news() {
    let mut ui = DmUi::default();
    news(&mut ui, snap(&[R], vec![]));
    let rows = vec![
        mine("m", T0),
        DmMessage {
            body: DmBody::Placeholder(Placeholder::CannotDecrypt),
            ..text("p", T0 + 1, "")
        },
    ];
    assert!(news(&mut ui, snap(&[R], vec![thread(R, P, true, rows)])).is_empty());
    // A peer who is no longer a member.
    let gone = vec![text("g", T0 + 2, WORDS)];
    assert!(news(&mut ui, snap(&[R], vec![thread(R, "1002", false, gone)])).is_empty());
}

#[test]
fn messages_of_a_thread_being_deleted_are_not_news() {
    let mut ui = DmUi::default();
    news(&mut ui, snap(&[R], vec![]));
    let mut s = snap(&[R], vec![thread(R, P, true, vec![text("d", T0, WORDS)])]);
    s.purges.push(DmPurge {
        room: R.to_owned(),
        peer_key: P.to_owned(),
        count: 1,
        pending: Pending {
            entry: 1,
            failed: None,
            retry: false,
        },
    });
    assert!(news(&mut ui, s).is_empty());
    // And it is not news later either, when the purge is gone and the message with it or not.
    let later = snap(&[R], vec![thread(R, P, true, vec![text("d", T0, WORDS)])]);
    assert!(news(&mut ui, later).is_empty());
}

#[test]
fn an_archived_thread_is_quiet_until_a_message_revives_it() {
    let mut ui = DmUi::default();
    let archived = |at: u64| {
        let mut s = snap(&[R], vec![thread(R, P, true, vec![text("a", T0, "x")])]);
        s.archived.push(super::dm_model::DmArchived {
            room: R.to_owned(),
            peer_key: P.to_owned(),
            hidden_at_ts: at,
        });
        s
    };
    news(&mut ui, archived(T0));
    // A message at or before the cutoff does not revive the thread: no news.
    let mut s = archived(T0 + 10);
    s.threads = vec![thread(
        R,
        P,
        true,
        vec![text("a", T0, "x"), text("late", T0 + 10, "y")],
    )];
    assert!(news(&mut ui, s).is_empty());
    // One past it does.
    let mut s = archived(T0 + 10);
    s.threads = vec![thread(
        R,
        P,
        true,
        vec![
            text("a", T0, "x"),
            text("late", T0 + 10, "y"),
            text("revive", T0 + 11, WORDS),
        ],
    )];
    assert_eq!(news(&mut ui, s), vec![(1, "revive".to_owned())]);
}

#[test]
fn an_invitation_previews_as_its_room_and_a_text_as_plain_words() {
    let mut ui = DmUi::default();
    news(&mut ui, snap(&[R], vec![]));
    let invite = DmMessage {
        body: DmBody::Invite {
            room: "other".to_owned(),
            room_label: "Book Club".to_owned(),
            note: Some("a secret note".to_owned()),
        },
        ..text("i", T0, "")
    };
    ui.apply(snap(&[R], vec![thread(R, P, true, vec![invite])]));
    let a = ui.take_arrivals();
    assert_eq!(a[0].preview, "Invitation to Book Club");
    assert!(
        !format!("{a:?}").contains("Book Club"),
        "Debug names nothing"
    );
    ui.apply(snap(
        &[R],
        vec![thread(
            R,
            P,
            true,
            vec![text("t", T0 + 1, "**bold** words\n\nmore")],
        )],
    ));
    assert_eq!(ui.take_arrivals()[0].preview, "bold words more");
}

// What is shown ------------------------------------------------------------------------------

struct World {
    s: RiverApp,
    rec: Recorder,
    room: String,
    peer: String,
    now: Instant,
}

/// The app on the example rooms, live, with the first room's first other member as the peer
/// (so a reply passes the member check), and the room met by direct messages.
fn world() -> World {
    let rec = Recorder::default();
    let mut s = RiverApp::detached().with_notifier(Box::new(rec.clone()));
    let rooms = example::data().rooms;
    s.chat.source = Source::Node(LinkStatus::Live);
    s.chat
        .show_node_rooms(rooms.clone(), None, LinkStatus::Live, 1000);
    let room = rooms[0].key.clone();
    let peer = peer_of(&rooms[0], 0);
    let mut w = World {
        s,
        rec,
        room,
        peer,
        now: Instant::now() + Duration::from_secs(1000),
    };
    let met = snap(&[&w.room.clone()], vec![]);
    w.give(met);
    w
}

fn peer_of(room: &super::Room, n: usize) -> String {
    room.members
        .iter()
        .filter(|m| m.id != room.me)
        .nth(n)
        .unwrap()
        .full_id
        .to_string()
}

impl World {
    /// Hand over a snapshot a minute after the last, as the background work does.
    fn give(&mut self, s: DmSnapshot) {
        self.now += Duration::from_secs(60);
        self.s.chat.apply_dms(s);
        let a = self.s.chat.take_dm_arrivals();
        self.s.notify_dm_arrivals(a, self.now);
    }

    fn thread_with(&self, peer: &str, messages: Vec<DmMessage>) -> DmSnapshot {
        snap(
            &[&self.room],
            vec![thread(&self.room, peer, true, messages)],
        )
    }

    fn shown(&self) -> Vec<Notification> {
        self.rec.shown.borrow().clone()
    }

    fn arrive(&mut self, key: &str, words: &str) {
        let peer = self.peer.clone();
        self.arrive_from(&peer, key, words);
    }

    fn arrive_from(&mut self, peer: &str, key: &str, words: &str) {
        let s = self.thread_with(peer, vec![text(key, T0 + 100, words)]);
        self.give(s);
    }
}

#[test]
fn by_default_a_direct_message_says_nothing_about_who_or_what() {
    let mut w = world();
    w.arrive("m1", WORDS);
    let n = w.shown();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].title, "River");
    assert_eq!(n[0].body, "New direct message");
    assert_eq!(n[0].message_key, "m1");
    assert_eq!(n[0].room_key, dm_target(&w.room, &w.peer));
    assert!(n[0].actions);
    let all = format!(
        "{} {} {} {}",
        n[0].title, n[0].body, n[0].room_name, n[0].room_key
    );
    for secret in [NAME, "mill", "Some Room"] {
        assert!(!all.contains(secret), "{secret} leaked: {all}");
    }
}

#[test]
fn with_previews_on_it_names_the_sender_and_a_plain_bounded_preview() {
    let mut w = world();
    w.s.private_previews = true;
    w.arrive("m1", &format!("**{WORDS}** {}", "long ".repeat(100)));
    let n = w.shown();
    assert_eq!(n[0].title, format!("{NAME} · direct message"));
    assert!(n[0].body.starts_with(WORDS), "{}", n[0].body);
    assert!(!n[0].body.contains('*'));
    assert!(n[0].body.chars().count() <= 200);
}

#[test]
fn an_invitation_notifies_as_its_room_and_never_shows_its_payload() {
    let mut w = world();
    w.s.private_previews = true;
    let peer = w.peer.clone();
    let invite = DmMessage {
        body: DmBody::Invite {
            room: "x".into(),
            room_label: "Book Club".into(),
            note: Some("NOTE-TEXT".into()),
        },
        ..text("inv", T0 + 1, "")
    };
    let s = w.thread_with(&peer, vec![invite]);
    w.give(s);
    let n = w.shown();
    assert_eq!(n[0].body, "Invitation to Book Club");
    assert!(!format!("{n:?}").contains("NOTE-TEXT"));
}

#[test]
fn a_muted_room_raises_none_and_mentions_only_still_lets_a_direct_message_through() {
    let mut w = world();
    let room = w.room.clone();
    let modes = |m| [(room.clone(), m)].into_iter().collect();
    w.s.chat.apply_modes(modes(NotificationMode::Muted), &[]);
    w.arrive("m1", WORDS);
    assert!(w.shown().is_empty(), "muted");
    w.s.chat
        .apply_modes(modes(NotificationMode::MentionsAndReplies), &[]);
    w.arrive("m2", WORDS);
    assert_eq!(w.shown().len(), 1, "a direct message is for you");
}

#[test]
fn do_not_disturb_holds_them_unless_mentions_are_allowed_through() {
    let mut w = world();
    let base = w.s.unread();
    w.s.chat.dnd.pinned = true;
    w.s.chat.dnd.now = 12 * 60;
    w.s.chat.dnd.settings.manual = true;
    w.arrive("m1", WORDS);
    assert!(w.shown().is_empty());
    // The unread count does not depend on do not disturb.
    assert_eq!(w.s.unread(), base + 1);
    w.s.chat.dnd.settings.allow_mentions = true;
    w.arrive("m2", WORDS);
    assert_eq!(w.shown().len(), 1);
}

#[test]
fn the_thread_on_screen_in_a_window_in_front_is_not_announced() {
    let mut w = world();
    w.s.focus_changed(true);
    assert!(w.s.chat.open_dm(&w.room, &w.peer));
    w.arrive("m1", WORDS);
    assert!(w.shown().is_empty());
    // Another thread is, in a window in front.
    let other = peer_of(&example::data().rooms[0], 1);
    w.arrive_from(&other, "m2", WORDS);
    assert_eq!(w.shown().len(), 1);
    // And the open thread is, when the window is away.
    w.s.focus_changed(false);
    w.arrive("m3", WORDS);
    assert_eq!(w.shown().len(), 2);
}

#[test]
fn threads_have_their_own_cooldown_and_never_merge_into_a_room_summary() {
    let mut w = world();
    let rooms = example::data().rooms;
    let peers: Vec<String> = (0..4).map(|n| peer_of(&rooms[0], n)).collect();
    // One thread, twice within the cooldown: the second is held, not shown.
    w.arrive_from(&peers[0], "a1", WORDS);
    w.now -= Duration::from_secs(55);
    w.arrive_from(&peers[0], "a2", WORDS);
    assert_eq!(w.shown().len(), 1, "held by the thread's own cooldown");
    // A different peer is not held by it.
    w.arrive_from(&peers[1], "b1", WORDS);
    assert_eq!(w.shown().len(), 2);
    assert_ne!(w.shown()[0].room_key, w.shown()[1].room_key);
    // Many at once: a summary of direct messages, with no names and no "rooms".
    let mut w = world();
    let threads = peers
        .iter()
        .enumerate()
        .map(|(i, p)| {
            thread(
                &w.room,
                p,
                true,
                vec![text(&format!("k{i}"), T0 + 5, WORDS)],
            )
        })
        .collect();
    w.give(snap(&[&w.room.clone()], threads));
    let n = w.shown();
    assert_eq!(n.len(), notify::MAX_PER_UPDATE);
    let last = n.last().unwrap();
    assert!(last.body.ends_with("new direct messages"), "{}", last.body);
    assert!(!last.actions);
    assert!(n.iter().all(|n| !n.body.contains("rooms")));
}

#[test]
fn what_was_held_is_dropped_when_the_thread_is_read_or_opened() {
    let mut w = world();
    w.arrive("a1", WORDS);
    w.now -= Duration::from_secs(55);
    w.arrive("a2", WORDS);
    assert!(w.s.limiter.next_due(w.now).is_some(), "one is held");
    assert!(w.s.open_message(&dm_target(&w.room, &w.peer), "a2"));
    assert!(w.s.limiter.next_due(w.now).is_none(), "opening reads it");
}

// Click, reply, mark as read -----------------------------------------------------------------

#[test]
fn clicking_opens_the_thread_not_the_room_and_a_missing_room_does_not_crash() {
    let mut w = world();
    w.arrive("m1", WORDS);
    let target = w.shown()[0].room_key.clone();
    assert!(w.s.open_message(&target, "m1"));
    assert_eq!(
        w.s.chat.dm.open_key(),
        Some((w.room.as_str(), w.peer.as_str()))
    );
    w.s.chat.close_dm();
    assert!(!w.s.open_message(&dm_target("gone", "5"), "m1"));
    w.s.open_room_to_reply(&dm_target("gone", "5"));
    assert_eq!(
        w.s.reply_from_notification(&dm_target("gone", "5"), "hi"),
        Err(ReplyRefusal::NoRoom)
    );
    w.s.mark_read_through(&dm_target("gone", "5"), "m1");
    // The Reply button opens the thread with its box ready.
    w.s.open_room_to_reply(&target);
    assert!(w.s.chat.dm.is_open());
}

#[test]
fn a_reply_goes_through_the_composers_checks_and_keeps_its_text_when_refused() {
    let mut w = world();
    w.arrive("m1", WORDS);
    let target = w.shown()[0].room_key.clone();
    assert_eq!(
        w.s.reply_from_notification(&target, "  on my way  "),
        Ok(())
    );
    let sent: Vec<Action> =
        w.s.chat
            .queued_commands()
            .iter()
            .map(|c| c.action.clone())
            .collect();
    let [Action::SendDm { peer, text, .. }] = sent.as_slice() else {
        panic!("one SendDm, got {sent:?}")
    };
    assert_eq!(rooms::dm::peer_key(*peer), w.peer, "to the full id");
    assert_eq!(text, "on my way");
    // Blank, too long, and a peer who is not a member.
    assert_eq!(
        w.s.reply_from_notification(&target, "  "),
        Err(ReplyRefusal::Empty)
    );
    let big = "x".repeat(rooms::dm_send::BODY_CAP + 1);
    w.s.chat.dm.compose.drafts_mut().clear();
    // A send is still waiting in this thread, so a second is refused and kept.
    assert_eq!(
        w.s.reply_from_notification(&target, "again"),
        Err(ReplyRefusal::Blocked)
    );
    let key = (w.room.clone(), w.peer.clone());
    assert_eq!(w.s.chat.dm.compose.draft(&key.0, &key.1), Some("again"));
    w.s.chat.dm.compose.drafts_mut().clear();
    w.s.chat.dm.compose.sends_clear_for_test();
    assert_eq!(
        w.s.reply_from_notification(&target, &big),
        Err(ReplyRefusal::TooLong)
    );
    assert_eq!(
        w.s.chat.dm.compose.draft(&key.0, &key.1).map(str::len),
        Some(rooms_cut()),
        "kept, cut to what a reply may hold"
    );
    let stranger = dm_target(&w.room, "424242");
    assert_eq!(
        w.s.reply_from_notification(&stranger, "hello"),
        Err(ReplyRefusal::Blocked)
    );
    assert_eq!(w.s.chat.dm.compose.draft(&w.room, "424242"), Some("hello"));
}

#[test]
fn a_refusal_from_the_background_work_keeps_the_reply_in_the_draft() {
    let mut w = world();
    w.arrive("m1", WORDS);
    let target = w.shown()[0].room_key.clone();
    assert_eq!(w.s.reply_from_notification(&target, "keep me"), Ok(()));
    let ticket = w.s.chat.dm_waiting_ticket_of(&w.room, &w.peer).unwrap();
    w.s.chat.dm_sent(&w.room, &w.peer, ticket, Err("no".into()));
    assert_eq!(w.s.chat.dm.compose.draft(&w.room, &w.peer), Some("keep me"));
}

#[test]
fn mark_as_read_reads_up_to_the_message_shown_and_leaves_newer_ones() {
    let mut w = world();
    let peer = w.peer.clone();
    let base = w.s.unread();
    let msgs = |n| {
        (0..n)
            .map(|i| text(&format!("k{i}"), T0 + i as u64, "x"))
            .collect()
    };
    let s = w.thread_with(&peer, msgs(1));
    w.give(s);
    let s = w.thread_with(&peer, msgs(4));
    w.give(s);
    assert_eq!(w.s.unread(), base + 4);
    w.s.mark_read_through(&dm_target(&w.room, &peer), "k2");
    assert_eq!(w.s.unread(), base + 1, "k3 is newer than what was shown");
    w.s.mark_read_through(&dm_target(&w.room, &peer), "nope");
    assert_eq!(w.s.unread(), base + 1);
}

// The count ----------------------------------------------------------------------------------

#[test]
fn the_title_count_includes_direct_messages_by_mode_and_archive() {
    let mut w = world();
    let before = w.s.unread();
    let peer = w.peer.clone();
    let s = w.thread_with(&peer, vec![text("a", T0, "x"), text("b", T0 + 1, "y")]);
    w.give(s);
    assert_eq!(w.s.unread(), before + 2);
    w.s.focus_changed(false);
    assert!(
        w.s.window_title(w.s.unread())
            .starts_with(&format!("({})", before + 2))
    );
    w.s.chat.apply_modes(
        [(w.room.clone(), NotificationMode::Muted)]
            .into_iter()
            .collect(),
        &[],
    );
    assert_eq!(w.s.unread(), before, "a muted room counts none");
}

#[test]
fn targets_round_trip_and_ordinary_keys_are_not_targets() {
    assert_eq!(
        parse_dm_target(&dm_target("r1", "-42")),
        Some(("r1", "-42"))
    );
    for bad in ["r1", "dm:r1", "dm::5", "dm:r1:x", "dm:r1:", "dm:r1:-"] {
        assert_eq!(parse_dm_target(bad), None, "{bad}");
    }
}

fn rooms_cut() -> usize {
    super::quick_reply::MAX_REPLY_CHARS
}
