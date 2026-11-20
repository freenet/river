//! Search in the open room: Ctrl+F (Cmd+F on macOS) opens a panel under the room's header with a box and the messages that contain what is typed, newest first. Up and
//! Down move the highlight, Enter or a click jumps to that message (through the same staged
//! scroll as the overview, and marks it for a moment), Escape closes the panel.
//!
//! What is searched: the messages this computer has loaded for the room, nothing older. The
//! text compared is the text as drawn (markup dropped, a mention as `@name`), the current text
//! of an edited message, and the author's nickname. A deleted message is no longer in the room,
//! so it is not found. Case and combining accents do not matter (the quick switcher's
//! [`fold`](super::switcher::fold_char)).
//!
//! Not searched: the quoted preview of a reply, reactions, and raw HTML (which the message
//! renderer does not draw either). The panel takes its room from the history and gives it back
//! when it closes; a dialog opening closes it, and closing it hands the keyboard back to the
//! composer if that had it.
//!
//! The matching is a pure function ([`search`]) over plain data, in characters throughout (no
//! byte offsets), so it is tested without a window and cannot cut a character in two.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use rune::tok::TextStyle;
use rune::{Color, El, Key, KeyEvent, TextEdit, col, row};
use rune_kit::{Kit, TextInput};

use super::model::{self, Message, Room};
use super::overview::Reveal;
use super::style::{self as st, Palette};
use super::switcher::{fold_char, window};
use super::view::label;
use super::{Chat, keys, markdown};
use crate::RiverApp;

/// The longest query used, in characters. A longer one (a paste) is cut, which also bounds the
/// cost of matching it.
pub const MAX_QUERY_CHARS: usize = 200;

/// The most messages the panel lists. Past it the panel says how many matched in all.
pub const MAX_HITS: usize = 100;

/// How many result rows the panel shows at once. The list scrolls with the highlight.
pub const VISIBLE_ROWS: usize = 5;

/// The widest the panel's content grows, so a wide window keeps it beside the messages.
const PANEL_MAX_W: f32 = 720.0;

/// How many characters of a message a row shows around the first match.
const SNIPPET_CHARS: usize = 110;

/// How many characters of context a snippet keeps before the first match.
const LEAD_CHARS: usize = 28;

/// How long a jumped-to message stays marked.
pub const FLASH_FOR: Duration = Duration::from_millis(1800);

/// The height of one result row, which the panel's row count is sized by.
const ROW_H: f32 = 52.0;

/// The least the history keeps while the panel is open, in points.
pub const HISTORY_MIN_H: f32 = 100.0;

/// What the header and its rule take above the panel.
const HEADER_AND_RULE_H: f32 = 66.0;

/// What the composer takes beyond its text field (padding, border, the buttons' row).
const COMPOSER_EXTRA_H: f32 = 76.0;

/// What the panel takes beyond its list: padding, the box, the status line and the rule under
/// it.
const PANEL_CHROME_H: f32 = 84.0;

/// One message to search, with its text as drawn.
#[derive(Clone, Copy, Debug)]
pub struct Item<'a> {
    pub key: &'a str,
    pub author: &'a str,
    pub time: u32,
    pub text: &'a str,
}

/// One message that matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// The message's key, which a jump names.
    pub key: String,
    pub author: String,
    pub time: u32,
    /// Where the query matches in `author`, as character ranges.
    pub author_marks: Vec<Range<usize>>,
    /// A one-line excerpt of the message around the first match, with an ellipsis where it was
    /// cut. Empty when only the author matched and the message has no text.
    pub snippet: String,
    /// Where the query matches in `snippet`, as character ranges.
    pub marks: Vec<Range<usize>>,
}

/// What a search found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Results {
    /// The newest [`MAX_HITS`] matching messages, newest first.
    pub hits: Vec<Hit>,
    /// How many messages matched in all, which is more than `hits.len()` when the list was cut.
    pub total: usize,
}

/// `s` folded for comparing, with the index in `s` (in characters) each folded character came
/// from. A character that folds to nothing (a combining mark) has no entry.
fn fold_map(s: &str) -> (Vec<char>, Vec<usize>) {
    let mut chars = Vec::new();
    let mut origin = Vec::new();
    for (i, c) in s.chars().enumerate() {
        for f in fold_char(c) {
            chars.push(f);
            origin.push(i);
        }
    }
    (chars, origin)
}

