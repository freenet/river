//! Telling the person about new messages when River is not in front of them.
//!
//! This module holds what is shared by every platform: what a notification says, the
//! [`Notifier`] a shell implements to show one with the operating system, and the detection of
//! which messages are new ([`arrivals`]). Nothing here touches the operating system, so it
//! is all tested headless; the shell (`apps/desktop`) owns the OS calls.

use crate::chat::model::{self, Message, Room, mention_scheme};

/// The most notifications one update of the rooms may show. Anything beyond it is folded into
/// one more that says how much else arrived, so a burst (a room that was quiet for a day)
/// never becomes a stack of banners.
pub const MAX_PER_UPDATE: usize = 3;

/// A message older than the newest one already shown by more than this many minutes is
/// history arriving late, not something to interrupt the person with.
const LATE_GRACE_MINUTES: u32 = 2;

/// Characters of a message shown in a notification's body.
const BODY_CHARS: usize = 140;

/// What to show: one per room per update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// The room this is about, so clicking it opens that room and a newer notification for the
    /// room can replace this one.
    pub room_key: String,
    pub room_name: String,
    pub title: String,
    pub body: String,
    /// Whether one of the messages mentions you.
    pub mention: bool,
    /// How many new messages it stands for.
    pub count: usize,
}

/// How a shell shows notifications and the unread count. Called on the UI thread: an
/// implementation must not block, so it hands the work to a thread of its own.
pub trait Notifier {
    /// Show `notification`, replacing the one before it for the same room where the platform
    /// can. A failure is the implementation's to log; it never reaches the app.
    fn notify(&self, notification: Notification);
    /// The number of unread messages across all rooms, for a dock badge or taskbar overlay.
    /// Called when the number changes, with 0 when everything is read.
    fn set_unread(&self, total: usize);
}

/// The notifier of a headless run or a shell without one: shows nothing.
pub struct Silent;

impl Notifier for Silent {
    fn notify(&self, _: Notification) {}
    fn set_unread(&self, _: usize) {}
}

/// New messages from others in one room, between two shows of the rooms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arrival {
    pub room_key: String,
    pub room_name: String,
    pub count: usize,
    pub mention: bool,
    /// The newest of them, for the body.
    pub author: String,
    pub text: String,
}

/// The messages in `new` that were not in `old` and are worth telling the person about,
/// one [`Arrival`] per room.
///
/// A message counts when its key is new in a room that was already shown, it is not yours or
/// still on its way (`pending`), and it is not history arriving late. A room that was not in
/// `old` (just joined, or just loaded) gives nothing. Edits and reactions keep a message's key,
/// so they give nothing either.
pub fn arrivals(old: &[Room], new: &[Room]) -> Vec<Arrival> {
    let chip = format!("{}:self", mention_scheme());
    let mut out = Vec::new();
    for room in new {
        let Some(before) = old.iter().find(|r| r.key == room.key) else {
            continue;
        };
        let newest = before.messages.iter().map(|m| m.time).max().unwrap_or(0);
        let fresh: Vec<&Message> = room
            .messages
            .iter()
            .filter(|m| {
                m.author != room.me
                    && m.pending.is_none()
                    && m.time.saturating_add(LATE_GRACE_MINUTES) >= newest
                    && !before.messages.iter().any(|b| b.key == m.key)
            })
            .collect();
        let Some(last) = fresh.last() else { continue };
        out.push(Arrival {
            room_key: room.key.clone(),
            room_name: room.name.clone(),
            count: fresh.len(),
            mention: fresh.iter().any(|m| m.md.contains(&chip)),
            author: room.nickname(last.author).to_owned(),
            text: model::preview(&last.text, BODY_CHARS),
        });
    }
    out
}

/// The notifications for `arrivals`: mentions first, at most [`MAX_PER_UPDATE`], and one more
/// for whatever did not fit.
pub fn compose(mut arrivals: Vec<Arrival>) -> Vec<Notification> {
    // Stable, so rooms keep their list order within each group.
    arrivals.sort_by_key(|a| !a.mention);
    let rest = arrivals.split_off(arrivals.len().min(MAX_PER_UPDATE));
    let mut out: Vec<Notification> = arrivals.into_iter().map(one).collect();
    if let Some(first) = rest.first() {
        let count: usize = rest.iter().map(|a| a.count).sum();
        out.push(Notification {
            room_key: first.room_key.clone(),
            room_name: first.room_name.clone(),
            title: "River".to_owned(),
            body: format!("{count} new messages in {} more rooms", rest.len()),
            mention: rest.iter().any(|a| a.mention),
            count,
        });
    }
    out
}

