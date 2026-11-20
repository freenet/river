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

mod background;
mod conversation;
mod details;
pub mod dnd;
mod emoji;
pub mod example;
mod icons;
mod identity;
/// What the identity dialogs say and hold, for tests that pin them to the web UI's.
pub mod identity_texts {
    pub use super::identity::{
        EXPORT_INTRO, EXPORT_WARNING, IMPORT_INTRO, REPLACE_SESSIONS_WARNING, REPLACE_WARNING,
        Secret,
    };
}
mod keys;
mod links;
pub(crate) mod markdown;
mod member_modal;
mod mention;
pub mod model;
pub(crate) mod node_ui;
mod notify_dialog;
pub mod overview;
mod quick_reply;
pub mod search;
/// The web UI's share-link validator, compiled here unchanged so the two cannot disagree on
/// what a share link is (its tests, and the shared vector file, run here too).
#[path = "../../../../ui/src/util/share_link.rs"]
#[allow(dead_code)]
mod share_link;
pub(crate) mod style;
pub mod switcher;
#[cfg(test)]
mod tests;
mod update_view;
mod view;
pub(crate) mod visual_fold;

pub use background::{BackgroundHost, BackgroundUi};
pub use dnd::{DndUi, Schedule as DndSchedule, Settings as DndSettings};
pub use quick_reply::{MAX_REPLY_CHARS, ReplyRefusal};
use std::collections::{HashMap, HashSet};

use rune::TextEdit;

use crate::rooms::modes::NotificationMode;
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
    /// Import ID ([`identity`]).
    Import,
}

/// What the create and join dialogs hold.
pub struct Form {
    pub open: Option<DialogKind>,
    pub name: TextEdit,
    pub nickname: TextEdit,
    pub code: TextEdit,
    pub private: bool,
    /// What is pasted in the import dialog: an identity token, which is a private key.
    pub token: TextEdit,
    /// The import waiting for the user to confirm replacing an identity.
    pub replace: Option<identity::ReplaceConfirm>,
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
            token: TextEdit::new(""),
            replace: None,
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
    /// The node River uses, and the form that picks another.
    pub node: node_ui::NodeUi,
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
    /// The link a right-click landed on, and where: the link menu (Open, Copy link) is open
    /// there. Cleared when the menu closes.
    pub link_menu: Option<(String, rune::PointerPress)>,
    /// The composer's emoji picker is open, and on which category.
    pub emoji_open: bool,
    pub emoji_category: usize,
    /// The highlighted row of the @mention popup, and the query it was moved under (a new query
    /// starts again at the top).
    mention_sel: (String, usize),
    /// The `@` (and what was typed after it) an Escape closed the popup at, closed until that word is gone.
    mention_off: Option<(usize, String)>,
    /// Whether the open room's history shows its newest message (the Latest button hides
    /// then). Reported by the history's scroll area.
    pub at_bottom: bool,
    /// The member (by `Member::uid`) whose info dialog is open, in the open room.
    pub member_modal: Option<String>,
    /// In the open member dialog: the deputy change being confirmed, by member (`Member::uid`)
    /// and whether it makes them a deputy (`true`) or ends that.
    pub deputy_confirm: Option<(String, bool)>,
    /// In the open member dialog: the member (`Member::uid`) whose ban is being confirmed.
    pub ban_confirm: Option<String>,
    /// What the open ban confirmation last showed about the room's other bans, and whether it
    /// has changed since the person last saw it (the next Confirm then only shows the new text).
    pub ban_seen: Option<model::BanPreview>,
    pub ban_changed: Option<std::time::Instant>,
    /// The room the open ban confirmation asked its question of.
    ban_room: Option<String>,
    /// What is typed in the nickname field of your own member info dialog.
    pub nickname: TextEdit,
    /// Why the nickname typed was refused when Save was pressed with nothing in it.
    pub nickname_error: Option<String>,
    /// The room details dialog, when open (in the open room).
    pub details: Option<details::DetailsForm>,
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
    /// The "Export Identity" dialog, when open ([`identity`]).
    pub export: Option<identity::ExportDialog>,
    /// "Background and startup", when the shell offers it ([`background`]).
    pub background: Option<background::BackgroundUi>,
    /// Do not disturb: this device's switch and schedule ([`dnd`]).
    pub dnd: dnd::DndUi,
    /// Whether the last status was not live, so the next live update is catch-up rather than
    /// news (see [`Chat::show_node_rooms`]).
    catching_up: bool,
    /// Whether the last rooms shown were live and not a catch-up: the whole list, not a partial
    /// one while reconnecting.
    full_list: bool,
    /// Each room's notification mode by room key, shared with the web UI; a room not named is
    /// [`NotificationMode::All`].
    modes: HashMap<String, NotificationMode>,
    /// Modes picked here that the node's updates have not echoed yet, so an update sent before
    /// the pick was taken does not put the old mode back on screen.
    picked: HashMap<String, NotificationMode>,
    /// The room whose notification dialog is open.
    notify_dialog: Option<String>,
    /// River's own updates and the settings dialog.
    pub update: crate::update::UpdateUi,
    /// The quick switcher (Ctrl+K), when open ([`switcher`]).
    pub switcher: Option<switcher::Switcher>,
    /// The message search panel (Ctrl+F), when open ([`search`]).
    pub search: Option<search::Search>,
    /// A message marked for a moment after a jump from the search panel.
    flash: Option<search::Flash>,
    /// Whether the unread and mentions overview is open ([`overview`]).
    pub overview: bool,
    /// A message to scroll to once the room it is in is on screen.
    reveal: Option<overview::Reveal>,
    /// Why a reply typed into a notification was not sent, shown above the composer.
    reply_notices: std::collections::BTreeMap<String, String>,
    /// When the switcher's chord last arrived, for [`switcher::chord_gate`].
    chord_at: Option<std::time::Instant>,
}

