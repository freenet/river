//! The chat screen: rooms on the left, the open room's conversation in the middle, its members
//! on the right.
//!
//! Nothing here talks to a node yet. The rooms come from a [`Source`], and today the only source
//! is [`Source::Example`], the built-in example data. Selecting a room, sending a message and
//! showing or hiding the members panel all work, in memory.

pub mod example;
mod icons;
pub mod model;
mod style;
mod view;

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
    pub rooms: Vec<Room>,
    /// Index into `rooms` of the open room.
    pub selected: usize,
    pub composer: TextEdit,
    pub members_open: bool,
    /// Today's day number, for the "Today" and "Yesterday" headings.
    pub today: u32,
    /// The time a message sent now is stamped with, in the example's minutes.
    pub now: u32,
}

impl Chat {
    pub fn example() -> Self {
        let data = example::data();
        Chat {
            source: Source::Example,
            rooms: data.rooms,
            selected: 0,
            composer: TextEdit::new(""),
            members_open: true,
            today: data.today,
            now: data.now,
        }
    }

    pub fn room(&self) -> Option<&Room> {
        self.rooms.get(self.selected)
    }

    /// Open the room keyed `key`, marking it read. Unknown keys are ignored.
    pub fn select(&mut self, key: &str) {
        if let Some(i) = self.rooms.iter().position(|r| r.key == key) {
            self.selected = i;
            self.rooms[i].unread = 0;
        }
    }

    /// Send what the composer holds to the open room. Blank text sends nothing; otherwise the
    /// composer clears (keeping focus, so the next message can be typed straight away).
    pub fn send(&mut self) -> bool {
        let text = self.composer.text.trim().to_owned();
        if text.is_empty() {
            return false;
        }
        let now = self.now;
        let Some(room) = self.rooms.get_mut(self.selected) else {
            return false;
        };
        room.send(&text, now);
        self.composer.clear();
        true
    }

    pub fn toggle_members(&mut self) {
        self.members_open = !self.members_open;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selecting_marks_the_room_read() {
        let mut chat = Chat::example();
        let (i, key) = chat
            .rooms
            .iter()
            .enumerate()
            .find(|(_, r)| r.unread > 0)
            .map(|(i, r)| (i, r.key.clone()))
            .expect("the example has an unread room");
        chat.select(&key);
        assert_eq!(chat.selected, i);
        assert_eq!(chat.rooms[i].unread, 0);
        chat.select("no-such-room");
        assert_eq!(chat.selected, i);
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
