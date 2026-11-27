//! The "Unread and mentions" overview: every room with unread messages in one list, each row
//! with the room, how many messages are unread, how many of them mention you or reply to one of
//! yours, and the newest one. A click opens that room at the first mention (or the first
//! unread message). "Mark all read" clears every room.
//!
//! What counts follows [`Chat::unread_of`], so the overview, the room list's badges, the quick
//! switcher and the window title always agree: a muted room counts for nothing and is left
//! out, a room in "Mentions & replies only" counts just those messages.
//!
//! The selection is a pure function ([`rows`]) over plain rooms, tested without a window.
//! [`Chat`] holds whether the overview is open and acts on a choice; [`overview_dialog`] draws
//! it.

use rune::{El, Tx, col, el, row};
use rune_kit::{Button, ButtonVariant, Kit, ScrollBar};

use super::dialog_kit::DialogSpec;
use super::model::{self, Message, Room};
use super::style::{self as st, Palette};
use super::view::label;
use super::{Chat, icons};
use crate::RiverApp;
use crate::rooms::modes::NotificationMode;

/// How many characters of the newest message a row shows before cutting it.
const PREVIEW_CHARS: usize = 110;

/// One room in the overview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverviewRow {
    pub key: String,
    pub name: String,
    pub private: bool,
    /// How many messages count as unread in this room, by its notification mode.
    pub unread: usize,
    /// How many of those mention you or reply to one of yours.
    pub mentions: usize,
    /// "Name: text" of the newest message that counts (the newest mention if there is one).
    pub preview: String,
    /// The key of the message a click scrolls to: the first mention, else the first unread.
    pub reveal: Option<String>,
}

/// The unread messages of `room` the node reports, taken as the newest ones from others
/// (the node says how many are unread, not which), oldest first.
fn newest_unread(room: &Room) -> Vec<&Message> {
    let mut v: Vec<&Message> = room
        .messages
        .iter()
        .rev()
        .filter(|m| m.author != room.me && m.pending.is_none())
        .take(room.unread)
        .collect();
    v.reverse();
    v
}

/// The overview row for `room` in `mode`, or `None` when nothing in it counts (read, or muted).
/// The count is the one [`Chat::unread_of`] gives.
pub fn row_of(room: &Room, mode: NotificationMode) -> Option<OverviewRow> {
    if mode == NotificationMode::Muted || room.unread == 0 {
        return None;
    }
    let unread_msgs = newest_unread(room);
    let mention_msgs: Vec<&Message> = unread_msgs
        .iter()
        .copied()
        .filter(|m| room.is_for_me(m))
        .collect();
    let (count, shown): (usize, &[&Message]) = match mode {
        NotificationMode::All => (room.unread, &unread_msgs),
        _ => (mention_msgs.len(), &mention_msgs),
    };
    if count == 0 {
        return None;
    }
    let newest = mention_msgs.last().or(shown.last()).copied();
    let first = mention_msgs.first().or(shown.first()).copied();
    Some(OverviewRow {
        key: room.key.clone(),
        name: room.name.clone(),
        private: room.private,
        unread: count,
        mentions: mention_msgs.len(),
        preview: newest
            .map(|m| {
                model::preview(
                    &format!("{}: {}", room.nickname(m.author), m.text),
                    PREVIEW_CHARS,
                )
            })
            .unwrap_or_default(),
        reveal: first.map(|m| m.key.clone()),
    })
}

/// The rows for `rooms`, each with its notification mode: rooms with mentions first, then more
/// unread first, then in the room list's order.
pub fn rows<'a>(rooms: impl IntoIterator<Item = (&'a Room, NotificationMode)>) -> Vec<OverviewRow> {
    let mut v: Vec<OverviewRow> = rooms
        .into_iter()
        .filter_map(|(r, mode)| row_of(r, mode))
        .collect();
    // Stable: equal rooms keep the room list's order.
    v.sort_by_key(|r| (r.mentions == 0, std::cmp::Reverse(r.unread)));
    v
}

/// The unread totals across all rooms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnreadSummary {
    /// Messages that count as unread, mentions included.
    pub total: usize,
    /// How many of them mention you or reply to one of yours.
    pub mentions: usize,
}

impl UnreadSummary {
    pub fn of(rows: &[OverviewRow]) -> Self {
        UnreadSummary {
            total: rows.iter().map(|r| r.unread).sum(),
            mentions: rows.iter().map(|r| r.mentions).sum(),
        }
    }
}

/// A message to scroll into view once its room is on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reveal {
    pub room: String,
    pub message: String,
    /// Frames to wait first: the room's history has to be laid out before it can be scrolled.
    wait: u8,
    /// Centre the message in the history (a jump from a quoted reply, as the web UI does)
    /// instead of only bringing it into view.
    centre: bool,
}