/// Where `needle` (already folded, not empty) occurs in `text`, as non-overlapping character
/// ranges of the original text, left to right.
fn find_all(needle: &[char], text: &str) -> Vec<Range<usize>> {
    let (hay, origin) = fold_map(text);
    let original: Vec<char> = text.chars().collect();
    let mut found: Vec<Range<usize>> = Vec::new();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i] == needle[0] && hay[i..i + needle.len()] == *needle {
            let start = origin[i];
            let mut end = origin[i + needle.len() - 1] + 1;
            // The combining marks after the last letter are part of it, so a mark never splits
            // an accent from its letter.
            while original
                .get(end)
                .is_some_and(|c| fold_char(*c).next().is_none())
            {
                end += 1;
            }
            found.push(start..end);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    // Two folded characters from one original (a letter that lowercases to two) can make
    // neighbouring matches share it; keep the ranges in order and apart.
    let mut last_end = 0;
    found.retain_mut(|r| {
        r.start = r.start.max(last_end);
        last_end = r.end.max(last_end);
        r.start < r.end
    });
    found
}

/// `text` with every control character and whitespace run character as one plain space, so a
/// query can match across a line break. One character in, one out: positions are unchanged.
fn flatten(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() || c.is_whitespace() {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// A line of `text` (already [`flatten`]ed) around its first match: at most [`SNIPPET_CHARS`] characters, an ellipsis where it was cut, and the matches inside it.
fn excerpt(text: &str, marks: &[Range<usize>]) -> (String, Vec<Range<usize>>) {
    let chars: Vec<char> = text
        .chars()
        .map(|c| {
            if c.is_control() || c.is_whitespace() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let first = marks.first().map_or(0, |r| r.start);
    let from = first.saturating_sub(LEAD_CHARS);
    let to = (from + SNIPPET_CHARS).min(chars.len());
    let lead = usize::from(from > 0);
    let mut out = String::new();
    if from > 0 {
        out.push('\u{2026}');
    }
    out.extend(&chars[from..to]);
    if to < chars.len() {
        out.push('\u{2026}');
    }
    let shown = to - from;
    let inside = marks
        .iter()
        .filter(|r| r.end > from && r.start < to)
        .map(|r| {
            let s = r.start.max(from) - from;
            let e = r.end.min(to) - from;
            s + lead..e.min(shown) + lead
        })
        .collect();
    (out, inside)
}

/// The messages of `items` that contain `query`, newest first (`items` is oldest first), at most
/// [`MAX_HITS`] of them, with the total that matched. A blank query finds nothing. The query
/// matches the message text or the author's name.
pub fn search(query: &str, items: &[Item]) -> Results {
    let query: String = flatten(query.trim())
        .chars()
        .take(MAX_QUERY_CHARS)
        .collect();
    let (needle, _) = fold_map(query.trim());
    if needle.is_empty() {
        return Results::default();
    }
    let mut results = Results::default();
    // Each distinct author is folded once, however many messages they wrote.
    let mut by_author: HashMap<&str, Vec<Range<usize>>> = HashMap::new();
    for item in items.iter().rev() {
        let text = flatten(item.text);
        let text_marks = find_all(&needle, &text);
        let author_marks = by_author
            .entry(item.author)
            .or_insert_with(|| find_all(&needle, item.author))
            .clone();
        if text_marks.is_empty() && author_marks.is_empty() {
            continue;
        }
        results.total += 1;
        if results.hits.len() >= MAX_HITS {
            continue;
        }
        let (snippet, marks) = excerpt(&text, &text_marks);
        results.hits.push(Hit {
            key: item.key.to_owned(),
            author: item.author.to_owned(),
            time: item.time,
            author_marks,
            snippet,
            marks,
        });
    }
    results
}

/// The text of `m` as it is drawn: the markdown with its markup dropped (a mention reads
/// `@name`), or the plain text when the message is not markdown.
pub fn displayed_text(m: &Message) -> String {
    if !m.md.is_empty() && markdown::is_cheap(&m.md) {
        markdown::strip_with(&m.md, false)
    } else {
        m.text.clone()
    }
}

/// The search over a whole room.
pub fn search_room(query: &str, room: &Room) -> Results {
    let texts: Vec<String> = room.messages.iter().map(displayed_text).collect();
    let items: Vec<Item> = room
        .messages
        .iter()
        .zip(&texts)
        .map(|(m, text)| Item {
            key: &m.key,
            author: room.nickname(m.author),
            time: m.time,
            text,
        })
        .collect();
    search(query, &items)
}

/// `chars` ranges of `s` as byte ranges, for drawing. A range past the end is cut to it.
pub fn byte_ranges(s: &str, marks: &[Range<usize>]) -> Vec<Range<usize>> {
    let at: Vec<usize> = s
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(s.len()))
        .collect();
    marks
        .iter()
        .filter_map(|r| {
            let start = *at.get(r.start)?;
            let end = *at.get(r.end).unwrap_or(&s.len());
            (start < end).then_some(start..end)
        })
        .collect()
}

/// Whether `ev` is the search chord: Ctrl+F, or Cmd+F on macOS (rune reports both as
/// `Command('f')`), without Shift or Alt.
pub fn is_chord(ev: &KeyEvent) -> bool {
    ev.is_command('f') && !ev.mods.shift && !ev.mods.alt
}

/// The search panel while it is open.
pub struct Search {
    pub query: TextEdit,
    /// The highlighted result, by message key, so a list that changes under it (a new message,
    /// an edit) keeps the same row highlighted. `None`, or a key no longer listed, highlights
    /// the top row. Cleared on every edit of the query.
    sel: Option<String>,
    /// Whether the composer had the keyboard when the panel opened, to give it back on close.
    restore_composer: bool,
    /// The last results, with what they were computed for (see [`Chat::search_results`]).
    cache: RefCell<Option<(u64, Rc<Results>)>>,
    /// How many times the search has run, for tests.
    pub runs: Cell<u32>,
}

impl Search {
    fn new(restore_composer: bool) -> Self {
        let mut query = TextEdit::new("");
        query.focused = true;
        Search {
            query,
            sel: None,
            restore_composer,
            cache: RefCell::new(None),
            runs: Cell::new(0),
        }
    }
}

/// What the results of a search depend on: the query, the room, and every message's text,
/// author and edit mark, and the members' nicknames. Hashed without parsing anything, so it is
/// cheap enough to compute each frame.
fn signature(query: &str, room: &Room) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    query.hash(&mut h);
    room.key.hash(&mut h);
    for m in &room.messages {
        (&m.key, &m.text, &m.md, m.edited, m.author, m.time).hash(&mut h);
    }
    for m in &room.members {
        (m.id, &m.nickname).hash(&mut h);
    }
    h.finish()
}

/// A message marked for a moment after a jump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flash {
    pub room: String,
    pub message: String,
    until: Instant,
}

impl Chat {
    /// The chord: opens the panel, or when it is open, puts the keyboard back in its box. It
    /// never closes it, so a held key repeating the chord changes nothing (no clock needed;
    /// Escape closes). Returns whether the key was used: it is not while a dialog is up or the
    /// quick switcher is open, or when there is no room to search.
    pub fn search_chord(&mut self) -> bool {
        self.close_search_under_dialog();
        if self.other_dialog_open() || self.switcher.is_some() || self.room().is_none() {
            return false;
        }
        if let Some(s) = &mut self.search {
            s.query.focused = true;
            self.composer.focused = false;
            return true;
        }
        self.emoji_open = false;
        self.picker = None;
        self.members_overlay = false;
        // The box takes the keyboard: the composer must not keep it, or typed text would go to
        // both.
        let had = std::mem::replace(&mut self.composer.focused, false);
        self.search = Some(Search::new(had));
        true
    }

    /// Close the panel for good: a dialog opened, or the room changed. Nothing gets the keyboard
    /// back, because something else has it.
    pub fn drop_search(&mut self) {
        self.search = None;
    }

    /// Close the panel and give the composer the keyboard again if it had it when the panel
    /// opened (and the room can still be written in).
    pub fn close_search(&mut self) {
        if let Some(s) = self.search.take()
            && s.restore_composer
            && self.actionable().is_some()
        {
            self.composer.focused = true;
        }
    }

    /// The one place that keeps the panel from sitting under a dialog: called every frame and
    /// before the chord acts, so whichever dialog opened (and however it did) closes the panel.
    pub fn close_search_under_dialog(&mut self) {
        if self.search.is_some() && self.modal_dialog_open() {
            self.drop_search();
        }
    }

    /// The query was edited: the highlight starts again at the top.
    pub fn search_edited(&mut self) {
        if let Some(s) = &mut self.search {
            s.sel = None;
        }
    }

    /// The results for what is typed now, in the open room. Kept until the query, the room's
    /// messages or the nicknames change, so a frame that changed nothing does not search again.
    pub fn search_results(&self) -> Rc<Results> {
        let (Some(s), Some(room)) = (&self.search, self.room()) else {
            return Rc::default();
        };
        let sig = signature(&s.query.text, room);
        if let Some((have, found)) = &*s.cache.borrow()
            && *have == sig
        {
            return found.clone();
        }
        s.runs.set(s.runs.get() + 1);
        let found = Rc::new(search_room(&s.query.text, room));
        *s.cache.borrow_mut() = Some((sig, found.clone()));
        found
    }

    /// The highlighted row of `hits`: where the highlighted message is now, or the top row.
    pub fn search_selected(&self, hits: &[Hit]) -> usize {
        self.search
            .as_ref()
            .and_then(|s| s.sel.as_ref())
            .and_then(|k| hits.iter().position(|h| &h.key == k))
            .unwrap_or(0)
    }

    pub fn search_highlight(&mut self, key: &str) {
        if let Some(s) = &mut self.search {
            s.sel = Some(key.to_owned());
        }
    }

    /// Move the highlight by `delta` rows, wrapping round.
    pub fn search_move(&mut self, delta: isize) {
        let found = self.search_results();
        if found.hits.is_empty() {
            return;
        }
        let n = found.hits.len() as isize;
        let next = (self.search_selected(&found.hits) as isize + delta).rem_euclid(n);
        self.search_highlight(&found.hits[next as usize].key.clone());
    }

    /// Scroll to the message keyed `key` in the open room and mark it for a moment. The panel
    /// stays open so the next match is one key away. Does nothing when the message is gone.
    pub fn search_jump(&mut self, key: &str, now: Instant) {
        let Some(room) = self.room().filter(|r| r.message_by_key(key).is_some()) else {
            return;
        };
        let room = room.key.clone();
        self.search_highlight(key);
        self.reveal = Some(Reveal::at_once(&room, key));
        self.flash = Some(Flash {
            room,
            message: key.to_owned(),
            until: now + FLASH_FOR,
        });
    }

    /// Jump to the highlighted row (Enter). With nothing listed it does nothing.
    pub fn search_jump_selected(&mut self, now: Instant) {
        let found = self.search_results();
        let i = self.search_selected(&found.hits);
        if let Some(hit) = found.hits.get(i) {
            self.search_jump(&hit.key.clone(), now);
        }
    }

    /// Keys in the panel's box: Up and Down move, Escape closes it. Enter is the box's submit.
    /// Returns whether the key was used.
    pub fn search_key(&mut self, ev: &KeyEvent) -> bool {
        if self.search.is_none() {
            return false;
        }
        let m = ev.mods;
        let bare = !(m.shift || m.ctrl || m.alt || m.logo);
        match ev.key {
            Key::Down if bare => self.search_move(1),
            Key::Up if bare => self.search_move(-1),
            // The same Escape the window's handler gives: it waits while something drawn over
            // the panel (a picker, a menu) has its own Escape to take.
            Key::Escape => return self.escape(),
            _ => return false,
        }
        true
    }

    /// The message to draw marked, if it is in the open room.
    pub fn flashing(&self) -> Option<&str> {
        let f = self.flash.as_ref()?;
        (self.room()?.key == f.room).then_some(f.message.as_str())
    }

    /// Whether the mark is still showing at `now`, ending it when its time is up. True while
    /// it is, so the caller asks for another frame.
    pub fn tick_flash(&mut self, now: Instant) -> bool {
        if self.flash.as_ref().is_some_and(|f| now >= f.until) {
            self.flash = None;
        }
        self.flash.is_some()
    }
}

/// How many result rows fit in a window `win_h` high whose composer field is `field_h` high
/// (it grows with the draft, up to eight lines), while the history keeps [`HISTORY_MIN_H`]. At
/// most [`VISIBLE_ROWS`]. Zero means there is only room for the box alone: the panel is then
/// one line and the list is left out (Up, Down and Enter still work on it).
pub fn visible_rows(win_h: f32, field_h: f32) -> usize {
    let spare = win_h - HEADER_AND_RULE_H - (field_h + COMPOSER_EXTRA_H) - HISTORY_MIN_H;
    (((spare - PANEL_CHROME_H) / ROW_H).floor().max(0.0) as usize).min(VISIBLE_ROWS)
}

/// The panel between the header and the history, when open. It takes room from the history
/// instead of covering it, so a message jumped to (which scrolls to the top of the history)
/// is never under it.
pub(super) fn search_panel(
    chat: &Chat,
    win_h: f32,
    field_h: f32,
    p: &Palette,
    k: &Kit,
) -> Option<El> {
    let s = chat.search.as_ref()?;
    let found = chat.search_results();
    let sel = chat.search_selected(&found.hits);
    let input = TextInput::new(&s.query)
        .placeholder("Search this room")
        .width(None)
        .on_edit(|s: &mut RiverApp| &mut s.chat.search.as_mut().expect("open").query)
        .on_change(|s: &mut RiverApp, _| s.chat.search_edited())
        .on_key(|s: &mut RiverApp, ev, _| {
            if is_chord(ev) {
                // Taken here whatever it does, so it never also reaches the window's handler.
                s.chat.search_chord();
                return true;
            }
            if s.chat.search_key(ev) {
                return true;
            }
            // Word keys edit without the field's change hook, so they reset the highlight here.
            let edited = s
                .chat
                .search
                .as_mut()
                .is_some_and(|s| keys::word_keys(&mut s.query, ev));
            if edited {
                s.chat.search_edited();
            }
            edited
        })
        .on_submit(|s: &mut RiverApp, _| s.chat.search_jump_selected(Instant::now()))
        .build(k)
        .key("search-input")
        .grow(1.0)
        .min_w(0.0);
    let rows = visible_rows(win_h, field_h);
    let compact = rows == 0;
    let blank = s.query.text.trim().is_empty();
    let mut status = if blank {
        "Type to search the messages loaded here".to_owned()
    } else if found.total == 0 {
        "No matches".to_owned()
    } else if found.total > found.hits.len() {
        format!(
            "{} of {} matches (the newest {} are listed)",
            sel + 1,
            found.total,
            found.hits.len()
        )
    } else {
        format!("{} of {} matches", sel + 1, found.total)
    };
    if s.query.text.trim().chars().count() > MAX_QUERY_CHARS {
        status.push_str(&format!(
            " (matching the first {MAX_QUERY_CHARS} characters)"
        ));
    }
    let close = row()
        .key("search-close")
        .role("button")
        .label("Close search")
        .help("Close search")
        .h(28.0)
        .px(8.0)
        .radius(6.0)
        .center()
        .hover(p.surface_hover)
        .cursor(rune::Cursor::Pointer)
        .on_press(|s: &mut RiverApp| s.chat.close_search())
        .child(label("Esc", st::SMALL, p.text_secondary));
    let mut list = col().key("search-results").w_full().gap(2.0);
    let (start, shown) = if found.hits.is_empty() {
        (0, 0)
    } else {
        window(sel, found.hits.len(), rows)
    };
    for (i, hit) in found.hits.iter().enumerate().skip(start).take(shown) {
        list = list.child(result_row(chat, hit, i, i == sel, p));
    }
    let status = label(status, st::SMALL, p.text_muted)
        .key("search-status")
        .px(2.0);
    let top = row().gap(8.0).center_y().child(input);
    let mut inner = col().w_full().max_w(PANEL_MAX_W).self_center().gap(8.0);
    if compact {
        // Only room for one line: the status sits beside the box and the list is left out.
        inner = inner.child(top.child(status.truncate().max_w(220.0)).child(close));
    } else {
        inner = inner.child(top.child(close)).child(status).child(list);
    }
    let panel = col()
        .key("search-panel")
        .role("search")
        .label("Search this room")
        .w_full()
        .px(16.0)
        .py(if compact { 6.0 } else { 10.0 })
        .fill(p.surface)
        .child(inner);
    Some(panel)
}

/// `text` as a one-line label with `marks` (character ranges) drawn bold in the accent colour.
fn marked(text: &str, marks: &[Range<usize>], style: TextStyle, base: Color, p: &Palette) -> El {
    let spans: Vec<rune::Span> = byte_ranges(text, marks)
        .into_iter()
        .map(|r| rune::Span::new(r.start, r.end).bold().color(p.accent))
        .collect();
    rune::rich(text.to_owned(), spans)
        .font(style)
        .color(base)
        .truncate()
        .w_full()
}

fn result_row(chat: &Chat, hit: &Hit, i: usize, selected: bool, p: &Palette) -> El {
    let key = hit.key.clone();
    let when = model::full_time(chat.day_zero, hit.time);
    // "Thu, Nov 19, 2026 14:05" is long for a row; the weekday is dropped.
    let when = when
        .split_once(", ")
        .map_or(when.as_str(), |(_, rest)| rest);
    let head = row()
        .gap(8.0)
        .center_y()
        .w_full()
        .child(marked(&hit.author, &hit.author_marks, st::SMALL_STRONG, p.text, p).shrink())
        .child(label(when.to_owned(), st::SMALL, p.text_muted));
    let mut r = col()
        .key(format!("search-row-{i}"))
        .role("button")
        .label(format!("{}: {}", hit.author, hit.snippet))
        .w_full()
        .px(10.0)
        .py(7.0)
        .gap(2.0)
        .radius(6.0)
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut RiverApp| s.chat.search_jump(&key, Instant::now()))
        .child(head)
        .child(marked(
            &hit.snippet,
            &hit.marks,
            st::BODY,
            p.text_secondary,
            p,
        ));
    r = if selected {
        r.fill(p.surface_hover)
    } else {
        r.hover(p.row_hover)
    };
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item<'a>(key: &'a str, author: &'a str, text: &'a str) -> Item<'a> {
        Item {
            key,
            author,
            time: 0,
            text,
        }
    }

    fn keys_of(r: &Results) -> Vec<&str> {
        r.hits.iter().map(|h| h.key.as_str()).collect()
    }

    #[test]
    fn a_blank_query_finds_nothing() {
        let items = [item("a", "Ana", "hello")];
        assert_eq!(search("", &items), Results::default());
        assert_eq!(search("   ", &items), Results::default());
    }

    #[test]
    fn matches_ignore_case_and_come_newest_first() {
        let items = [
            item("1", "Ana", "Hello there"),
            item("2", "Bo", "nothing"),
            item("3", "Cy", "HELLO again"),
        ];
        let r = search("hello", &items);
        assert_eq!(keys_of(&r), ["3", "1"]);
        assert_eq!(r.total, 2);
        assert_eq!(r.hits[1].marks, vec![0..5]);
    }

    #[test]
    fn combining_accents_are_ignored_on_both_sides() {
        let items = [item("1", "Ana", "un cafe\u{301} noir")];
        assert_eq!(keys_of(&search("cafe", &items)), ["1"]);
        assert_eq!(keys_of(&search("cafe\u{301}", &items)), ["1"]);
        let r = search("cafe", &items);
        // The mark covers the letter and its accent.
        assert_eq!(r.hits[0].marks, vec![3..8]);
    }

    #[test]
    fn the_author_name_matches_too() {
        let items = [item("1", "Zoë Rivers", "hi"), item("2", "Bo", "hi")];
        let r = search("rivers", &items);
        assert_eq!(keys_of(&r), ["1"]);
        assert_eq!(r.hits[0].author_marks, vec![4..10]);
        assert!(r.hits[0].marks.is_empty());
    }

    #[test]
    fn every_occurrence_in_a_message_is_marked() {
        let items = [item("1", "A", "ab ab ab")];
        assert_eq!(search("ab", &items).hits[0].marks, vec![0..2, 3..5, 6..8]);
    }

    #[test]
    fn wide_and_multi_char_text_never_splits_a_character() {
        let text = "\u{1F600}\u{1F600} \u{65E5}\u{672C}\u{8A9E} find \u{1F680}";
        let items = [item("1", "A", text)];
        let r = search("find", &items);
        let hit = &r.hits[0];
        let bytes = byte_ranges(&hit.snippet, &hit.marks);
        assert_eq!(&hit.snippet[bytes[0].clone()], "find");
        // A capital with a two-character lowercase form does not shift the marks.
        let r = search("x", &[item("1", "A", "\u{130}x")]);
        assert_eq!(r.hits[0].marks, vec![1..2]);
    }

    #[test]
    fn a_long_message_is_cut_around_the_first_match_with_ellipses() {
        let text = format!("{} needle {}", "a".repeat(300), "b".repeat(300));
        let r = search("needle", &[item("1", "A", &text)]);
        let hit = &r.hits[0];
        assert!(hit.snippet.starts_with('\u{2026}') && hit.snippet.ends_with('\u{2026}'));
        assert!(hit.snippet.chars().count() <= SNIPPET_CHARS + 2);
        let bytes = byte_ranges(&hit.snippet, &hit.marks);
        assert_eq!(&hit.snippet[bytes[0].clone()], "needle");
    }

    #[test]
    fn a_match_cut_off_at_the_window_edge_is_clipped_not_dropped_wrongly() {
        // A second match beyond the window is left out; the first stays.
        let text = format!("needle {} needle", "x".repeat(300));
        let r = search("needle", &[item("1", "A", &text)]);
        assert_eq!(r.hits[0].marks.len(), 1);
    }

    #[test]
    fn line_breaks_become_spaces_in_the_snippet() {
        let r = search("b c", &[item("1", "A", "a\nb\tc")]);
        assert_eq!(r.hits[0].snippet, "a b c");
    }

    #[test]
    fn hits_are_capped_and_the_total_still_counts_all() {
        let keys: Vec<String> = (0..MAX_HITS + 25).map(|i| format!("k{i}")).collect();
        let items: Vec<Item> = keys.iter().map(|k| item(k, "A", "word")).collect();
        let r = search("word", &items);
        assert_eq!(r.hits.len(), MAX_HITS);
        assert_eq!(r.total, MAX_HITS + 25);
        // The newest are the ones kept.
        assert_eq!(r.hits[0].key, format!("k{}", MAX_HITS + 24));
    }

    #[test]
    fn byte_ranges_stay_inside_the_text() {
        let one = |a: usize, b: usize| vec![Range { start: a, end: b }];
        assert_eq!(byte_ranges("a\u{e9}b", &one(1, 2)), vec![1..3]);
        assert!(byte_ranges("ab", &one(5, 9)).is_empty());
        assert_eq!(byte_ranges("ab", &one(1, 9)), vec![1..2]);
    }

    fn message(key: &str, md: &str, text: &str) -> Message {
        Message {
            id: 1,
            key: key.into(),
            author: 1,
            time: 0,
            text: text.into(),
            source: String::new(),
            md: md.into(),
            reply_to: None,
            reactions: Vec::new(),
            edited: false,
            pending: None,
        }
    }

    #[test]
    fn markup_is_not_searched_but_what_it_shows_is() {
        let m = message("1", "**bold** [link text](https://example.com/path)", "x");
        let shown = displayed_text(&m);
        assert_eq!(shown, "bold link text");
        assert_eq!(
            keys_of(&search("example.com", &[item("1", "A", &shown)])),
            Vec::<&str>::new()
        );
        assert_eq!(
            keys_of(&search("bold link", &[item("1", "A", &shown)])),
            ["1"]
        );
        // A message that is not markdown is searched as written.
        assert_eq!(displayed_text(&message("2", "", "a *b*")), "a *b*");
    }

    #[test]
    fn the_flash_lasts_a_set_time_by_the_clock_it_is_given() {
        let mut chat = Chat::example();
        let t0 = Instant::now();
        assert!(!chat.tick_flash(t0));
        chat.flash = Some(Flash {
            room: "r".into(),
            message: "m".into(),
            until: t0 + FLASH_FOR,
        });
        // Many frames in a short time (a moving mouse) do not shorten it.
        for _ in 0..500 {
            assert!(chat.tick_flash(t0 + FLASH_FOR / 2));
        }
        assert!(!chat.tick_flash(t0 + FLASH_FOR));
        assert!(chat.flash.is_none());
    }

    fn with_panel() -> Chat {
        let mut chat = Chat::example();
        assert!(chat.search_chord());
        chat
    }

    #[test]
    fn a_dialog_open_over_the_panel_closes_it_and_the_chord_will_not_reopen_over_it() {
        let uid = Chat::example().room().expect("room").members[0].uid.clone();
        type Open = fn(&mut Chat, &str);
        let openers: [(&str, Open); 5] = [
            ("do not disturb", |c, _| c.open_dnd()),
            ("details", |c, _| c.open_details()),
            ("member", |c, uid| c.open_member(uid)),
            ("notify", |c, _| c.open_notify_dialog()),
            ("node status", |c, _| c.status.open()),
        ];
        for (name, open) in openers {
            let mut chat = with_panel();
            open(&mut chat, &uid);
            assert!(chat.other_dialog_open(), "{name} is open");
            // The chord checks the dialog first: it is refused, and the box never takes focus.
            assert!(!chat.search_chord(), "{name}: the chord is refused");
            assert!(chat.search.is_none(), "{name}: the panel closed");
            // The per-frame hook does the same for a dialog that opened without a chord.
            let mut chat = with_panel();
            open(&mut chat, &uid);
            chat.close_search_under_dialog();
            assert!(chat.search.is_none(), "{name}: closed by the frame hook");
        }
    }

    #[test]
    fn a_link_menu_leaves_the_panel_and_its_query_alone_but_a_dialog_closes_it() {
        let mut chat = with_panel();
        chat.search
            .as_mut()
            .expect("open")
            .query
            .replace_text("the");
        chat.open_link_menu(
            "https://freenet.org/",
            rune::PointerPress { x: 1.0, y: 2.0 },
        );
        assert!(chat.link_menu.is_some());
        chat.close_search_under_dialog();
        let s = chat.search.as_ref().expect("the panel stays");
        assert_eq!(s.query.text, "the");
        // It still does not open anew over the menu.
        chat.close_search();
        assert!(!chat.search_chord());
        chat.link_menu = None;
        assert!(chat.search_chord());
        chat.open_details();
        chat.close_search_under_dialog();
        assert!(chat.search.is_none(), "a dialog closes it");
    }

    #[test]
    fn closing_the_panel_gives_the_composer_the_keyboard_back_only_if_it_had_it() {
        let mut chat = Chat::example();
        chat.composer.focused = true;
        assert!(chat.search_chord());
        assert!(!chat.composer.focused);
        chat.close_search();
        assert!(chat.composer.focused, "restored");
        let mut chat = Chat::example();
        chat.composer.focused = false;
        assert!(chat.search_chord());
        chat.close_search();
        assert!(!chat.composer.focused, "it did not have it");
        // A room that cannot be written in takes no focus.
        let mut chat = Chat::example();
        chat.composer.focused = true;
        assert!(chat.search_chord());
        chat.edit_rooms(|r| r[0].send_block = Some("read only".into()));
        chat.close_search();
        assert!(!chat.composer.focused);
    }

    #[test]
    fn escape_in_the_box_and_the_window_handler_agree() {
        // Nothing over the panel: both close it.
        let mut chat = with_panel();
        assert!(chat.search_key(&esc()));
        assert!(chat.search.is_none());
        let mut chat = with_panel();
        assert!(chat.escape());
        assert!(chat.search.is_none());
        // A picker or the narrow members panel has its own Escape: both leave the panel alone.
        let mut chat = with_panel();
        chat.picker = Some("m".into());
        assert!(!chat.search_key(&esc()));
        assert!(!chat.escape());
        assert!(chat.search.is_some());
        let mut chat = with_panel();
        chat.members_overlay = true;
        assert!(!chat.search_key(&esc()));
        assert!(chat.search.is_some());
    }

    fn esc() -> KeyEvent {
        KeyEvent {
            key: Key::Escape,
            mods: rune::Mods::default(),
        }
    }

    #[test]
    fn a_frame_that_changed_nothing_does_not_search_again_and_an_edit_does() {
        let mut chat = with_panel();
        chat.search
            .as_mut()
            .expect("open")
            .query
            .replace_text("the");
        let first = chat.search_results();
        for _ in 0..5 {
            assert!(Rc::ptr_eq(&first, &chat.search_results()));
        }
        assert_eq!(chat.search.as_ref().expect("open").runs.get(), 1);
        // A new query, an edited message and a renamed member each search again.
        chat.search
            .as_mut()
            .expect("open")
            .query
            .replace_text("then");
        chat.search_results();
        assert_eq!(chat.search.as_ref().expect("open").runs.get(), 2);
        chat.edit_rooms(|r| r[0].messages[0].text.push_str(" more"));
        chat.search_results();
        assert_eq!(chat.search.as_ref().expect("open").runs.get(), 3);
        chat.edit_rooms(|r| r[0].members[0].nickname.push('!'));
        chat.search_results();
        assert_eq!(chat.search.as_ref().expect("open").runs.get(), 4);
    }

    #[test]
    fn a_pasted_long_query_is_cut_and_a_query_with_a_line_break_matches_across_one() {
        let long = "ab".repeat(5000);
        let text = "ab".repeat(5000);
        let r = search(&long, &[item("1", "A", &text)]);
        assert_eq!(r.total, 1, "the first {MAX_QUERY_CHARS} characters match");
        let r = search("b\nc", &[item("1", "A", "a b c")]);
        assert_eq!(r.total, 1);
    }

    #[test]
    fn the_rows_shown_shrink_with_the_window_and_the_composer() {
        assert_eq!(visible_rows(800.0, 24.0), VISIBLE_ROWS);
        // A tall composer takes rows from the list.
        assert!(visible_rows(640.0, 192.0) < visible_rows(640.0, 24.0));
        assert_eq!(visible_rows(480.0, 192.0), 0);
        assert_eq!(visible_rows(10.0, 24.0), 0);
    }

    #[test]
    fn raw_html_is_not_searched_because_it_is_not_drawn() {
        let m = message("1", "a <b>bold</b> word", "x");
        assert_eq!(displayed_text(&m), "a bold word");
    }

    #[test]
    fn the_chord_is_command_f_without_shift_or_alt() {
        let f = |shift, alt| {
            is_chord(&KeyEvent {
                key: Key::Command('f'),
                mods: rune::Mods {
                    cmd: true,
                    ctrl: !cfg!(target_os = "macos"),
                    logo: cfg!(target_os = "macos"),
                    shift,
                    alt,
                },
            })
        };
        assert!(f(false, false));
        assert!(!f(true, false));
        assert!(!f(false, true));
    }
}
