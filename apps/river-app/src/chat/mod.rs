//! The chat screen: rooms on the left, the open room's conversation in the middle, its members
//! on the right.
//!
//! The rooms come from a [`Source`]: the built-in example data ([`Source::Example`]), where
//! sending, replying, editing, deleting and reacting work in memory, or the user's real rooms
//! read from the node ([`Source::Node`], see `crate::rooms`), where each of those becomes a
//! [`Command`] for the background work, which sends it and shows it on its way. Selecting a
//! room and showing or hiding the members panel work either way. Creating a room, joining one
//! from an invite code and inviting someone are only for rooms from the node: each is a
//! [`Command`] too, and what became of it comes back as [`WorkView`]s and an invitation's link.

mod conversation;
pub mod example;
mod icons;
pub mod model;
pub(crate) mod style;
#[cfg(test)]
mod tests;
mod view;

use std::collections::{HashMap, HashSet};

use rune::TextEdit;

use crate::rooms::work::{WorkState, WorkView};
use crate::rooms::{Action, Command};

pub use model::{Member, Message, Reaction, ReplyTo, Role, Room};
pub use view::{NARROW_WIDTH, view};

/// Where the chat screen's rooms come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Built-in example rooms. Nothing is read from or sent to Freenet.
    Example,
    /// The user's rooms, read from the local node. What the user does is sent to it.
    Node(LinkStatus),
}

/// What the composer is doing besides writing a new message.
#[derive(Clone, Debug, Default)]
pub enum Mode {
    #[default]
    Write,
    /// The next message replies to the message keyed so.
    Reply(String),
    /// The composer holds the text of your message keyed so; sending saves it. `draft` is what
    /// the composer held before, given back afterwards.
    Edit { key: String, draft: Box<TextEdit> },
    /// Asking whether to delete your message keyed so.
    Delete(String),
}

/// Where commands for the node go.
pub type Sink = Box<dyn Fn(Command)>;

/// A dialog over the chat screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogKind {
    Create,
    Join,
}

/// What the create and join dialogs hold.
pub struct Form {
    pub open: Option<DialogKind>,
    pub name: TextEdit,
    pub nickname: TextEdit,
    pub code: TextEdit,
    pub private: bool,
    /// Why what was entered can't be used, said in the dialog.
    pub error: Option<String>,
}

impl Default for Form {
    fn default() -> Self {
        Form {
            open: None,
            name: TextEdit::new(""),
            nickname: TextEdit::new(""),
            code: TextEdit::new(""),
            private: false,
            error: None,
        }
    }
}

/// Where the invitation asked for in a room stands. Its `Debug` hides the link, which is a
/// credential.
#[derive(Clone, PartialEq, Eq)]
pub enum InviteState {
    Making,
    /// Made, and its link copied.
    Copied(String),
    Failed(String),
}