impl Reveal {
    /// A scroll to `message` in `room` that needs no wait: the room is already laid out.
    pub fn at_once(room: &str, message: &str) -> Self {
        Reveal {
            room: room.to_owned(),
            message: message.to_owned(),
            wait: 0,
            centre: false,
        }
    }

    /// [`Reveal::at_once`], centring the message in the history without a glide.
    pub fn centred(room: &str, message: &str) -> Self {
        Reveal {
            centre: true,
            ..Self::at_once(room, message)
        }
    }
}

impl Chat {
    /// The overview's rows now.
    pub fn overview_rows(&self) -> Vec<OverviewRow> {
        rows(self.rooms().iter().map(|r| (r, self.mode_of(&r.key))))
    }

    /// The unread totals across all rooms.
    pub fn unread_summary(&self) -> UnreadSummary {
        UnreadSummary::of(&self.overview_rows())
    }

    /// Open the overview, or close it when it is open. False (and nothing happens) while
    /// another dialog is up or there are no rooms.
    pub fn toggle_overview(&mut self) -> bool {
        if self.overview {
            self.overview = false;
            return true;
        }
        if self.other_dialog_open() || self.switcher.is_some() || self.rooms().is_empty() {
            return false;
        }
        self.emoji_open = false;
        self.picker = None;
        self.members_overlay = false;
        self.drop_search();
        self.composer.focused = false;
        self.overview = true;
        true
    }

    pub fn close_overview(&mut self) {
        self.overview = false;
    }

    /// A click on a row: close the overview, open that room, and scroll to `reveal` in it.
    pub fn overview_open(&mut self, room: &str, reveal: Option<&str>) {
        self.overview = false;
        // Computed before opening: opening a room marks it read.
        let already_open = self.room().is_some_and(|r| r.key == room);
        if !self.open_room(room) {
            return;
        }
        self.reveal = reveal.map(|m| Reveal {
            centre: false,
            room: room.to_owned(),
            message: m.to_owned(),
            wait: if already_open { 0 } else { 1 },
        });
    }

    /// Mark read the rooms the overview lists, as opening each of them would, and no others: a
    /// muted room, or unread messages a mentions-only room does not count, stay as they are.
    ///
    /// Each room marked is also reported to the node as read up to its newest message, as
    /// opening it would, so the web UI and other devices agree and a refresh does not bring the
    /// count back.
    pub fn mark_all_read(&mut self) {
        let keys: Vec<String> = self.overview_rows().into_iter().map(|r| r.key).collect();
        for key in keys {
            self.mark_read(&key);
            self.report_room_read(&key);
        }
    }

    /// Wait at least one frame before the pending reveal scrolls.
    pub(super) fn delay_reveal(&mut self) {
        if let Some(r) = &mut self.reveal {
            r.wait = r.wait.max(1);
        }
    }

    /// The message waiting to be scrolled to, if any.
    pub fn pending_reveal(&self) -> Option<&Reveal> {
        self.reveal.as_ref()
    }

    /// Scroll a pending [`Reveal`] into view once its room's history is laid out. Called every
    /// frame; true while it is still waiting, so the caller asks for another frame.
    pub fn apply_reveal(&mut self, tx: &mut Tx) -> bool {
        let restoring = self.apply_restore(tx);
        self.apply_staged_reveal(tx) || restoring
    }

    fn apply_staged_reveal(&mut self, tx: &mut Tx) -> bool {
        let Some(r) = &mut self.reveal else {
            return false;
        };
        if self.selected != r.room {
            self.reveal = None;
            return false;
        }
        if r.wait > 0 {
            r.wait -= 1;
            return true;
        }
        let (scope, row) = (
            format!("history-scroll-{}", r.room),
            format!("msg-{}", r.message),
        );
        if r.centre {
            tx.scroll_center_in_instant(scope, row);
        } else {
            tx.scroll_into_view_in(scope, row);
        }
        self.reveal = None;
        false
    }
}

impl RiverApp {
    /// Mark the listed rooms and the counted direct messages read, and reset the count of what the window missed while away in
    /// the open room. `missed.0` (which room that count is for) is kept on purpose: the next
    /// arrival in the same room keeps counting from zero, and one in another room replaces it.
    pub(crate) fn mark_everything_read(&mut self) {
        self.chat.mark_all_read();
        self.chat.mark_all_dms_seen();
        // The open room has nothing unread of its own (it is being read), but what the window
        // missed while away is on the badge: it is read now, here and on the node.
        if self.missed.1 > 0 {
            let room = self.missed.0.clone();
            self.chat.report_room_read(&room);
        }
        self.missed.1 = 0;
    }
}

