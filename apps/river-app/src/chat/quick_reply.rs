//! What a notification's buttons do inside the app: send a quick reply, mark a room read, and
//! open a room at the message the notification showed. The operating system's text comes in
//! from outside the app, so it is bounded and cleaned here, before anything is sent.

use super::{Chat, Mode, Source};
use crate::rooms::Action;
use crate::rooms::room::is_hidden;

/// The most characters read from a reply typed into a notification. A room's own byte limit is
/// far smaller and decides what is sent; this only bounds what is held when something hands
/// over a huge string, and a reply over it is refused like one over the room's limit.
pub const MAX_REPLY_CHARS: usize = 20_000;

/// Why a reply from a notification was not sent. The app says so (a line above the composer),
/// and where the room can take a draft the text is kept in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyRefusal {
    /// Nothing but blanks, or nothing left after cleaning.
    Empty,
    /// No such room (left, or not loaded yet), or the app is showing example rooms.
    NoRoom,
    /// The room does not take messages now (not a member, banned, no key yet, update needed).
    Blocked,
    /// Longer than the room's message size limit.
    TooLong,
}

/// `text` as a message: control and hidden characters dropped (newlines and tabs stay) and
/// trimmed as the composer's text is when sent. Cut to [`MAX_REPLY_CHARS`]; the flag says it
/// was cut.
pub fn clean_reply(text: &str) -> (String, bool) {
    let mut chars = text
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || !is_hidden(*c));
    let kept: String = chars.by_ref().take(MAX_REPLY_CHARS).collect();
    let cut = chars.next().is_some();
    (kept.trim().to_owned(), cut)
}

/// The most characters one room's draft may hold through quick replies. A reply that would take
/// it past this is refused and not kept.
pub const MAX_DRAFT_CHARS: usize = 20_000;

/// `add` after what a draft already holds, on a line of its own.
pub(super) fn appended(old: &str, add: &str) -> String {
    if old.trim().is_empty() {
        add.to_owned()
    } else {
        format!("{}\n{add}", old.trim_end())
    }
}

/// The key notices for a room that is gone are kept under (there is no room to show them in).
const NO_ROOM: &str = "";

impl Chat {
    /// Send `text` to the room keyed `room_key` as a plain message (not a reply to one: a
    /// notification names the room, and a quick answer is just the next message), the same
    /// command the composer sends (`Action::Send`, signed downstream by the same code), after
    /// the same checks: the room exists and takes messages now, and the text fits its size
    /// limit. The open room, the window and what is typed in any composer are left alone.
    ///
    /// Not sent: the text goes to that room's draft (after what is there, up to
    /// [`MAX_DRAFT_CHARS`]), and a line in that room says why.
    pub fn send_reply(&mut self, room_key: &str, text: &str) -> Result<(), ReplyRefusal> {
        let (text, cut) = clean_reply(text);
        if text.is_empty() {
            return Err(ReplyRefusal::Empty);
        }
        let Some(room) = self.rooms.iter().find(|r| r.key == room_key) else {
            self.reply_notices.insert(
                NO_ROOM.to_owned(),
                "A reply typed into a notification was not sent, and could not be kept: that room is no longer here."
                    .to_owned(),
            );
            return Err(ReplyRefusal::NoRoom);
        };
        if !matches!(self.source, Source::Node(_)) {
            return Err(ReplyRefusal::NoRoom);
        }
        let why = if room.send_block.is_some() {
            Some((ReplyRefusal::Blocked, "you cannot send there now"))
        } else if cut || Self::size_of(room, &Mode::Write, &text).is_some_and(|(b, m)| b > m) {
            Some((ReplyRefusal::TooLong, "it is too long"))
        } else {
            None
        };
        let Some((refusal, reason)) = why else {
            self.reply_notices.remove(room_key);
            self.emit(
                room_key.to_owned(),
                Action::Send {
                    text,
                    reply_to: None,
                },
            );
            return Ok(());
        };
        let name = room.name.clone();
        let kept = self.keep_in_draft(room_key, &text);
        let tail = if kept && self.selected == room_key {
            // The open room's composer is live; the stored draft is read when it is next opened.
            "It is saved, and will be in the draft when you leave this room and come back."
        } else if kept {
            "It is kept in that room's draft."
        } else {
            "It could not be kept: the draft is full."
        };
        self.reply_notices.insert(
            room_key.to_owned(),
            format!("Your reply to {name} was not sent because {reason}. {tail}"),
        );
        Err(refusal)
    }

