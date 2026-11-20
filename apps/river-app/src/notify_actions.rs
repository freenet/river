//! The app's side of a notification's click and buttons. The desktop shell calls these from
//! its notification threads (through `AppHandle::update`); each is a thin step over a tested
//! `Chat` method, and keeps held-back notifications consistent with what was just seen.

use crate::RiverApp;
use crate::chat::ReplyRefusal;
use crate::notify::parse_dm_target;

impl RiverApp {
    /// A notification was clicked: open its room and, when it showed a message, bring that
    /// message into view (the overview's reveal). False if the room is gone.
    ///
    /// A direct-message notification names its thread ([`crate::notify::dm_target`]) and opens
    /// that thread, as the rail does.
    pub fn open_message(&mut self, room: &str, message: &str) -> bool {
        if let Some((r, p)) = parse_dm_target(room) {
            return self.open_dm_thread(r, p);
        }
        let active = self.focused || !self.focus_known;
        let opened = self.chat.open_message(room, message, active);
        if opened {
            self.limiter.retain_held(|k| k != room);
        }
        opened
    }

    /// The notification's Reply button, where the system has no text field for it: open the
    /// room with the composer ready.
    pub fn open_room_to_reply(&mut self, room: &str) {
        if let Some((r, p)) = parse_dm_target(room) {
            self.open_dm_thread(r, p);
            return;
        }
        if self.chat.open_room_to_reply(room) {
            self.limiter.retain_held(|k| k != room);
        }
    }

    /// The text typed into a notification's reply field: sent to `room` as a message through
    /// the composer's own command and checks. When it cannot be sent, the room, the window and
    /// every composer are left as they are: the text goes to that room's draft and a line in
    /// the room says why (see `Chat::send_reply`).
    pub fn reply_from_notification(&mut self, room: &str, text: &str) -> Result<(), ReplyRefusal> {
        if let Some((r, p)) = parse_dm_target(room) {
            let result = self
                .chat
                .send_dm_reply(r, p, text, std::time::Instant::now());
            if result.is_ok() {
                self.limiter.retain_held_dm(|a, b| !(a == r && b == p));
            }
            return result;
        }
        let result = self.chat.send_reply(room, text);
        if result.is_ok() {
            self.limiter.retain_held(|k| k != room);
        }
        result
    }

    /// The notification's Mark as read button: the room is read up to the message the
    /// notification showed, and what is newer stays unread. When that was the newest message,
    /// nothing is left to tell the person about in that room either, even if it is the open
    /// one in a window that is not in front.
    pub fn mark_read_through(&mut self, room: &str, message: &str) {
        if let Some((r, p)) = parse_dm_target(room) {
            self.chat.dm.mark_read_through(r, p, message);
            self.drop_read_held();
            return;
        }
        self.chat.mark_read_through(room, message);
        let newest = self
            .chat
            .rooms()
            .iter()
            .find(|r| r.key == room)
            .and_then(|r| r.messages.last())
            .is_some_and(|m| m.key == message);
        if newest {
            self.limiter.retain_held(|k| k != room);
            if self.missed.0 == room {
                self.missed.1 = 0;
            }
        }
        self.drop_read_held();
    }

    /// Open the direct-message thread `(room, peer)`. False if the room is gone, or the peer has
    /// no messages and is no longer a member.
    fn open_dm_thread(&mut self, room: &str, peer: &str) -> bool {
        let opened = self.chat.open_dm(room, peer);
        if opened {
            self.limiter
                .retain_held_dm(|r, p| !(r == room && p == peer));
        }
        opened
    }
}
