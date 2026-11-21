//! End-of-word Ctrl+Right on Linux.
//!
//! Rune moves a word forward to the START of the next word everywhere but macOS (Windows' and,
//! since its 2026-11-12 word-boundary change, Linux's rule). GTK, Qt and Chromium on Linux stop
//! at the END of the current word. This gives Linux that for Ctrl+Right in every text field.
//! Windows keeps Rune's start of the next word and macOS its end of the word (Option+Right):
//! `word_end` does nothing there.
//!
//! How: Rune has no setting for the rule, and a key a field's handler takes never reaches
//! Rune's follow-up that scrolls the caret into view (`caret_follow`, not reachable from
//! outside). So the handler takes no key. For a plain Ctrl+Right it selects from the caret to
//! the end of the word and declines the key. Rune then handles Ctrl+Right with a selection and
//! no Shift as it does on every platform: it collapses the selection to its end, and does
//! the rest of a key (undo step closed, column forgotten, caret scrolled into view).
//!
//! What is left to Rune on purpose:
//!
//! - Ctrl+Shift+Right. Selecting to a word's end would need the handler to take the key, which
//!   loses the scroll, so on Linux it selects to the start of the next word, a different stop
//!   from Ctrl+Right. A Rune setting for the forward-word rule would fix that.
//! - Ctrl+Left and Ctrl+Backspace (the start of the word on every platform).
//! - Ctrl+Delete: Rune records a word delete for undo as `EditKind::DeleteRun`, which is
//!   crate-private, so a delete made from here would join later edits differently from every
//!   other field. It deletes to the start of the next word, where GTK deletes to the word's end.
//! - Alt+Right and Ctrl+Alt+Right (the start of the next word).
//! - Ctrl+Right with a selection: Rune collapses to the selection's end and does not move on,
//!   on every platform.
//! - A field with a strong right-to-left character anywhere in it (`has_rtl`, so `hi مرحبا`
//!   too), or a direction forced with Ctrl+Shift: Rune then moves by the line as drawn, which
//!   needs the field's layout.
//! - An input method composition in progress, and secure fields.
//! - A run of more than 4096 segments with no word in it (5000 dots) for that press, so a press
//!   there follows Rune's rule while the presses before it followed this one.
//!
//! The end of the word is Rune's macOS rule: skip what is not a word (UAX #29 segments with no
//! letter or digit), then stop at the end of the next word segment, or at the start of a mention
//! pill inside it. Whole segments are used, so the caret never lands inside a grapheme cluster
//! (an accent, a keycap, an emoji with a joiner, a Thai tone mark). Pills are read with
//! `TextEdit::pills`, which skips a malformed pill that Rune's own lookup would still count;
//! River's fields hold none.
//!
//! Cost of a press: the segment walk is capped (`MAX_SEGMENTS`) and copies nothing. Looking for
//! a right-to-left character scans the field's text up to the first one found, so it is linear
//! in the text for the left-to-right text it exists to let through.

use std::ops::Range;

use rune::{Key, KeyEvent, TextEdit, Tx};
use unicode_segmentation::UnicodeSegmentation;

use crate::RiverApp;

/// How a field's `TextEdit` is reached from the app, as `on_edit` takes it.
pub(crate) type Get = fn(&mut RiverApp) -> &mut TextEdit;

/// Segments one press walks before giving up and leaving the key to Rune: far past any real
/// run of punctuation, so a megabyte of `.` costs a bounded walk.
const MAX_SEGMENTS: usize = 4096;

/// A field's `TextEdit`, or `None` when the field is not there (a panel a key arrived after it
/// closed), so the handler can never panic on one.
pub(crate) trait Find:
    for<'a> Fn(&'a mut RiverApp) -> Option<&'a mut TextEdit> + 'static
{
}
impl<F: for<'a> Fn(&'a mut RiverApp) -> Option<&'a mut TextEdit> + 'static> Find for F {}

