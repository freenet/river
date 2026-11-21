//! Notifications for new direct messages: which messages are news, what a notification says
//! (and does not), who it is held back for, and what its click, reply and mark-read do. Clocks
//! are passed in; nothing here waits.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::dm_model::{
    DmArchived, DmArrival, DmBody, DmMessage, DmPurge, DmSnapshot, DmThread, Placeholder,
};
use super::dm_view::DmUi;
use super::model::Pending;
use super::{LinkStatus, ReplyRefusal, Source, dm_compose, example};
use crate::notify::{self, Arrival, Notification, Notifier, dm_target, parse_dm_target};
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
const ROOM_NAME: &str = "Sentinel Room Name";
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
        room_name: ROOM_NAME.to_owned(),
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
/// The clock the detection tests run at: just after the messages they use.
const NOW: u64 = T0 + 200;

fn news(ui: &mut DmUi, s: DmSnapshot) -> Vec<(usize, String)> {
    news_at(ui, s, NOW)
}

fn news_at(ui: &mut DmUi, s: DmSnapshot, now: u64) -> Vec<(usize, String)> {
    ui.apply_at(s, now);
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
fn a_room_that_leaves_the_loaded_list_and_returns_announces_what_it_got_meanwhile_once() {
    let mut ui = DmUi::default();
    let old = vec![text("a", T0, "x")];
    assert!(news(&mut ui, snap(&[R], vec![thread(R, P, true, old.clone())])).is_empty());
    // An outage: the room is not listed as loaded (and its threads are not sent).
    assert!(news(&mut ui, snap(&[], vec![])).is_empty());
    // It returns with a recent message it did not have.
    let mut back = old;
    back.push(text("b", T0 + 150, WORDS));
    assert_eq!(
        news(&mut ui, snap(&[R], vec![thread(R, P, true, back.clone())])),
        vec![(1, "b".to_owned())]
    );
    // And the node delivering it again is not news.
    assert!(news(&mut ui, snap(&[], vec![])).is_empty());
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
fn a_message_of_the_user_is_not_news() {
    let mut ui = DmUi::default();
    news(&mut ui, snap(&[R], vec![]));
    let rows = vec![mine("m", T0 + 100)];
    assert!(news(&mut ui, snap(&[R], vec![thread(R, P, true, rows)])).is_empty());
    assert_eq!(ui.unread_total(), 0, "and not unread either");
}

#[test]
fn an_unreadable_row_is_unread_but_quiet_until_it_can_be_read() {
    let mut ui = DmUi::default();
    news(&mut ui, snap(&[R], vec![]));
    let row = |body| DmMessage {
        body,
        ..text("p", T0 + 100, WORDS)
    };
    let sealed = row(DmBody::Placeholder(Placeholder::NoKey));
    assert!(news(&mut ui, snap(&[R], vec![thread(R, P, true, vec![sealed])])).is_empty());
    assert_eq!(ui.unread_total(), 1, "it counts as unread");
    // The key arrives, and the same message opens: now it is news (it is recent).
    let open = row(DmBody::Text(WORDS.to_owned()));
    assert_eq!(
        news(&mut ui, snap(&[R], vec![thread(R, P, true, vec![open])])),
        vec![(1, "p".to_owned())]
    );
}

#[test]
fn a_departed_peer_is_not_news() {
    let mut ui = DmUi::default();
    news(&mut ui, snap(&[R], vec![]));
    let gone = vec![text("g", T0 + 100, WORDS)];
    assert!(news(&mut ui, snap(&[R], vec![thread(R, "1002", false, gone)])).is_empty());
}

#[test]
fn messages_of_a_thread_being_deleted_are_not_news() {
    let mut ui = DmUi::default();
    news(&mut ui, snap(&[R], vec![]));
    let mut s = snap(
        &[R],
        vec![thread(R, P, true, vec![text("d", T0 + 100, WORDS)])],
    );
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
    // And it is not news later either, once the deletion is gone from the list.
    let later = snap(
        &[R],
        vec![thread(R, P, true, vec![text("d", T0 + 100, WORDS)])],
    );
    assert!(news(&mut ui, later).is_empty());
}

#[test]
fn an_archived_thread_is_quiet_until_a_message_revives_it() {
    let mut ui = DmUi::default();
    let archived = |at: u64| {
        let mut s = snap(&[R], vec![thread(R, P, true, vec![text("a", T0, "x")])]);
        s.archived.push(DmArchived {
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
fn only_recent_messages_are_announced_and_old_ones_still_count_as_unread() {
    let mut held: Vec<DmMessage> = Vec::new();
    let mut one = |ui: &mut DmUi, key: &str, ts: u64| {
        held.push(text(key, ts, WORDS));
        let t = thread(R, P, true, held.clone());
        news_at(ui, snap(&[R], vec![t]), NOW)
    };
    let mut ui = DmUi::default();
    news(&mut ui, snap(&[R], vec![]));
    let hour = super::dm_view::MAX_NOTIFY_AGE_SECS;
    // Fresh, and exactly at the bound.
    assert_eq!(one(&mut ui, "fresh", NOW - 60).len(), 1);
    assert_eq!(one(&mut ui, "edge", NOW - hour).len(), 1);
    // Two hours old: silent, but unread.
    let before = ui.unread_total();
    assert!(one(&mut ui, "stale", NOW - 2 * hour).is_empty());
    assert_eq!(ui.unread_total(), before + 1);
    // A time far in the future is "recent", and announces once.
    assert_eq!(one(&mut ui, "future", NOW + 1_000_000_000).len(), 1);
    let again = ui.take_arrivals();
    assert!(again.is_empty());
    assert!(
        news_at(
            &mut ui,
            snap(&[R], vec![thread(R, P, true, held.clone())]),
            NOW
        )
        .is_empty()
    );
}

#[test]
fn the_remembered_tokens_are_bounded_and_the_oldest_go_first() {
    let mut ui = DmUi::default();
    news(&mut ui, snap(&[R], vec![]));
    for i in 0..8200 {
        let t = thread(R, P, true, vec![text(&format!("t{i}"), T0 + 100, "x")]);
        ui.apply_at(snap(&[R], vec![t]), NOW);
        ui.take_arrivals();
    }
    assert!(!ui.knows("t0"), "the oldest was forgotten");
    assert!(ui.knows("t8199"));
    // What a snapshot still holds is never forgotten, however many it holds.
    let many: Vec<DmMessage> = (0..9000)
        .map(|i| text(&format!("m{i}"), T0 + 100, "x"))
        .collect();
    ui.apply_at(snap(&[R], vec![thread(R, P, true, many)]), NOW);
    ui.take_arrivals();
    assert!(ui.knows("m0") && ui.knows("m8999"));
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
        ..text("i", T0 + 100, "")
    };
    ui.apply_at(snap(&[R], vec![thread(R, P, true, vec![invite])]), NOW);
    let a = ui.take_arrivals();
    assert_eq!(a[0].preview, "Invitation to Book Club");
    assert!(
        !format!("{a:?}").contains("Book Club"),
        "Debug names nothing"
    );
    ui.apply_at(
        snap(
            &[R],
            vec![thread(
                R,
                P,
                true,
                vec![text("t", T0 + 101, "**bold** words\n\nmore")],
            )],
        ),
        NOW,
    );
    assert_eq!(ui.take_arrivals()[0].preview, "bold words more");
}

// What is shown ------------------------------------------------------------------------------

struct World {
    s: RiverApp,
    rec: Recorder,
    room: String,
    peer: String,
    now: Instant,
    secs: u64,
}

/// The example rooms, with a sentinel for the first room's name and its first other member's
/// nickname, so a leak of either is found by searching for it.
fn rooms_with_sentinels() -> Vec<super::Room> {
    let mut rooms = example::data().rooms;
    rooms[0].name = ROOM_NAME.to_owned();
    let me = rooms[0].me;
    rooms[0]
        .members
        .iter_mut()
        .find(|m| m.id != me)
        .unwrap()
        .nickname = NAME.to_owned();
    rooms
}

/// The app on the example rooms, live, with the first room's first other member as the peer
/// (so a reply passes the member check), and the room met by direct messages. The window is
/// away (so a thread on screen is not being read) and previews are off, as by default.
fn world() -> World {
    let rec = Recorder::default();
    let mut s = RiverApp::detached().with_notifier(Box::new(rec.clone()));
    let rooms = rooms_with_sentinels();
    s.chat.source = Source::Node(LinkStatus::Live);
    s.chat
        .show_node_rooms(rooms.clone(), None, LinkStatus::Live, 1000);
    s.focus_changed(false);
    let room = rooms[0].key.clone();
    let peer = peer_of(&rooms[0], 0);
    let mut w = World {
        s,
        rec,
        room,
        peer,
        now: Instant::now() + Duration::from_secs(1000),
        secs: NOW,
    };
    let met = snap(&[&w.room.clone()], vec![]);
    w.give(met);
    w
}

fn world_with_previews() -> World {
    let mut w = world();
    w.s.chat.private_previews = true;
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
        self.secs += 60;
        self.s.chat.apply_dms_at(s, self.secs);
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
        // Written ten seconds before it is handed over.
        let ts = self.secs + 60 - 10;
        let s = self.thread_with(peer, vec![text(key, ts, words)]);
        self.give(s);
    }

    fn target(&self) -> String {
        dm_target(&self.room, &self.peer)
    }

    fn commands(&self) -> Vec<Action> {
        self.s
            .chat
            .queued_commands()
            .iter()
            .map(|c| c.action.clone())
            .collect()
    }
}

/// Every word that must never be shown with previews off.
const SECRETS: [&str; 3] = [NAME, ROOM_NAME, "mill"];

fn assert_quiet(what: &str, shown: &str) {
    for secret in SECRETS {
        assert!(!shown.contains(secret), "{what} shows {secret:?}: {shown}");
    }
}

/// Every field of a notification, named so that a new field must be looked at here.
fn assert_notification_quiet(what: &str, n: &Notification) {
    let Notification {
        room_key,
        room_name,
        title,
        body,
        mention,
        count: _,
        message_key,
        actions: _,
        reply: _,
    } = n;
    assert!(!mention);
    for (field, value) in [
        ("room_key", room_key),
        ("room_name", room_name),
        ("title", title),
        ("body", body),
        ("message_key", message_key),
    ] {
        assert_quiet(&format!("{what}.{field}"), value);
    }
    assert!(room_name.is_empty(), "no room name at all");
    assert_quiet(&format!("{what} (Debug)"), &format!("{n:?}"));
}

fn assert_arrival_quiet(what: &str, a: &Arrival) {
    let Arrival {
        room_key,
        room_name,
        private: _,
        count: _,
        mention: _,
        for_me_count: _,
        message_key,
        author,
        text,
        quiet: _,
    } = a;
    for (field, value) in [
        ("room_key", room_key),
        ("room_name", room_name),
        ("message_key", message_key),
        ("author", author),
        ("text", text),
    ] {
        assert_quiet(&format!("{what}.{field}"), value);
    }
    assert!(author.is_empty() && text.is_empty() && room_name.is_empty());
    assert_quiet(&format!("{what} (Debug)"), &format!("{a:?}"));
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
    assert_eq!(n[0].room_key, w.target());
    assert!(
        n[0].actions && !n[0].reply,
        "no Reply: the sender is hidden"
    );
    assert_notification_quiet("notification", &n[0]);
    // The same through the conversion directly, with the sentinels in the input.
    let d = DmArrival {
        room: w.room.clone(),
        peer_key: w.peer.clone(),
        peer_name: NAME.to_owned(),
        count: 2,
        key: "k".to_owned(),
        preview: WORDS.to_owned(),
    };
    assert_arrival_quiet("arrival", &notify::dm_arrival(&d, false));
    let on = notify::dm_arrival(&d, true);
    assert!(
        on.author.contains(NAME) && on.text.contains("mill"),
        "not vacuous"
    );
}

#[test]
fn nothing_leaks_through_a_held_arrival_or_the_summary_with_previews_off() {
    let mut w = world();
    // A second message within the cooldown is held.
    w.arrive("m1", WORDS);
    w.now -= Duration::from_secs(50);
    w.arrive("m2", WORDS);
    let held = w.s.limiter.held_for_test();
    assert_eq!(held.len(), 1);
    for a in &held {
        assert_arrival_quiet("held", a);
    }
    // A summary of many threads (several rooms' worth, built directly).
    let arrivals: Vec<Arrival> = (0..5)
        .map(|i| {
            notify::dm_arrival(
                &DmArrival {
                    room: format!("room{i}"),
                    peer_key: i.to_string(),
                    peer_name: NAME.to_owned(),
                    count: 1,
                    key: "k".to_owned(),
                    preview: WORDS.to_owned(),
                },
                false,
            )
        })
        .collect();
    let out = notify::compose(arrivals, false);
    assert_eq!(out.len(), notify::MAX_PER_UPDATE);
    for n in &out {
        assert_notification_quiet("composed", n);
    }
}

#[test]
fn with_previews_on_it_names_the_sender_and_a_plain_bounded_preview() {
    let mut w = world_with_previews();
    w.arrive("m1", &format!("**{WORDS}** {}", "long ".repeat(100)));
    let n = w.shown();
    assert_eq!(n[0].title, format!("{NAME} · direct message"));
    assert!(n[0].body.starts_with(WORDS), "{}", n[0].body);
    assert!(!n[0].body.contains('*'));
    assert!(n[0].body.chars().count() <= 200);
    assert!(n[0].reply);
    assert!(!n[0].room_name.contains(ROOM_NAME), "never the room's name");
}

#[test]
fn an_invitation_notifies_as_its_room_and_never_shows_its_payload() {
    let mut w = world_with_previews();
    let peer = w.peer.clone();
    let invite = DmMessage {
        body: DmBody::Invite {
            room: "x".into(),
            room_label: "Book Club".into(),
            note: Some("NOTE-TEXT".into()),
        },
        ..text("inv", w.secs + 50, "")
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
fn muted_wins_over_do_not_disturb_letting_mentions_through() {
    let mut w = world();
    w.s.chat.apply_modes(
        [(w.room.clone(), NotificationMode::Muted)]
            .into_iter()
            .collect(),
        &[],
    );
    w.s.chat.dnd.pinned = true;
    w.s.chat.dnd.now = 12 * 60;
    w.s.chat.dnd.settings.manual = true;
    w.s.chat.dnd.settings.allow_mentions = true;
    w.arrive("m1", WORDS);
    assert!(w.shown().is_empty());
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
fn a_scheduled_window_silences_them_only_inside_it() {
    let mut w = world();
    w.s.chat.dnd.pinned = true;
    w.s.chat.dnd.settings.schedule = Some(chat_schedule(22 * 60, 7 * 60));
    w.s.chat.dnd.now = 23 * 60;
    w.arrive("m1", WORDS);
    assert!(w.shown().is_empty(), "inside the window");
    w.s.chat.dnd.now = 12 * 60;
    w.arrive("m2", WORDS);
    assert_eq!(w.shown().len(), 1, "outside it");
}

fn chat_schedule(start: u16, end: u16) -> super::DndSchedule {
    super::DndSchedule { start, end }
}

#[test]
fn do_not_disturb_with_mentions_allowed_passes_a_message_with_its_preview() {
    let mut w = world_with_previews();
    w.s.chat.dnd.pinned = true;
    w.s.chat.dnd.now = 12 * 60;
    w.s.chat.dnd.settings.manual = true;
    w.s.chat.dnd.settings.allow_mentions = true;
    w.arrive("m1", WORDS);
    assert_eq!(w.shown()[0].body, WORDS);
}

#[test]
fn a_held_message_is_dropped_by_do_not_disturb_and_passes_when_mentions_are_allowed() {
    for allowed in [false, true] {
        let mut w = world();
        w.arrive("m1", WORDS);
        w.now -= Duration::from_secs(50);
        w.arrive("m2", WORDS);
        assert_eq!(w.shown().len(), 1, "the second is held by the cooldown");
        w.s.chat.dnd.pinned = true;
        w.s.chat.dnd.now = 12 * 60;
        w.s.chat.dnd.settings.manual = true;
        w.s.chat.dnd.settings.allow_mentions = allowed;
        w.s.flush_held(w.now + Duration::from_secs(40));
        assert_eq!(w.shown().len(), if allowed { 2 } else { 1 }, "{allowed}");
        assert!(w.s.limiter.next_due(w.now).is_none() || allowed);
    }
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
fn a_window_that_never_reported_focus_counts_as_in_front_for_the_open_thread() {
    let rec = Recorder::default();
    let mut w = world();
    w.s = RiverApp::detached().with_notifier(Box::new(rec.clone()));
    w.rec = rec;
    let rooms = rooms_with_sentinels();
    w.s.chat.source = Source::Node(LinkStatus::Live);
    w.s.chat
        .show_node_rooms(rooms, None, LinkStatus::Live, 1000);
    w.give(snap(&[&w.room.clone()], vec![]));
    assert!(w.s.chat.open_dm(&w.room, &w.peer));
    w.arrive("m1", WORDS);
    assert!(w.shown().is_empty(), "the same rule marks it read");
}

#[test]
fn threads_have_their_own_cooldown_and_a_room_its_per_minute_cap() {
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
    // Four threads of one room at once: the cap lets 3 through, the 4th waits.
    let mut w = world();
    let threads = peers
        .iter()
        .enumerate()
        .map(|(i, p)| {
            thread(
                &w.room,
                p,
                true,
                vec![text(&format!("k{i}"), w.secs + 55, WORDS)],
            )
        })
        .collect();
    w.give(snap(&[&w.room.clone()], threads));
    assert_eq!(w.shown().len(), notify::DM_PER_ROOM);
    assert!(w.s.limiter.next_due(w.now).is_some(), "the 4th is held");
}

#[test]
fn many_threads_fold_into_a_summary_that_names_none_and_only_raises_the_window() {
    let arrivals = |n: usize| -> Vec<Arrival> {
        (0..n)
            .map(|i| {
                notify::dm_arrival(
                    &DmArrival {
                        room: format!("room{i}"),
                        peer_key: i.to_string(),
                        peer_name: "N".to_owned(),
                        count: 1,
                        key: format!("k{i}"),
                        preview: "p".to_owned(),
                    },
                    true,
                )
            })
            .collect()
    };
    // Up to the limit, one each.
    for n in 1..=notify::MAX_PER_UPDATE {
        let out = notify::compose(arrivals(n), true);
        assert_eq!(out.len(), n);
        assert!(
            out.iter()
                .all(|o| o.actions && o.room_key != notify::DM_SUMMARY_KEY)
        );
    }
    // Past it: the limit less one, and a summary of the rest.
    for (n, summed) in [(4, 2), (5, 3)] {
        let out = notify::compose(arrivals(n), true);
        assert_eq!(out.len(), notify::MAX_PER_UPDATE);
        let last = out.last().unwrap();
        assert_eq!(last.body, format!("{summed} new direct messages"));
        assert_eq!(last.room_key, notify::DM_SUMMARY_KEY);
        assert!(!last.actions && !last.reply && last.message_key.is_empty());
        assert!(
            parse_dm_target(&last.room_key).is_none(),
            "no thread to open"
        );
        assert!(
            out[..out.len() - 1]
                .iter()
                .all(|o| o.room_key != last.room_key)
        );
        assert!(out.iter().all(|o| !o.body.contains("rooms")));
    }
    // A click on it only raises the window.
    let w = world();
    let mut s = w.s;
    assert!(!s.open_message(notify::DM_SUMMARY_KEY, ""));
    assert!(!s.chat.dm.is_open());
}

#[test]
fn a_held_message_is_shown_with_its_count_when_its_cooldown_ends() {
    let mut w = world();
    w.arrive("a1", WORDS);
    w.now -= Duration::from_secs(55);
    w.arrive("a2", WORDS);
    w.now -= Duration::from_secs(55);
    w.arrive("a3", WORDS);
    assert_eq!(w.shown().len(), 1);
    w.s.flush_held(w.now + notify::ROOM_COOLDOWN + Duration::from_secs(1));
    let n = w.shown();
    assert_eq!(n.len(), 2);
    assert_eq!(n[1].body, "2 new direct messages");
    assert_eq!(n[1].room_key, w.target());
    assert_eq!(n[1].message_key, "a3");
}

#[test]
fn what_was_held_is_dropped_when_the_thread_is_opened() {
    let mut w = world();
    w.arrive("a1", WORDS);
    w.now -= Duration::from_secs(55);
    w.arrive("a2", WORDS);
    assert!(w.s.limiter.next_due(w.now).is_some(), "one is held");
    assert!(w.s.open_message(&w.target(), "a2"));
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
    w.s.mark_read_through(&dm_target("gone", "5"), "m1");
    // The Reply button opens the thread with its box ready.
    w.s.open_room_to_reply(&target);
    assert!(w.s.chat.dm.is_open());
}

#[test]
fn a_reply_is_sent_to_the_full_id_through_the_composers_command() {
    let mut w = world_with_previews();
    w.arrive("m1", WORDS);
    let target = w.target();
    assert_eq!(
        w.s.reply_from_notification(&target, "  on my way  "),
        Ok(())
    );
    let sent = w.commands();
    let [Action::SendDm { peer, text, .. }] = sent.as_slice() else {
        panic!("one SendDm")
    };
    assert_eq!(rooms::dm::peer_key(*peer), w.peer);
    assert_eq!(text, "on my way");
}

#[test]
fn a_reply_that_arrives_without_previews_is_not_sent() {
    let mut w = world();
    w.arrive("m1", WORDS);
    let target = w.target();
    assert_eq!(
        w.s.reply_from_notification(&target, "hello"),
        Err(ReplyRefusal::Blocked)
    );
    assert!(w.commands().is_empty());
    assert!(w.s.chat.dm.compose.draft(&w.room, &w.peer).is_none());
}

#[test]
fn a_blank_reply_is_refused_and_nothing_is_kept() {
    let mut w = world_with_previews();
    w.arrive("m1", WORDS);
    assert_eq!(
        w.s.reply_from_notification(&w.target(), "  "),
        Err(ReplyRefusal::Empty)
    );
    assert!(w.commands().is_empty());
}

#[test]
fn a_reply_to_a_room_that_is_gone_is_refused() {
    let mut w = world_with_previews();
    assert_eq!(
        w.s.reply_from_notification(&dm_target("gone", "5"), "hi"),
        Err(ReplyRefusal::NoRoom)
    );
    assert!(w.commands().is_empty());
}

#[test]
fn a_reply_to_someone_who_is_not_a_member_is_refused_and_kept() {
    let mut w = world_with_previews();
    let stranger = dm_target(&w.room, "424242");
    assert_eq!(
        w.s.reply_from_notification(&stranger, "hello"),
        Err(ReplyRefusal::Blocked)
    );
    assert_eq!(w.s.chat.dm.compose.draft(&w.room, "424242"), Some("hello"));
    assert!(w.commands().is_empty());
}

#[test]
fn a_reply_over_the_cap_is_refused_and_kept_cut_to_what_a_reply_may_hold() {
    let mut w = world_with_previews();
    let big = "x".repeat(rooms::dm_send::BODY_CAP + 1);
    assert_eq!(
        w.s.reply_from_notification(&w.target(), &big),
        Err(ReplyRefusal::TooLong)
    );
    assert_eq!(
        w.s.chat.dm.compose.draft(&w.room, &w.peer).map(str::len),
        Some(super::quick_reply::MAX_REPLY_CHARS)
    );
}

#[test]
fn a_reply_is_refused_while_a_send_of_the_thread_waits() {
    let mut w = world_with_previews();
    let t = w.target();
    assert_eq!(w.s.reply_from_notification(&t, "first"), Ok(()));
    assert_eq!(
        w.s.reply_from_notification(&t, "second"),
        Err(ReplyRefusal::Blocked)
    );
    assert_eq!(w.commands().len(), 1);
    assert_eq!(w.s.chat.dm.compose.draft(&w.room, &w.peer), Some("second"));
}

#[test]
fn a_reply_that_was_taken_leaves_the_box_and_clears_the_held_notification() {
    let mut w = world_with_previews();
    w.arrive("a1", WORDS);
    w.now -= Duration::from_secs(55);
    w.arrive("a2", WORDS);
    assert!(w.s.limiter.next_due(w.now).is_some(), "one is held");
    // The user is typing the very same words in the open thread.
    w.s.chat.open_dm(&w.room, &w.peer);
    w.s.chat.dm_write("same");
    assert_eq!(w.s.reply_from_notification(&w.target(), "same"), Ok(()));
    assert!(w.s.limiter.next_due(w.now).is_none(), "nothing held now");
    let ticket = w.s.chat.dm_waiting_ticket_of(&w.room, &w.peer).unwrap();
    w.s.chat.dm_sent(&w.room, &w.peer, ticket, Ok(()));
    assert_eq!(
        w.s.chat.dm.compose.draft(&w.room, &w.peer),
        Some("same"),
        "the box is the user's, not cleared by the reply's answer"
    );
}

#[test]
fn a_refused_reply_never_touches_the_open_threads_live_box_until_it_is_left() {
    let mut w = world_with_previews();
    let t = w.target();
    assert!(w.s.chat.open_dm(&w.room, &w.peer));
    w.s.chat.dm_write("typing");
    assert_eq!(w.s.reply_from_notification(&t, "first"), Ok(()));
    assert_eq!(
        w.s.reply_from_notification(&t, "second"),
        Err(ReplyRefusal::Blocked)
    );
    assert_eq!(
        w.s.chat.dm.compose.draft(&w.room, &w.peer),
        Some("typing"),
        "the live box is as the user left it"
    );
    let line = w.s.chat.dm.compose.refusal_for(&w.room, &w.peer).unwrap();
    assert!(line.contains("leave this conversation"), "{line}");
    w.s.chat.close_dm();
    assert_eq!(
        w.s.chat.dm.compose.draft(&w.room, &w.peer),
        Some("typing\nsecond"),
        "kept for when they come back"
    );
}

#[test]
fn leaving_for_another_thread_also_merges_what_a_reply_left() {
    let mut w = world_with_previews();
    let t = w.target();
    assert!(w.s.chat.open_dm(&w.room, &w.peer));
    assert_eq!(w.s.reply_from_notification(&t, "first"), Ok(()));
    assert_eq!(
        w.s.reply_from_notification(&t, "second"),
        Err(ReplyRefusal::Blocked)
    );
    assert!(
        w.s.chat
            .dm
            .compose
            .draft(&w.room, &w.peer)
            .is_none_or(str::is_empty)
    );
    let other = peer_of(&example::data().rooms[0], 1);
    assert!(w.s.chat.open_dm(&w.room, &other));
    assert_eq!(w.s.chat.dm.compose.draft(&w.room, &w.peer), Some("second"));
}

/// A thread open, a reply refused into its stash (a first reply still waits), and `then` done.
fn stashed_world() -> World {
    let mut w = world_with_previews();
    let t = w.target();
    assert!(w.s.chat.open_dm(&w.room, &w.peer));
    w.s.chat.dm_write("typing");
    assert_eq!(w.s.reply_from_notification(&t, "first"), Ok(()));
    assert_eq!(
        w.s.reply_from_notification(&t, "second"),
        Err(ReplyRefusal::Blocked)
    );
    w
}

#[test]
fn choosing_another_room_merges_the_stash_into_the_threads_draft() {
    let mut w = stashed_world();
    let other = example::data().rooms[1].key.clone();
    w.s.chat.select(&other);
    assert!(!w.s.chat.dm.is_open());
    assert_eq!(
        w.s.chat.dm.compose.draft(&w.room, &w.peer),
        Some("typing\nsecond")
    );
}

#[test]
fn a_thread_that_goes_away_under_the_user_merges_its_stash_too() {
    let mut w = stashed_world();
    // The peer leaves the room and the thread has no messages: nothing is left to show it as.
    let mut rooms = rooms_with_sentinels();
    let peer = w.peer.clone();
    rooms[0].members.retain(|m| m.full_id.to_string() != peer);
    w.s.chat
        .show_node_rooms(rooms, None, LinkStatus::Live, 1001);
    w.s.chat
        .apply_dms_at(snap(&[&w.room.clone()], vec![]), w.secs);
    assert!(!w.s.chat.dm.is_open(), "closed by the settle");
    assert_eq!(
        w.s.chat.dm.compose.draft(&w.room, &w.peer),
        Some("typing\nsecond")
    );
}

#[test]
fn a_stash_that_does_not_fit_the_draft_is_kept_not_dropped() {
    let mut w = stashed_world();
    let key = (w.room.clone(), w.peer.clone());
    // The user fills the box to the brim after the reply was stashed.
    let max = super::quick_reply::MAX_DRAFT_CHARS;
    w.s.chat.dm_write(&"x".repeat(max));
    w.s.chat.close_dm();
    assert_eq!(
        w.s.chat.dm.compose.draft(&w.room, &w.peer).map(str::len),
        Some(max)
    );
    assert_eq!(
        w.s.chat.dm.compose.stash.get(&key).map(String::as_str),
        Some("second"),
        "still aside, tried again at the next leave"
    );
    // Room is made, and the next leave merges it.
    assert!(w.s.chat.open_dm(&w.room, &w.peer));
    w.s.chat.dm_write("short");
    w.s.chat.close_dm();
    assert_eq!(
        w.s.chat.dm.compose.draft(&w.room, &w.peer),
        Some("short\nsecond")
    );
}

#[test]
fn a_reply_is_refused_at_once_when_stash_and_live_draft_together_would_not_fit() {
    let mut w = world_with_previews();
    let t = w.target();
    assert!(w.s.chat.open_dm(&w.room, &w.peer));
    let max = super::quick_reply::MAX_DRAFT_CHARS;
    w.s.chat.dm_write(&"x".repeat(max - 3));
    assert_eq!(w.s.reply_from_notification(&t, "first"), Ok(()));
    assert_eq!(
        w.s.reply_from_notification(&t, "second"),
        Err(ReplyRefusal::Blocked)
    );
    let line = w.s.chat.dm.compose.refusal_for(&w.room, &w.peer).unwrap();
    assert!(line.contains("draft is full"), "{line}");
    assert!(w.s.chat.dm.compose.stash.is_empty());
}

#[test]
fn a_refusal_from_the_background_work_keeps_the_reply_in_the_stash_or_the_draft() {
    // Thread not open: the draft.
    let mut w = world_with_previews();
    assert_eq!(w.s.reply_from_notification(&w.target(), "keep me"), Ok(()));
    let ticket = w.s.chat.dm_waiting_ticket_of(&w.room, &w.peer).unwrap();
    w.s.chat.dm_sent(&w.room, &w.peer, ticket, Err("no".into()));
    assert_eq!(w.s.chat.dm.compose.draft(&w.room, &w.peer), Some("keep me"));
    // Thread open: not the live box until it is left.
    let mut w = world_with_previews();
    assert!(w.s.chat.open_dm(&w.room, &w.peer));
    w.s.chat.dm_write("typing");
    assert_eq!(w.s.reply_from_notification(&w.target(), "keep me"), Ok(()));
    let ticket = w.s.chat.dm_waiting_ticket_of(&w.room, &w.peer).unwrap();
    w.s.chat.dm_sent(&w.room, &w.peer, ticket, Err("no".into()));
    assert_eq!(w.s.chat.dm.compose.draft(&w.room, &w.peer), Some("typing"));
    w.s.chat.close_dm();
    assert_eq!(
        w.s.chat.dm.compose.draft(&w.room, &w.peer),
        Some("typing\nkeep me")
    );
}

#[test]
fn an_identical_reply_after_an_unanswered_one_is_refused_instead_of_sent_twice() {
    let mut w = world_with_previews();
    let t = w.target();
    assert_eq!(w.s.reply_from_notification(&t, "hi"), Ok(()));
    // No answer within the wait: the send is "silent".
    assert!(
        w.s.chat
            .dm_tick(Instant::now() + dm_compose::ANSWER_WAIT * 2)
    );
    assert_eq!(
        w.s.reply_from_notification(&t, "hi"),
        Err(ReplyRefusal::Blocked),
        "may be delivered twice"
    );
    assert_eq!(w.commands().len(), 1);
    // A different text is a different message.
    assert_eq!(w.s.reply_from_notification(&t, "hello"), Ok(()));
    assert_eq!(w.commands().len(), 2);
    // The way out is the conversation, where the composer asks first, as for its own sends.
    assert!(
        w.s.chat
            .dm_tick(Instant::now() + dm_compose::ANSWER_WAIT * 4)
    );
    assert!(w.s.chat.open_dm(&w.room, &w.peer));
    w.s.chat.dm_write("hi");
    assert!(w.s.chat.dm_needs_confirm());
    assert!(!w.s.chat.dm_send(), "Enter does not send it again unasked");
    assert!(w.s.chat.dm_send_again(), "the confirmed send goes");
    assert_eq!(w.commands().len(), 3);
}

#[test]
fn mark_as_read_reads_up_to_the_message_shown_and_leaves_newer_ones() {
    let mut w = world();
    let peer = w.peer.clone();
    let base = w.s.unread();
    let ts = w.secs + 30;
    let msgs = |n: u64| {
        (0..n)
            .map(|i| text(&format!("k{i}"), ts + i, "x"))
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
    let ts = w.secs + 30;
    let s = w.thread_with(&peer, vec![text("a", ts, "x"), text("b", ts + 1, "y")]);
    w.give(s);
    assert_eq!(w.s.unread(), before + 2);
    assert!(
        w.s.window_title(w.s.unread())
            .starts_with(&format!("({})", before + 2))
    );
    // Counting is by the rows built when the snapshot changed, not by a walk of the messages.
    let rebuilds = w.s.chat.dm.rebuilds();
    for _ in 0..10 {
        w.s.unread();
    }
    assert_eq!(w.s.chat.dm.rebuilds(), rebuilds);
    w.s.chat.apply_modes(
        [(w.room.clone(), NotificationMode::Muted)]
            .into_iter()
            .collect(),
        &[],
    );
    assert_eq!(w.s.unread(), before, "a muted room counts none");
    // Archived: off the count until the peer writes past the cutoff.
    w.s.chat.apply_modes(Default::default(), &[]);
    let mut s = w.thread_with(&peer, vec![text("a", ts, "x"), text("b", ts + 1, "y")]);
    s.archived.push(DmArchived {
        room: w.room.clone(),
        peer_key: peer.clone(),
        hidden_at_ts: ts + 5,
    });
    w.give(s);
    assert_eq!(w.s.unread(), before);
}

#[test]
fn an_unread_placeholder_counts_but_never_notifies() {
    let mut w = world();
    let peer = w.peer.clone();
    let before = w.s.unread();
    let sealed = DmMessage {
        body: DmBody::Placeholder(Placeholder::CannotDecrypt),
        ..text("p", w.secs + 30, "")
    };
    let s = w.thread_with(&peer, vec![sealed]);
    w.give(s);
    assert!(w.shown().is_empty());
    assert_eq!(w.s.unread(), before + 1);
}

#[test]
fn targets_round_trip_and_ordinary_keys_are_not_targets() {
    assert_eq!(
        parse_dm_target(&dm_target("r1", "-42")),
        Some(("r1", "-42"))
    );
    for bad in [
        "r1",
        "dm:r1",
        "dm::5",
        "dm:r1:x",
        "dm:r1:",
        "dm:r1:-",
        notify::DM_SUMMARY_KEY,
    ] {
        assert_eq!(parse_dm_target(bad), None, "{bad}");
    }
}