impl Chat {
    /// The notification mode of the room keyed `room`.
    pub fn mode_of(&self, room: &str) -> NotificationMode {
        self.modes.get(room).copied().unwrap_or_default()
    }

    /// Take the modes the node says the rooms have. A pick made here stays until the node
    /// echoes it, or says it `settled` (stored, or failed); after that the node's word is
    /// shown, whatever it is.
    pub fn apply_modes(&mut self, modes: HashMap<String, NotificationMode>, settled: &[String]) {
        self.picked.retain(|room, mode| {
            !settled.contains(room) && modes.get(room).copied().unwrap_or_default() != *mode
        });
        self.modes = modes;
        for (room, mode) in &self.picked {
            self.modes.insert(room.clone(), *mode);
        }
    }

    /// Set the notification mode of the room keyed `room`, and close the dialog. For a room
    /// from the node it is sent to be stored beside the web UI's, so both apps share it.
    pub fn set_mode(&mut self, room: &str, mode: NotificationMode) {
        self.notify_dialog = None;
        if self.mode_of(room) == mode && !self.picked.contains_key(room) {
            return;
        }
        self.modes.insert(room.to_owned(), mode);
        if matches!(self.source, Source::Node(_)) {
            self.picked.insert(room.to_owned(), mode);
            self.emit(room.to_owned(), Action::SetNotificationMode { mode });
        }
    }

    /// Open the notification dialog of the open room.
    pub fn open_notify_dialog(&mut self) {
        if let Some(room) = self.room() {
            self.notify_dialog = Some(room.key.clone());
        }
    }

    pub fn close_notify_dialog(&mut self) {
        self.notify_dialog = None;
    }

    /// How many messages of `room` count as unread, by its mode, as the web UI's title does:
    /// all of them, only those that mention you or reply to yours, or none when muted.
    pub fn unread_of(&self, room: &Room) -> usize {
        match self.mode_of(&room.key) {
            NotificationMode::All => room.unread,
            NotificationMode::MentionsAndReplies => room.unread_for_me(room.unread),
            NotificationMode::Muted => 0,
        }
    }

