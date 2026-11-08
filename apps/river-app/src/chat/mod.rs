//! The chat screen: rooms on the left, the open room's conversation in the middle, its members
//! on the right.
//!
//! The rooms come from a [`Source`]: the built-in example data ([`Source::Example`]), where
//! sending works in memory, or the user's real rooms read from the node ([`Source::Node`], see
//! `crate::rooms`), which are read-only for now. Selecting a room and showing or hiding the
//! members panel work either way.

mod conversation;
pub mod example;
mod icons;
pub mod model;
pub(crate) mod style;
#[cfg(test)]
mod tests;
mod view;

use std::collections::HashMap;

use rune::TextEdit;

pub use model::{Member, Message, Reaction, ReplyTo, Role, Room};
pub use view::{NARROW_WIDTH, view};

/// Where the chat screen's rooms come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Built-in example rooms. Nothing is read from or sent to Freenet.
    Example,
    /// The user's rooms, read from the local node. Nothing is sent yet.
    Node(LinkStatus),
}

impl Source {
    /// Whether messages can be sent from this screen.
    pub fn can_send(self) -> bool {
        matches!(self, Source::Example)
    }
}

/// The connection to the node behind [`Source::Node`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkStatus {
    /// The stored rooms are shown; the network's latest is still being fetched.
    Loading,
    /// Subscribed: new messages arrive as they are sent.
    Live,
    /// The connection was lost. The rooms shown are as last seen; River is reconnecting.
    Reconnecting,
}

/// Where the user had read a room up to, kept for rooms read from the node (which this app
/// does not write back): the time of the newest message then, and how many messages had that
/// exact time, so messages arriving later in the same minute still count as unread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReadMark {
    time: u32,
    at_time: usize,
}

impl ReadMark {
    fn of(room: &Room) -> Option<ReadMark> {
        let time = room.last_message()?.time;
        let at_time = room.messages.iter().filter(|m| m.time == time).count();
        Some(ReadMark { time, at_time })
    }

    /// Messages from others after this mark.
    fn unread(self, room: &Room) -> usize {
        let mut same = 0;
        let mut count = 0;
        for m in &room.messages {
            let after = if m.time == self.time {
                same += 1;
                same > self.at_time
            } else {
                m.time > self.time
            };
            if after && m.author != room.me {
                count += 1;
            }
        }
        count
    }
}

pub struct Chat {
    pub source: Source,
    /// The rooms, in the room list's order. Private so every change goes through
    /// [`Chat::set_rooms`], which keeps a room open whenever there is one.
    rooms: Vec<Room>,
    /// The key of the open room. Rooms are looked up by key, so adding or removing other rooms
    /// never opens a different one.
    selected: String,
    /// The room that was open when the list last went empty, to reopen when it comes back.
    last_open: String,
    /// What is typed in the open room's composer.
    pub composer: TextEdit,
    /// Unsent text in the other rooms' composers, by room key, so switching rooms never carries
    /// text from one room to another.
    drafts: HashMap<String, TextEdit>,
    /// Whether a wide window shows the members panel beside the conversation.
    pub members_open: bool,
    /// Whether a narrow window shows the members panel over the conversation. Separate from
    /// `members_open`, so a narrow window opens on the conversation however the wide one was left.
    pub members_overlay: bool,
    /// Today's day number in message time (whose day 0 is `day_zero`).
    pub today: u32,
    /// The date of message time's day 0, in days since 1970-01-01, for the day headings.
    pub day_zero: i64,
    /// The time a message sent now is stamped with, in message time's minutes.
    /// Example data uses a fixed time; rooms read from the node use the clock
    /// ([`Chat::show_node_rooms`]).
    pub now: u32,
    /// Where each room was read up to on this device, by room key (rooms from the node only).
    read: HashMap<String, ReadMark>,
    /// Whether rooms from the node have been shown yet, so the room the web UI had open opens
    /// only the first time.
    node_rooms_shown: bool,
}

