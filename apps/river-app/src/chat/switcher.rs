//! The quick switcher: Ctrl+K (Cmd+K on macOS) opens a box that finds a room by name or a
//! member of the open room by nickname. Enter on a room opens it; Enter on a member opens that
//! member's dialog.
//!
//! The matching is a pure function ([`search`]) over plain data, so it is tested without a
//! window. [`Chat`] holds the open state and acts on a choice; `view.rs` draws it.

use rune::{El, Key, KeyEvent, TextEdit, col, el, row};
use rune_kit::{Kit, TextInput};

use super::Chat;
use super::conversation::avatar;
use super::dialog_kit::DialogSpec;
use super::style::{self as st, Palette};
use super::view::label;
use super::word_end::{Get, word_end_after};
use crate::RiverApp;

/// The most results one search returns.
pub const MAX_HITS: usize = 40;

/// How many result rows the box shows at once. The list scrolls with the highlight.
pub const VISIBLE_ROWS: usize = 8;

/// What the switcher can open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// The room with this key.
    Room(String),
    /// The member of the open room with this `Member::uid`.
    Member(String),
}

/// One result: what it opens and how well it matched (higher is better; 0 for an empty query).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub target: Target,
    pub score: u32,
}

/// A room to search: its key, name, and how many unread messages count for it.
#[derive(Clone, Copy, Debug)]
pub struct RoomItem<'a> {
    pub key: &'a str,
    pub name: &'a str,
    pub unread: usize,
}

/// A member to search: their `uid` and nickname.
#[derive(Clone, Copy, Debug)]
pub struct MemberItem<'a> {
    pub uid: &'a str,
    pub name: &'a str,
}

/// The characters matching compares: lowercased, with combining marks (U+0300 to U+036F)
/// dropped, so "cafe" finds "cafe" typed as `e` plus a combining acute accent, and the two
/// spellings of a decomposed name compare equal.
///
/// Limit, on purpose: there is no Unicode normalisation here (it would need a new dependency
/// for this one box). An accent typed as a single precomposed character ("é") does not match
/// the same letter in decomposed form, and a plain "e" does not match a precomposed "é".
fn fold(s: &str) -> Vec<char> {
    s.chars().flat_map(fold_char).collect()
}

/// One character as [`fold`] compares it: lowercased (which may give several characters) and
/// with combining marks dropped (which gives none). Shared with message search, which has to
/// know which original character each folded one came from.
pub(super) fn fold_char(c: char) -> impl Iterator<Item = char> {
    c.to_lowercase()
        .filter(|c| !('\u{300}'..='\u{36f}').contains(c))
}

/// The fewest characters a query needs before a fuzzy (in-order, gaps allowed) match counts.
/// One or two letters would match almost every name, so they only match as a prefix, the start
/// of a word or a substring.
const FUZZY_MIN: usize = 3;

/// How well `query` matches `text`, or `None` when it does not match at all. Case does not
/// matter. In falling order of score: the whole text, a prefix, a substring that starts a word,
/// any other substring, and last a fuzzy match, where the query's characters appear in order
/// with other characters between them (only for queries of [`FUZZY_MIN`] characters or more). A blank query matches everything with score 0.
pub fn match_score(query: &str, text: &str) -> Option<u32> {
    let q = fold(query.trim());
    let t = fold(text);
    if q.is_empty() {
        return Some(0);
    }
    if q.len() > t.len() {
        return None;
    }
    if q == t {
        return Some(1000);
    }
    let at = |i: usize| t[i..].starts_with(&q);
    if let Some(i) = (0..=t.len() - q.len()).find(|&i| at(i)) {
        // The first occurrence that starts a word beats an earlier one in the middle of one.
        let word_start = |i: usize| i == 0 || !t[i - 1].is_alphanumeric();
        if i == 0 {
            return Some(900 - (t.len() - q.len()).min(100) as u32);
        }
        if let Some(w) = (i..=t.len() - q.len()).find(|&j| at(j) && word_start(j)) {
            return Some(800 - w.min(100) as u32);
        }
        return Some(700 - i.min(100) as u32);
    }
    if q.len() < FUZZY_MIN {
        return None;
    }
    // Fuzzy: greedy in-order match; reward characters that follow each other and word starts.
    let mut score = 300u32;
    let mut from = 0;
    let mut prev: Option<usize> = None;
    for c in &q {
        let i = from + t[from..].iter().position(|x| x == c)?;
        if prev.is_some_and(|p| p + 1 == i) {
            score += 20;
        } else if i == 0 || !t[i - 1].is_alphanumeric() {
            score += 10;
        } else if prev.is_some() {
            score = score.saturating_sub(3);
        }
        prev = Some(i);
        from = i + 1;
    }
    Some(score.min(599))
}

