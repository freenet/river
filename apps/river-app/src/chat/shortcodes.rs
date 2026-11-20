//! `:shortcode:` autocomplete in the composer: typing `:sm` at the start of a word lists the
//! emoji whose name contains it, and picking one writes the emoji. Typing the closing colon of a
//! complete name (`:smile:`) writes the emoji at once.
//!
//! The emoji offered are the ones the picker lists (`emoji.rs`, the web UI's list); this module
//! only adds their names, taken from the `emojis` crate (GitHub's shortcodes). The popup is
//! worked out from the composer's text and caret each time the screen is drawn, like the
//! `@mention` popup (`mention.rs`), so it closes by itself when the caret leaves the word. Only
//! the highlighted row and an Escape are remembered.
//!
//! Not completed: a colon that does not start a word (`10:30`, `http://`, `a:b`), `::`, anything
//! inside a code span or a fenced code block, and a query under [`MIN_QUERY`] characters.

use std::sync::OnceLock;

use rune::{Key, KeyEvent};

use super::Chat;
use super::emoji::EMOJI_CATEGORIES;

/// Most emoji listed.
pub const LIMIT: usize = 8;

/// Fewest characters after the colon before the popup opens: `:s` would match half the table.
pub const MIN_QUERY: usize = 2;

/// An emoji and one of its names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: &'static str,
    pub glyph: &'static str,
}

/// Every name of every emoji in the picker's list, sorted by name. Built once.
fn table() -> &'static [Entry] {
    static TABLE: OnceLock<Vec<Entry>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut v: Vec<Entry> = EMOJI_CATEGORIES
            .iter()
            .flat_map(|(_, _, glyphs)| glyphs.iter().copied())
            .filter_map(|glyph| emojis::get(glyph).map(|e| (glyph, e)))
            .flat_map(|(glyph, e)| e.shortcodes().map(move |name| Entry { name, glyph }))
            .collect();
        v.sort_by(|a, b| a.name.cmp(b.name).then(a.glyph.cmp(b.glyph)));
        v.dedup();
        v
    })
}

/// Whether `c` can be in a shortcode name.
fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-')
}

/// Whether byte offset `pos` of `text` is inside a code span or a fenced code block, so a colon
/// there is code, not an emoji.
pub fn in_code(text: &str, pos: usize) -> bool {
    let mut fenced = false;
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let end = start + line.len();
        if line.trim_start().starts_with("```") {
            // The fence line itself is code too.
            if pos < end {
                return true;
            }
            fenced = !fenced;
        } else if pos < end {
            if fenced {
                return true;
            }
            // Backtick runs on this line before `pos` open and close spans.
            let mut open = false;
            let mut prev_tick = false;
            for c in line[..pos - start].chars() {
                let tick = c == '`';
                if tick && !prev_tick {
                    open = !open;
                }
                prev_tick = tick;
            }
            return open;
        }
        start = end;
    }
    fenced
}

/// If the caret sits in a `:query` (a colon at the start of a word, only name characters
/// between it and the caret, at least [`MIN_QUERY`] of them, not in code), the byte offset of
/// the colon and the query.
pub fn detect(text: &str, caret: usize) -> Option<(usize, &str)> {
    if caret > text.len() || !text.is_char_boundary(caret) {
        return None;
    }
    let prefix = &text[..caret];
    let mut colon = None;
    for (i, c) in prefix.char_indices().rev() {
        if c == ':' {
            colon = Some(i);
            break;
        }
        if !name_char(c) {
            return None;
        }
    }
    let colon = colon?;
    // Start of a word: this rules out `10:30`, `http://x`, `a:b` and `::`.
    if let Some(before) = prefix[..colon].chars().next_back()
        && !before.is_whitespace()
    {
        return None;
    }
    let query = &prefix[colon + 1..];
    if query.chars().count() < MIN_QUERY || in_code(text, colon) {
        return None;
    }
    Some((colon, query))
}

/// The emoji `query` offers, at most [`LIMIT`]: a name that is the query first, then names that
/// start with it, then names that contain it, each shorter name first. An emoji with several
/// names is listed once, under the best one.
pub fn matches(query: &str) -> Vec<Entry> {
    let q = query.to_ascii_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<(u8, usize, Entry)> = table()
        .iter()
        .filter_map(|e| {
            let rank = if e.name == q {
                0
            } else if e.name.starts_with(&q) {
                1
            } else if e.name.contains(&q) {
                2
            } else {
                return None;
            };
            Some((rank, e.name.len(), *e))
        })
        .collect();
    // Stable: equal rank and length stay in name order.
    found.sort_by_key(|(rank, len, _)| (*rank, *len));
    let mut out: Vec<Entry> = Vec::new();
    for (_, _, e) in found {
        if out.iter().all(|o| o.glyph != e.glyph) {
            out.push(e);
        }
        if out.len() == LIMIT {
            break;
        }
    }
    out
}

