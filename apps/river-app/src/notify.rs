//! Telling the person about new messages when River is not in front of them.
//!
//! This module holds what is shared by every platform: what a notification says, the
//! [`Notifier`] a shell implements to show one with the operating system, the detection of
//! which messages are new ([`arrivals`]) and the limits on how often a room, or all rooms
//! together, may interrupt ([`Limiter`]). Nothing here touches the operating system, so it
//! is all tested headless; the shell (`apps/desktop`) owns the OS calls.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::chat::model::{Message, Room, mention_scheme};
use crate::rooms::room::is_hidden;

/// The most notifications one update of the rooms may show. Beyond it, the rest are folded
/// into one more that says how much else arrived, so the total is still this many.
pub const MAX_PER_UPDATE: usize = 3;

/// The least time between two notifications for one room; what arrives in between is counted
/// into the next one.
pub const ROOM_COOLDOWN: Duration = Duration::from_secs(30);
/// The same for a room whose new message mentions you.
pub const MENTION_COOLDOWN: Duration = Duration::from_secs(10);
/// Notifications allowed per minute across all rooms, so no room or crowd of rooms can fill
/// the operating system's notification list.
pub const PER_MINUTE: f64 = 6.0;
/// How long a room that appeared after the first show (joined, or just created) is given to
/// fill with its history before its messages count as new.
pub const SETTLE: Duration = Duration::from_secs(30);

/// Limits on what a notification says, in characters.
const TITLE_CHARS: usize = 60;
const AUTHOR_CHARS: usize = 40;
const BODY_CHARS: usize = 200;

/// What to show: one per room per update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// The room this is about, so clicking it opens that room and a newer notification for the
    /// room can replace this one.
    pub room_key: String,
    pub room_name: String,
    pub title: String,
    pub body: String,
    /// Whether the message shown mentions you.
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
    pub private: bool,
    pub count: usize,
    /// Whether the message shown mentions you.
    pub mention: bool,
    /// The author of the message shown.
    pub author: String,
    /// The message shown, as a one-line preview: the newest that mentions you, else the newest.
    pub text: String,
}

/// `text` for a notification: hidden and control characters (including the ones that reverse
/// text direction) dropped, whitespace runs collapsed, cut to `max` characters.
fn tidy(text: &str, max: usize) -> String {
    let one_line: String = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !is_hidden(*c))
        .collect();
    let words = one_line.split(' ').filter(|w| !w.is_empty());
    let flat = words.collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// The messages in `new` that were not in `old`, one [`Arrival`] per room.
///
/// A message counts when its key is new in a room that already had messages, it is not yours
/// and not still on its way (`pending`). Edits and reactions keep a message's key, so they
/// give nothing. A room that was not in `old`, or had no messages, gives nothing: it is
/// being joined or loaded, and what fills it is history.
///
/// Message times are NOT used. They are whatever their author's clock said (one far in the
/// future, or a skewed member, would otherwise silence a room), and a message that reaches
/// this device late is still news.
pub fn arrivals(old: &[Room], new: &[Room]) -> Vec<Arrival> {
    let chip = format!("{}:self", mention_scheme());
    let mut out = Vec::new();
    for room in new {
        let Some(before) = old.iter().find(|r| r.key == room.key) else {
            continue;
        };
        if before.messages.is_empty() {
            continue;
        }
        let fresh: Vec<&Message> = room
            .messages
            .iter()
            .filter(|m| {
                m.author != room.me
                    && m.pending.is_none()
                    && !before.messages.iter().any(|b| b.key == m.key)
            })
            .collect();
        let Some(newest) = fresh.last() else { continue };
        let shown = fresh
            .iter()
            .rev()
            .find(|m| m.md.contains(&chip))
            .unwrap_or(newest);
        out.push(Arrival {
            room_key: room.key.clone(),
            room_name: room.name.clone(),
            private: room.private,
            count: fresh.len(),
            mention: shown.md.contains(&chip),
            author: tidy(room.nickname(shown.author), AUTHOR_CHARS),
            text: tidy(&shown.text, BODY_CHARS),
        });
    }
    out
}

/// What the limits remember about one room.
#[derive(Default)]
struct RoomClock {
    /// When a notification for it was last let through.
    last: Option<Instant>,
    /// Messages held back since, counted into the next notification.
    held: usize,
    /// When it first appeared after the first show, while it may still be filling.
    joined: Option<Instant>,
}