/// The results for `query`, best first, at most [`MAX_HITS`].
///
/// With a blank query: the rooms with unread messages first (most unread first, the room list's
/// order among equals), then the other rooms in the room list's order, then the members by
/// nickname. Otherwise rooms and members that match, by score; on a tie a room comes before a
/// member, then more unread, then the shorter, then the alphabetically first name.
pub fn search(query: &str, rooms: &[RoomItem], members: &[MemberItem]) -> Vec<Hit> {
    let hit = |target, score| Hit { target, score };
    if query.trim().is_empty() {
        let mut order: Vec<&RoomItem> = rooms.iter().collect();
        // Stable: rooms with equal unread keep the room list's order.
        order.sort_by_key(|r| std::cmp::Reverse(r.unread));
        let mut named: Vec<&MemberItem> = members.iter().collect();
        named.sort_by_cached_key(|m| (fold(m.name), m.uid.to_owned()));
        return order
            .into_iter()
            .map(|r| hit(Target::Room(r.key.to_owned()), 0))
            .chain(
                named
                    .into_iter()
                    .map(|m| hit(Target::Member(m.uid.to_owned()), 0)),
            )
            .take(MAX_HITS)
            .collect();
    }
    // Sort keys: best score, rooms before members, more unread, shorter, then by name.
    type Order = (
        std::cmp::Reverse<u32>,
        bool,
        std::cmp::Reverse<usize>,
        usize,
    );
    let mut found: Vec<(Order, Vec<char>, usize, Hit)> = Vec::new();
    for (i, r) in rooms.iter().enumerate() {
        if let Some(s) = match_score(query, r.name) {
            let name = fold(r.name);
            let order = (
                std::cmp::Reverse(s),
                false,
                std::cmp::Reverse(r.unread),
                name.len(),
            );
            found.push((order, name, i, hit(Target::Room(r.key.to_owned()), s)));
        }
    }
    for (i, m) in members.iter().enumerate() {
        if let Some(s) = match_score(query, m.name) {
            let name = fold(m.name);
            let order = (std::cmp::Reverse(s), true, std::cmp::Reverse(0), name.len());
            found.push((order, name, i, hit(Target::Member(m.uid.to_owned()), s)));
        }
    }
    found.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));
    found.into_iter().map(|f| f.3).take(MAX_HITS).collect()
}

/// The rows to draw: the index of the first, and how many, so the highlighted one `sel` is
/// inside the window of `visible` rows out of `total`. The window moves only as far as it must.
pub fn window(sel: usize, total: usize, visible: usize) -> (usize, usize) {
    let shown = total.min(visible);
    let start = (sel + 1).saturating_sub(shown).min(total - shown);
    (start, shown)
}

/// How long the chord must have been quiet before it acts again. The chord is a toggle and a
/// key event carries no repeat flag, so a held Ctrl+K would open and close the box every repeat.
/// Every chord event, acted on or not, restarts this wait, so a held key (repeats arrive well
/// inside it, after the first one's start-up delay of up to about a second) acts once, on the
/// press, and never again until released. The price: two deliberate presses less than this
/// apart act once. Escape always closes.
pub const CHORD_QUIET: std::time::Duration = std::time::Duration::from_millis(1000);

