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
use crate::rooms::modes::NotificationMode;
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
    /// The message it shows (the newest that mentions you, else the newest), for a click to
    /// scroll to. Empty when it stands for no single message.
    pub message_key: String,
    /// Whether it is about one room, so Reply and Mark as read make sense on it. False for the
    /// summary of several rooms and for notices that are not about a room.
    pub actions: bool,
}

/// How a shell shows notifications and the unread count. Called on the UI thread: an
/// implementation must not block, so it hands the work to a thread of its own.
pub trait Notifier {
    /// Show `notification`, replacing the one before it for the same room where the platform
    /// can. A failure is the implementation's to log; it never reaches the app. False if it
    /// could not even be queued, so the app gives back what it counted.
    fn notify(&self, notification: Notification) -> bool;
    /// The number of unread messages across all rooms, for a dock badge or taskbar overlay.
    /// Called when the number changes, with 0 when everything is read.
    fn set_unread(&self, total: usize);
}

/// The notifier of a headless run or a shell without one: shows nothing.
pub struct Silent;

impl Notifier for Silent {
    fn notify(&self, _: Notification) -> bool {
        true
    }
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
    /// The key of the message shown.
    pub message_key: String,
    /// The author of the message shown.
    pub author: String,
    /// The message shown, as a one-line preview: the newest that mentions you, else the newest.
    pub text: String,
    /// Set by the chat for the first live update after a catch-up: its messages count as unread
    /// while the window is away but are not shown as notifications.
    pub quiet: bool,
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
/// A message counts when its key is new in a room that was in `old`, it is not yours and not
/// still on its way (`pending`), and the room's [`NotificationMode`] (`mode_of`, by room key)
/// lets it through: every message in `All`, only those that mention you or reply to one of
/// yours in `MentionsAndReplies` (`Room::is_for_me`), none in `Muted`. The count and the
/// message shown are of what the mode lets through, so an away window's unread count follows
/// the mode too, as the web UI's title does. Edits and reactions keep a message's key, so they
/// give nothing. A room that was not in `old` gives nothing, and the [`Limiter`] drops what fills a
/// room that just appeared or was empty: that is history.
///
/// Message times are NOT used. They are whatever their author's clock said (one far in the
/// future, or a skewed member, would otherwise silence a room), and a message that reaches
/// this device late is still news.
pub fn arrivals(
    old: &[Room],
    new: &[Room],
    mode_of: &dyn Fn(&str) -> NotificationMode,
) -> Vec<Arrival> {
    let chip = format!("{}:self", mention_scheme());
    let mut out = Vec::new();
    for room in new {
        let Some(before) = old.iter().find(|r| r.key == room.key) else {
            continue;
        };
        let mode = mode_of(&room.key);
        if mode == NotificationMode::Muted {
            continue;
        }
        let fresh: Vec<&Message> = room
            .messages
            .iter()
            .filter(|m| {
                m.author != room.me
                    && m.pending.is_none()
                    && !before.messages.iter().any(|b| b.key == m.key)
                    && (mode == NotificationMode::All || room.is_for_me(m))
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
            message_key: shown.key.clone(),
            author: tidy(room.nickname(shown.author), AUTHOR_CHARS),
            text: tidy(&shown.text, BODY_CHARS),
            quiet: false,
        });
    }
    out
}

/// What the limits remember about one room.
#[derive(Default)]
struct RoomClock {
    /// When a notification for it was last let through, and the time before that (to give back
    /// if the notification could not be queued).
    last: Option<Instant>,
    prev_last: Option<Instant>,
    /// What was held back since: one arrival standing for all of it (counted, mention kept),
    /// shown by the next notification or, if the room goes quiet, when the cooldown ends.
    held: Option<Arrival>,
    /// When it first appeared, or first had messages after being seen empty: while it may
    /// still be filling.
    joined: Option<Instant>,
    /// Whether it was empty when last seen, so its first fill counts as history however long it
    /// was empty.
    empty: bool,
}

impl RoomClock {
    /// Count `a` into what is held back, keeping a mention's flag and text.
    fn hold(&mut self, a: Arrival) {
        match self.held.as_mut() {
            Some(h) => {
                h.count += a.count;
                if a.mention || !h.mention {
                    h.message_key = a.message_key;
                    h.text = a.text;
                    h.author = a.author;
                }
                h.mention |= a.mention;
            }
            None => self.held = Some(a),
        }
    }
}

/// How often notifications may happen: a cooldown per room, a token bucket over all of them,
/// and a settling time for rooms that just appeared. The clock is passed in, so tests set it.
pub struct Limiter {
    rooms: HashMap<String, RoomClock>,
    tokens: f64,
    refilled: Option<Instant>,
    /// Whether the first show of the rooms (empty or not) has been seen: a room first seen
    /// after it was joined or created, and may still be filling.
    started: bool,
    /// After a notification could not be queued, nothing is let through until then.
    paused_until: Option<Instant>,
}

/// How long to hold everything back after the shell could not take a notification.
const RETRY: Duration = Duration::from_secs(5);

impl Default for Limiter {
    fn default() -> Self {
        Limiter {
            rooms: HashMap::new(),
            tokens: PER_MINUTE,
            refilled: None,
            started: false,
            paused_until: None,
        }
    }
}

const TOKEN_RATE: f64 = PER_MINUTE / 60.0;

impl Limiter {
    /// Note the rooms now shown. A room first seen after the first show, or first seen empty,
    /// is given [`SETTLE`] to fill with its history, and again when it first has messages
    /// after being empty.
    ///
    /// `full` says the list is the whole truth: live and not a catch-up. Only then is a room
    /// that is gone forgotten (so leaving and rejoining settles again), and an empty list
    /// forgets them all. A partial list while reconnecting forgets nothing.
    pub fn observe(&mut self, rooms: &[Room], now: Instant, full: bool) {
        if full {
            self.rooms.retain(|k, _| rooms.iter().any(|r| &r.key == k));
        }
        for room in rooms {
            let new = !self.rooms.contains_key(&room.key);
            let clock = self.rooms.entry(room.key.clone()).or_default();
            if new && (self.started || room.messages.is_empty()) {
                clock.joined = Some(now);
            }
            if !new && clock.empty && !room.messages.is_empty() {
                clock.joined = Some(now);
            }
            clock.empty = room.messages.is_empty();
        }
        self.started = true;
    }