/// How often notifications may happen: a cooldown per room, a token bucket over all of them,
/// and a settling time for rooms that just appeared. The clock is passed in, so tests set it.
pub struct Limiter {
    rooms: HashMap<String, RoomClock>,
    tokens: f64,
    refilled: Option<Instant>,
    /// Whether the first show of the rooms has been seen: rooms in it are settled.
    started: bool,
}

impl Default for Limiter {
    fn default() -> Self {
        Limiter {
            rooms: HashMap::new(),
            tokens: PER_MINUTE,
            refilled: None,
            started: false,
        }
    }
}

impl Limiter {
    /// Note the rooms now shown. A room that is new since the first show is given [`SETTLE`]
    /// to fill with its history.
    pub fn observe(&mut self, rooms: &[Room], now: Instant) {
        for room in rooms {
            let known = self.rooms.contains_key(&room.key);
            let clock = self.rooms.entry(room.key.clone()).or_default();
            if !known && self.started {
                clock.joined = Some(now);
            }
        }
        if !rooms.is_empty() {
            self.started = true;
        }
    }

    /// The arrivals that may notify now. What is held back (by a cooldown, the bucket or a
    /// room still settling) is counted into that room's next notification, except a room's
    /// first messages while it settles, which are history.
    pub fn admit(&mut self, arrivals: Vec<Arrival>, now: Instant) -> Vec<Arrival> {
        let refilled = self.refilled.replace(now).unwrap_or(now);
        let gained = now.saturating_duration_since(refilled).as_secs_f64() * PER_MINUTE / 60.0;
        self.tokens = (self.tokens + gained).min(PER_MINUTE);
        // Mentions first, so they get the tokens.
        let mut arrivals = arrivals;
        arrivals.sort_by_key(|a| !a.mention);
        let mut out = Vec::new();
        for mut a in arrivals {
            let clock = self.rooms.entry(a.room_key.clone()).or_default();
            if clock.joined.is_some_and(|t| now.duration_since(t) < SETTLE) {
                continue;
            }
            let cooldown = if a.mention {
                MENTION_COOLDOWN
            } else {
                ROOM_COOLDOWN
            };
            let cooling = clock
                .last
                .is_some_and(|t| now.saturating_duration_since(t) < cooldown);
            if cooling || self.tokens < 1.0 {
                clock.held += a.count;
                continue;
            }
            self.tokens -= 1.0;
            clock.last = Some(now);
            a.count += std::mem::take(&mut clock.held);
            out.push(a);
        }
        out
    }
}

/// The notifications for `arrivals`: mentions first, at most [`MAX_PER_UPDATE`], the last of
/// them a summary when more rooms than that arrived. `previews` says whether a private room's
/// message text may appear; a public room's always does.
pub fn compose(mut arrivals: Vec<Arrival>, previews: bool) -> Vec<Notification> {
    // Stable, so rooms keep their list order within each group.
    arrivals.sort_by_key(|a| !a.mention);
    let mut rest = Vec::new();
    if arrivals.len() > MAX_PER_UPDATE {
        rest = arrivals.split_off(MAX_PER_UPDATE - 1);
    }
    let mut out: Vec<Notification> = arrivals.into_iter().map(|a| one(a, previews)).collect();
    if let Some(first) = rest.first() {
        let count: usize = rest.iter().map(|a| a.count).sum();
        out.push(Notification {
            room_key: first.room_key.clone(),
            room_name: tidy(&first.room_name, TITLE_CHARS),
            title: "River".to_owned(),
            body: format!("{count} new messages in {} other rooms", rest.len()),
            mention: rest.iter().any(|a| a.mention),
            count,
        });
    }
    out
}