/// Whether a chord at `now` should act, given when the last one arrived. Records `now`.
pub fn chord_gate(last: &mut Option<std::time::Instant>, now: std::time::Instant) -> bool {
    let quiet = last.is_none_or(|t| now.saturating_duration_since(t) >= CHORD_QUIET);
    *last = Some(now);
    quiet
}

/// The switcher while it is open.
pub struct Switcher {
    pub query: TextEdit,
    /// The highlighted result, by what it opens, so a list that changes under it (an unread
    /// count moving a room, a member joining) keeps the same row highlighted. `None`, or a
    /// target no longer listed, highlights the top row. Cleared on every edit of the query.
    sel: Option<Target>,
}

impl Switcher {
    fn new() -> Self {
        let mut query = TextEdit::new("");
        query.focused = true;
        Switcher { query, sel: None }
    }
}

impl Chat {
    /// Whether a dialog the switcher must not open over is showing.
    pub(super) fn other_dialog_open(&self) -> bool {
        self.modal_dialog_open() || self.link_menu.is_some()
    }

    /// Whether a dialog is showing, as opposed to a small popup such as the link menu. What
    /// closes the search panel: a popup opened from the history does not.
    pub(super) fn modal_dialog_open(&self) -> bool {
        self.form.open.is_some()
            || self.member_modal.is_some()
            || self.details.is_some()
            || self.notify_dialog.is_some()
            || self.dm.purge.confirming()
            || self.export.is_some()
            || self.node.form.is_some()
            || self.update.settings_open
            || self.status.open
            || self.background.as_ref().is_some_and(|b| b.open)
            || self.dnd.open
            || self.overview
            || self.shortcuts_open
    }

    /// The chord arrived at `now` (see [`CHORD_QUIET`]): opens or closes the switcher unless it
    /// follows another chord too closely. Returns whether the key was used.
    pub fn switcher_chord(&mut self, now: std::time::Instant) -> bool {
        if !chord_gate(&mut self.chord_at, now) {
            // A repeat of a held key: used up, so it reaches no text field.
            return true;
        }
        self.toggle_switcher()
    }

    /// Tests: set when the chord last arrived, so a test never depends on the real clock (the
    /// chord ignores a second press inside [`CHORD_QUIET`]).
    #[cfg(test)]
    pub fn pin_chord_time(&mut self, at: Option<std::time::Instant>) {
        self.chord_at = at;
    }

    /// Open the switcher, or close it when it is open. Returns whether the key was used: it is
    /// not while another dialog is up, or when there are no rooms to switch between.
    pub fn toggle_switcher(&mut self) -> bool {
        if self.switcher.is_some() {
            self.switcher = None;
            return true;
        }
        if self.other_dialog_open() || self.rooms().is_empty() {
            return false;
        }
        // What was open over the conversation (not a dialog) gives way.
        self.emoji_open = false;
        self.picker = None;
        self.members_overlay = false;
        self.drop_search();
        // The box takes the keyboard: the composer must not keep it, or typed text would go to
        // both.
        self.composer.focused = false;
        self.switcher = Some(Switcher::new());
        true
    }

    pub fn close_switcher(&mut self) {
        self.switcher = None;
    }

    /// The query was edited: the highlight starts again at the top, whatever it was moved to
    /// and whatever the query is now (an edit away and back does not bring it back).
    pub fn switcher_edited(&mut self) {
        if let Some(sw) = &mut self.switcher {
            sw.sel = None;
        }
    }