    /// Open the link menu for `url` at `press`: only for a web link, and with the address
    /// trimmed so Copy and Open act on the same text.
    pub fn open_link_menu(&mut self, url: &str, press: rune::PointerPress) {
        if markdown::openable(url) {
            self.link_menu = Some((url.trim().to_owned(), press));
        }
    }

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
            link_menu: None,
            emoji_open: false,
            emoji_category: 0,
            mention_sel: (String::new(), 0),
            mention_off: None,
            at_bottom: true,
            member_modal: None,
            deputy_confirm: None,
            ban_confirm: None,
            ban_seen: None,
            ban_changed: None,
            ban_room: None,
            nickname: TextEdit::new(""),
            nickname_error: None,
            details: None,
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
            export: None,
            background: None,
            dnd: Default::default(),
            catching_up: true,
            full_list: false,
            modes: HashMap::new(),
            picked: HashMap::new(),
            notify_dialog: None,
            switcher: None,
            search: None,
            flash: None,
            overview: false,
            reveal: None,
            reply_notices: Default::default(),
            chord_at: None,
            node: Default::default(),
            update: Default::default(),
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

    /// Test support: mark the first message of room `room` as not yet taken by the node.
    #[cfg(test)]
    pub(crate) fn mark_first_message_pending_for_test(
        &mut self,
        room: &str,
        pending: model::Pending,
    ) {
        if let Some(r) = self.rooms.iter_mut().find(|r| r.key == room)
            && let Some(m) = r.messages.first_mut()
        {
            m.pending = Some(pending);
        }
    }