impl std::fmt::Debug for InviteState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InviteState::Making => f.write_str("Making"),
            InviteState::Copied(_) => f.write_str("Copied(<hidden>)"),
            InviteState::Failed(why) => f.debug_tuple("Failed").field(why).finish(),
        }
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
    /// What the rooms from the node are missing, for the room list and the empty screen.
    pub issues: crate::rooms::Issues,
    /// Replying, editing or deleting in the open room.
    pub mode: Mode,
    /// The message (by key) whose reaction picker is open.
    pub picker: Option<String>,
    /// Commands for the node, when there is no sink (tests); with one they go straight to it.
    pub outgoing: Vec<Command>,
    sink: Option<Sink>,
    /// The create or join dialog.
    pub form: Form,
    /// Rooms being created or joined, and those that failed.
    pub work: Vec<WorkView>,
    /// Finished creates and joins already handled, so each opens its room once.
    opened: HashSet<u64>,
    /// A room just created or joined, to open as soon as it is in the list.
    open_when_shown: Option<String>,
    /// The invitation asked for, by the room it was asked for in.
    pub invite: Option<(String, InviteState)>,
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
            issues: Default::default(),
            mode: Mode::Write,
            picker: None,
            outgoing: Vec::new(),
            sink: None,
            form: Form::default(),
            work: Vec::new(),
            opened: HashSet::new(),
            open_when_shown: None,
            invite: None,
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
        // A room just created or joined opens once it is here.
        if let Some(key) = self.open_when_shown.clone()
            && self.rooms.iter().any(|r| r.key == key)
        {
            self.open_when_shown = None;
            self.select(&key);
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
        // A reply, edit or deletion belongs to the room it was started in.
        self.cancel();
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
    ///
    /// In a reply or an edit it sends that instead; while a deletion is being asked about it
    /// sends nothing.
    pub fn send(&mut self) -> bool {
        let text = self.composer.text.trim().to_owned();
        if text.is_empty() || matches!(self.mode, Mode::Delete(_)) {
            return false;
        }
        let Some(room) = self.actionable() else {
            return false;
        };
        let key = room.key.clone();
        let now = self.now;
        match (self.source, std::mem::take(&mut self.mode)) {
            (_, Mode::Delete(_)) => unreachable!("checked above"),
            (source, Mode::Edit { key: target, draft }) => {
                match source {
                    Source::Example => {
                        if let Some(room) = self.room_mut() {
                            room.edit(&target, &text);
                        }
                    }
                    Source::Node(_) => self.emit(key, Action::Edit { target, text }),
                }
                self.composer = *draft;
                self.composer.focused = true;
                return true;
            }
            (Source::Example, Mode::Reply(target)) => {
                if let Some(room) = self.room_mut() {
                    room.reply(&text, now, &target);
                }
            }
            (Source::Example, Mode::Write) => {
                if let Some(room) = self.room_mut() {
                    room.send(&text, now);
                }
            }
            (Source::Node(_), Mode::Reply(target)) => self.emit(
                key,
                Action::Send {
                    text,
                    reply_to: Some(target),
                },
            ),
            (Source::Node(_), Mode::Write) => self.emit(
                key,
                Action::Send {
                    text,
                    reply_to: None,
                },
            ),
        }
        self.composer.clear();
        true
    }

    /// Send commands for the node to `sink` from now on.
    pub fn set_sink(&mut self, sink: Sink) {
        self.sink = Some(sink);
    }

    fn emit(&mut self, room: String, action: Action) {
        let command = Command { room, action };
        match &self.sink {
            Some(sink) => sink(command),
            None => self.outgoing.push(command),
        }
    }

    /// The open room, if the user can act in it now.
    fn actionable(&self) -> Option<&Room> {
        self.room().filter(|r| r.send_block.is_none())
    }

    /// Your message keyed `target` in the open room, if it can be changed now (not one still
    /// on its way).
    fn own_sent(&self, target: &str) -> Option<&model::Message> {
        let room = self.actionable()?;
        room.message_by_key(target)
            .filter(|m| m.author == room.me && m.pending.is_none())
    }

    /// Reply to the message keyed `target` with the next message.
    pub fn start_reply(&mut self, target: &str) {
        if self.actionable().is_none() {
            return;
        }
        self.cancel();
        self.mode = Mode::Reply(target.to_owned());
        self.composer.focused = true;
    }

    /// Edit your message keyed `target` in the composer.
    pub fn start_edit(&mut self, target: &str) {
        // As written, so the edit keeps the message's mentions.
        let Some(text) = self
            .own_sent(target)
            .map(|m| m.source.clone())
            .filter(|t| !t.is_empty())
        else {
            return;
        };
        self.cancel();
        let mut editing = TextEdit::new(&text);
        editing.focused = true;
        let draft = std::mem::replace(&mut self.composer, editing);
        self.mode = Mode::Edit {
            key: target.to_owned(),
            draft: Box::new(draft),
        };
    }

    /// Ask whether to delete your message keyed `target`.
    pub fn ask_delete(&mut self, target: &str) {
        if self.own_sent(target).is_none() {
            return;
        }
        self.cancel();
        self.mode = Mode::Delete(target.to_owned());
    }

    /// Delete the message the user was asked about.
    pub fn confirm_delete(&mut self) {
        let Mode::Delete(target) = std::mem::take(&mut self.mode) else {
            return;
        };
        let Some(room) = self.actionable() else {
            return;
        };
        let key = room.key.clone();
        match self.source {
            Source::Example => {
                if let Some(room) = self.room_mut() {
                    room.delete(&target);
                }
            }
            Source::Node(_) => self.emit(key, Action::Delete { target }),
        }
    }

    /// Stop replying, editing or deleting; an edit gives the composer its draft back.
    pub fn cancel(&mut self) {
        if let Mode::Edit { draft, .. } = std::mem::take(&mut self.mode) {
            self.composer = *draft;
        }
        self.picker = None;
    }

    /// Open or close the reaction picker on the message keyed `target`.
    pub fn toggle_picker(&mut self, target: &str) {
        self.picker = match &self.picker {
            Some(k) if k == target => None,
            _ if self.actionable().is_some() => Some(target.to_owned()),
            _ => None,
        };
    }

    /// React to the message keyed `target` with `emoji`; the same emoji as yours removes it.
    pub fn react(&mut self, target: &str, emoji: &str) {
        self.picker = None;
        let Some(room) = self.actionable() else {
            return;
        };
        let key = room.key.clone();
        match self.source {
            Source::Example => {
                if let Some(room) = self.room_mut() {
                    room.react(target, emoji);
                }
            }
            Source::Node(_) => self.emit(
                key,
                Action::React {
                    target: target.to_owned(),
                    emoji: emoji.to_owned(),
                },
            ),
        }
    }

    /// Send a failed message or action in the open room again.
    pub fn retry(&mut self, entry: u64) {
        if let Some(key) = self.room().map(|r| r.key.clone()) {
            self.emit(key, Action::Retry { entry });
        }
    }

    /// Drop a failed message or action in the open room.
    pub fn discard(&mut self, entry: u64) {
        if let Some(key) = self.room().map(|r| r.key.clone()) {
            self.emit(key, Action::Discard { entry });
        }
    }

    /// Whether rooms can be created, joined and invited to: only rooms from the node.
    pub fn can_change_rooms(&self) -> bool {
        matches!(self.source, Source::Node(_))
    }

    /// Open the create or join dialog, empty.
    pub fn open_dialog(&mut self, kind: DialogKind) {
        if !self.can_change_rooms() {
            return;
        }
        self.form = Form {
            open: Some(kind),
            ..Form::default()
        };
        match kind {
            DialogKind::Create => self.form.name.focused = true,
            DialogKind::Join => self.form.code.focused = true,
        }
    }

    pub fn close_dialog(&mut self) {
        self.form = Form::default();
    }

    /// Create the room the dialog describes. What can't be used is said in the dialog, which
    /// stays open; otherwise it closes and the room's progress shows in the room list.
    pub fn submit_create(&mut self) {
        let name = self.form.name.text.trim().to_owned();
        let nickname = self.form.nickname.text.trim().to_owned();
        if let Some(why) = crate::rooms::create::create_refusal(&name, &nickname) {
            self.form.error = Some(why);
            return;
        }
        let private = self.form.private;
        self.close_dialog();
        self.emit(
            String::new(),
            Action::CreateRoom {
                name,
                nickname,
                private,
            },
        );
    }

    /// Join the room the pasted invite code or link invites to.
    pub fn submit_join(&mut self) {
        let code = river_core::invitation::invitation_text_from_paste(&self.form.code.text);
        let nickname = self.form.nickname.text.trim().to_owned();
        let refusal = if code.is_empty() {
            Some("Paste an invite code.".to_owned())
        } else if let Err(e) = river_core::invitation::Invitation::from_encoded_string(&code) {
            Some(format!("That doesn't look like a valid invite code: {e}"))
        } else {
            crate::rooms::create::join_refusal(&nickname)
        };
        if let Some(why) = refusal {
            self.form.error = Some(why);
            return;
        }
        self.close_dialog();
        self.emit(
            String::new(),
            Action::Join {
                code: crate::rooms::InviteCode(code),
                nickname,
            },
        );
    }

    /// Take where creates and joins stand. One that finished opens its room once the room is
    /// in the list.
    pub fn show_work(&mut self, work: Vec<WorkView>) {
        for w in &work {
            if let WorkState::Done { room } = &w.state
                && self.opened.insert(w.id)
            {
                if self.rooms.iter().any(|r| &r.key == room) {
                    self.select(room);
                } else {
                    self.open_when_shown = Some(room.clone());
                }
            }
        }
        self.work = work;
    }

    pub fn retry_work(&mut self, id: u64) {
        self.emit(String::new(), Action::RetryWork { id });
    }

    pub fn dismiss_work(&mut self, id: u64) {
        self.work.retain(|w| w.id != id);
        self.emit(String::new(), Action::DismissWork { id });
    }

    /// Make an invitation to the open room; its link is copied when it comes back.
    pub fn invite(&mut self) {
        if !self.can_change_rooms() {
            return;
        }
        let Some(key) = self.room().map(|r| r.key.clone()) else {
            return;
        };
        self.invite = Some((key.clone(), InviteState::Making));
        self.emit(key, Action::Invite);
    }

    /// The invitation for the room keyed `room` came back. True when its link is to be copied.
    pub fn invitation_made(&mut self, room: String, link: Result<String, String>) -> bool {
        let copied = link.is_ok();
        self.invite = Some((
            room,
            match link {
                Ok(l) => InviteState::Copied(l),
                Err(why) => InviteState::Failed(why),
            },
        ));
        copied
    }

    /// The invitation for the open room, if one was asked for there.
    pub fn open_invite(&self) -> Option<&InviteState> {
        let (room, state) = self.invite.as_ref()?;
        (self.room().map(|r| &r.key) == Some(room)).then_some(state)
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

    fn done(id: u64, room: &str) -> Vec<WorkView> {
        vec![WorkView {
            id,
            what: String::new(),
            state: WorkState::Done { room: room.into() },
        }]
    }

    #[test]
    fn a_room_just_created_or_joined_opens_once_it_is_in_the_list_and_only_once() {
        let target = example::PRIVATE_ROOM;
        let without: Vec<Room> = example::data()
            .rooms
            .into_iter()
            .filter(|r| r.key != target)
            .collect();
        let mut chat = Chat::new(Source::Node(LinkStatus::Live), Vec::new(), 0, 0, 0);
        chat.show_node_rooms(without.clone(), None, LinkStatus::Live, 0);
        let first = chat.room().unwrap().key.clone();
        // Finished before the list has it: opens when it arrives.
        chat.show_work(done(1, target));
        assert_eq!(chat.room().unwrap().key, first);
        chat.show_node_rooms(example::data().rooms, None, LinkStatus::Live, 0);
        assert_eq!(chat.room().unwrap().key, target);
        // The same report again does not take the user back to it.
        chat.select(&first);
        chat.show_work(done(1, target));
        chat.show_node_rooms(example::data().rooms, None, LinkStatus::Live, 0);
        assert_eq!(chat.room().unwrap().key, first);
        // Finished with the room already listed: opens at once.
        chat.show_work(done(2, target));
        assert_eq!(chat.room().unwrap().key, target);
    }

    /// An invite code is a credential: a command carrying one never prints it.
    #[test]
    fn a_join_command_does_not_print_its_invite_code() {
        let command = Command {
            room: String::new(),
            action: Action::Join {
                code: crate::rooms::InviteCode("SECRETCODE".into()),
                nickname: "Bea".into(),
            },
        };
        let shown = format!("{command:?}");
        assert!(!shown.contains("SECRETCODE"), "{shown}");
    }

    #[test]
    fn example_rooms_cannot_be_created_joined_or_invited_to() {
        let mut chat = Chat::example();
        chat.open_dialog(DialogKind::Create);
        assert!(chat.form.open.is_none());
        chat.invite();
        assert!(chat.outgoing.is_empty() && chat.invite.is_none());
    }

    #[test]
    fn an_invitation_shows_only_in_the_room_it_was_made_for() {
        let mut chat = Chat::new(
            Source::Node(LinkStatus::Live),
            example::data().rooms,
            0,
            0,
            0,
        );
        let open = chat.room().unwrap().key.clone();
        chat.invite();
        assert_eq!(chat.open_invite(), Some(&InviteState::Making));
        assert!(chat.invitation_made(open.clone(), Ok("link".into())));
        assert_eq!(
            chat.open_invite(),
            Some(&InviteState::Copied("link".into()))
        );
        chat.select(example::PRIVATE_ROOM);
        assert_eq!(chat.open_invite(), None);
        assert!(!chat.invitation_made(open, Err("no".into())));
    }
}