    /// The results for what is typed now.
    pub fn switcher_hits(&self) -> Vec<Hit> {
        let Some(sw) = &self.switcher else {
            return Vec::new();
        };
        let rooms: Vec<RoomItem> = self
            .rooms()
            .iter()
            .map(|r| RoomItem {
                key: &r.key,
                name: &r.name,
                unread: self.unread_of(r),
            })
            .collect();
        let members: Vec<MemberItem> = self
            .room()
            .map(|r| {
                r.members
                    .iter()
                    .map(|m| MemberItem {
                        uid: &m.uid,
                        name: &m.nickname,
                    })
                    .collect()
            })
            .unwrap_or_default();
        search(&sw.query.text, &rooms, &members)
    }

    /// The highlighted row for `hits` (as [`Chat::switcher_hits`] returned them): where the
    /// highlighted target is now, or the top row when it is not listed.
    pub fn switcher_selected(&self, hits: &[Hit]) -> usize {
        self.switcher
            .as_ref()
            .and_then(|sw| sw.sel.as_ref())
            .and_then(|t| hits.iter().position(|h| &h.target == t))
            .unwrap_or(0)
    }

    /// Highlight the result that opens `target`.
    pub fn switcher_highlight(&mut self, target: Target) {
        if let Some(sw) = &mut self.switcher {
            sw.sel = Some(target);
        }
    }

    /// Move the highlight by `delta` rows, wrapping round.
    pub fn switcher_move(&mut self, delta: isize) {
        let hits = self.switcher_hits();
        if hits.is_empty() {
            return;
        }
        let n = hits.len() as isize;
        let next = (self.switcher_selected(&hits) as isize + delta).rem_euclid(n);
        self.switcher_highlight(hits[next as usize].target.clone());
    }

    /// Open `target` and close the switcher. A click carries the target of the row it landed
    /// on, so it opens what was drawn there even if the list has since changed.
    pub fn switcher_choose(&mut self, target: &Target) {
        // Closed first: opening a member's dialog must not find another dialog up, and nothing
        // below should see the switcher still open.
        self.switcher = None;
        match target {
            Target::Room(key) => {
                self.open_room(key);
            }
            Target::Member(uid) => self.open_member(uid),
        }
    }

    /// Open the highlighted row (Enter). With nothing listed it does nothing.
    pub fn switcher_choose_selected(&mut self) {
        // The hits are read while the switcher is still open; `switcher_choose` closes it.
        let hits = self.switcher_hits();
        let i = self.switcher_selected(&hits);
        if let Some(hit) = hits.get(i) {
            self.switcher_choose(&hit.target);
        }
    }

    /// Keys in the switcher's box: Up and Down move, Escape closes it. The chord is handled by
    /// the caller through [`Chat::switcher_chord`]. Enter is the box's submit. Returns whether
    /// the key was used.
    pub fn switcher_key(&mut self, ev: &KeyEvent) -> bool {
        if self.switcher.is_none() {
            return false;
        }
        let m = ev.mods;
        let bare = !(m.shift || m.ctrl || m.alt || m.logo);
        match ev.key {
            Key::Down if bare => self.switcher_move(1),
            Key::Up if bare => self.switcher_move(-1),
            Key::Escape => self.switcher = None,
            _ => return false,
        }
        true
    }
}

/// Whether `ev` is the switcher's chord: Ctrl+K, or Cmd+K on macOS (rune reports both as
/// `Command('k')`), without Shift or Alt.
pub fn is_chord(ev: &KeyEvent) -> bool {
    ev.is_command('k') && !ev.mods.shift && !ev.mods.alt
}