/// The emoji named exactly `name`, if the picker lists it.
pub fn exact(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    table()
        .binary_search_by(|e| e.name.cmp(name.as_str()))
        .ok()
        .map(|i| table()[i].glyph)
}

/// What the popup remembers between frames.
#[derive(Default)]
pub struct State {
    /// The highlighted row, and the query it was moved under (a new query starts at the top).
    sel: (String, usize),
    /// An Escape closed the popup for the colon at this offset, while what is typed still
    /// starts with this.
    off: Option<(usize, String)>,
}

/// The open popup.
#[derive(Clone, Debug, PartialEq)]
pub struct Popup {
    /// Byte offset of the colon in the composer text.
    pub anchor: usize,
    pub query: String,
    pub entries: Vec<Entry>,
    pub selected: usize,
}

impl Chat {
    fn shortcode_closed(&self, anchor: usize, query: &str) -> bool {
        self.shortcode
            .off
            .as_ref()
            .is_some_and(|(a, q)| *a == anchor && query.starts_with(q.as_str()))
    }

    /// Forget an Escape once its word is gone. Run before each key.
    pub fn shortcode_sync(&mut self) {
        if self.shortcode.off.is_none() {
            return;
        }
        let c = &self.composer;
        let open = c.anchor == c.focus
            && detect(&c.text, c.focus).is_some_and(|(a, q)| self.shortcode_closed(a, q));
        if !open {
            self.shortcode.off = None;
        }
    }

    /// The popup the composer shows now, if any.
    pub fn shortcode_popup(&self) -> Option<Popup> {
        self.actionable()?;
        let c = &self.composer;
        if !c.focused || c.anchor != c.focus || c.preedit().is_some() {
            return None;
        }
        let (anchor, query) = detect(&c.text, c.focus)?;
        if self.shortcode_closed(anchor, query) {
            return None;
        }
        let entries = matches(query);
        if entries.is_empty() {
            return None;
        }
        let selected = match &self.shortcode.sel {
            (q, i) if q == query => (*i).min(entries.len() - 1),
            _ => 0,
        };
        Some(Popup {
            anchor,
            query: query.to_owned(),
            entries,
            selected,
        })
    }

    /// Write the emoji named `name` in place of the `:query`.
    pub fn shortcode_pick(&mut self, name: &str) {
        let Some(p) = self.shortcode_popup() else {
            return;
        };
        let Some(e) = p.entries.iter().find(|e| e.name == name) else {
            return;
        };
        self.composer.set_selection(p.anchor, self.composer.focus);
        self.composer.insert(e.glyph);
        self.focus_composer();
        self.shortcode.off = None;
    }

    /// Highlight row `index` (the pointer is over it).
    pub fn shortcode_highlight(&mut self, index: usize) {
        if let Some(p) = self.shortcode_popup() {
            self.shortcode.sel = (p.query, index);
        }
    }

    /// The closing colon of a complete name: `:smile` and a typed `:` become the emoji. Returns
    /// whether the key was used (the colon is then not typed).
    fn shortcode_close(&mut self, ev: &KeyEvent) -> bool {
        let Key::Text(t) = &ev.key else {
            return false;
        };
        let m = ev.mods;
        if t != ":" || m.ctrl || m.alt || m.cmd || m.logo {
            return false;
        }
        let c = &self.composer;
        if !c.focused || c.anchor != c.focus || c.preedit().is_some() || self.actionable().is_none() {
            return false;
        }
        let Some((anchor, query)) = detect(&c.text, c.focus) else {
            return false;
        };
        let Some(glyph) = exact(query) else {
            return false;
        };
        self.composer.set_selection(anchor, self.composer.focus);
        self.composer.insert(glyph);
        self.shortcode.off = None;
        true
    }

    /// Keys while typing a shortcode: Up and Down move, Enter and Tab pick, Escape closes, and a
    /// colon after a complete name converts it. Returns whether the key was used.
    pub fn shortcode_key(&mut self, ev: &KeyEvent) -> bool {
        if self.shortcode_close(ev) {
            return true;
        }
        let Some(p) = self.shortcode_popup() else {
            return false;
        };
        let n = p.entries.len();
        // Only the bare key: with Ctrl, Alt or Cmd or Shift it belongs to the composer.
        let m = ev.mods;
        let bare = !(m.shift || m.ctrl || m.alt || m.logo);
        match ev.key {
            Key::Down | Key::Up if bare => {
                let i = if ev.key == Key::Down {
                    (p.selected + 1) % n
                } else {
                    (p.selected + n - 1) % n
                };
                self.shortcode.sel = (p.query, i);
            }
            Key::Escape => self.shortcode.off = Some((p.anchor, p.query)),
            Key::Enter | Key::Tab if bare => {
                let name = p.entries[p.selected].name;
                self.shortcode_pick(name);
            }
            _ => return false,
        }
        true
    }
}