/// A field that is always there, as a [`Find`].
pub(crate) fn always(get: Get) -> impl Find {
    move |s: &mut RiverApp| Some(get(s))
}

/// The key handler of a field: Ctrl+Right on Linux (see the module comment). Always `false`
/// elsewhere. For a field with a key handler of its own, use [`word_end_after`].
pub(crate) fn word_end(
    find: impl Find,
) -> impl Fn(&mut RiverApp, &KeyEvent, &mut Tx) -> bool + 'static {
    move |s, ev, _| {
        prepare(&find, s, ev);
        false
    }
}

/// [`word_end`] after `own`, the field's own key handler, which keeps the keys it takes.
pub(crate) fn word_end_after(
    find: impl Find,
    own: impl Fn(&mut RiverApp, &KeyEvent, &mut Tx) -> bool + 'static,
) -> impl Fn(&mut RiverApp, &KeyEvent, &mut Tx) -> bool + 'static {
    move |s, ev, tx| {
        if own(s, ev, tx) {
            return true;
        }
        prepare(&find, s, ev);
        false
    }
}

/// Ctrl+Right with no Shift, Alt or Super. Off Linux never: the callers then skip the field
/// lookup too.
fn is_word_right(ev: &KeyEvent) -> bool {
    let m = ev.mods;
    cfg!(target_os = "linux") && ev.key == Key::Right && m.ctrl && !m.alt && !m.logo && !m.shift
}

/// For a Ctrl+Right, selects the field from its caret to the end of the word, for Rune's
/// Ctrl+Right to collapse. The key is always Rune's: this only leaves the selection it acts on.
fn prepare(find: &impl Find, s: &mut RiverApp, ev: &KeyEvent) {
    if is_word_right(ev)
        && let Some(edit) = find(s)
    {
        select_to_word_end(edit);
    }
}

/// Whether it selected: not for a secure field, a composition, a forced direction, a selection
/// already there (Rune collapses it), text with right-to-left characters, a walk too long, or a
/// caret with no word after it.
fn select_to_word_end(edit: &mut TextEdit) -> bool {
    if edit.is_secure() || edit.preedit().is_some() || edit.forced_dir().is_some() {
        return false;
    }
    if edit.anchor != edit.focus {
        return false;
    }
    let Some(to) = forward_word_end(edit, edit.focus) else {
        return false;
    };
    if to == edit.focus || has_rtl(&edit.text) {
        return false;
    }
    edit.set_selection(edit.focus, to);
    true
}