    /// Whether `room_key` is still filling with history, so its messages are not news.
    pub fn settling(&self, room_key: &str, now: Instant) -> bool {
        self.rooms
            .get(room_key)
            .and_then(|c| c.joined)
            .is_some_and(|t| now.saturating_duration_since(t) < SETTLE)
    }

    fn refill(&mut self, now: Instant) {
        let before = self.refilled.replace(now).unwrap_or(now);
        let gained = now.saturating_duration_since(before).as_secs_f64() * TOKEN_RATE;
        self.tokens = (self.tokens + gained).min(PER_MINUTE);
    }

    /// The arrivals that may notify now, including held-back ones whose cooldown has ended.
    /// What is held back (by a cooldown or the bucket) is counted into that room's next
    /// notification; a room still settling gives nothing, as what fills it is history.
    pub fn admit(&mut self, arrivals: Vec<Arrival>, now: Instant) -> Vec<Arrival> {
        self.refill(now);
        let mut all = arrivals;
        let quiet: Vec<String> = self
            .rooms
            .iter()
            .filter(|(k, c)| c.held.is_some() && !all.iter().any(|a| &a.room_key == *k))
            .map(|(k, _)| k.clone())
            .collect();
        for key in quiet {
            if let Some(held) = self.rooms.get_mut(&key).and_then(|c| c.held.take()) {
                all.push(held);
            }
        }
        // Mentions first, so they get the tokens.
        all.sort_by_key(|a| !a.mention);
        let mut out = Vec::new();
        for mut a in all {
            let clock = self.rooms.entry(a.room_key.clone()).or_default();
            if clock.joined.is_some_and(|t| now.duration_since(t) < SETTLE) {
                continue;
            }
            let mention = a.mention || clock.held.as_ref().is_some_and(|h| h.mention);
            let cooldown = if mention {
                MENTION_COOLDOWN
            } else {
                ROOM_COOLDOWN
            };
            let cooling = clock
                .last
                .is_some_and(|t| now.saturating_duration_since(t) < cooldown);
            let paused = self.paused_until.is_some_and(|t| now < t);
            if cooling || paused || self.tokens < 1.0 {
                clock.hold(a);
                continue;
            }
            self.tokens -= 1.0;
            clock.prev_last = clock.last;
            clock.last = Some(now);
            if let Some(h) = clock.held.take() {
                a.count += h.count;
                if h.mention && !a.mention {
                    a.mention = true;
                    a.message_key = h.message_key;
                    a.text = h.text;
                    a.author = h.author;
                }
            }
            out.push(a);
        }
        out
    }

    /// How long until something held back can be shown, if anything is held.
    pub fn next_due(&self, now: Instant) -> Option<Duration> {
        let since = now.saturating_duration_since(self.refilled.unwrap_or(now));
        let tokens = (self.tokens + since.as_secs_f64() * TOKEN_RATE).min(PER_MINUTE);
        let token_wait = Duration::from_secs_f64(((1.0 - tokens) / TOKEN_RATE).max(0.0));
        self.rooms
            .values()
            .filter_map(|c| {
                let held = c.held.as_ref()?;
                let cooldown = if held.mention {
                    MENTION_COOLDOWN
                } else {
                    ROOM_COOLDOWN
                };
                let ready = c.last.map_or(Duration::ZERO, |t| {
                    cooldown.saturating_sub(now.saturating_duration_since(t))
                });
                let paused = self
                    .paused_until
                    .map_or(Duration::ZERO, |t| t.saturating_duration_since(now));
                Some(ready.max(token_wait).max(paused) + Duration::from_millis(50))
            })
            .min()
    }