/// The count badge the room list and the overview use.
fn count_badge(n: usize, key: &str, p: &Palette) -> El {
    el().key(key.to_owned())
        .h(18.0)
        .min_w(20.0)
        .px(6.0)
        .radius(9.0)
        .center()
        .fill(p.accent_soft)
        .child(label(n.to_string(), st::BADGE, p.accent))
}

/// The room list's entry that opens the overview, with the total unread beside it.
pub(super) fn entry(chat: &Chat, p: &Palette) -> El {
    let total = chat.unread_total();
    let mut r = row()
        .key("overview-open")
        .role("button")
        .label("Unread and mentions")
        .w_full()
        .h(40.0)
        .px(10.0)
        .gap(8.0)
        .center_y()
        .radius(8.0)
        .cursor(rune::Cursor::Pointer)
        .on_press(|s: &mut RiverApp| {
            s.chat.toggle_overview();
        })
        .hover(p.row_hover)
        .child(rune::icon(&icons::BELL, 16.0).color(p.text_secondary))
        .child(
            label("Unread and mentions", st::ROOM, p.text_secondary)
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        );
    if total > 0 {
        r = r.child(count_badge(total, "overview-total", p));
    }
    r
}

fn overview_row(r: &OverviewRow, p: &Palette) -> El {
    let key = r.key.clone();
    let reveal = r.reveal.clone();
    let mut name = row().gap(6.0).center_y().min_w(0.0).child(
        label(r.name.clone(), st::ROOM_STRONG, p.text)
            .truncate()
            .shrink(),
    );
    if r.private {
        name = name.child(
            rune::icon(&icons::LOCK, 13.0)
                .color(p.text_muted)
                .label("Private"),
        );
    }
    let mut line = row()
        .key(format!("overview-row-{}", r.key))
        .role("button")
        .label(format!("{}, {} unread", r.name, r.unread))
        .w_full()
        .h(54.0)
        .px(12.0)
        .gap(8.0)
        .center_y()
        .radius(8.0)
        .cursor(rune::Cursor::Pointer)
        .hover(p.row_hover)
        .on_press(move |s: &mut RiverApp| s.chat.overview_open(&key, reveal.as_deref()))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(3.0)
                .child(name)
                .child(label(r.preview.clone(), st::SMALL, p.text_muted).truncate()),
        );
    if r.mentions > 0 {
        line = line.child(
            el().key("overview-mentions")
                .h(18.0)
                .px(6.0)
                .radius(9.0)
                .center()
                .fill(p.accent)
                .child(label(
                    if r.mentions == 1 {
                        "@ mention".to_owned()
                    } else {
                        format!("@ {} mentions", r.mentions)
                    },
                    st::BADGE,
                    p.bg,
                )),
        );
    }
    line.child(count_badge(r.unread, "overview-count", p))
}