impl Chat {
    /// A chat screen showing `rooms`, with the first one open.
    pub fn new(source: Source, rooms: Vec<Room>, today: u32, day_zero: i64, now: u32) -> Self {
        let mut chat = Chat {
            source,
            rooms: Vec::new(),
            selected: String::new(),
            last_open: String::new(),
            composer: TextEdit::new(""),
            drafts: HashMap::new(),
            members_open: true,
            members_overlay: false,
            today,
            day_zero,
            now,
            read: HashMap::new(),
            node_rooms_shown: false,
        };
        chat.set_rooms(rooms);
        chat
    }

    pub fn example() -> Self {
        let data = example::data();
        Chat::new(
            Source::Example,
            data.rooms,
            data.today,
            data.day_zero,
            data.now,
        )
    }

    pub fn rooms(&self) -> &[Room] {
        &self.rooms
    }

    /// Replace the rooms. The open room stays open if it is still there. Otherwise the room
    /// that was open before the list last went empty opens, if it is back, and else the first
    /// room; either way through [`Chat::select`], so it is marked read and its draft comes back.
    ///
    /// Drafts are never dropped here. A list can refill in steps (empty, then some rooms, then
    /// all of them), and a room missing from one step may be in the next; a draft costs a few
    /// bytes, and losing typed text costs the user.
    pub fn set_rooms(&mut self, rooms: Vec<Room>) {
        self.rooms = rooms;
        if self.rooms.is_empty() {
            // An empty list is often a reload on its way to the full one: keep the open room's
            // text as its draft, remember which room it was, and open nothing.
            let mut open = std::mem::replace(&mut self.composer, TextEdit::new(""));
            open.focused = false;
            let key = std::mem::take(&mut self.selected);
            if !key.is_empty() {
                if !open.text.is_empty() {
                    self.drafts.insert(key.clone(), open);
                }
                self.last_open = key;
            }
            return;
        }
        if self.room().is_none() {
            let back = self.rooms.iter().any(|r| r.key == self.last_open);
            let key = if back {
                self.last_open.clone()
            } else {
                self.rooms[0].key.clone()
            };
            self.select(&key);
        }
    }

    /// Show the user's rooms as read from the node, replacing what is shown. `open` is the room
    /// the web UI had open, opened the first time rooms arrive. `now` is the current time in
    /// message time (local minutes since 1970-01-01), which also sets today.
    ///
    /// A room's unread count comes from the node until the room is opened here; after that it
    /// counts from where it was read on this device, since this app does not write the read
    /// position back yet.
    pub fn show_node_rooms(
        &mut self,
        mut rooms: Vec<Room>,
        open: Option<&str>,
        status: LinkStatus,
        now: u32,
    ) {
        self.source = Source::Node(status);
        self.now = now;
        self.day_zero = 0;
        self.today = now / model::MINUTES_PER_DAY;
        for room in &mut rooms {
            if let Some(mark) = self.read.get(&room.key) {
                room.unread = mark.unread(room);
            }
        }
        let first = !self.node_rooms_shown && !rooms.is_empty();
        self.set_rooms(rooms);
        if first {
            self.node_rooms_shown = true;
            if let Some(open) = open {
                self.select(open);
            }
        }
        // The open room is being read.
        if let Some(room) = self.room()
            && let Some(mark) = ReadMark::of(room)
        {
            self.read.insert(room.key.clone(), mark);
        }
    }

    /// Change the rooms in place, then settle the open room as [`Chat::set_rooms`] does.
    pub fn edit_rooms(&mut self, f: impl FnOnce(&mut Vec<Room>)) {
        let mut rooms = std::mem::take(&mut self.rooms);
        f(&mut rooms);
        self.set_rooms(rooms);
    }

    /// The open room. `None` only when there are no rooms.
    pub fn room(&self) -> Option<&Room> {
        self.rooms.iter().find(|r| r.key == self.selected)
    }

    fn room_mut(&mut self) -> Option<&mut Room> {
        self.rooms.iter_mut().find(|r| r.key == self.selected)
    }