    /// What quitting now would lose, in words, or `None`: messages (and other changes) still
    /// being sent or not yet taken, rooms being created or joined, and text in the composer or
    /// a saved draft. These live in memory only.
    pub fn unsent_summary(&self) -> Option<String> {
        let mut pending = self
            .rooms
            .iter()
            .map(|r| {
                r.messages.iter().filter(|m| m.pending.is_some()).count()
                    + usize::from(r.own_nickname.is_some())
                    + usize::from(r.details.rotation.is_some())
            })
            .sum::<usize>();
        pending += self
            .work
            .iter()
            .filter(|w| !matches!(w.state, WorkState::Done { .. }))
            .count();
        // Text in the composer or a draft, and text typed into a dialog or form that is open
        // (a new room, an invite code, an identity token, a room's details, a nickname).
        let form = &self.form;
        let nickname_edit = self.member_modal.is_some()
            && !self.nickname.text.trim().is_empty()
            && self
                .room()
                .is_some_and(|r| r.nickname(r.me) != self.nickname.text.trim());
        let text = !self.composer.text.trim().is_empty()
            || self.drafts.values().any(|d| !d.text.trim().is_empty())
            || (form.open.is_some()
                && [&form.name, &form.nickname, &form.code, &form.token]
                    .iter()
                    .any(|t| !t.text.trim().is_empty()))
            || self.details.as_ref().is_some_and(|d| d.has_edits())
            || nickname_edit;
        let mut parts = Vec::new();
        match pending {
            0 => {}
            1 => parts.push("1 message or change is still being sent.".to_string()),
            n => parts.push(format!("{n} messages or changes are still being sent.")),
        }
        if text {
            parts.push("You have text you have not sent.".to_string());
        }
        (!parts.is_empty()).then(|| parts.join(" "))
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
            self.emoji_open = false;
            self.member_modal = None;
            self.clear_ban_confirm();
            self.details = None;
            // An empty list is often a reload on its way to the full one: keep the open room's
            // text as its draft, remember which room it was, and open nothing.
            let mut open = std::mem::replace(&mut self.composer, TextEdit::new(""));
            open.focused = false;
            let key = std::mem::take(&mut self.selected);
            if !key.is_empty() {
                self.park_draft(key.clone(), open);
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
        // A member who left, or a list that no longer has them, closes their dialog for good:
        // it does not come back if they reappear.
        if let Some(uid) = &self.member_modal
            && !self.room().is_some_and(|r| r.member_by_uid(uid).is_some())
        {
            self.member_modal = None;
            self.clear_ban_confirm();
        }
        if let Some(room) = &self.notify_dialog
            && !self.rooms.iter().any(|r| &r.key == room)
        {
            self.notify_dialog = None;
        }
        self.settle_details();
        self.settle_ban_confirm();
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
    ) -> Vec<crate::notify::Arrival> {
        // Only an update that follows a live one can carry news. The first rooms, the catch-up
        // that ends "Catching up", and what arrives after a reconnect are history, not messages
        // to interrupt the person with. So the first live update after any not-live one is
        // swallowed: a genuine message that lands in exactly that update is not notified (the
        // unread count and the title still show it). That is accepted: the alternative is a
        // burst of old messages after every reconnect.
        //
        // That swallowed update's arrivals are still returned, marked `quiet`: they count as
        // unread while the window is away, they just are not shown as notifications.
        let arrived = if status == LinkStatus::Live && self.node_rooms_shown {
            let quiet = self.catching_up;
            let mut arrived = crate::notify::arrivals(&self.rooms, &rooms, &|k| self.mode_of(k));
            arrived.iter_mut().for_each(|a| a.quiet = quiet);
            arrived
        } else {
            Vec::new()
        };
        self.full_list = status == LinkStatus::Live && !self.catching_up;
        self.catching_up = status != LinkStatus::Live;
        self.source = Source::Node(status);
        // The menu belongs to what was on screen; a new list may have removed it.
        self.link_menu = None;
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
        // The open room is being read, so nothing in it is unread (a window that is away
        // counts what it missed itself, see `RiverApp::unread`).
        if let Some(room) = self.room_mut() {
            room.unread = 0;
        }
        if let Some(room) = self.room()
            && let Some(mark) = ReadMark::of(room)
        {
            self.read.insert(room.key.clone(), mark);
        }
        arrived
    }

    /// The connection's status changed without new rooms. Anything but live makes the next live
    /// update catch-up.
    pub fn note_status(&mut self, status: LinkStatus) {
        if let Source::Node(_) = self.source {
            self.source = Source::Node(status);
            self.catching_up |= status != LinkStatus::Live;
        }
    }

    /// Whether the rooms last shown are the whole list (live, not catching up).
    pub fn list_is_full(&self) -> bool {
        self.full_list
    }

    /// Unread messages across all rooms.
    pub fn unread_total(&self) -> usize {
        self.rooms.iter().map(|r| self.unread_of(r)).sum()
    }

    /// Mark the room keyed `key` read, without opening it.
    pub fn mark_read(&mut self, key: &str) {
        let Some(room) = self.rooms.iter_mut().find(|r| r.key == key) else {
            return;
        };
        room.unread = 0;
        if let Some(mark) = ReadMark::of(room) {
            self.read.insert(key.to_owned(), mark);
        }
    }

    /// Bring the room keyed `key` to the front, as a click on its notification does: open it,
    /// and show the conversation rather than the members panel on a narrow window. False if
    /// there is no such room.
    pub fn open_room(&mut self, key: &str) -> bool {
        if !self.rooms.iter().any(|r| r.key == key) {
            return false;
        }
        self.select(key);
        self.members_overlay = false;
        true
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
        self.emoji_open = false;
        // The new room's history reports where it stands once laid out.
        self.at_bottom = true;
        // A member's dialog, and the details dialog, belong to the room they were opened in.
        self.member_modal = None;
        self.clear_ban_confirm();
        self.notify_dialog = None;
        self.details = None;
        // The search lists the room it was opened in.
        self.drop_search();
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
        self.park_draft(self.selected.clone(), outgoing);
        self.selected = key.to_owned();
    }
    /// Send what the composer holds to the open room. Blank text sends nothing; otherwise the
    /// composer clears (keeping focus, so the next message can be typed straight away).
    ///
    /// In a reply or an edit it sends that instead; while a deletion is being asked about it
    /// sends nothing.
    pub fn send(&mut self) -> bool {
        let text = self.composer.text.trim().to_owned();
        if text.is_empty() || matches!(self.mode, Mode::Delete(_)) || self.over_limit() {
            return false;
        }
        self.mention_off = None;
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

    /// What the composer would send, in the bytes the room measures against its size limit
    /// (the web UI's `measure_draft`: the encoded message body, so a reply, a private room's
    /// encryption and multi-byte characters all count), and that limit. `None` while deleting
    /// or with no open room, and for a room with no limit known.
    pub fn draft_size(&self) -> Option<(usize, usize)> {
        Self::size_of(self.room()?, &self.mode, self.composer.text.trim())
    }

    /// [`Chat::draft_size`] for `text` sent into `room` in `mode`.
    pub(crate) fn size_of(room: &Room, mode: &Mode, text: &str) -> Option<(usize, usize)> {
        use river_core::room_state::message::{MessageId, RoomMessageBody};
        let max = room.details.limits.max_message_size;
        if max == 0 {
            return None;
        }
        // Ids are 64-bit hashes that encode in nine bytes almost always; the longest form is
        // measured, so the count is never short.
        // Ids are 64-bit hashes that encode in nine bytes almost always; where the real id is
        // not known the longest form is measured, so the count is never short.
        let worst = || MessageId(freenet_scaffold::util::FastHash(i64::MAX));
        let wrap = |key: &str| room.wraps.as_ref()?.get(key);
        let bytes = match mode {
            Mode::Delete(_) => return None,
            Mode::Edit { key, .. } => RoomMessageBody::measure_edit(
                wrap(key).map_or_else(worst, |w| w.id.clone()),
                text,
                room.private,
            ),
            Mode::Reply(key) => match (&room.wraps, wrap(key)) {
                (_, Some(w)) => RoomMessageBody::measure_reply(
                    text,
                    w.id.clone(),
                    &w.author_name,
                    &w.preview,
                    room.private,
                ),
                // A node room that no longer shows the target: sending fails ("the message you
                // replied to is gone") and nothing is wrapped, so only the text counts.
                (Some(_), None) => RoomMessageBody::measure_text(text, room.private),
                // An example room, which sends nothing: as the view shows it.
                (None, None) => match room.message_by_key(key) {
                    Some(m) => RoomMessageBody::measure_reply(
                        text,
                        worst(),
                        room.nickname(m.author),
                        &m.text
                            .chars()
                            .take(model::REPLY_WRAP_CHARS)
                            .collect::<String>(),
                        room.private,
                    ),
                    None => RoomMessageBody::measure_text(text, room.private),
                },
            },
            Mode::Write => RoomMessageBody::measure_text(text, room.private),
        };
        Some((bytes, max))
    }

    /// Whether what the composer holds is too long for the room, so Send is refused.
    pub fn over_limit(&self) -> bool {
        self.draft_size().is_some_and(|(bytes, max)| bytes > max)
    }

    /// Start editing your newest message that can still be changed, as Up in an empty composer
    /// does in the web UI. Returns whether it did: not while replying or editing, or with nothing of yours to edit.
    pub fn edit_last(&mut self) -> bool {
        if !matches!(self.mode, Mode::Write) {
            return false;
        }
        // Your NEWEST message, whatever its state: one still on its way, one that failed, or one
        // this computer can't read is refused, never skipped for an older one.
        let Some(key) = self.actionable().and_then(|room| {
            room.messages
                .iter()
                .rev()
                .find(|m| m.author == room.me)
                .filter(|m| m.pending.is_none() && !m.source.is_empty())
                .map(|m| m.key.clone())
        }) else {
            return false;
        };
        self.start_edit(&key);
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
        self.mention_off = None;
    }

    /// Escape with focus nowhere in particular (the window's key handler, and the conversation's
    /// when a control in it has focus): closes the emoji picker, then abandons a reply or an
    /// edit. Does nothing while a dialog or the narrow members panel is open, which take their
    /// own Escape. Returns whether it acted.
    pub fn escape(&mut self) -> bool {
        let covered = self.form.open.is_some()
            || self.member_modal.is_some()
            || self.details.is_some()
            || self.notify_dialog.is_some()
            || self.switcher.is_some()
            || self.background.as_ref().is_some_and(|b| b.open)
            || self.dnd.open
            || self.overview
            || self.link_menu.is_some()
            || self.members_overlay
            || self.picker.is_some();
        if covered {
            return false;
        }
        if self.search.is_some() {
            self.close_search();
            return true;
        }
        if self.emoji_open {
            self.emoji_open = false;
            return true;
        }
        self.cancel_compose()
    }

    /// Abandon a reply or an edit in progress. Returns whether there was one.
    pub fn cancel_compose(&mut self) -> bool {
        let active = matches!(self.mode, Mode::Reply(_) | Mode::Edit { .. });
        if active {
            self.cancel();
        }
        active
    }

    /// Open or close the composer's emoji picker.
    pub fn toggle_emoji(&mut self) {
        self.emoji_open = !self.emoji_open;
        // The expanded description and the picker together would crowd a short window.
        if self.emoji_open {}
    }

    /// Put `emoji` into the composer at the caret (in place of the selection) and give the
    /// composer back the focus; the picker stays open for more, as a person picks a few.
    pub fn insert_emoji(&mut self, emoji: &str) {
        if self.room().is_some_and(|r| r.send_block.is_some()) {
            return;
        }
        self.composer.insert(emoji);
        self.composer.focused = true;
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
            DialogKind::Import => self.form.token.focused = true,
        }
    }

    /// Open the info dialog of the member with `uid` in the open room.
    pub fn open_member(&mut self, uid: &str) {
        if self.room().and_then(|r| r.member_by_uid(uid)).is_none() {
            return;
        }
        // Already open for this member: leave it as it is, so a nickname being typed stays.
        if self.member_modal.as_deref() == Some(uid) {
            return;
        }
        // What else is open (opened from a message, which a dialog does not cover) closes.
        self.emoji_open = false;
        self.link_menu = None;
        self.notify_dialog = None;
        self.picker = None;
        let Some(member) = self.room().and_then(|r| r.member_by_uid(uid)) else {
            return;
        };
        // Your own dialog edits your nickname, starting from the one you chose; with none
        // chosen (the shown name is a stand-in) the field starts empty and ready to type.
        let mut edit = TextEdit::new(if member.claimed { &member.nickname } else { "" });
        edit.focused = member.id == self.room().map_or(0, |r| r.me);
        self.nickname = edit;
        self.nickname_error = None;
        self.deputy_confirm = None;
        self.clear_ban_confirm();
        self.details = None;
        self.member_modal = Some(uid.to_owned());
    }

    pub fn close_member(&mut self) {
        self.member_modal = None;
        self.clear_ban_confirm();
        self.nickname_error = None;
        self.deputy_confirm = None;
    }

    /// Whether the open member dialog offers Ban for its member (the room's own gate).
    pub fn ban_allowed(&self) -> bool {
        let Some(uid) = self.member_modal.as_deref() else {
            return false;
        };
        self.room().is_some_and(|r| {
            r.member_by_uid(uid).is_some_and(|m| m.id != r.me)
                && r.ban_offers.get(uid) == Some(&model::BanOffer::Allowed)
        })
    }

    /// Ask to confirm banning the member in the open dialog. A room from the node is asked what
    /// the ban would do to its other bans (the room keeps a fixed number), and the confirmation
    /// waits for the answer.
    pub fn ask_ban(&mut self) {
        if !self.ban_allowed() {
            return;
        }
        self.ban_confirm = self.member_modal.clone();
        self.ban_seen = None;
        self.ban_changed = None;
        self.ban_room = self.room().map(|r| r.key.clone());
        if let (Some(uid), Some(key), Source::Node(_)) = (
            self.ban_confirm.clone(),
            self.ban_room.clone(),
            &self.source,
        ) {
            self.emit(key, Action::PreviewBan { target: uid });
        }
        self.settle_ban_confirm();
    }

    /// Close the ban confirmation, and tell the room it no longer needs to work out what the
    /// ban would do.
    pub(super) fn clear_ban_confirm(&mut self) {
        let was_open = self.ban_confirm.take().is_some();
        self.ban_seen = None;
        self.ban_changed = None;
        if let (true, Some(key), Source::Node(_)) = (was_open, self.ban_room.take(), &self.source) {
            self.emit(
                key,
                Action::PreviewBan {
                    target: String::new(),
                },
            );
        }
    }

    pub fn cancel_ban(&mut self) {
        self.clear_ban_confirm();
    }

    /// What the open confirmation knows of the ban's effect on the other bans: `None` until a
    /// room from the node has answered; example rooms have nothing to ask.
    pub fn ban_preview(&self) -> Option<&model::BanPreview> {
        let uid = self.ban_confirm.as_deref()?;
        self.room()?.ban_preview.get(uid)
    }

    /// Whether Confirm can be pressed: the answer is in (or there is nothing to ask) and the
    /// room would keep the ban.
    pub fn ban_ready(&self) -> bool {
        match (&self.source, self.ban_preview()) {
            (Source::Example, _) => true,
            (_, Some(p)) => !p.refused,
            (_, None) => false,
        }
    }

    /// After the rooms changed: if what the ban would do to the other bans is not what the
    /// person was shown, say so before anything is sent.
    pub(super) fn settle_ban_confirm(&mut self) {
        self.settle_ban_confirm_at(std::time::Instant::now());
    }

    /// [`Chat::settle_ban_confirm`] at `now`.
    pub fn settle_ban_confirm_at(&mut self, now_at: std::time::Instant) {
        if self.ban_confirm.is_none() {
            self.ban_seen = None;
            self.ban_changed = None;
            return;
        }
        let now = self.ban_preview().cloned();
        if self.ban_seen.is_some() && now != self.ban_seen {
            self.ban_changed = Some(now_at);
        }
        if now.is_some() {
            self.ban_seen = now;
        }
    }

    /// The confirmed ban goes to the node (rooms from the node), or is made in memory (example
    /// rooms), and the dialog closes, as the web UI's does. Nothing goes if the dialog no longer
    /// offers it. A ban that fails is said above the composer, with Retry and Discard.
    pub fn confirm_ban(&mut self) {
        self.confirm_ban_at(std::time::Instant::now());
    }

    /// How long changed text is on screen before a Confirm counts: a double click or a held key
    /// must not confirm what the person has not had time to read.
    pub const BAN_READ_TIME: std::time::Duration = std::time::Duration::from_millis(400);

    /// [`Chat::confirm_ban`] at `now`.
    pub fn confirm_ban_at(&mut self, now: std::time::Instant) {
        let Some(uid) = self.ban_confirm.clone() else {
            return;
        };
        if self.member_modal.as_deref() != Some(uid.as_str())
            || !self.ban_allowed()
            || !self.ban_ready()
        {
            if self.member_modal.as_deref() != Some(uid.as_str()) {
                self.clear_ban_confirm();
            }
            return;
        }
        // What the ban does to the other bans is not what the person was shown: the new text
        // is shown, and a press before it has been on screen for [`Chat::BAN_READ_TIME`] is
        // ignored instead of sending.
        if let Some(since) = self.ban_changed
            && now.saturating_duration_since(since) < Chat::BAN_READ_TIME
        {
            return;
        }
        self.clear_ban_confirm();
        let Some(key) = self.room().map(|r| r.key.clone()) else {
            return;
        };
        match self.source {
            Source::Example => {
                if let Some(r) = self.room_mut() {
                    r.members.retain(|m| m.uid != uid);
                }
            }
            Source::Node(_) => self.emit(key, Action::Ban { target: uid }),
        }
        self.close_member();
    }

    /// What the open member dialog offers about making this member your deputy, and what stops
    /// it now: the offer (as the web UI's deputy button decides it), then why it can't be used
    /// yet, if it can't.
    pub fn deputy_offer(&self) -> Option<(member_modal::DeputyOffer, Option<String>)> {
        let room = self.room()?;
        let m = room.member_by_uid(self.member_modal.as_deref()?)?;
        let offer = member_modal::deputy_offer(room, m)?;
        let sending = room
            .deputy_changes
            .iter()
            .any(|c| c.uid == m.uid && c.pending.failed.is_none());
        let why = if sending {
            Some("Sending\u{2026}".to_owned())
        } else if let Some(block) = &room.deputy_block {
            Some(block.clone())
        } else if offer == member_modal::DeputyOffer::Deputize && room.deputies_full {
            Some(crate::rooms::outbox::too_many_deputies())
        } else if offer == member_modal::DeputyOffer::Deputize {
            room.deputy_grant_block.clone()
        } else {
            None
        };
        Some((offer, why))
    }

    /// Ask to confirm making the member in the open dialog your deputy, or ending that.
    pub fn ask_deputy(&mut self) {
        if let Some(uid) = self.member_modal.clone()
            && let Some((offer, None)) = self.deputy_offer()
        {
            self.deputy_confirm = Some((uid, offer == member_modal::DeputyOffer::Deputize));
        }
    }

    /// Back out of the confirmation.
    pub fn cancel_deputy(&mut self) {
        self.deputy_confirm = None;
    }

    /// The confirmed change goes to the node (rooms from the node), or is made in memory
    /// (example rooms). Nothing goes if it is no longer what the dialog offers.
    pub fn confirm_deputy(&mut self) {
        let Some((uid, grant)) = self.deputy_confirm.take() else {
            return;
        };
        if self.member_modal.as_deref() != Some(uid.as_str()) {
            return;
        }
        let offered = match self.deputy_offer() {
            Some((offer, None)) => offer == member_modal::DeputyOffer::Deputize,
            _ => return,
        };
        if offered != grant {
            return;
        }
        let Some(key) = self.room().map(|r| r.key.clone()) else {
            return;
        };
        match self.source {
            Source::Example => {
                if let Some(m) = self
                    .room_mut()
                    .and_then(|r| r.members.iter_mut().find(|m| m.uid == uid))
                {
                    m.my_deputy = grant;
                }
            }
            Source::Node(_) => self.emit(key, Action::SetDeputy { target: uid, grant }),
        }
    }

    /// Try the deputy change of the open member that failed again, or drop it.
    pub fn retry_deputy(&mut self) {
        if let Some(entry) = self.deputy_change_entry() {
            self.retry(entry);
        }
    }

    pub fn discard_deputy(&mut self) {
        if let Some(entry) = self.deputy_change_entry() {
            self.discard(entry);
        }
    }

    fn deputy_change_entry(&self) -> Option<u64> {
        let uid = self.member_modal.as_deref()?;
        self.room()?
            .deputy_changes
            .iter()
            .find(|c| c.uid == uid)
            .map(|c| c.pending.entry)
    }

    /// Whether the open member dialog is yours.
    pub fn member_modal_is_me(&self) -> bool {
        self.room().is_some_and(|r| {
            self.member_modal
                .as_deref()
                .and_then(|uid| r.member_by_uid(uid))
                .is_some_and(|m| m.id == r.me)
        })
    }

    /// Why the nickname typed can't be saved, as the field is typed in: too long for the room,
    /// or characters that don't show. Nothing typed is not a problem yet, only on Save.
    pub fn nickname_problem(&self) -> Option<String> {
        let typed = self.nickname.text.trim();
        let limit = self.room()?.nickname_limit;
        if typed.is_empty() {
            return None;
        }
        crate::rooms::outbox::nickname_refusal_in(typed, limit)
    }

    /// Save the nickname typed as yours. What can't be used is said in the dialog, which stays
    /// open; the same name closes it; otherwise the change goes to the node (rooms from the
    /// node), and the dialog shows it on its way, or changes it in memory (example rooms).
    pub fn submit_nickname(&mut self) {
        if !self.member_modal_is_me() {
            return;
        }
        let Some(room) = self.room() else {
            return;
        };
        // The dialog says why, and Save is disabled.
        if room.send_block.is_some() || room.nickname_block.is_some() {
            return;
        }
        // One change at a time: wait for the node to take or refuse the last.
        if room
            .own_nickname
            .as_ref()
            .is_some_and(|p| p.failed.is_none())
        {
            return;
        }
        let typed = self.nickname.text.trim().to_owned();
        let refusal = crate::rooms::outbox::nickname_refusal_in(&typed, room.nickname_limit);
        let key = room.key.clone();
        let me = room.me;
        // The name is compared as it is shown (the web UI's sanitiser, which also collapses
        // runs of spaces), so "Bob  Smith" is the name "Bob Smith" already is.
        let shown = visual_fold::sanitize_display_name(&typed);
        let same = room
            .member(me)
            .is_some_and(|m| m.claimed && m.nickname == shown);
        if refusal.is_some() {
            self.nickname_error = refusal;
            return;
        }
        self.nickname_error = None;
        if same {
            self.close_member();
            return;
        }
        match self.source {
            Source::Example => {
                if let Some(m) = self
                    .room_mut()
                    .and_then(|r| r.members.iter_mut().find(|m| m.id == me))
                {
                    m.nickname = typed;
                    m.claimed = true;
                }
                self.close_member();
            }
            Source::Node(_) => self.emit(key, Action::SetNickname { nickname: typed }),
        }
    }

    /// Try the nickname change that failed again.
    pub fn retry_nickname(&mut self) {
        if let Some(entry) = self
            .room()
            .and_then(|r| r.own_nickname.as_ref())
            .map(|p| p.entry)
        {
            self.retry(entry);
        }
    }

    /// Drop the nickname change that failed.
    pub fn discard_nickname(&mut self) {
        if let Some(entry) = self
            .room()
            .and_then(|r| r.own_nickname.as_ref())
            .map(|p| p.entry)
        {
            self.discard(entry);
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
