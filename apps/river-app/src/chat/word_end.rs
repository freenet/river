//! End-of-word Ctrl+Right on Linux.
//!
//! Rune moves a word forward to the START of the next word everywhere but macOS (Windows' and,
//! since its 2026-11-12 word-boundary change, Linux's rule). GTK, Qt and Chromium on Linux stop
//! at the END of the current word. This gives Linux that for the two chords that only move the
//! caret, Ctrl+Right and Ctrl+Shift+Right, in every text field. Windows keeps Rune's start of
//! the next word and macOS its end of the word (Option+Right): `word_end` does nothing there.
//!
//! What is left to Rune on purpose:
//!
//! - Ctrl+Left and Ctrl+Backspace (the start of the word on every platform).
//! - Ctrl+Delete: Rune's word delete is recorded for undo as `EditKind::DeleteRun`, which is
//!   crate-private, so a delete made from here would join (or not join) later edits differently
//!   from every other field. It keeps deleting to the start of the next word.
//! - Alt+Right and Ctrl+Alt+Right (Rune's rule: the start of the next word).
//! - A non-extending Ctrl+Right with a selection: Rune collapses to the selection's end and
//!   does not move on, on every platform. Ctrl+Shift+Right extends the selection's focus end.
//! - Text with right-to-left characters, or a direction forced with Ctrl+Shift: Rune then moves
//!   by the line as drawn, which needs the field's layout, so Rune's rule applies there.
//! - An input method composition in progress.
//!
//! The end of the word is Rune's macOS rule, with its pills: skip what is not a word (UAX #29
//! segments with no letter or digit), then stop at the end of the next word segment, or at the
//! start of a mention pill inside it. Whole segments are used, so the caret never lands inside
//! a grapheme cluster (an accent, a keycap, an emoji with a joiner, a Thai tone mark).
//!
//! Limits: a key taken here skips Rune's follow-up for a key (it scrolls the caret into view), so
//! in a field that scrolls the caret may stay out of view until the next key.

use std::ops::Range;

use rune::{Key, KeyEvent, TextEdit, Tx};
use unicode_segmentation::UnicodeSegmentation;

use crate::RiverApp;

/// How a field's `TextEdit` is reached from the app, as `on_edit` takes it.
pub(crate) type Get = fn(&mut RiverApp) -> &mut TextEdit;

/// Segments one press walks before giving up and leaving the key to Rune: far past any real
/// run of punctuation, so a megabyte of `.` costs a bounded amount per key press.
const MAX_SEGMENTS: usize = 4096;

/// The key handler of a field whose `TextEdit` is `get`: Ctrl+Right and Ctrl+Shift+Right on
/// Linux (see the module comment). Always `false` elsewhere. For a field with a key handler of
/// its own, use [`word_end_after`].
pub(crate) fn word_end(get: Get) -> impl Fn(&mut RiverApp, &KeyEvent, &mut Tx) -> bool + 'static {
    move |s, ev, _| is_word_right(ev) && word_end_key(get(s), ev)
}

/// [`word_end`] after `own`, the field's own key handler, which keeps the keys it takes.
pub(crate) fn word_end_after(
    get: Get,
    own: impl Fn(&mut RiverApp, &KeyEvent, &mut Tx) -> bool + 'static,
) -> impl Fn(&mut RiverApp, &KeyEvent, &mut Tx) -> bool + 'static {
    move |s, ev, tx| own(s, ev, tx) || (is_word_right(ev) && word_end_key(get(s), ev))
}

/// Ctrl+Right or Ctrl+Shift+Right, no Alt and no Super. Off Linux never: the callers then skip
/// the field lookup too.
fn is_word_right(ev: &KeyEvent) -> bool {
    let m = ev.mods;
    cfg!(target_os = "linux") && ev.key == Key::Right && m.ctrl && !m.alt && !m.logo
}

/// Applies Ctrl+Right (`ev.mods.shift` unset) or Ctrl+Shift+Right to `edit`: whether it did. The
/// caller has checked [`is_word_right`].
fn word_end_key(edit: &mut TextEdit, ev: &KeyEvent) -> bool {
    let extend = ev.mods.shift;
    if edit.is_secure() || edit.preedit().is_some() || edit.forced_dir().is_some() {
        return false;
    }
    // A selection collapses to its end, as Rune does on every platform.
    if !extend && edit.anchor != edit.focus {
        return false;
    }
    if has_rtl(&edit.text) {
        return false;
    }
    let Some(to) = forward_word_end(edit, edit.focus) else {
        return false;
    };
    // Rune closes the open undo step on every key but typing and deleting, so typing after the
    // move is a step of its own. `set_caret` and `set_selection` also reset the column a run of
    // Up and Down keeps, the caret's side, and the pending typing style.
    edit.break_history_group();
    if extend {
        edit.set_selection(edit.anchor, to);
    } else {
        edit.set_caret(to);
    }
    true
}

