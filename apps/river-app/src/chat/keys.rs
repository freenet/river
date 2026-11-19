//! Keyboard fixes for text fields.

use rune::{Key, KeyEvent, Mods, TextEdit};

/// Ctrl+Left, Ctrl+Right, Ctrl+Backspace and Ctrl+Delete move or delete by word.
///
/// Off macOS, rune reports Ctrl as its "Cmd" modifier, and its editor reads Cmd with an arrow,
/// Backspace or Delete as "to the line edge": Ctrl+Left and Ctrl+Right jumped to the ends of the
/// line and Ctrl+Backspace deleted to the start of it. A word is what every other desktop app
/// does, and what rune's own `alt` modifier does, so the key is handed to the editor as Alt.
/// Returns whether it handled the key.
///
/// Limits: a consumed key skips rune's own follow-up for a key (it scrolls the caret into view
/// and snaps the field's height animation), so in a field that scrolls (a composer past eight
/// lines, a dialog field wider than its box) the caret may stay out of view after one of these
/// keys until the next key; and a field using `on_change` does not see these edits, so such a
/// field (the import dialog's token box) compares its text before and after the key itself. Both
/// go away with the upstream fix. Remove this when the pinned rune revision has the fix
/// (rune's `dev` reads `line` as Cmd without Ctrl off macOS).
pub fn word_keys(edit: &mut TextEdit, ev: &KeyEvent) -> bool {
    if cfg!(target_os = "macos") {
        return false;
    }
    let m = ev.mods;
    if !m.ctrl || m.alt || m.logo {
        return false;
    }
    if !matches!(
        ev.key,
        Key::Left | Key::Right | Key::Backspace | Key::Delete
    ) {
        return false;
    }
    edit.key(
        &KeyEvent {
            key: ev.key.clone(),
            mods: Mods {
                shift: m.shift,
                alt: true,
                ..Mods::default()
            },
        },
        None,
    );
    true
}