    /// Keep only the held-back arrivals of the rooms `keep` says are still unread.
    pub fn retain_held(&mut self, mut keep: impl FnMut(&str) -> bool) {
        for (key, clock) in &mut self.rooms {
            if clock.held.is_some() && !keep(key) {
                clock.held = None;
            }
        }
    }

    /// Give back what the notifications for `arrivals` used when the shell could not take
    /// them: each token spent, each room's previous cooldown, and the messages (count and
    /// mention) held again to be shown later. Nothing is let through for [`RETRY`], so a
    /// shell that stays full is not retried in a loop.
    pub fn refund(&mut self, arrivals: Vec<Arrival>, now: Instant) {
        for a in arrivals {
            self.tokens = (self.tokens + 1.0).min(PER_MINUTE);
            if let Some(clock) = self.rooms.get_mut(&a.room_key) {
                clock.last = clock.prev_last;
                clock.hold(a);
            }
        }
        self.paused_until = Some(now + RETRY);
    }
}

/// The notifications for `arrivals`: mentions first, at most [`MAX_PER_UPDATE`], the last of
/// them a summary when more rooms than that arrived. `previews` says whether a private room's
/// message text may appear; a public room's always does.
pub fn compose(arrivals: Vec<Arrival>, previews: bool) -> Vec<Notification> {
    compose_from(arrivals, previews)
        .into_iter()
        .map(|(n, _)| n)
        .collect()
}

/// [`compose`], with the arrivals each notification stands for, to give back if it cannot be
/// shown.
pub fn compose_from(
    mut arrivals: Vec<Arrival>,
    previews: bool,
) -> Vec<(Notification, Vec<Arrival>)> {
    // Stable, so rooms keep their list order within each group.
    arrivals.sort_by_key(|a| !a.mention);
    let mut rest = Vec::new();
    if arrivals.len() > MAX_PER_UPDATE {
        rest = arrivals.split_off(MAX_PER_UPDATE - 1);
    }
    let mut out: Vec<(Notification, Vec<Arrival>)> = arrivals
        .into_iter()
        .map(|a| (one(a.clone(), previews), vec![a]))
        .collect();
    if let Some(first) = rest.first() {
        let count: usize = rest.iter().map(|a| a.count).sum();
        let summary = Notification {
            room_key: first.room_key.clone(),
            room_name: tidy(&first.room_name, TITLE_CHARS),
            title: "River".to_owned(),
            body: format!("{count} new messages in {} other rooms", rest.len()),
            mention: rest.iter().any(|a| a.mention),
            count,
            message_key: String::new(),
            actions: false,
        };
        out.push((summary, rest));
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
        message_key: a.message_key,
        actions: true,
    }
}

/// The window title, as the web UI's document title: `River - <room>` with a room open,
/// `River` without one, and `(3) ` in front while the window is in the background with 3
/// messages unread.
pub fn window_title(unread: usize, room: Option<&str>, focused: bool) -> String {
    let base = match room {
        Some(name) => format!("River - {name}"),
        None => "River".to_owned(),
    };
    match unread {
        0 => base,
        _ if focused => base,
        n => format!("({n}) {base}"),
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
        /// Refuse to queue, as a backed-up shell does.
        full: Rc<std::cell::Cell<bool>>,
    }

    impl Notifier for Recorder {
        fn notify(&self, n: Notification) -> bool {
            if self.full.get() {
                return false;
            }
            self.shown.borrow_mut().push(n);
            true
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
        s.limiter.observe(&rooms, Instant::now(), true);
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
    fn a_renamed_member_notifies_under_the_new_name() {
        let (mut s, rec, rooms) = live();
        let m = from_other(&rooms[0], "n1", "supper is ready");
        let mut renamed = rooms.clone();
        renamed[0]
            .members
            .iter_mut()
            .find(|x| x.id == m.author)
            .unwrap()
            .nickname = "Renamed Rae".into();
        deliver(&mut s, with(&renamed, 0, vec![m]));
        assert_eq!(shown(&rec)[0].body, "Renamed Rae: supper is ready");
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

    /// The desktop shell hides the window to the tray on close, which sends no focus event, so
    /// it reports `focus_changed(false)` itself (`background::on_close`). A message in the room
    /// that was open then notifies, where it was silent a moment before.
    #[test]
    fn a_hidden_window_does_not_keep_the_open_room_silent() {
        let (mut s, rec, rooms) = live();
        let open = s.chat.room().unwrap().key.clone();
        let i = rooms.iter().position(|r| r.key == open).unwrap();
        s.focus_changed(true);
        let first = with(&rooms, i, vec![from_other(&rooms[i], "a", "x")]);
        deliver(&mut s, first.clone());
        assert!(shown(&rec).is_empty(), "focused: silent");
        // The window is hidden to the tray.
        s.focus_changed(false);
        deliver(
            &mut s,
            with(&first, i, vec![from_other(&rooms[i], "b", "y")]),
        );
        assert_eq!(shown(&rec).len(), 1);
        assert_eq!(shown(&rec)[0].room_key, open);
    }

    fn arrival(i: usize, mention: bool) -> Arrival {
        Arrival {
            room_key: format!("r{i}"),
            room_name: format!("Room {i}"),
            private: false,
            count: 2,
            mention,
            message_key: format!("m{i}"),
            author: "A".into(),
            text: "t".into(),
            quiet: false,
        }
    }

    #[test]
    fn a_room_notification_carries_its_message_and_offers_actions_the_summary_does_not() {
        let n = compose((0..5).map(|i| arrival(i, false)).collect(), true);
        assert_eq!(n[0].message_key, "m0");
        assert!(n[0].actions);
        let summary = n.last().unwrap();
        assert!(summary.message_key.is_empty());
        assert!(!summary.actions, "it stands for several rooms");
    }

    #[test]
    fn a_held_mention_keeps_its_own_message_for_the_click() {
        let mut clock = RoomClock::default();
        let mut mention = arrival(0, true);
        mention.message_key = "the-mention".into();
        clock.hold(mention);
        let mut plain = arrival(0, false);
        plain.message_key = "later".into();
        clock.hold(plain);
        assert_eq!(clock.held.unwrap().message_key, "the-mention");
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
        lim.observe(&rooms[..1], t0, true);
        let mut joined = rooms[1].clone();
        joined.key = "joined".into();
        let both = [rooms[0].clone(), joined];
        lim.observe(&both, t0, true);
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
    fn the_title_counts_unread_and_the_notifier_hears_changes() {
        assert_eq!(window_title(0, None, false), "River");
        assert_eq!(window_title(7, None, false), "(7) River");
        assert_eq!(window_title(7, None, true), "River");
        assert_eq!(window_title(0, Some("Rust"), false), "River - Rust");
        assert_eq!(window_title(7, Some("Rust"), false), "(7) River - Rust");
        assert_eq!(window_title(7, Some("Rust"), true), "River - Rust");
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
    fn the_unread_count_never_counts_the_room_you_are_looking_at() {
        let (mut s, _, rooms) = live();
        let open = s.chat.room().unwrap().key.clone();
        let i = rooms.iter().position(|r| r.key == open).unwrap();
        s.chat
            .edit_rooms(|r| r.iter_mut().for_each(|r| r.unread = 0));
        let base = s.chat.rooms().to_vec();
        s.focus_changed(true);
        let one = with(&base, i, vec![from_other(&base[i], "a", "x")]);
        deliver(&mut s, one.clone());
        assert_eq!(s.unread(), 0, "focused in the room: nothing unread");
        // Away: exactly one per message, however often the rooms are shown.
        s.focus_changed(false);
        let two = with(&one, i, vec![from_other(&base[i], "b", "y")]);
        deliver(&mut s, two.clone());
        assert_eq!(s.unread(), 1);
        deliver(&mut s, two.clone());
        assert_eq!(s.unread(), 1);
        let three = with(&two, i, vec![from_other(&base[i], "c", "z")]);
        deliver(&mut s, three);
        assert_eq!(s.unread(), 2);
        s.focus_changed(true);
        assert_eq!(s.unread(), 0);
    }

    #[test]
    fn a_held_mention_is_shown_when_its_cooldown_ends_even_if_the_room_goes_quiet() {
        let (mut s, rec, rooms) = live();
        let t0 = minute_later();
        let first = with(&rooms, 0, vec![from_other(&rooms[0], "a", "plain")]);
        deliver_at(&mut s, first.clone(), t0);
        assert_eq!(shown(&rec).len(), 1);
        let mut m = from_other(&rooms[0], "b", "hey @you");
        m.md = format!("hey [@you]({}:self)", mention_scheme());
        deliver_at(
            &mut s,
            with(&first, 0, vec![m]),
            t0 + Duration::from_secs(5),
        );
        assert_eq!(shown(&rec).len(), 1, "held by the mention cooldown");
        s.flush_held(t0 + Duration::from_secs(9));
        assert_eq!(shown(&rec).len(), 1);
        s.flush_held(t0 + Duration::from_secs(11));
        let n = shown(&rec);
        assert_eq!(n.len(), 2);
        assert!(n[1].mention && n[1].title.ends_with("mentioned you"));
        // And it is not shown twice.
        s.flush_held(t0 + Duration::from_secs(60));
        assert_eq!(shown(&rec).len(), 2);
        // The timer asks to be woken for what is held.
        assert!(s.limiter.next_due(t0).is_none());
    }

    #[test]
    fn held_messages_already_read_are_not_reported() {
        let (mut s, rec, rooms) = live();
        let open = s.chat.room().unwrap().key.clone();
        let i = rooms.iter().position(|r| r.key == open).unwrap();
        s.focus_changed(false);
        let t0 = minute_later();
        let a = with(&rooms, i, vec![from_other(&rooms[i], "a", "one")]);
        deliver_at(&mut s, a.clone(), t0);
        deliver_at(
            &mut s,
            with(&a, i, vec![from_other(&rooms[i], "b", "two")]),
            t0 + Duration::from_secs(5),
        );
        assert_eq!(shown(&rec).len(), 1);
        assert!(s.limiter.next_due(t0).is_some());
        // The person comes back to the room before the cooldown ends.
        s.focus_changed(true);
        s.flush_held(t0 + ROOM_COOLDOWN + Duration::from_secs(1));
        assert_eq!(shown(&rec).len(), 1);
        assert!(s.limiter.next_due(t0).is_none());
    }

    /// An away app with one notification shown and a second held back for a room that is not
    /// open, and the room's index and the key of the newest message in it.
    fn held_for_another_room() -> (RiverApp, Instant, String, String) {
        let (mut s, rec, rooms) = live();
        let open = s.chat.room().unwrap().key.clone();
        let i = rooms.iter().position(|r| r.key != open).unwrap();
        s.focus_changed(false);
        let t0 = minute_later();
        let a = with(&rooms, i, vec![from_other(&rooms[i], "a", "one")]);
        deliver_at(&mut s, a.clone(), t0);
        let b = with(&a, i, vec![from_other(&rooms[i], "b", "two")]);
        deliver_at(&mut s, b.clone(), t0 + Duration::from_secs(5));
        assert_eq!(shown(&rec).len(), 1);
        assert!(s.limiter.next_due(t0).is_some(), "the second is held");
        let newest = b[i].messages.last().unwrap().key.clone();
        (s, t0, b[i].key.clone(), newest)
    }

    #[test]
    fn each_notification_action_drops_what_it_makes_stale() {
        // A click opens the room: nothing held for it is shown later.
        let (mut s, t0, room, newest) = held_for_another_room();
        assert!(s.open_message(&room, &newest));
        assert!(s.limiter.next_due(t0).is_none());
        // Reply to the room (sent, so the person is in the conversation).
        let (mut s, t0, room, _) = held_for_another_room();
        s.chat.set_sink(Box::new(|_| {}));
        assert_eq!(s.reply_from_notification(&room, "ok"), Ok(()));
        assert!(s.limiter.next_due(t0).is_none());
        // Reply button.
        let (mut s, t0, room, _) = held_for_another_room();
        s.open_room_to_reply(&room);
        assert!(s.limiter.next_due(t0).is_none());
        // Mark as read through the newest message: nothing unread is left to report.
        let (mut s, t0, room, newest) = held_for_another_room();
        s.mark_read_through(&room, &newest);
        assert!(s.limiter.next_due(t0).is_none());
        // Through an older message: what is newer is still unread, so it is still held.
        let (mut s, t0, room, _) = held_for_another_room();
        let first = s
            .chat
            .rooms()
            .iter()
            .find(|r| r.key == room)
            .unwrap()
            .messages[0]
            .key
            .clone();
        s.mark_read_through(&room, &first);
        assert!(s.limiter.next_due(t0).is_some());
    }

    #[test]
    fn mark_as_read_on_the_open_unfocused_room_clears_what_was_missed_and_held() {
        let (mut s, rec, rooms) = live();
        let open = s.chat.room().unwrap().key.clone();
        let i = rooms.iter().position(|r| r.key == open).unwrap();
        s.focus_changed(false);
        let t0 = minute_later();
        let a = with(&rooms, i, vec![from_other(&rooms[i], "a", "one")]);
        deliver_at(&mut s, a.clone(), t0);
        let b = with(&a, i, vec![from_other(&rooms[i], "b", "two")]);
        deliver_at(&mut s, b.clone(), t0 + Duration::from_secs(5));
        assert_eq!(shown(&rec).len(), 1);
        assert!(s.limiter.next_due(t0).is_some(), "the second is held");
        assert!(s.unread() > 0, "missed while away");
        let newest = b[i].messages.last().unwrap().key.clone();
        s.mark_read_through(&open, &newest);
        assert!(
            s.limiter.next_due(t0).is_none(),
            "nothing held to show later"
        );
        assert_eq!(
            s.unread(),
            s.chat.unread_total(),
            "nothing missed is left in the title"
        );
    }

    #[test]
    fn the_update_notice_offers_no_room_buttons() {
        use crate::update::{UpdateEvent, UpdateStatus};
        let rec = Recorder::default();
        let mut s = RiverApp::detached().with_notifier(Box::new(rec.clone()));
        s.focus_changed(false);
        s.apply_update(UpdateEvent::Status(UpdateStatus::Ready {
            version: "9.9.9".into(),
        }));
        let n = shown(&rec);
        assert_eq!(n.len(), 1);
        assert!(!n[0].actions && n[0].message_key.is_empty());
    }

    #[test]
    fn a_first_room_on_a_fresh_account_settles_before_its_history_counts() {
        let rec = Recorder::default();
        let mut s = RiverApp::detached().with_notifier(Box::new(rec.clone()));
        let t0 = Instant::now();
        let at = |s: &mut RiverApp, rooms: Vec<Room>, status, secs| {
            let a = s.chat.show_node_rooms(rooms, None, status, 1);
            s.notify_arrivals(a, t0 + Duration::from_secs(secs));
        };
        at(&mut s, vec![], LinkStatus::Loading, 0);
        at(&mut s, vec![], LinkStatus::Live, 1);
        let base = example::data().rooms[0].clone();
        let mut joined = base.clone();
        joined.messages.clear();
        at(&mut s, vec![joined.clone()], LinkStatus::Live, 2);
        let mut part = joined.clone();
        part.messages = base.messages[..2].to_vec();
        at(&mut s, vec![part], LinkStatus::Live, 3);
        at(&mut s, vec![base.clone()], LinkStatus::Live, 4);
        assert!(shown(&rec).is_empty());
        // After it settles, a message is news.
        let news = with(
            std::slice::from_ref(&base),
            0,
            vec![from_other(&base, "n", "hi")],
        );
        at(&mut s, news, LinkStatus::Live, 40);
        assert_eq!(shown(&rec).len(), 1);
    }

    #[test]
    fn a_rejoined_room_settles_again() {
        let mut lim = Limiter::default();
        let rooms = example::data().rooms;
        let t0 = Instant::now();
        lim.observe(&rooms, t0, true);
        lim.observe(&rooms[1..], t0 + Duration::from_secs(100), true);
        lim.observe(&rooms, t0 + Duration::from_secs(200), true);
        let mut a = arrival(0, false);
        a.room_key = rooms[0].key.clone();
        assert!(lim.admit(vec![a], t0 + Duration::from_secs(205)).is_empty());
    }

    #[test]
    fn a_notification_that_could_not_be_queued_is_held_and_shown_later() {
        let (mut s, rec, rooms) = live();
        rec.full.set(true);
        let t0 = minute_later();
        let a = with(&rooms, 0, vec![from_other(&rooms[0], "a", "one")]);
        deliver_at(&mut s, a.clone(), t0);
        assert!(shown(&rec).is_empty());
        rec.full.set(false);
        deliver_at(
            &mut s,
            with(&a, 0, vec![from_other(&rooms[0], "b", "two")]),
            t0 + Duration::from_secs(1),
        );
        assert!(shown(&rec).is_empty(), "nothing is retried straight away");
        s.flush_held(t0 + RETRY + Duration::from_secs(1));
        let n = shown(&rec);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].count, 2);
    }

    #[test]
    fn a_refused_burst_gives_back_every_token_and_the_held_messages() {
        let mut lim = Limiter::default();
        let t0 = Instant::now();
        let burst: Vec<Arrival> = (0..6).map(|i| arrival(i, false)).collect();
        let sent = lim.admit(burst, t0);
        assert_eq!(sent.len(), 6);
        lim.refund(sent, t0);
        let later = lim.admit(Vec::new(), t0 + RETRY + Duration::from_secs(1));
        assert_eq!(later.len(), 6);
        assert!(later.iter().all(|a| a.count == 2));
    }

    #[test]
    fn messages_that_arrive_while_away_count_even_in_the_swallowed_update() {
        let (mut s, rec, rooms) = live();
        let open = s.chat.room().unwrap().key.clone();
        let i = rooms.iter().position(|r| r.key == open).unwrap();
        s.chat
            .edit_rooms(|r| r.iter_mut().for_each(|r| r.unread = 0));
        let base = s.chat.rooms().to_vec();
        s.focus_changed(false);
        s.chat.note_status(LinkStatus::Reconnecting);
        deliver(&mut s, with(&base, i, vec![from_other(&base[i], "a", "x")]));
        assert_eq!(s.unread(), 1, "counted for the title");
        assert!(shown(&rec).is_empty(), "but not shown: it may be catch-up");
    }

    #[test]
    fn history_filling_an_empty_room_is_not_unread() {
        let (mut s, rec, _) = live();
        let open = s.chat.room().unwrap().key.clone();
        s.chat
            .edit_rooms(|r| r.iter_mut().for_each(|r| r.unread = 0));
        let rooms = s.chat.rooms().to_vec();
        let i = rooms.iter().position(|r| r.key == open).unwrap();
        s.focus_changed(false);
        let mut emptied = rooms.clone();
        emptied[i].messages.clear();
        let t0 = minute_later();
        deliver_at(&mut s, emptied.clone(), t0);
        // It stays empty far longer than the settling time, then loads its history.
        let mut part = rooms.clone();
        part[i].messages.truncate(3);
        deliver_at(&mut s, part, t0 + Duration::from_secs(300));
        deliver_at(&mut s, rooms.clone(), t0 + Duration::from_secs(310));
        assert_eq!(s.unread(), 0);
        assert!(shown(&rec).is_empty());
    }

    #[test]
    fn a_partial_list_while_reconnecting_forgets_no_room_but_an_empty_live_one_forgets_all() {
        let mut lim = Limiter::default();
        let rooms = example::data().rooms;
        let t0 = Instant::now();
        let mut a = arrival(0, false);
        a.room_key = rooms[0].key.clone();
        lim.observe(&rooms, t0, true);
        lim.observe(&rooms[1..], t0 + Duration::from_secs(100), false);
        lim.observe(&rooms, t0 + Duration::from_secs(200), true);
        assert!(!lim.settling(&a.room_key, t0 + Duration::from_secs(205)));
        // Leaving the last room, then rejoining, settles again.
        lim.observe(&[], t0 + Duration::from_secs(300), true);
        lim.observe(&rooms, t0 + Duration::from_secs(400), true);
        assert!(lim.settling(&a.room_key, t0 + Duration::from_secs(405)));
    }

    #[test]
    fn a_mention_joining_held_messages_keeps_its_flag() {
        let mut lim = Limiter::default();
        let t0 = Instant::now();
        assert_eq!(lim.admit(vec![arrival(0, false)], t0).len(), 1);
        assert!(
            lim.admit(vec![arrival(0, false)], t0 + Duration::from_secs(2))
                .is_empty()
        );
        assert!(
            lim.admit(vec![arrival(0, true)], t0 + Duration::from_secs(4))
                .is_empty()
        );
        let out = lim.admit(Vec::new(), t0 + MENTION_COOLDOWN + Duration::from_secs(1));
        assert_eq!(out.len(), 1);
        assert!(out[0].mention);
        assert_eq!(out[0].count, 4);
    }

    // Per-room modes ---------------------------------------------------------------------------

    fn set_mode(s: &mut RiverApp, room: &Room, mode: NotificationMode) {
        s.chat.set_mode(&room.key, mode);
    }

    fn mention_of_you(room: &Room, key: &str) -> Message {
        let mut m = from_other(room, key, "hey you");
        m.md = format!("hey [@you]({}:self)", mention_scheme());
        m
    }

    /// A reply from someone else to the message of `target_author`.
    fn reply_to(room: &Room, key: &str, target_author: crate::chat::model::MemberId) -> Message {
        let mut m = from_other(room, key, "replying");
        m.reply_to = Some(crate::chat::model::ReplyTo {
            message: room.messages[0].id,
            author: target_author,
            preview: "earlier".into(),
        });
        m
    }

    fn someone_else(room: &Room) -> crate::chat::model::MemberId {
        room.members
            .iter()
            .find(|m| m.id != room.me)
            .expect("another member")
            .id
    }

    #[test]
    fn all_messages_notifies_for_every_kind_of_message() {
        let (mut s, rec, rooms) = live();
        set_mode(&mut s, &rooms[0], NotificationMode::All);
        deliver(
            &mut s,
            with(&rooms, 0, vec![from_other(&rooms[0], "p", "plain")]),
        );
        assert_eq!(shown(&rec).len(), 1);
    }

    #[test]
    fn mentions_and_replies_lets_through_a_mention() {
        let (mut s, rec, rooms) = live();
        set_mode(&mut s, &rooms[0], NotificationMode::MentionsAndReplies);
        deliver(
            &mut s,
            with(&rooms, 0, vec![mention_of_you(&rooms[0], "m")]),
        );
        let n = shown(&rec);
        assert_eq!(n.len(), 1);
        assert!(n[0].mention);
        assert!(n[0].title.contains("mentioned you"));
    }

    #[test]
    fn mentions_and_replies_lets_through_a_reply_to_you_but_not_to_someone_else() {
        let (mut s, rec, rooms) = live();
        let room = &rooms[0];
        set_mode(&mut s, room, NotificationMode::MentionsAndReplies);
        // A reply to someone else's message: nothing.
        deliver(
            &mut s,
            with(&rooms, 0, vec![reply_to(room, "r1", someone_else(room))]),
        );
        assert!(shown(&rec).is_empty());
        // A reply to yours: shown, though it does not mention you.
        let next = with(&rooms, 0, vec![reply_to(room, "r2", room.me)]);
        deliver(&mut s, next);
        let n = shown(&rec);
        assert_eq!(n.len(), 1);
        assert!(!n[0].mention);
        assert_eq!(n[0].count, 1);
    }

    #[test]
    fn mentions_and_replies_drops_a_plain_message_and_your_own() {
        let (mut s, rec, rooms) = live();
        let room = &rooms[0];
        set_mode(&mut s, room, NotificationMode::MentionsAndReplies);
        let mut own = mention_of_you(room, "own");
        own.author = room.me;
        deliver(
            &mut s,
            with(&rooms, 0, vec![from_other(room, "p", "plain"), own]),
        );
        assert!(shown(&rec).is_empty());
    }

    #[test]
    fn a_burst_in_mentions_mode_counts_only_what_qualifies() {
        let (mut s, rec, rooms) = live();
        let room = &rooms[0];
        set_mode(&mut s, room, NotificationMode::MentionsAndReplies);
        let add = vec![
            from_other(room, "a", "noise"),
            mention_of_you(room, "b"),
            from_other(room, "c", "more noise"),
            reply_to(room, "d", room.me),
        ];
        deliver(&mut s, with(&rooms, 0, add));
        let n = shown(&rec);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].count, 2);
        assert!(n[0].mention, "the mention is the one shown");
    }

    #[test]
    fn muted_notifies_nothing_even_for_a_mention() {
        let (mut s, rec, rooms) = live();
        let room = &rooms[0];
        set_mode(&mut s, room, NotificationMode::Muted);
        deliver(
            &mut s,
            with(
                &rooms,
                0,
                vec![mention_of_you(room, "m"), reply_to(room, "r", room.me)],
            ),
        );
        assert!(shown(&rec).is_empty());
    }

    #[test]
    fn a_mode_is_per_room() {
        let (mut s, rec, rooms) = live();
        set_mode(&mut s, &rooms[0], NotificationMode::Muted);
        let mut next = with(&rooms, 0, vec![from_other(&rooms[0], "x", "muted room")]);
        next[1]
            .messages
            .push(from_other(&rooms[1], "y", "other room"));
        deliver(&mut s, next);
        let n = shown(&rec);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].room_key, rooms[1].key);
    }