/// The switcher as a dialog, when open.
pub(super) fn switcher_dialog(chat: &Chat, k: &Kit, layer: &mut Vec<El>) {
    let Some(sw) = &chat.switcher else {
        return;
    };
    let p = st::palette();
    let hits = chat.switcher_hits();
    let sel = chat.switcher_selected(&hits);
    let get: Get = |s| &mut s.chat.switcher.as_mut().expect("open").query;
    let input = TextInput::new(&sw.query)
        .placeholder("Jump to a room or member")
        .width(None)
        .on_edit(get)
        .on_change(|s: &mut RiverApp, _| s.chat.switcher_edited())
        .on_key(word_end_after(get, |s: &mut RiverApp, ev, _| {
            if is_chord(ev) {
                // Taken here whatever it does, so it never also reaches the window's handler.
                s.chat.switcher_chord(std::time::Instant::now());
                return true;
            }
            s.chat.switcher_key(ev)
        }))
        .on_submit(|s: &mut RiverApp, _| s.chat.switcher_choose_selected())
        .build(k)
        .key("switcher-input")
        .w_full();
    let mut list = col().key("switcher-results").w_full().gap(2.0);
    if hits.is_empty() {
        list = list.child(
            label("No matching room or member", st::BODY, p.text_muted)
                .key("switcher-empty")
                .px(12.0)
                .py(10.0),
        );
    }
    let (start, shown) = window(sel, hits.len(), VISIBLE_ROWS);
    for (i, hit) in hits.iter().enumerate().skip(start).take(shown) {
        list = list.child(result_row(chat, hit, i, i == sel, &p));
    }
    if hits.len() > shown {
        list = list.child(
            label(
                format!("{} results. Keep typing to narrow them.", hits.len()),
                st::SMALL,
                p.text_muted,
            )
            .key("switcher-more")
            .px(12.0)
            .pt(6.0),
        );
    }
    DialogSpec::new("switcher", "Quick switcher", |s: &mut RiverApp| {
        s.chat.close_switcher()
    })
    .body_key("switcher-body")
    .width(480.0)
    .child(input)
    .child(list)
    .build(k, layer);
}