    /// Open the room keyed `key`, marking it read. The composer's text stays with the room it
    /// was typed in. Unknown keys are ignored.
    pub fn select(&mut self, key: &str) {
        let Some(room) = self.rooms.iter_mut().find(|r| r.key == key) else {
            return;
        };
        room.unread = 0;
        if let Some(mark) = ReadMark::of(room) {
            self.read.insert(key.to_owned(), mark);
        }
        if key == self.selected {
            return;
        }
        if self.selected.is_empty() {
            // Nothing was open, so what the composer holds belongs to no room yet: keep it,
            // unless it is empty and this room has a draft.
            if self.composer.text.is_empty()
                && let Some(draft) = self.drafts.remove(key)
            {
                self.composer = draft;
            }
            self.selected = key.to_owned();
            return;
        }
        let incoming = self.drafts.remove(key).unwrap_or_else(|| TextEdit::new(""));
        let mut outgoing = std::mem::replace(&mut self.composer, incoming);
        outgoing.focused = false;
        if !outgoing.text.is_empty() {
            self.drafts.insert(self.selected.clone(), outgoing);
        }
        self.selected = key.to_owned();
    }
    /// Send what the composer holds to the open room. Blank text sends nothing; otherwise the
    /// composer clears (keeping focus, so the next message can be typed straight away).
    pub fn send(&mut self) -> bool {
        if !self.source.can_send() {
            return false;
        }
        let text = self.composer.text.trim().to_owned();
        if text.is_empty() {
            return false;
        }
        let now = self.now;
        let Some(room) = self.room_mut() else {
            return false;
        };
        room.send(&text, now);
        self.composer.clear();
        true
    }

    /// Show or hide the members panel, in the layout the window is in now.
    pub fn toggle_members(&mut self, narrow: bool) {
        let open = if narrow {
            &mut self.members_overlay
        } else {
            &mut self.members_open
        };
        *open = !*open;
    }