/// The popup, floating above the composer. It takes no focus, so typing carries on.
pub(super) fn shortcode_overlay(chat: &Chat, k: &rune_kit::Kit, layer: &mut Vec<rune::El>) {
    let Some(popup) = chat.shortcode_popup() else {
        return;
    };
    let items = popup
        .entries
        .iter()
        .map(|e| rune_kit::PopupItem::new(e.name.to_owned(), format!("{}  :{}:", e.glyph, e.name)));
    rune_kit::PopupList::new("shortcode-popup", "composer-box")
        .label("Insert an emoji")
        .width(280.0)
        .items(items)
        .active(Some(popup.selected))
        .on_active(|s: &mut crate::RiverApp, i| s.chat.shortcode_highlight(i))
        .on_choose(|s: &mut crate::RiverApp, name: &str, _: &mut rune::Tx| {
            s.chat.shortcode_pick(name)
        })
        .build(k, layer);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(q: &str) -> Vec<&'static str> {
        matches(q).iter().map(|e| e.name).collect()
    }

    #[test]
    fn a_colon_at_a_word_start_with_two_characters_opens() {
        assert_eq!(detect(":sm", 3), Some((0, "sm")));
        assert_eq!(detect("hello :sm", 9), Some((6, "sm")));
        assert_eq!(detect("a\n:smile", 8), Some((2, "smile")));
        assert_eq!(detect(":s", 2), None, "one character is too few");
        assert_eq!(detect(":", 1), None);
    }

    #[test]
    fn times_urls_pairs_and_double_colons_do_not_open() {
        assert_eq!(detect("meet at 10:30", 13), None);
        assert_eq!(detect("see http://example.com", 22), None);
        assert_eq!(detect("key:value", 9), None);
        assert_eq!(detect("a ::smile", 9), None);
        assert_eq!(detect("::smile", 7), None);
        assert_eq!(detect("a :sm ile", 9), None, "a space ends the word");
        assert_eq!(detect("a :sm.", 6), None);
    }

    #[test]
    fn code_spans_and_blocks_do_not_open() {
        assert_eq!(detect("`:smile", 7), None);
        assert_eq!(detect("run `x` then :smile", 19), Some((13, "smile")));
        assert_eq!(detect("``a :smile", 10), None);
        assert_eq!(detect("```\n:smile", 10), None);
        assert_eq!(detect("```\ncode\n```\n:smile", 19), Some((13, "smile")));
        assert_eq!(detect("```rust\nlet a = :smile", 22), None);
        // A span that was closed on an earlier line does not leak into the next.
        assert_eq!(detect("`a`\n:smile", 10), Some((4, "smile")));
    }

    #[test]
    fn the_caret_decides_not_the_end_of_the_text() {
        assert_eq!(detect(":smile later", 3), Some((0, "sm")));
        assert_eq!(detect(":smile later", 12), None);
        // Inside a multi-byte character: no panic, no popup.
        assert_eq!(detect("é:sm", 1), None);
    }

    #[test]
    fn matching_ranks_exact_then_prefix_then_contains_and_is_bounded() {
        let r = names("smile");
        assert_eq!(r[0], "smile", "an exact name is first");
        assert!(r.len() <= LIMIT);
        assert!(names("sm").len() <= LIMIT);
        assert!(names("sm").iter().all(|n| n.contains("sm")));
        let r = names("heart");
        assert!(r.len() <= LIMIT && !r.is_empty());
        // Prefix before substring.
        let first_contains = r.iter().position(|n| !n.starts_with("heart"));
        let last_prefix = r.iter().rposition(|n| n.starts_with("heart"));
        if let (Some(c), Some(p)) = (first_contains, last_prefix) {
            assert!(p < c, "{r:?}");
        }
        assert!(matches("").is_empty());
        assert!(matches("zzzzzzqq").is_empty());
        assert_eq!(names("SMILE")[0], "smile", "case does not matter");
    }

    #[test]
    fn an_emoji_with_several_names_is_listed_once() {
        let r = matches("thumbs");
        let mut glyphs: Vec<_> = r.iter().map(|e| e.glyph).collect();
        let n = glyphs.len();
        glyphs.sort_unstable();
        glyphs.dedup();
        assert_eq!(glyphs.len(), n, "{r:?}");
    }

    #[test]
    fn only_the_pickers_emoji_are_offered() {
        let all: Vec<&str> = EMOJI_CATEGORIES
            .iter()
            .flat_map(|(_, _, g)| g.iter().copied())
            .collect();
        for e in table() {
            assert!(all.contains(&e.glyph), "{} is not in the picker", e.glyph);
        }
        // Most of the picker has a name; a table that lost them all would offer nothing.
        let named = all
            .iter()
            .filter(|g| table().iter().any(|e| e.glyph == **g))
            .count();
        assert!(named * 10 > all.len() * 8, "{named} of {}", all.len());
    }

    #[test]
    fn a_complete_name_is_found_exactly() {
        assert_eq!(exact("smile"), Some("😄"));
        assert_eq!(exact("SMILE"), Some("😄"));
        assert_eq!(exact("smil"), None);
        assert_eq!(exact("not_an_emoji"), None);
    }
}