fn result_row(chat: &Chat, hit: &Hit, i: usize, selected: bool, p: &Palette) -> El {
    let (name, lead, tag, unread) = match &hit.target {
        Target::Room(key) => {
            let Some(room) = chat.rooms().iter().find(|r| &r.key == key) else {
                // Hits are built from these same rooms in this frame, so a row is never
                // dangling; draw nothing rather than panic if that ever changes.
                debug_assert!(false, "a switcher hit names a room that is gone");
                return el().key(format!("switcher-row-{i}"));
            };
            (room.name.clone(), None, "Room", chat.unread_of(room))
        }
        Target::Member(uid) => {
            let Some(m) = chat.room().and_then(|r| r.member_by_uid(uid)) else {
                debug_assert!(false, "a switcher hit names a member who is gone");
                return el().key(format!("switcher-row-{i}"));
            };
            (
                m.nickname.clone(),
                Some(avatar(Some(m), m.id, 24.0)),
                "Member",
                0,
            )
        }
    };
    let target = hit.target.clone();
    let mut r = row()
        .key(format!("switcher-row-{i}"))
        .role("button")
        .label(format!("{tag}: {name}"))
        .w_full()
        .h(40.0)
        .px(12.0)
        .gap(10.0)
        .center_y()
        .radius(6.0)
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut RiverApp| s.chat.switcher_choose(&target));
    if let Some(lead) = lead {
        r = r.child(lead);
    }
    r = r.child(
        label(
            name,
            if unread > 0 {
                st::ROOM_STRONG
            } else {
                st::ROOM
            },
            p.text,
        )
        .truncate()
        .grow(1.0)
        .min_w(0.0),
    );
    if unread > 0 {
        r = r.child(
            el().key("unread")
                .h(18.0)
                .min_w(20.0)
                .px(6.0)
                .radius(9.0)
                .center()
                .fill(p.accent_soft)
                .child(label(unread.to_string(), st::BADGE, p.accent)),
        );
    }
    r = r.child(label(tag, st::SMALL, p.text_muted));
    if selected {
        r.fill(p.surface_hover)
    } else {
        r.hover(p.row_hover)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rooms<'a>(v: &[(&'a str, &'a str, usize)]) -> Vec<RoomItem<'a>> {
        v.iter()
            .map(|&(key, name, unread)| RoomItem { key, name, unread })
            .collect()
    }

    fn members<'a>(v: &[(&'a str, &'a str)]) -> Vec<MemberItem<'a>> {
        v.iter()
            .map(|&(uid, name)| MemberItem { uid, name })
            .collect()
    }

    fn targets(hits: &[Hit]) -> Vec<String> {
        hits.iter()
            .map(|h| match &h.target {
                Target::Room(k) => format!("room:{k}"),
                Target::Member(u) => format!("member:{u}"),
            })
            .collect()
    }

    #[test]
    fn a_blank_query_matches_everything() {
        assert_eq!(match_score("", "Anything"), Some(0));
        assert_eq!(match_score("   ", "Anything"), Some(0));
    }

    #[test]
    fn matching_ignores_case_and_surrounding_space() {
        assert!(match_score("  RUST ", "rust help").is_some());
        assert!(match_score("CAFÉ", "the café").is_some());
    }

    #[test]
    fn the_kinds_of_match_rank_in_order() {
        let whole = match_score("rust", "Rust").unwrap();
        let prefix = match_score("rust", "Rust help").unwrap();
        let word = match_score("help", "Rust help").unwrap();
        let inside = match_score("ust", "Rust help").unwrap();
        let fuzzy = match_score("rsh", "Rust help").unwrap();
        assert!(whole > prefix, "{whole} {prefix}");
        assert!(prefix > word, "{prefix} {word}");
        assert!(word > inside, "{word} {inside}");
        assert!(inside > fuzzy, "{inside} {fuzzy}");
    }

    #[test]
    fn a_word_start_beats_a_middle_of_word_hit_earlier_in_the_text() {
        // "ar" appears inside "Barn" first, and starts the word "area".
        let a = match_score("ar", "Barn area").unwrap();
        let b = match_score("ar", "Barn").unwrap();
        assert!(a > b);
    }

    #[test]
    fn fuzzy_needs_every_character_in_order() {
        assert!(match_score("fnt", "Freenet talk").is_some());
        assert!(match_score("tnf", "Freenet talk").is_none());
        assert!(match_score("zzz", "Freenet talk").is_none());
        assert!(match_score("longer than text", "short").is_none());
    }

    #[test]
    fn fuzzy_prefers_characters_that_follow_each_other() {
        let tight = match_score("abd", "abxd").unwrap();
        let loose = match_score("abd", "axbxd").unwrap();
        assert!(tight > loose, "{tight} {loose}");
    }

    #[test]
    fn an_empty_query_puts_unread_rooms_first_then_the_rest_then_members() {
        let r = rooms(&[
            ("a", "Alpha", 0),
            ("b", "Beta", 2),
            ("c", "Gamma", 0),
            ("d", "Delta", 5),
        ]);
        let m = members(&[("2", "zed"), ("1", "Amy")]);
        let hits = search("", &r, &m);
        assert_eq!(
            targets(&hits),
            [
                "room:d", "room:b", "room:a", "room:c", "member:1", "member:2"
            ]
        );
    }

    #[test]
    fn equal_unread_keeps_the_room_lists_order() {
        let r = rooms(&[("a", "Alpha", 1), ("b", "Beta", 1), ("c", "Gamma", 1)]);
        assert_eq!(
            targets(&search("", &r, &[])),
            ["room:a", "room:b", "room:c"]
        );
    }

    #[test]
    fn a_query_finds_rooms_and_members_best_first() {
        let r = rooms(&[
            ("r1", "Rust help", 0),
            ("r2", "Garden", 0),
            ("r3", "Trust", 0),
        ]);
        let m = members(&[("7", "Rusty"), ("8", "Bo")]);
        let hits = search("rust", &r, &m);
        // Prefix matches first (the shorter name scores higher), the mid-word hit last.
        assert_eq!(targets(&hits), ["member:7", "room:r1", "room:r3"]);
        assert!(hits[0].score > 0);
    }

    #[test]
    fn a_room_comes_before_a_member_with_the_same_score() {
        let r = rooms(&[("r", "Sam", 0)]);
        let m = members(&[("1", "Sam")]);
        assert_eq!(targets(&search("sam", &r, &m)), ["room:r", "member:1"]);
    }

    #[test]
    fn among_equal_scores_unread_rooms_come_first() {
        let r = rooms(&[("a", "Dev one", 0), ("b", "Dev two", 3)]);
        assert_eq!(targets(&search("dev", &r, &[])), ["room:b", "room:a"]);
    }

    #[test]
    fn nothing_matching_is_an_empty_list() {
        let r = rooms(&[("a", "Alpha", 0)]);
        assert!(search("qqq", &r, &members(&[("1", "Bo")])).is_empty());
    }

    #[test]
    fn results_are_capped() {
        let names: Vec<String> = (0..200).map(|i| format!("Room {i}")).collect();
        let r: Vec<RoomItem> = names
            .iter()
            .map(|n| RoomItem {
                key: n,
                name: n,
                unread: 0,
            })
            .collect();
        assert_eq!(search("", &r, &[]).len(), MAX_HITS);
        assert_eq!(search("room", &r, &[]).len(), MAX_HITS);
    }

    #[test]
    fn the_window_keeps_the_highlight_inside() {
        assert_eq!(window(0, 3, 8), (0, 3));
        assert_eq!(window(0, 20, 8), (0, 8));
        assert_eq!(window(7, 20, 8), (0, 8));
        assert_eq!(window(8, 20, 8), (1, 8));
        assert_eq!(window(19, 20, 8), (12, 8));
        assert_eq!(window(0, 0, 8), (0, 0));
    }

    #[test]
    fn one_and_two_characters_match_only_as_a_prefix_word_start_or_substring() {
        // "rh" is in order in "Rust help" but is neither a prefix nor a substring.
        assert!(match_score("rh", "Rust help").is_none());
        assert!(match_score("rsh", "Rust help").is_some());
        assert!(match_score("he", "Rust help").is_some(), "a word start");
        assert!(match_score("us", "Rust help").is_some(), "a substring");
        assert!(match_score("r", "Rust help").is_some(), "a prefix");
    }

    #[test]
    fn cjk_names_match_by_character() {
        assert!(match_score("日本", "日本語の部屋").is_some());
        assert!(match_score("部屋", "日本語の部屋").is_some());
        assert!(
            match_score("語部", "日本語の部屋").is_none(),
            "two characters do not go fuzzy"
        );
        assert!(match_score("日語屋", "日本語の部屋").is_some(), "three do");
        assert!(match_score("韓国", "日本語の部屋").is_none());
    }

    #[test]
    fn decomposed_accents_match_a_plain_query_and_each_other() {
        let decomposed = "Cafe\u{301} Nord";
        assert!(match_score("cafe", decomposed).is_some());
        assert!(match_score("cafe\u{301}", decomposed).is_some());
        assert_eq!(
            match_score("cafe", decomposed),
            match_score("cafe", "Cafe Nord")
        );
        // Documented limit: no normalisation, so precomposed and plain do not meet.
        assert!(match_score("caf\u{e9}", decomposed).is_none());
        assert!(match_score("cafe", "Caf\u{e9} Nord").is_none());
        assert!(match_score("caf\u{e9}", "Caf\u{e9} Nord").is_some());
    }

    #[test]
    fn the_chord_acts_once_for_a_held_key_and_again_after_a_quiet_spell() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let mut last = None;
        assert!(chord_gate(&mut last, t0), "the press");
        // Repeats every 30 ms from a 500 ms start-up delay, held for two seconds.
        let mut acted = 0;
        let mut t = Duration::from_millis(500);
        while t < Duration::from_millis(2500) {
            acted += chord_gate(&mut last, t0 + t) as u32;
            t += Duration::from_millis(30);
        }
        assert_eq!(acted, 0, "a held key never acts again");
        // Released, then pressed after a quiet spell.
        assert!(chord_gate(&mut last, t0 + t + CHORD_QUIET));
    }
}