    /// Whether the members panel shows, in a window that is `narrow` or not.
    pub fn members_shown(&self, narrow: bool) -> bool {
        if narrow {
            self.members_overlay
        } else {
            self.members_open
        }
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

    fn counts(c: &Chat) -> Vec<usize> {
        c.rooms().iter().map(|r| r.messages.len()).collect()
    }

    #[test]
    fn selecting_marks_the_room_read() {
        let mut chat = Chat::example();
        let i = chat
            .rooms()
            .iter()
            .position(|r| r.unread > 0)
            .expect("the example has an unread room");
        let key = chat.rooms()[i].key.clone();
        chat.select(&key);
        assert_eq!(chat.room().unwrap().key, key);
        assert_eq!(chat.rooms()[i].unread, 0);
        chat.select("no-such-room");
        assert_eq!(chat.room().unwrap().key, key);
    }

    #[test]
    fn the_first_room_opens_read() {
        let mut data = example::data();
        data.rooms[0].unread = 3;
        let first = data.rooms[0].key.clone();
        let chat = Chat::new(Source::Example, data.rooms, 0, 0, 0);
        assert_eq!(chat.room().unwrap().key, first);
        assert_eq!(chat.room().unwrap().unread, 0);
    }

    #[test]
    fn the_open_room_is_kept_by_key_when_rooms_move() {
        let mut chat = Chat::example();
        chat.select(example::PRIVATE_ROOM);
        chat.edit_rooms(|rooms| rooms.reverse());
        assert_eq!(chat.room().unwrap().key, example::PRIVATE_ROOM);
    }

    #[test]
    fn removing_the_open_room_opens_the_first_one() {
        let mut chat = Chat::example();
        chat.select(example::PRIVATE_ROOM);
        chat.composer.replace_text("typed in the room that goes");
        chat.edit_rooms(|rooms| {
            rooms.retain(|r| r.key != example::PRIVATE_ROOM);
            rooms.rotate_left(1);
            rooms[0].unread = 2;
        });
        let first = chat.rooms()[0].key.clone();
        assert_eq!(chat.room().unwrap().key, first);
        assert_eq!(chat.room().unwrap().unread, 0);
        // The gone room's draft does not follow into the room that opened.
        assert_eq!(chat.composer.text, "");
        chat.composer.replace_text("hello");
        assert!(chat.send());
        assert_eq!(chat.room().unwrap().last_message().unwrap().text, "hello");
    }

    #[test]
    fn with_no_rooms_nothing_is_open_until_rooms_arrive() {
        let mut chat = Chat::new(Source::Example, Vec::new(), 0, 0, 0);
        assert!(chat.room().is_none());
        // Text in the composer, so only the missing room can stop the send.
        chat.composer.replace_text("into the void");
        assert!(!chat.send());
        chat.set_rooms(example::data().rooms);
        assert!(chat.room().is_some());
        assert!(chat.send());
    }

    #[test]
    fn a_reload_through_an_empty_list_keeps_drafts_and_marks_the_open_room_read() {
        let mut chat = Chat::example();
        let a = chat.room().unwrap().key.clone();
        let b = example::PRIVATE_ROOM;
        chat.composer.replace_text("draft in A");
        chat.select(b);
        chat.composer.replace_text("draft in B");
        chat.composer.focused = true;
        chat.set_rooms(Vec::new());
        assert!(chat.room().is_none());
        assert_eq!(chat.composer.text, "");
        let mut rooms = example::data().rooms;
        for r in &mut rooms {
            r.unread = r.from_others().min(1);
        }
        chat.set_rooms(rooms);
        // The room that was open reopens, read, with its draft, and does not take focus.
        assert_eq!(chat.room().unwrap().key, b);
        assert_eq!(chat.room().unwrap().unread, 0);
        assert_eq!(chat.composer.text, "draft in B");
        assert!(!chat.composer.focused);
        chat.select(&a);
        assert_eq!(chat.composer.text, "draft in A");
    }

    #[test]
    fn a_list_that_refills_in_steps_keeps_every_draft() {
        let mut chat = Chat::example();
        let all = example::data().rooms;
        let b = example::PRIVATE_ROOM;
        chat.select(b);
        chat.composer.replace_text("draft in B");
        chat.composer.focused = true;
        chat.set_rooms(Vec::new());
        // First only the other rooms come back, then B too.
        chat.set_rooms(all.iter().filter(|r| r.key != b).cloned().collect());
        assert_ne!(chat.room().unwrap().key, b);
        assert_eq!(chat.composer.text, "");
        chat.set_rooms(all);
        chat.select(b);
        assert_eq!(chat.composer.text, "draft in B");
        assert!(!chat.composer.focused);
    }

    #[test]
    fn each_room_keeps_its_own_draft() {
        let mut chat = Chat::example();
        let a = chat.room().unwrap().key.clone();
        let b = example::PRIVATE_ROOM;
        chat.composer.replace_text("for A");
        chat.select(b);
        assert_eq!(chat.composer.text, "");
        // B's own text goes to B, and A's draft goes nowhere.
        chat.composer.replace_text("for B");
        let before = counts(&chat);
        assert!(chat.send());
        let after = counts(&chat);
        let bi = chat.rooms().iter().position(|r| r.key == b).unwrap();
        for (i, (x, y)) in before.iter().zip(&after).enumerate() {
            assert_eq!(*y, x + usize::from(i == bi));
        }
        assert_eq!(chat.room().unwrap().last_message().unwrap().text, "for B");
        chat.select(&a);
        assert_eq!(chat.composer.text, "for A");
        assert!(!chat.composer.focused);
    }

    #[test]
    fn example_unread_counts_are_possible() {
        for room in Chat::example().rooms() {
            assert!(room.unread <= room.from_others(), "{}", room.key);
        }
    }

    #[test]
    fn blank_messages_are_not_sent() {
        let mut chat = Chat::example();
        let before = chat.room().unwrap().messages.len();
        chat.composer.replace_text("   \n ");
        assert!(!chat.send());
        assert_eq!(chat.room().unwrap().messages.len(), before);
    }
}