/// The overview as a dialog, when open.
pub(super) fn overview_dialog(chat: &Chat, k: &Kit, layer: &mut Vec<El>) {
    if !chat.overview {
        return;
    }
    let p = st::palette();
    let rows = chat.overview_rows();
    let sum = UnreadSummary::of(&rows);
    let summary = if rows.is_empty() {
        "You are all caught up.".to_owned()
    } else {
        let rooms = if rows.len() == 1 {
            "1 room".to_owned()
        } else {
            format!("{} rooms", rows.len())
        };
        let mentions = match sum.mentions {
            0 => String::new(),
            1 => ", 1 mention".to_owned(),
            n => format!(", {n} mentions"),
        };
        format!("{} unread in {rooms}{mentions}", sum.total)
    };
    let header = row()
        .key("overview-header")
        .w_full()
        .gap(12.0)
        .center_y()
        .child(
            label(summary, st::BODY, p.text_secondary)
                .key("overview-summary")
                .grow(1.0)
                .min_w(0.0),
        )
        .child(
            Button::new("Mark all read")
                .variant(ButtonVariant::Ghost)
                .disabled(rows.is_empty())
                .on_press(|s: &mut RiverApp| s.mark_everything_read())
                .build(k)
                .key("overview-mark-all"),
        );
    let mut list = col().key("overview-rows").w_full().gap(2.0);
    for r in &rows {
        list = list.child(overview_row(r, &p));
    }
    DialogSpec::new("overview", "Unread and mentions", |s: &mut RiverApp| {
        s.chat.close_overview()
    })
    .body_key("overview-body")
    .width(560.0)
    .child(header)
    .child(
        list.scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .scroll_bar(k),
    )
    .child(
        label(
            "Muted rooms are left out. Rooms set to mentions and replies only count just those.",
            st::SMALL,
            p.text_muted,
        )
        .wrap()
        .w_full()
        .key("overview-note"),
    )
    .build(k, layer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::example;
    use crate::chat::model::mention_scheme;

    fn room(key: &str, unread: usize) -> Room {
        let mut r = example::data()
            .rooms
            .into_iter()
            .find(|r| r.key == "town-square")
            .expect("example room");
        r.key = key.into();
        r.name = key.into();
        r.unread = unread;
        r
    }

    #[test]
    fn a_read_room_has_no_row() {
        assert!(row_of(&room("a", 0), NotificationMode::All).is_none());
    }

    #[test]
    fn a_muted_room_has_no_row_whatever_it_holds() {
        assert!(row_of(&room("a", 3), NotificationMode::Muted).is_none());
    }

    #[test]
    fn all_mode_counts_every_unread_and_shows_the_newest() {
        let r = room("a", 2);
        let row = row_of(&r, NotificationMode::All).expect("row");
        assert_eq!(row.unread, 2);
        let others: Vec<_> = r.messages.iter().filter(|m| m.author != r.me).collect();
        let newest = others.last().unwrap();
        assert!(row.preview.starts_with(r.nickname(newest.author)));
        assert_eq!(
            row.reveal.as_deref(),
            Some(others[others.len() - 2].key.as_str())
        );
    }

    #[test]
    fn a_mention_is_counted_flagged_and_what_a_click_scrolls_to() {
        let mut r = room("a", 3);
        let others: Vec<usize> = r
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.author != r.me)
            .map(|(i, _)| i)
            .collect();
        let at = others[others.len() - 2];
        r.messages[at].md = format!("hi [@me]({}:self)", mention_scheme());
        let row = row_of(&r, NotificationMode::All).expect("row");
        assert_eq!(row.unread, 3);
        assert_eq!(row.mentions, 1);
        assert_eq!(row.reveal.as_deref(), Some(r.messages[at].key.as_str()));
        assert!(row.preview.contains(&r.messages[at].text));
    }

    #[test]
    fn mentions_only_mode_counts_just_the_mentions() {
        let mut r = room("a", 3);
        assert!(row_of(&r, NotificationMode::MentionsAndReplies).is_none());
        let last = r
            .messages
            .iter()
            .rposition(|m| m.author != r.me)
            .expect("a message from another");
        r.messages[last].md = format!("hi [@me]({}:self)", mention_scheme());
        let row = row_of(&r, NotificationMode::MentionsAndReplies).expect("row");
        assert_eq!((row.unread, row.mentions), (1, 1));
    }

    #[test]
    fn rooms_with_mentions_come_first_then_more_unread() {
        let plain_big = room("big", 3);
        let plain_small = room("small", 1);
        let mut mention = room("mention", 1);
        let last = mention
            .messages
            .iter()
            .rposition(|m| m.author != mention.me)
            .unwrap();
        mention.messages[last].md = format!("x [@me]({}:self)", mention_scheme());
        let order: Vec<String> = rows([
            (&plain_small, NotificationMode::All),
            (&plain_big, NotificationMode::All),
            (&mention, NotificationMode::All),
        ])
        .into_iter()
        .map(|r| r.key)
        .collect();
        assert_eq!(order, ["mention", "big", "small"]);
    }

    #[test]
    fn equal_rooms_keep_the_room_lists_order() {
        let a = room("a", 1);
        let b = room("b", 1);
        let order: Vec<String> = rows([(&a, NotificationMode::All), (&b, NotificationMode::All)])
            .into_iter()
            .map(|r| r.key)
            .collect();
        assert_eq!(order, ["a", "b"]);
    }

    #[test]
    fn the_summary_agrees_with_the_room_lists_counts_in_every_mode() {
        for mode in NotificationMode::ALL {
            let mut chat = Chat::example();
            let keys: Vec<String> = chat.rooms().iter().map(|r| r.key.clone()).collect();
            chat.edit_rooms(|rooms| {
                for r in rooms {
                    r.unread = 2;
                    // The newest message from someone else mentions you, so the two modes
                    // count differently (2 in All, 1 in mentions only).
                    if let Some(m) = r.messages.iter_mut().rev().find(|m| m.author != r.me) {
                        m.md = format!("hi [@me]({}:self)", mention_scheme());
                    }
                }
            });
            for k in &keys {
                chat.set_mode(k, mode);
            }
            // The open room was marked read when it was selected; the rest keep their counts.
            let by_rows: Vec<(String, usize)> = chat
                .overview_rows()
                .into_iter()
                .map(|r| (r.key, r.unread))
                .collect();
            for r in chat.rooms() {
                let want = chat.unread_of(r);
                let got = by_rows
                    .iter()
                    .find(|(k, _)| k == &r.key)
                    .map_or(0, |(_, n)| *n);
                assert_eq!(got, want, "{} in {mode:?}", r.key);
            }
            assert_eq!(chat.unread_summary().total, chat.unread_total());
            // Not all zero, or the comparison above would prove nothing.
            if mode != NotificationMode::Muted {
                assert!(chat.unread_total() > 0, "{mode:?}");
            }
        }
    }
}
