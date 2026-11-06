//! The chat screen: rooms on the left, the open room's conversation in the middle, its members
//! on the right.
//!
//! Nothing here talks to a node yet. The rooms come from a [`Source`], and today the only source
//! is [`Source::Example`], the built-in example data. Selecting a room, sending a message and
//! showing or hiding the members panel all work, in memory.

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
}

pub struct Chat {
    pub source: Source,
    /// The rooms, in the room list's order. Private so every change goes through
    /// [`Chat::set_rooms`], which keeps a room open whenever there is one.
    rooms: Vec<Room>,
    /// The key of the open room. Rooms are looked up by key, so adding or removing other rooms
    /// never opens a different one.
    selected: String,
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
    // TODO: a fixed time for example data; it goes once the app reads real rooms and stamps
    // messages with the clock.
    pub now: u32,
}

impl Chat {
    /// A chat screen showing `rooms`, with the first one open.
    pub fn new(source: Source, rooms: Vec<Room>, today: u32, day_zero: i64, now: u32) -> Self {
        let mut chat = Chat {
            source,
            rooms: Vec::new(),
            selected: String::new(),
            composer: TextEdit::new(""),
            drafts: HashMap::new(),
            members_open: true,
            members_overlay: false,
            today,
            day_zero,
            now,
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

    /// Replace the rooms. The open room stays open if it is still there; otherwise the first
    /// room opens (through [`Chat::select`], so it is marked read and its draft comes back).
    /// Drafts of rooms that are gone are dropped.
    pub fn set_rooms(&mut self, rooms: Vec<Room>) {
        self.rooms = rooms;
        if self.room().is_none()
            && let Some(first) = self.rooms.first().map(|r| r.key.clone())
        {
            self.select(&first);
        }
        let rooms = &self.rooms;
        self.drafts
            .retain(|key, _| rooms.iter().any(|r| &r.key == key));
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
        if key == self.selected {
            return;
        }
        if self.selected.is_empty() {
            // Nothing was open, so what the composer holds belongs to no room yet: keep it.
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