    /// Add `text` to the draft stored for the room keyed `key`, after what is there. Never the
    /// live composer: what the person is typing, or an edit or reply in progress, is not
    /// touched. (Leaving the open room merges this with its composer, see `park_draft`.)
    /// False if the draft would pass [`MAX_DRAFT_CHARS`].
    fn keep_in_draft(&mut self, key: &str, text: &str) -> bool {
        let held = self.drafts.get(key).map_or("", |d| d.text.as_str());
        let joined = appended(held, text);
        if joined.chars().count() > MAX_DRAFT_CHARS {
            return false;
        }
        match self.drafts.get_mut(key) {
            Some(draft) => draft.replace_text(joined),
            None => {
                self.drafts
                    .insert(key.to_owned(), rune::TextEdit::new(joined));
            }
        }
        true
    }

    /// Keep `outgoing`, the composer of the room keyed `key` that is being left, as that room's
    /// draft, after any text a quick reply already left there.
    pub(super) fn park_draft(&mut self, key: String, mut outgoing: rune::TextEdit) {
        if let Some(held) = self.drafts.remove(&key)
            && !held.text.is_empty()
        {
            let joined = appended(&outgoing.text, &held.text);
            outgoing.replace_text(joined);
        }
        if !outgoing.text.is_empty() {
            self.drafts.insert(key, outgoing);
        }
    }

    /// The lines saying a reply from a notification was not sent that belong in the room keyed
    /// `room`: its own, and the one for a room that was gone. Each is a key to dismiss it by
    /// and the text.
    pub fn reply_notices(&self, room: &str) -> Vec<(&str, &str)> {
        [room, NO_ROOM]
            .into_iter()
            .filter_map(|k| self.reply_notices.get_key_value(k))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    /// Dismiss the line kept under `key`.
    pub fn dismiss_reply_notice(&mut self, key: &str) {
        self.reply_notices.remove(key);
    }

    /// Mark the room keyed `key` read up to its message keyed `message`, without opening it:
    /// what a notification showed was seen, and what arrived after it was not. The unread
    /// count falls to what is newer than that message (so a mention newer than it stays
    /// counted), and the read position never moves back. A message no longer in the room
    /// marks nothing. False if nothing changed.
    pub fn mark_read_through(&mut self, key: &str, message: &str) -> bool {
        let Some(room) = self.rooms.iter_mut().find(|r| r.key == key) else {
            return false;
        };
        let Some(at) = room.messages.iter().position(|m| m.key == message) else {
            return false;
        };
        let time = room.messages[at].time;
        let at_time = room.messages[..=at]
            .iter()
            .filter(|m| m.time == time)
            .count();
        let mark = super::ReadMark { time, at_time };
        let unread = mark.unread(room).min(room.unread);
        let moved = unread != room.unread;
        room.unread = unread;
        let later = self
            .read
            .get(key)
            .is_none_or(|old| (old.time, old.at_time) < (mark.time, mark.at_time));
        if later {
            self.read.insert(key.to_owned(), mark);
        }
        moved || later
    }

    /// Open the room keyed `key` with the composer ready to type in. False if there is no
    /// such room.
    pub fn open_room_to_reply(&mut self, key: &str) -> bool {
        if !self.open_room(key) {
            return false;
        }
        if self.actionable().is_some() {
            self.composer.focused = true;
        }
        true
    }

    /// A click on a notification: open the room and bring the message it showed into view,
    /// through the same staged reveal as the overview (it waits a frame when the room was not
    /// already open, or when the window is not the active one). False if the room is gone. A message
    /// that is gone opens the room alone.
    pub fn open_message(&mut self, key: &str, message: &str, window_active: bool) -> bool {
        let known = self
            .rooms
            .iter()
            .find(|r| r.key == key)
            .is_some_and(|r| !message.is_empty() && r.message_by_key(message).is_some());
        if !self.rooms.iter().any(|r| r.key == key) {
            return false;
        }
        self.overview_open(key, known.then_some(message));
        if !window_active {
            // A window that was hidden, minimised or in the background has not laid the room
            // out yet, even when it was the open room.
            self.delay_reveal();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_trimmed_and_stripped_of_hidden_characters() {
        assert_eq!(clean_reply("  hi\u{202e} there\u{0}  ").0, "hi there");
        assert_eq!(clean_reply("a\nb\tc").0, "a\nb\tc");
        assert_eq!(clean_reply(" \n\u{200b} ").0, "");
    }

    #[test]
    fn a_reply_over_the_sanity_cap_is_flagged_not_silently_cut() {
        let long = "x".repeat(MAX_REPLY_CHARS * 3);
        let (kept, cut) = clean_reply(&long);
        assert_eq!(kept.chars().count(), MAX_REPLY_CHARS);
        assert!(cut);
        assert!(!clean_reply(&"x".repeat(MAX_REPLY_CHARS)).1);
    }

    #[test]
    fn a_draft_gets_the_text_after_what_it_holds() {
        assert_eq!(appended("", "b"), "b");
        assert_eq!(appended("  ", "b"), "b");
        assert_eq!(appended("a ", "b"), "a\nb");
    }
}
