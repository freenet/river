//! Keyboard fixes for text fields.

use rune::{Key, KeyEvent, Mods, TextEdit};

/// Word motion that Rune does not give Linux the way Linux apps do.
///
/// Rune moves a word forward to the START of the next word off macOS (Windows' rule, applied on
/// Linux too since its 2026-11-12 word-boundary change). GTK, Qt and Chromium on Linux stop at
/// the END of the current word, as River did before it moved to that Rune. This keeps the Linux
/// behaviour for the three keys that go forward: Ctrl+Right, Ctrl+Shift+Right and Ctrl+Delete.
/// Ctrl+Left and Ctrl+Backspace go to the start of the word on every platform and are left to
/// Rune, as is everything on Windows (start of the next word) and macOS (end of the word).
/// Returns whether it handled the key.
///
/// The end of a word is found with Rune's own word rule (UAX #29): take where Rune's Ctrl+Right
/// lands, back up over the spaces and punctuation before it, and go on to the next word if that
/// is no further than the caret. Limits: a consumed key skips rune's own follow-up for a key (it
/// scrolls the caret into view and snaps the field's height animation), so in a field that
/// scrolls (a composer past eight lines, a dialog field wider than its box) the caret may stay
/// out of view after one of these keys until the next key; and a field using `on_change` does
/// not see these edits, so such a field (the import dialog's token box) compares its text before
/// and after the key itself.
pub fn word_keys(edit: &mut TextEdit, ev: &KeyEvent) -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let m = ev.mods;
    if !m.ctrl || m.alt || m.logo {
        return false;
    }
    match ev.key {
        Key::Right => {
            let end = word_end(&edit.text, edit.focus);
            if m.shift {
                edit.set_selection(edit.anchor, end);
            } else {
                edit.set_caret(end);
            }
            true
        }
        // With a selection Rune deletes the selection, which is what every app does.
        Key::Delete if !m.shift && edit.anchor == edit.focus => {
            let end = word_end(&edit.text, edit.focus);
            edit.set_selection(edit.focus, end);
            edit.key(
                &KeyEvent {
                    key: Key::Delete,
                    mods: Mods::default(),
                },
                None,
            );
            true
        }
        _ => false,
    }
}

/// Where the word at or after byte `from` ends (the end of `text` when none does).
fn word_end(text: &str, from: usize) -> usize {
    let right = KeyEvent {
        key: Key::Right,
        mods: Mods {
            ctrl: true,
            cmd: true,
            ..Mods::default()
        },
    };
    let mut probe = TextEdit::new(text);
    let mut at = from.min(text.len());
    loop {
        probe.set_caret(at);
        probe.key(&right, None);
        let next = probe.focus;
        let end = text[..next]
            .trim_end_matches(|c: char| !(c.is_alphanumeric() || c == '_'))
            .len();
        if end > from {
            return end;
        }
        if next <= at {
            return text.len();
        }
        at = next;
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(super) fn word_end_for_test(text: &str, from: usize) -> usize {
    word_end(text, from)
}