/// Where a word forward from byte `from` ends in `edit`'s text: Rune's rule on macOS. `None`
/// when `from` is not a character boundary or the walk is too long to do here.
pub(crate) fn forward_word_end(edit: &TextEdit, from: usize) -> Option<usize> {
    let s = edit.text.as_str();
    if from > s.len() || !s.is_char_boundary(from) {
        return None;
    }
    let pills: Vec<Range<usize>> = edit.pills().map(|(r, _)| r).collect();
    let len = s.len();
    let mut j = from;
    // The end of the line `j` is on, found again only after a line break.
    let mut line_end = None::<usize>;
    for _ in 0..MAX_SEGMENTS {
        if let Some(p) = pills.iter().find(|p| p.start <= j && j < p.end) {
            return Some(p.end);
        }
        if j >= len {
            return Some(len);
        }
        let end_of_line = match line_end {
            Some(e) if e >= j => e,
            _ => {
                let e = s[j..].find('\n').map_or(len, |k| j + k);
                line_end = Some(e);
                e
            }
        };
        if end_of_line == j {
            // A line break is a segment of its own, and no word.
            j += s[j..].graphemes(true).next().map_or(1, str::len);
            continue;
        }
        // Rune cuts a segment from `j` itself, not from the line's start.
        let seg = s[j..end_of_line].split_word_bounds().next()?;
        let end = j + seg.len();
        if !seg.chars().any(char::is_alphanumeric) {
            j = end;
            continue;
        }
        // A word runs up to a pill at most.
        let clipped = pills
            .iter()
            .filter(|p| j < p.start && p.start < end)
            .map(|p| p.start)
            .min();
        return Some(clipped.unwrap_or(end));
    }
    None
}

/// Whether `text` holds a character that runs right to left (Hebrew, Arabic, Syriac, Thaana, N'Ko
/// and the other right-to-left blocks, their presentation forms, and the explicit marks and
/// embeddings). A little wider than the characters Unicode gives the class, which only means
/// Rune's rule applies a little more often.
pub(crate) fn has_rtl(text: &str) -> bool {
    text.chars().any(is_rtl)
}

fn is_rtl(c: char) -> bool {
    if c < '\u{0590}' {
        return false;
    }
    matches!(c,
        '\u{0590}'..='\u{08FF}'
        | '\u{200F}' | '\u{202B}' | '\u{202E}' | '\u{2067}'
        | '\u{FB1D}'..='\u{FDFF}'
        | '\u{FE70}'..='\u{FEFF}'
        | '\u{10800}'..='\u{10FFF}'
        | '\u{1E800}'..='\u{1EFFF}')
}

#[cfg(test)]
mod tests {
    use rune::{Ime, KeyEvent, Mods};

    use super::*;

    const LINE: &str = "see @maya.chen http://x.org/a";

    fn right(shift: bool) -> KeyEvent {
        KeyEvent {
            key: Key::Right,
            mods: Mods {
                shift,
                cmd: true,
                ctrl: true,
                ..Mods::default()
            },
        }
    }

    fn typed(edit: &mut TextEdit, s: &str) {
        edit.key(&KeyEvent::from(Key::Text(s.into())), None);
    }

    /// Every stop going forward from the start.
    fn stops(text: &str) -> Vec<usize> {
        let edit = TextEdit::new(text);
        let mut out = Vec::new();
        let mut at = 0;
        while at < text.len() {
            at = forward_word_end(&edit, at).expect("a stop");
            out.push(at);
        }
        out
    }

    /// The stops are Rune's macOS rule: the end of each Unicode word, past the punctuation
    /// between them. `@maya.chen` and `x.org` are one word each; `@`, `://` and `/` are not.
    #[test]
    fn the_stops_are_the_ends_of_unicode_words() {
        assert_eq!(stops(LINE), [3, 14, 19, 27, 29]);
        // Written out: see|, @maya.chen|, http|, ://x.org|, /a|.
        assert_eq!(&LINE[..3], "see");
        assert_eq!(&LINE[..14], "see @maya.chen");
        assert_eq!(&LINE[..19], "see @maya.chen http");
        assert_eq!(&LINE[..27], "see @maya.chen http://x.org");
        assert_eq!(stops("one  two"), [3, 8]);
        assert_eq!(stops("  lead"), [6]);
        assert_eq!(stops("end  "), [3, 5]);
        assert_eq!(stops("a\nb"), [1, 3]);
        assert_eq!(stops(""), Vec::<usize>::new());
    }