fn one(a: Arrival) -> Notification {
    let title = if a.mention {
        format!("{} · mentioned you", a.room_name)
    } else {
        a.room_name.clone()
    };
    let body = if a.count > 1 {
        format!("{} new messages", a.count)
    } else if a.text.is_empty() {
        format!("{}: new message", a.author)
    } else {
        format!("{}: {}", a.author, a.text)
    };
    Notification {
        room_key: a.room_key,
        room_name: a.room_name,
        title,
        body,
        mention: a.mention,
        count: a.count,
    }
}

/// The window title: `(3) River` while 3 messages are unread.
pub fn window_title(unread: usize) -> String {
    match unread {
        0 => "River".to_owned(),
        n => format!("({n}) River"),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use rune::{Env, Runtime};

    use super::*;
    use crate::chat::{LinkStatus, example};
    use crate::{RiverApp, view};

    /// A notifier that records what it was asked to show.
    #[derive(Clone, Default)]
    struct Recorder {
        shown: Rc<RefCell<Vec<Notification>>>,
        unread: Rc<RefCell<Vec<usize>>>,
    }

    impl Notifier for Recorder {
        fn notify(&self, n: Notification) {
            self.shown.borrow_mut().push(n);
        }
        fn set_unread(&self, total: usize) {
            self.unread.borrow_mut().push(total);
        }
    }

    /// An app on the example rooms with the web UI's two rooms shown live, and a recorder.
    fn live() -> (RiverApp, Recorder, Vec<Room>) {
        let rec = Recorder::default();
        let mut s = RiverApp::detached().with_notifier(Box::new(rec.clone()));
        let rooms = example::data().rooms;
        s.chat
            .show_node_rooms(rooms.clone(), None, LinkStatus::Live, 1000);
        // A second live update, so the next is compared with something live.
        s.chat
            .show_node_rooms(rooms.clone(), None, LinkStatus::Live, 1000);
        (s, rec, rooms)
    }

    /// A message from someone else in `room`, newer than all it has.
    fn from_other(room: &Room, key: &str, text: &str) -> Message {
        let other = room.members.iter().find(|m| m.id != room.me).unwrap().id;
        let mut m = room.messages.last().unwrap().clone();
        m.key = key.to_owned();
        m.id += 1000;
        m.author = other;
        m.time = room.messages.iter().map(|m| m.time).max().unwrap();
        m.text = text.to_owned();
        m.md = text.to_owned();
        m.source = text.to_owned();
        m.pending = None;
        m.reply_to = None;
        m.reactions.clear();
        m
    }

    fn with(rooms: &[Room], room: usize, add: Vec<Message>) -> Vec<Room> {
        let mut next = rooms.to_vec();
        next[room].messages.extend(add);
        next
    }

    fn deliver(s: &mut RiverApp, rooms: Vec<Room>) {
        let arrived = s.chat.show_node_rooms(rooms, None, LinkStatus::Live, 1000);
        s.notify_arrivals(arrived);
    }

    fn shown(rec: &Recorder) -> Vec<Notification> {
        rec.shown.borrow().clone()
    }

    #[test]
    fn a_new_message_from_someone_else_notifies_with_author_and_text() {
        let (mut s, rec, rooms) = live();
        let m = from_other(&rooms[0], "n1", "supper is ready");
        let author = rooms[0].nickname(m.author).to_owned();
        deliver(&mut s, with(&rooms, 0, vec![m]));
        let n = shown(&rec);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].room_key, rooms[0].key);
        assert_eq!(n[0].title, rooms[0].name);
        assert_eq!(n[0].body, format!("{author}: supper is ready"));
        assert!(!n[0].mention);
    }

    #[test]
    fn the_first_rooms_and_the_catch_up_notify_nothing() {
        let rec = Recorder::default();
        let mut s = RiverApp::detached().with_notifier(Box::new(rec.clone()));
        let rooms = example::data().rooms;
        // Nothing shown yet, then the stored rooms, then the catch-up with a new message.
        let a = s
            .chat
            .show_node_rooms(rooms.clone(), None, LinkStatus::Loading, 1);
        s.notify_arrivals(a);
        let news = with(&rooms, 0, vec![from_other(&rooms[0], "n1", "hi")]);
        let a = s
            .chat
            .show_node_rooms(news.clone(), None, LinkStatus::Loading, 1);
        s.notify_arrivals(a);
        // The update that ends the catch-up is history too.
        let a = s
            .chat
            .show_node_rooms(news.clone(), None, LinkStatus::Live, 1);
        s.notify_arrivals(a);
        assert!(shown(&rec).is_empty());
        // A reconnect's catch-up is silent as well.
        let a = s
            .chat
            .show_node_rooms(news.clone(), None, LinkStatus::Reconnecting, 1);
        s.notify_arrivals(a);
        let more = with(&news, 0, vec![from_other(&rooms[0], "n2", "back")]);
        let a = s.chat.show_node_rooms(more, None, LinkStatus::Live, 1);
        s.notify_arrivals(a);
        assert!(shown(&rec).is_empty());
    }

    #[test]
    fn your_own_pending_old_and_unchanged_messages_notify_nothing() {
        let (mut s, rec, rooms) = live();
        let room = &rooms[0];
        let mut own = from_other(room, "own", "mine");
        own.author = room.me;
        let mut pending = from_other(room, "pend", "on its way");
        pending.pending = Some(crate::chat::model::Pending {
            entry: 1,
            failed: None,
            retry: false,
        });
        let mut history = from_other(room, "old", "from long ago");
        history.time = 1;
        deliver(&mut s, with(&rooms, 0, vec![own, pending, history]));
        // The same rooms again, and an edit and a reaction on an old message.
        let mut edited = rooms.clone();
        edited[0].messages[0].text = "edited".into();
        edited[0].messages[0].edited = true;
        deliver(&mut s, edited);
        assert!(shown(&rec).is_empty());
    }

    #[test]
    fn a_room_that_first_appears_notifies_nothing() {
        let (mut s, rec, rooms) = live();
        let mut few = rooms.clone();
        let joined = few.pop().unwrap();
        deliver(&mut s, few);
        deliver(
            &mut s,
            [rooms[..rooms.len() - 1].to_vec(), vec![joined]].concat(),
        );
        assert!(shown(&rec).is_empty());
    }

    #[test]
    fn several_messages_in_one_room_make_one_notification_with_a_count() {
        let (mut s, rec, rooms) = live();
        let add = vec![
            from_other(&rooms[0], "a", "one"),
            from_other(&rooms[0], "b", "two"),
            from_other(&rooms[0], "c", "three"),
        ];
        deliver(&mut s, with(&rooms, 0, add));
        let n = shown(&rec);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].count, 3);
        assert_eq!(n[0].body, "3 new messages");
    }

    #[test]
    fn a_mention_of_you_is_flagged() {
        let (mut s, rec, rooms) = live();
        let mut m = from_other(&rooms[0], "m1", "hey @you");
        m.md = format!("hey [@you]({}:self)", mention_scheme());
        let plain = from_other(&rooms[1], "m2", "hello");
        let mut next = with(&rooms, 0, vec![m]);
        next[1].messages.push(plain);
        deliver(&mut s, next);
        let n = shown(&rec);
        assert_eq!(n.len(), 2);
        // Mentions come first.
        assert!(n[0].mention && n[0].title.ends_with("mentioned you"));
        assert!(!n[1].mention);
    }

    #[test]
    fn the_open_room_is_silent_while_focused_but_not_while_away() {
        let (mut s, rec, rooms) = live();
        let open = s.chat.room().unwrap().key.clone();
        let i = rooms.iter().position(|r| r.key == open).unwrap();
        s.focus_changed(true);
        deliver(
            &mut s,
            with(&rooms, i, vec![from_other(&rooms[i], "a", "x")]),
        );
        assert!(shown(&rec).is_empty());
        // Another room still notifies while focused.
        let j = (i + 1) % rooms.len();
        let next = with(&rooms, i, vec![from_other(&rooms[i], "a", "x")]);
        deliver(
            &mut s,
            with(&next, j, vec![from_other(&rooms[j], "b", "y")]),
        );
        assert_eq!(shown(&rec).len(), 1);
        assert_eq!(shown(&rec)[0].room_key, rooms[j].key);
        // Unfocused, the open room notifies too, and is counted as missed.
        s.focus_changed(false);
        let next = with(&next, i, vec![from_other(&rooms[i], "c", "z")]);
        deliver(&mut s, next);
        assert_eq!(shown(&rec).len(), 2);
        assert_eq!(shown(&rec)[1].room_key, open);
        assert_eq!(s.missed.1, 1);
        // Coming back clears what was missed.
        s.focus_changed(true);
        assert_eq!(s.missed.1, 0);
    }

    #[test]
    fn a_burst_is_capped_and_summarized() {
        let arrivals: Vec<Arrival> = (0..6)
            .map(|i| Arrival {
                room_key: format!("r{i}"),
                room_name: format!("Room {i}"),
                count: 2,
                mention: i == 5,
                author: "A".into(),
                text: "t".into(),
            })
            .collect();
        let n = compose(arrivals);
        assert_eq!(n.len(), MAX_PER_UPDATE + 1);
        // The mention is not the one that gets folded away.
        assert_eq!(n[0].room_key, "r5");
        assert_eq!(n[MAX_PER_UPDATE].body, "6 new messages in 3 more rooms");
    }

    #[test]
    fn the_switch_turns_notifications_off() {
        let (mut s, rec, rooms) = live();
        s.set_notifications(false);
        deliver(
            &mut s,
            with(&rooms, 0, vec![from_other(&rooms[0], "a", "x")]),
        );
        assert!(shown(&rec).is_empty());
        s.set_notifications(true);
        let next = with(&rooms, 0, vec![from_other(&rooms[0], "a", "x")]);
        deliver(
            &mut s,
            with(&next, 0, vec![from_other(&rooms[0], "b", "y")]),
        );
        assert_eq!(shown(&rec).len(), 1);
    }

    #[test]
    fn the_title_counts_unread_and_the_notifier_hears_changes() {
        assert_eq!(window_title(0), "River");
        assert_eq!(window_title(7), "(7) River");
        let rec = Recorder::default();
        let mut s = RiverApp::detached().with_notifier(Box::new(rec.clone()));
        s.chat.source = crate::chat::Source::Node(LinkStatus::Live);
        s.chat.edit_rooms(|rooms| rooms[1].unread = 4);
        let mut rt = Runtime::new(
            s,
            view,
            (900.0, 700.0),
            Env {
                system_dark: true,
                scale: 1.0,
            },
        );
        rt.poll = crate::spec().poll;
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);
        let total = rt.state.unread();
        assert!(total >= 4);
        assert_eq!(
            *rec.unread.borrow(),
            vec![total],
            "told once, not every frame"
        );
    }

    #[test]
    fn clicking_a_notification_opens_its_room() {
        let (mut s, _, rooms) = live();
        let target = rooms
            .iter()
            .find(|r| r.key != s.chat.room().unwrap().key)
            .unwrap()
            .key
            .clone();
        s.chat.members_overlay = true;
        s.open_room(&target);
        assert_eq!(s.chat.room().unwrap().key, target);
        assert!(!s.chat.members_overlay);
        // A room that is gone is ignored.
        s.open_room("nope");
        assert_eq!(s.chat.room().unwrap().key, target);
    }

    #[test]
    fn the_footer_switch_turns_notifications_off() {
        let mut s = RiverApp::detached();
        s.setup = crate::Setup::Connected {
            version: "0.2.155".into(),
        };
        s.chat.source = crate::chat::Source::Node(LinkStatus::Live);
        let mut rt = Runtime::new(
            s,
            view,
            (1000.0, 700.0),
            Env {
                system_dark: true,
                scale: 1.0,
            },
        );
        rt.frame(0.0);
        rt.frame(1.0 / 60.0);
        assert!(rt.state.chat.notifications);
        assert!(
            rt.find("notifications").is_some(),
            "the switch row is shown"
        );
        let (cx, cy) = rt.find("control").expect("the switch control");
        rt.pointer_down(cx, cy);
        rt.pointer_up(cx, cy);
        rt.frame(0.1);
        rt.frame(0.2);
        assert!(!rt.state.chat.notifications);
    }
}