/// Where a word forward from byte `from` ends in `edit`'s text: Rune's rule on macOS. `None`
/// when `from` is not a character boundary or the walk is too long to do here.
pub(crate) fn forward_word_end(edit: &TextEdit, from: usize) -> Option<usize> {
    let s = edit.text.as_str();
    if from > s.len() || !s.is_char_boundary(from) {
        return None;
    }
    let pills: Vec<Range<usize>> = if edit.marks().is_empty() {
        Vec::new()
    } else {
        edit.pills().map(|(r, _)| r).collect()
    };
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

/// Whether `text` holds a strong right-to-left character: a Hebrew, Arabic, Syriac, Thaana,
/// N'Ko, Samaritan or Mandaic letter, a presentation form, one of the right-to-left historic
/// scripts, or a right-to-left mark, embedding, override or isolate. Digits do not count (the
/// Arabic-Indic digits are numbers, and the byte order mark U+FEFF is not text), so a number
/// does not turn a field over to Rune's rule. A little wider than Unicode's classes in the
/// marks that only follow letters, which only means Rune's rule applies a little more often.
/// Anywhere in the text counts, so `hi مرحبا` is Rune's whole.
pub(crate) fn has_rtl(text: &str) -> bool {
    text.chars().any(is_rtl)
}

fn is_rtl(c: char) -> bool {
    if c < '\u{0590}' {
        return false;
    }
    match c {
        // Arabic-Indic digits and signs that are numbers, not letters.
        '\u{0600}'..='\u{0605}'
        | '\u{0660}'..='\u{0669}'
        | '\u{066B}'..='\u{066C}'
        | '\u{06DD}'
        | '\u{06F0}'..='\u{06F9}'
        | '\u{08E2}' => false,
        '\u{0590}'..='\u{08FF}' => true,
        // The right-to-left mark, embedding, override and isolate.
        '\u{200F}' | '\u{202B}' | '\u{202E}' | '\u{2067}' => true,
        '\u{FB1D}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFC}' => true,
        '\u{10800}'..='\u{10FFF}' | '\u{1E800}'..='\u{1EFFF}' => true,
        _ => false,
    }
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
        let rtl = [
            "\u{5E9}\u{5DC}\u{5D5}\u{5DD}",           // Hebrew
            "hi \u{645}\u{631}\u{62D}\u{628}\u{627}", // Arabic, after Latin
            "\u{710}\u{712}",                         // Syriac
            "\u{78B}\u{7A8}",                         // Thaana
            "\u{7CA}\u{7CB}",                         // N'Ko
            "\u{FB1D}",                               // a Hebrew presentation form
            "\u{FE8D}",                               // an Arabic presentation form
            "\u{10800}",                              // Cypriot, in the 10800 range
            "\u{1E900}",                              // Adlam
            "a\u{200F}b",                             // right-to-left mark
            "a\u{61C}b",                              // Arabic letter mark
            "a\u{202B}b",                             // right-to-left embedding
            "a\u{202E}b",                             // right-to-left override
            "a\u{2067}b",                             // right-to-left isolate
        ];
        for text in rtl {
            assert!(has_rtl(text), "{text:?}");
        }
        let ltr = [
            "plain text, caf\u{E9}, \u{65E5}\u{672C}\u{8A9E}, \u{E01}\u{E32}, \u{1F600}",
            "",
            "\u{FEFF}hello",                           // a byte order mark
            "\u{660}\u{661}\u{662} \u{6F0}\u{6F1} 12", // Arabic-Indic digits
            "a\u{200E}b \u{202A} \u{202D} \u{2066}",   // left-to-right marks and embeddings
        ];
        for text in ltr {
            assert!(!has_rtl(text), "{text:?}");
        }
    }

    /// The end-of-word key as the field sees it: this handler's selection, then Rune's key.
    fn ctrl_right(edit: &mut TextEdit) {
        select_to_word_end(edit);
        edit.key(&right(false), None);
    }

    #[test]
    fn it_moves_the_caret_to_the_end_of_the_word() {
        let mut edit = TextEdit::new(LINE);
        edit.set_caret(0);
        ctrl_right(&mut edit);
        assert_eq!((edit.anchor, edit.focus), (3, 3));
        ctrl_right(&mut edit);
        assert_eq!((edit.anchor, edit.focus), (14, 14));
        // Without this handler Rune's own key goes to the start of the next word.
        let mut rune = TextEdit::new(LINE);
        rune.set_caret(0);
        rune.key(&right(false), None);
        assert_ne!(rune.focus, 3);
    }

    /// The handler only selects: the key itself is Rune's.
    #[test]
    fn it_leaves_a_selection_for_runes_ctrl_right_to_collapse() {
        let mut edit = TextEdit::new(LINE);
        edit.set_caret(0);
        assert!(select_to_word_end(&mut edit));
        assert_eq!((edit.anchor, edit.focus), (0, 3));
    }

    /// Ctrl+Shift+Right is Rune's: it never reaches the handler's selection.
    #[test]
    fn shift_right_is_not_taken() {
        assert!(!is_word_right(&right(true)));
    }

    /// Ctrl+Right with a selection collapses it to its end and goes no further, as Rune does
    /// on every platform, so the handler leaves it alone.
    #[test]
    fn a_selection_collapses_to_its_end_by_runes_rule() {
        for (anchor, focus) in [(0, 3), (3, 0)] {
            let mut edit = TextEdit::new(LINE);
            edit.set_selection(anchor, focus);
            assert!(!select_to_word_end(&mut edit));
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
    fn at_the_end_of_the_text_nothing_is_selected() {
        let mut edit = TextEdit::new("one");
        edit.set_caret(3);
        assert!(!select_to_word_end(&mut edit));
        assert_eq!((edit.anchor, edit.focus), (3, 3));
    }

    #[test]
    fn right_to_left_text_is_left_to_rune() {
        let texts = [
            "\u{5E9}\u{5DC}\u{5D5}\u{5DD} abc",
            "abc \u{645}\u{631}\u{62D}\u{628}\u{627}",
            // Left to right first: the whole field is Rune's, not just the right-to-left part.
            "hi \u{645}\u{631}\u{62D}\u{628}\u{627}",
            "one two\u{200F}",
        ];
        for text in texts {
            let mut edit = TextEdit::new(text);
            edit.set_caret(0);
            assert!(!select_to_word_end(&mut edit), "{text:?}");
            assert_eq!((edit.anchor, edit.focus), (0, 0));
        }
    }

    /// A byte order mark or an Arabic-Indic number is not a right-to-left field.
    #[test]
    fn a_bom_or_a_number_does_not_hand_the_field_to_rune() {
        for text in ["\u{FEFF}one two", "one two \u{661}\u{662}\u{663}"] {
            let mut edit = TextEdit::new(text);
            edit.set_caret(0);
            assert!(select_to_word_end(&mut edit), "{text:?}");
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
        assert!(!select_to_word_end(&mut edit));
        assert!(edit.preedit().is_some());
    }

    /// Rune closes the open undo step on a caret move, so typing before and after one are two
    /// steps. The key is Rune's, so it does.
    #[test]
    fn typing_after_the_move_is_an_undo_step_of_its_own() {
        let mut edit = TextEdit::new("one two");
        edit.set_caret(0);
        typed(&mut edit, "X");
        ctrl_right(&mut edit);
        typed(&mut edit, "Y");
        assert_eq!(edit.text, "XoneY two");
        edit.undo();
        assert_eq!(edit.text, "Xone two");
        edit.undo();
        assert_eq!(edit.text, "one two");
    }

    /// Edits join the open step only when they touch the last one, so the break is seen where
    /// the caret does not move (at the end of the text).
    #[test]
    fn a_move_that_stays_put_still_closes_the_undo_step() {
        let mut edit = TextEdit::new("one");
        edit.set_caret(3);
        typed(&mut edit, "X");
        ctrl_right(&mut edit);
        typed(&mut edit, "Y");
        assert_eq!(edit.text, "oneXY");
        edit.undo();
        assert_eq!(edit.text, "oneX");
    }

    /// What the segmenter makes a word, pinned: these are the stops Rune's macOS rule gives.
    #[test]
    fn what_counts_as_a_word_is_the_segmenters() {
        // An underscore joins a word; an apostrophe inside one does not split it.
        assert_eq!(stops("snake_case x"), [10, 12]);
        assert_eq!(stops("don't stop"), [5, 10]);
        // A decimal point between digits does not split a number.
        assert_eq!(stops("3.14 pi"), [4, 7]);
        // A hyphen does: `well-known` is two stops.
        assert_eq!(stops("well-known"), [4, 10]);
        // Han and Hiragana step one character or run at a time, Thai by its own breaks.
        let cjk = stops("\u{65E5}\u{672C}\u{8A9E}\u{30C6}\u{30B9}\u{30C8}");
        assert_eq!(cjk, [3, 6, 9, 18]);
        assert_eq!(
            stops("\u{E2A}\u{E27}\u{E31}\u{E2A}\u{E14}\u{E35}"),
            [3, 9, 12, 18]
        );
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