    #[test]
    fn an_all_room_notifies_with_no_master_switch() {
        let (mut s, rec, rooms) = live();
        set_mode(&mut s, &rooms[0], NotificationMode::All);
        deliver(
            &mut s,
            with(&rooms, 0, vec![from_other(&rooms[0], "a", "hello")]),
        );
        assert_eq!(shown(&rec).len(), 1);
    }

    /// The title's count follows the mode as the web UI's does: nothing when muted, only
    /// mentions and replies in that mode, as the window is away.
    #[test]
    fn the_unread_count_follows_the_mode() {
        let (mut s, _rec, rooms) = live();
        let room = &rooms[1];
        let mut next = rooms.clone();
        next[1].messages.extend([
            from_other(room, "a", "noise"),
            mention_of_you(room, "b"),
            reply_to(room, "c", room.me),
        ]);
        next[1].unread = 3;
        let count = |s: &RiverApp| s.chat.unread_of(&s.chat.rooms()[1]);
        s.chat.edit_rooms(|r| *r = next.clone());
        // Reading the open room would clear it: show a different room as open.
        s.chat.select(&rooms[0].key);
        s.chat.edit_rooms(|r| *r = next.clone());
        assert_eq!(count(&s), 3);
        set_mode(&mut s, room, NotificationMode::MentionsAndReplies);
        assert_eq!(count(&s), 2);
        set_mode(&mut s, room, NotificationMode::Muted);
        assert_eq!(count(&s), 0);
        // The room's share of the total is what moves.
        let total = s.chat.unread_total();
        set_mode(&mut s, room, NotificationMode::All);
        assert_eq!(s.chat.unread_total(), total + 3);
        set_mode(&mut s, room, NotificationMode::Muted);
        set_mode(&mut s, room, NotificationMode::All);
        assert_eq!(count(&s), 3);
    }

    /// While the window is away, what arrives in the open room is counted as the mode lets it.
    #[test]
    fn the_away_count_of_the_open_room_follows_the_mode() {
        let (mut s, _rec, rooms) = live();
        let open = s.chat.room().unwrap().clone();
        let i = rooms.iter().position(|r| r.key == open.key).unwrap();
        set_mode(&mut s, &rooms[i], NotificationMode::MentionsAndReplies);
        s.focus_changed(false);
        let before = s.unread();
        deliver(
            &mut s,
            with(
                &rooms,
                i,
                vec![
                    from_other(&rooms[i], "n", "noise"),
                    mention_of_you(&rooms[i], "m"),
                ],
            ),
        );
        assert_eq!(s.unread(), before + 1);
    }
}