    /// A stop never falls inside a grapheme cluster, from any place the caret can start.
    #[test]
    fn a_stop_never_splits_a_grapheme_cluster() {
        let texts = [
            "cafe\u{301} au",                                  // e + combining acute
            "1\u{FE0F}\u{20E3} 2\u{FE0F}\u{20E3}",             // keycaps
            "x \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} y", // ZWJ family
            "\u{1F44D}\u{1F3FD} ok",                           // skin tone
            "\u{0E2A}\u{0E27}\u{0E31}\u{0E2A}\u{0E14}\u{0E35} \u{0E04}\u{0E23}\u{0E31}\u{0E1A}\u{0E48}", // Thai with tone marks
            "na\u{EF}ve r\u{E9}sum\u{E9} \u{1F1EB}\u{1F1F7} flag",
            "a\u{200D}b e\u{FE0F} z",
        ];
        for text in texts {
            let bounds: Vec<usize> = text
                .grapheme_indices(true)
                .map(|(i, _)| i)
                .chain([text.len()])
                .collect();
            for &from in &bounds {
                let mut edit = TextEdit::new(text);
                edit.set_caret(from);
                let to = forward_word_end(&edit, from).expect("a stop");
                assert!(
                    bounds.contains(&to),
                    "{text:?} from {from}: {to} is inside a cluster"
                );
                assert!(to >= from);
            }
        }
        // The cases that broke the first attempt, by value.
        assert_eq!(
            stops("cafe\u{301} au"),
            ["cafe\u{301}".len(), "cafe\u{301} au".len()]
        );
        assert_eq!(stops("1\u{FE0F}\u{20E3} x"), ["1\u{FE0F}\u{20E3}".len(), 9]);
    }

    #[test]
    fn a_mention_pill_is_a_word_of_its_own() {
        let mut edit = TextEdit::new("see ");
        edit.insert_pill("rv:1", "@Maya");
        typed(&mut edit, " now");
        assert_eq!(edit.text, "see @Maya now");
        // see| -> the pill's end -> now|
        assert_eq!(stops_of(&edit), [3, 9, 13]);
        // A word runs up to a pill at most.
        let mut edit = TextEdit::new("go");
        edit.insert_pill("rv:1", "@Maya");
        assert_eq!(edit.text, "go@Maya");
        assert_eq!(forward_word_end(&edit, 0), Some(2));
    }

