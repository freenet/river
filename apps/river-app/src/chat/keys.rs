//! Keyboard fixes for text fields.

use rune::{Key, KeyEvent, Mods, TextEdit};

/// Ctrl+Left, Ctrl+Right, Ctrl+Backspace and Ctrl+Delete move or delete by word.
///
/// Off macOS, rune reports Ctrl as its "Cmd" modifier, and its editor reads Cmd with an arrow,
/// Backspace or Delete as "to the line edge": Ctrl+Left and Ctrl+Right jumped to the ends of the
/// line and Ctrl+Backspace deleted to the start of it. A word is what every other desktop app
/// does, and what rune's own `alt` modifier does, so the key is handed to the editor as Alt.
/// Returns whether it handled the key. Remove this when the pinned rune revision has the fix
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