fn one(a: Arrival, previews: bool) -> Notification {
    let room = tidy(&a.room_name, TITLE_CHARS);
    let title = if a.mention {
        format!("{room} · mentioned you")
    } else {
        room.clone()
    };
    let body = if a.count > 1 {
        format!("{} new messages", a.count)
    } else if a.text.is_empty() || (a.private && !previews) {
        format!("New message from {}", a.author)
    } else {
        format!("{}: {}", a.author, a.text)
    };
    Notification {
        room_key: a.room_key,
        room_name: room,
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
        // As the first update did in the app.
        s.limiter.observe(&rooms, Instant::now());
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

    thread_local! {
        static CLOCK: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    }

    /// A fake clock that moves a minute at each call, so limits from an earlier delivery have
    /// passed (a minute refills the whole bucket and outlasts every cooldown).
    fn minute_later() -> Instant {
        let now = CLOCK.get().unwrap_or_else(Instant::now) + Duration::from_secs(60);
        CLOCK.set(Some(now));
        now
    }

    fn deliver(s: &mut RiverApp, rooms: Vec<Room>) {
        deliver_at(s, rooms, minute_later());
    }

    fn deliver_at(s: &mut RiverApp, rooms: Vec<Room>, now: Instant) {
        let arrived = s.chat.show_node_rooms(rooms, None, LinkStatus::Live, 1000);
        s.notify_arrivals(arrived, now);
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
        s.notify_arrivals(a, Instant::now());
        let news = with(&rooms, 0, vec![from_other(&rooms[0], "n1", "hi")]);
        let a = s
            .chat
            .show_node_rooms(news.clone(), None, LinkStatus::Loading, 1);
        s.notify_arrivals(a, Instant::now());
        // The update that ends the catch-up is history too.
        let a = s
            .chat
            .show_node_rooms(news.clone(), None, LinkStatus::Live, 1);
        s.notify_arrivals(a, Instant::now());
        assert!(shown(&rec).is_empty());
        // A reconnect's catch-up is silent as well.
        let a = s
            .chat
            .show_node_rooms(news.clone(), None, LinkStatus::Reconnecting, 1);
        s.notify_arrivals(a, Instant::now());
        let more = with(&news, 0, vec![from_other(&rooms[0], "n2", "back")]);
        let a = s.chat.show_node_rooms(more, None, LinkStatus::Live, 1);
        s.notify_arrivals(a, Instant::now());
        assert!(shown(&rec).is_empty());
    }

    #[test]
    fn your_own_pending_and_unchanged_messages_notify_nothing() {
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
        deliver(&mut s, with(&rooms, 0, vec![own, pending]));
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

    fn arrival(i: usize, mention: bool) -> Arrival {
        Arrival {
            room_key: format!("r{i}"),
            room_name: format!("Room {i}"),
            private: false,
            count: 2,
            mention,
            author: "A".into(),
            text: "t".into(),
        }
    }

    #[test]
    fn a_burst_is_capped_and_summarized() {
        let arrivals: Vec<Arrival> = (0..6).map(|i| arrival(i, i == 5)).collect();
        let n = compose(arrivals, true);
        assert_eq!(n.len(), MAX_PER_UPDATE);
        // The mention is not the one that gets folded away.
        assert_eq!(n[0].room_key, "r5");
        assert_eq!(
            n[MAX_PER_UPDATE - 1].body,
            "8 new messages in 4 other rooms"
        );
        // Exactly one over the cap folds two rooms, never "1 other rooms".
        let n = compose((0..4).map(|i| arrival(i, false)).collect(), true);
        assert_eq!(n.len(), MAX_PER_UPDATE);
        assert_eq!(n[2].body, "4 new messages in 2 other rooms");
        assert_eq!(
            compose((0..3).map(|i| arrival(i, false)).collect(), true).len(),
            3
        );
    }

    #[test]
    fn message_times_decide_nothing() {
        let (mut s, rec, rooms) = live();
        let mut future = from_other(&rooms[0], "future", "from the year 10000");
        future.time = u32::MAX;
        deliver(&mut s, with(&rooms, 0, vec![future.clone()]));
        assert_eq!(shown(&rec).len(), 1);
        // A far-future message already in the room does not silence what follows.
        let base = with(&rooms, 0, vec![future]);
        let mut skewed = from_other(&rooms[0], "skewed", "my clock is slow");
        skewed.time = 1;
        deliver(&mut s, with(&base, 0, vec![skewed]));
        assert_eq!(shown(&rec).len(), 2);
        assert_eq!(
            shown(&rec)[1].body.split(": ").nth(1),
            Some("my clock is slow")
        );
    }

    #[test]
    fn a_room_that_fills_after_it_appears_notifies_nothing() {
        let (mut s, rec, rooms) = live();
        // A joined room appears empty...
        let mut joined = rooms[0].clone();
        joined.key = "joined".into();
        joined.messages.clear();
        let mut with_joined = rooms.clone();
        with_joined.push(joined.clone());
        let t0 = minute_later();
        deliver_at(&mut s, with_joined.clone(), t0);
        // ...then fills with its history, in two steps.
        let mut part = joined.clone();
        part.messages = rooms[0].messages[..2].to_vec();
        let mut step1 = rooms.clone();
        step1.push(part.clone());
        deliver_at(&mut s, step1.clone(), t0 + Duration::from_secs(2));
        let mut full = joined.clone();
        full.messages = rooms[0].messages.clone();
        let mut step2 = rooms.clone();
        step2.push(full);
        deliver_at(&mut s, step2, t0 + Duration::from_secs(4));
        assert!(shown(&rec).is_empty());
    }

    #[test]
    fn a_room_with_history_settles_in_time() {
        let mut lim = Limiter::default();
        let rooms = example::data().rooms;
        let t0 = Instant::now();
        lim.observe(&rooms[..1], t0);
        let mut joined = rooms[1].clone();
        joined.key = "joined".into();
        let both = [rooms[0].clone(), joined];
        lim.observe(&both, t0);
        let mut a = arrival(0, false);
        a.room_key = "joined".into();
        assert!(
            lim.admit(vec![a.clone()], t0 + Duration::from_secs(5))
                .is_empty()
        );
        assert_eq!(lim.admit(vec![a], t0 + SETTLE).len(), 1);
    }

    #[test]
    fn a_room_cannot_notify_more_than_once_per_cooldown() {
        let mut lim = Limiter::default();
        let t0 = Instant::now();
        let one = || vec![arrival(0, false)];
        assert_eq!(lim.admit(one(), t0).len(), 1);
        // Two more arrive inside the cooldown: held, and counted into the next.
        assert!(lim.admit(one(), t0 + Duration::from_secs(5)).is_empty());
        assert!(lim.admit(one(), t0 + Duration::from_secs(29)).is_empty());
        let out = lim.admit(one(), t0 + ROOM_COOLDOWN);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].count, 2 * 3);
        // A mention gets the shorter cooldown, which is still not zero.
        let mention = || vec![arrival(0, true)];
        assert!(
            lim.admit(mention(), t0 + ROOM_COOLDOWN + Duration::from_secs(9))
                .is_empty()
        );
        assert_eq!(
            lim.admit(mention(), t0 + ROOM_COOLDOWN + MENTION_COOLDOWN)
                .len(),
            1
        );
    }

    #[test]
    fn many_rooms_cannot_exceed_the_per_minute_cap() {
        let mut lim = Limiter::default();
        let t0 = Instant::now();
        let many: Vec<Arrival> = (0..20).map(|i| arrival(i, false)).collect();
        assert_eq!(lim.admit(many, t0).len(), PER_MINUTE as usize);
        // Ten seconds later one token has come back.
        let more: Vec<Arrival> = (20..30).map(|i| arrival(i, false)).collect();
        assert_eq!(lim.admit(more, t0 + Duration::from_secs(10)).len(), 1);
        // Mentions take the tokens first.
        let mut lim = Limiter::default();
        let mut mixed: Vec<Arrival> = (0..8).map(|i| arrival(i, false)).collect();
        mixed.push(arrival(99, true));
        let out = lim.admit(mixed, t0);
        assert_eq!(out[0].room_key, "r99");
    }

    #[test]
    fn private_rooms_show_no_text_unless_previews_are_on() {
        let mut a = arrival(0, false);
        a.count = 1;
        a.text = "the secret plan".into();
        a.private = true;
        assert_eq!(
            compose(vec![a.clone()], false)[0].body,
            "New message from A"
        );
        assert_eq!(compose(vec![a.clone()], true)[0].body, "A: the secret plan");
        a.private = false;
        assert_eq!(compose(vec![a], false)[0].body, "A: the secret plan");
    }

    #[test]
    fn what_is_shown_is_one_clean_line_of_limited_length() {
        let mut a = arrival(0, false);
        a.count = 1;
        a.room_name = format!("Evil\u{202E}room\n{}", "x".repeat(100));
        a.author = "Mal".into();
        a.text = tidy("  a\u{202E}b\u{0007}\n\nc  ", 200);
        assert_eq!(a.text, "ab c");
        let n = compose(vec![a], true);
        assert!(!n[0].title.contains('\u{202E}') && !n[0].title.contains('\n'));
        assert!(n[0].title.chars().count() <= TITLE_CHARS);
        assert_eq!(
            tidy(&"y".repeat(500), BODY_CHARS).chars().count(),
            BODY_CHARS
        );
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