    fn stops_of(edit: &TextEdit) -> Vec<usize> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < edit.text.len() {
            at = forward_word_end(edit, at).expect("a stop");
            out.push(at);
        }
        out
    }

    /// Long text is bounded: a long run of punctuation is left to Rune, a long word is one
    /// segment, and many lines cost only the lines walked.
    #[test]
    fn long_text_costs_a_bounded_walk() {
        let dots = TextEdit::new(".".repeat(1_000_000));
        assert_eq!(forward_word_end(&dots, 0), None);
        let word = TextEdit::new("a".repeat(1_000_000));
        assert_eq!(forward_word_end(&word, 0), Some(1_000_000));
        let lines = TextEdit::new("\n".repeat(100_000) + "end");
        assert_eq!(forward_word_end(&lines, 0), None);
        let text = "word ".repeat(200_000);
        let edit = TextEdit::new(text);
        assert_eq!(forward_word_end(&edit, 0), Some(4));
    }

    #[test]
    fn right_to_left_text_is_found() {
        assert!(has_rtl("\u{5E9}\u{5DC}\u{5D5}\u{5DD}")); // Hebrew
        assert!(has_rtl("hi \u{645}\u{631}\u{62D}\u{628}\u{627}")); // Arabic
        assert!(has_rtl("\u{FB1D}")); // a presentation form
        assert!(has_rtl("a\u{200F}b")); // a right-to-left mark
        assert!(!has_rtl(
            "plain text, caf\u{E9}, \u{65E5}\u{672C}\u{8A9E}, \u{E01}\u{E32}, \u{1F600}"
        ));
        assert!(!has_rtl(""));
    }

    #[test]
    fn it_moves_the_caret_to_the_end_of_the_word() {
        let mut edit = TextEdit::new(LINE);
        edit.set_caret(0);
        assert!(word_end_key(&mut edit, &right(false)));
        assert_eq!((edit.anchor, edit.focus), (3, 3));
        assert!(word_end_key(&mut edit, &right(false)));
        assert_eq!((edit.anchor, edit.focus), (14, 14));
    }

    #[test]
    fn shift_extends_from_the_focus_and_keeps_the_anchor() {
        let mut edit = TextEdit::new(LINE);
        edit.set_selection(4, 5);
        assert!(word_end_key(&mut edit, &right(true)));
        assert_eq!((edit.anchor, edit.focus), (4, 14));
        // A backwards selection shrinks from its focus: the caret end moves, the anchor stays.
        edit.set_selection(14, 4);
        assert!(word_end_key(&mut edit, &right(true)));
        assert_eq!((edit.anchor, edit.focus), (14, 14));
    }

    /// Ctrl+Right with a selection collapses it to its end and goes no further, as Rune does
    /// on every platform, so the key is Rune's.
    #[test]
    fn a_selection_collapses_to_its_end_by_runes_rule() {
        for (anchor, focus) in [(0, 3), (3, 0)] {
            let mut edit = TextEdit::new(LINE);
            edit.set_selection(anchor, focus);
            assert!(!word_end_key(&mut edit, &right(false)));
            assert_eq!((edit.anchor, edit.focus), (anchor, focus), "untouched");
            edit.key(&right(false), None);
            assert_eq!(
                (edit.anchor, edit.focus),
                (3, 3),
                "Rune collapses to the end"
            );
        }
    }

    #[test]
    fn right_to_left_text_is_left_to_rune() {
        for text in [
            "\u{5E9}\u{5DC}\u{5D5}\u{5DD} abc",
            "abc \u{645}\u{631}\u{62D}\u{628}\u{627}",
        ] {
            let mut edit = TextEdit::new(text);
            edit.set_caret(0);
            assert!(!word_end_key(&mut edit, &right(false)), "{text:?}");
            assert!(!word_end_key(&mut edit, &right(true)), "{text:?}");
            assert_eq!((edit.anchor, edit.focus), (0, 0));
        }
    }

    #[test]
    fn a_composition_is_left_alone() {
        let mut edit = TextEdit::new("one two");
        edit.set_caret(0);
        edit.ime(&Ime::Preedit(
            "\u{304B}\u{3093}".into(),
            Some((0, 6)),
            Vec::new(),
        ));
        assert!(edit.preedit().is_some());
        assert!(!word_end_key(&mut edit, &right(false)));
        assert!(!word_end_key(&mut edit, &right(true)));
        assert!(edit.preedit().is_some());
    }

    /// Rune closes the open undo step on a caret move, so typing before and after one are two
    /// steps; the move made here must too.
    #[test]
    fn typing_after_the_move_is_an_undo_step_of_its_own() {
        let mut edit = TextEdit::new("one two");
        edit.set_caret(0);
        typed(&mut edit, "X");
        assert!(word_end_key(&mut edit, &right(false)));
        typed(&mut edit, "Y");
        assert_eq!(edit.text, "XoneY two");
        edit.undo();
        assert_eq!(edit.text, "Xone two");
        edit.undo();
        assert_eq!(edit.text, "one two");
        // As Rune's own caret move does.
        let mut rune = TextEdit::new("one two");
        rune.set_caret(0);
        typed(&mut rune, "X");
        rune.key(&KeyEvent::from(Key::Right), None);
        typed(&mut rune, "Y");
        rune.undo();
        assert_eq!(rune.text, "Xone two");
    }

    /// Edits join the open step only when they touch the last one, so the break is seen where
    /// the caret does not move (at the end of the text): typing on both sides of the key is
    /// two steps, as with Rune's own Ctrl+Right there. Without the break it would be one.
    #[test]
    fn a_move_that_stays_put_still_closes_the_undo_step() {
        let mut edit = TextEdit::new("one");
        edit.set_caret(3);
        typed(&mut edit, "X");
        assert!(word_end_key(&mut edit, &right(false)));
        typed(&mut edit, "Y");
        assert_eq!(edit.text, "oneXY");
        edit.undo();
        assert_eq!(edit.text, "oneX");
        let mut rune = TextEdit::new("one");
        rune.set_caret(3);
        typed(&mut rune, "X");
        rune.key(&right(false), None);
        typed(&mut rune, "Y");
        rune.undo();
        assert_eq!(rune.text, "oneX");
        // And with no key between, they are one step.
        let mut edit = TextEdit::new("one");
        edit.set_caret(3);
        typed(&mut edit, "X");
        typed(&mut edit, "Y");
        edit.undo();
        assert_eq!(edit.text, "one");
    }

    #[test]
    fn a_hook_is_the_linux_chord_only() {
        let chord = |key, ctrl, alt, logo| KeyEvent {
            key,
            mods: Mods {
                cmd: ctrl,
                ctrl,
                alt,
                logo,
                shift: false,
            },
        };
        let linux = cfg!(target_os = "linux");
        assert_eq!(is_word_right(&chord(Key::Right, true, false, false)), linux);
        // Alt+Right, Ctrl+Alt+Right, Super, plain Right, Ctrl+Left and Ctrl+Delete are Rune's.
        assert!(!is_word_right(&chord(Key::Right, false, true, false)));
        assert!(!is_word_right(&chord(Key::Right, true, true, false)));
        assert!(!is_word_right(&chord(Key::Right, true, false, true)));
        assert!(!is_word_right(&chord(Key::Right, false, false, false)));
        assert!(!is_word_right(&chord(Key::Left, true, false, false)));
        assert!(!is_word_right(&chord(Key::Delete, true, false, false)));
    }
}
