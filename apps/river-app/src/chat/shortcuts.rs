//! The keyboard shortcuts page: Ctrl+/ (Cmd+/ on macOS) or the footer entry opens a dialog that
//! lists every shortcut River has.
//!
//! The list is [`SHORTCUTS`], one table. The page is drawn from it, and a test reads the app's
//! own sources and fails when a Ctrl/Cmd chord is matched anywhere (`is_command('x')`,
//! `Key::Command('x' | 'y')`) without an entry here, when an entry names a chord nothing
//! matches, or when a chord is matched through something other than a character literal (which
//! the scan cannot read). So the page lists every Ctrl/Cmd chord River handles itself. Keys
//! that are not such chords (Enter, Esc, arrows, Tab) and the text editing the UI library
//! provides (copy, paste, undo) cannot be found that way; they are in the table by hand.

use rune::{El, KeyEvent, col, row};
use rune_kit::Kit;

use super::Chat;
use super::dialog_kit::{self as dk, DialogSpec};
use super::style as st;
use super::view::label;
use crate::RiverApp;

/// One shortcut.
#[derive(Clone, Copy, Debug)]
pub struct Shortcut {
    /// The heading it is listed under.
    pub group: &'static str,
    /// The keys, joined by `+`. `Mod` is Ctrl (Cmd on macOS) and `Word` is Ctrl (Option on
    /// macOS); anything else is shown as written. Alternatives are separated by ` / `.
    pub chord: &'static str,
    pub action: &'static str,
    /// The characters the handler matches this chord by (`ev.is_command('k')`), lowercase.
    /// Empty for a key that is not a Ctrl/Cmd chord.
    pub commands: &'static [char],
}

const fn s(
    group: &'static str,
    chord: &'static str,
    action: &'static str,
    commands: &'static [char],
) -> Shortcut {
    Shortcut {
        group,
        chord,
        action,
        commands,
    }
}

pub const GENERAL: &str = "Anywhere";
pub const COMPOSER: &str = "Writing a message";

/// Every shortcut, in the order the page lists them.
pub const SHORTCUTS: &[Shortcut] = &[
    s(GENERAL, "Mod+K", "Jump to a room or member", &['k']),
    s(GENERAL, "Mod+F", "Search the messages in this room", &['f']),
    s(GENERAL, "Mod+,", "Open the settings", &[',']),
    s(GENERAL, "Mod+/", "Show this list of shortcuts", &['/']),
    s(
        GENERAL,
        "Mod+= / Mod+-",
        "Make everything bigger or smaller",
        &['=', '+', '-', '_'],
    ),
    s(GENERAL, "Mod+0", "Back to normal size", &['0']),
    s(
        GENERAL,
        "Mod+Q",
        "Quit River (when it keeps running in the background)",
        &['q'],
    ),
    s(
        GENERAL,
        "Up / Down, Enter",
        "In the jump box and the search box: choose a result, then open it",
        &[],
    ),
    s(GENERAL, "Tab", "Move between buttons and fields", &[]),
    s(
        GENERAL,
        "Esc",
        "Close the open dialog or popup, or cancel a reply or an edit",
        &[],
    ),
    s(COMPOSER, "Enter", "Send the message", &[]),
    s(
        COMPOSER,
        "Shift+Enter",
        "New line (Option+Enter on a Mac)",
        &[],
    ),
    s(
        COMPOSER,
        "Up",
        "In an empty box: edit your last message",
        &[],
    ),
    s(
        COMPOSER,
        "@name",
        "Mention a member: Up and Down choose, Enter or Tab inserts, Esc closes",
        &[],
    ),
    s(
        COMPOSER,
        ":name",
        "Insert an emoji by name, such as :smile (two letters list some). Enter or Tab inserts the highlighted one and does not send; a closing : completes a full name",
        &[],
    ),
    s(COMPOSER, "Word+Left / Word+Right", "Move by a word", &[]),
    s(
        COMPOSER,
        "Word+Backspace / Word+Delete",
        "Delete a word",
        &[],
    ),
    s(
        COMPOSER,
        "Mod+Z / Mod+Shift+Z",
        "Undo and redo, in any text box",
        &[],
    ),
    s(
        COMPOSER,
        "Mod+A / Mod+C / Mod+X / Mod+V",
        "Select all, copy, cut and paste, in any text box",
        &[],
    ),
];

/// `chord` as shown on this platform: `Mod` is Ctrl or Cmd, `Word` is Ctrl or Option.
pub fn shown(chord: &str, mac: bool) -> String {
    let (mod_key, word_key) = if mac {
        ("Cmd", "Option")
    } else {
        ("Ctrl", "Ctrl")
    };
    chord
        .split(" / ")
        .map(|alt| {
            alt.split('+')
                .map(|part| match part {
                    "Mod" => mod_key,
                    "Word" => word_key,
                    other => other,
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Whether `ev` is the chord that opens the page: Ctrl+/, or Cmd+/ on macOS.
pub fn is_chord(ev: &KeyEvent) -> bool {
    ev.is_command('/') && !ev.mods.alt
}

impl Chat {
    /// Open the page. It does not open over another dialog, the switcher or a search panel
    /// (the guard the switcher uses), and when it is open already the chord does nothing: a
    /// held key repeats the chord, and a toggle would flicker.
    pub fn open_shortcuts(&mut self) -> bool {
        if self.shortcuts_open {
            return true;
        }
        if self.other_dialog_open() || self.switcher.is_some() {
            return false;
        }
        self.emoji_open = false;
        self.picker = None;
        self.members_overlay = false;
        // The dialog takes the keyboard.
        self.composer.focused = false;
        self.shortcuts_open = true;
        true
    }

    pub fn close_shortcuts(&mut self) {
        self.shortcuts_open = false;
    }

    /// Close the page when another dialog has opened over the screen. The link menu does not
    /// count: it is a menu, and it cannot open while this modal is up.
    pub fn close_shortcuts_under_dialog(&mut self) {
        if !self.shortcuts_open {
            return;
        }
        self.shortcuts_open = false;
        let link = self.link_menu.take();
        let other = self.other_dialog_open() || self.switcher.is_some();
        self.link_menu = link;
        self.shortcuts_open = !other;
    }

    /// The window-wide keys of this module: zoom, and the chord that opens the page. Returns
    /// whether `ev` was one.
    pub fn app_key(&mut self, ev: &KeyEvent, tx: &mut rune::Tx) -> bool {
        if self.zoom_key(ev, tx) {
            return true;
        }
        if is_chord(ev) {
            self.open_shortcuts();
            return true;
        }
        if super::settings::is_chord(ev) {
            self.open_settings();
            return true;
        }
        false
    }
}

/// Height of the list's scroll area in a window `win_h` high: room is kept for the title, the
/// Close button and the dialog's margins, within 120 to 380.
fn scroll_height(win_h: f32) -> f32 {
    (win_h - 190.0).clamp(120.0, 380.0)
}

/// The dialog, when open.
/// `win_h` is the window's height in layout points: the list scrolls in what the dialog's title
/// and Close button leave, so a short window (or a large zoom) still shows both.
pub(super) fn shortcuts_dialog(chat: &Chat, win_h: f32, k: &Kit, layer: &mut Vec<El>) {
    if !chat.shortcuts_open {
        return;
    }
    let p = st::palette();
    let mac = cfg!(target_os = "macos");
    let mut list = col().key("shortcuts-list").w_full().gap(6.0);
    let mut group = "";
    for (i, sc) in SHORTCUTS.iter().enumerate() {
        if sc.group != group {
            group = sc.group;
            list = list.child(
                label(group, st::ROOM, p.text)
                    .key(format!("shortcuts-group-{i}"))
                    .pt(if i == 0 { 0.0 } else { 10.0 }),
            );
        }
        let chord = row()
            .key(format!("shortcut-chord-{i}"))
            .w(190.0)
            .shrink()
            .child(
                label(shown(sc.chord, mac), st::SMALL, p.text)
                    .wrap()
                    .w_full(),
            );
        list = list.child(
            row()
                .key(format!("shortcut-{i}"))
                .w_full()
                .gap(12.0)
                .child(chord)
                .child(
                    label(sc.action, st::SMALL, p.text_secondary)
                        .wrap()
                        .grow(1.0)
                        .min_w(0.0),
                ),
        );
    }
    DialogSpec::new("shortcuts", "Keyboard shortcuts", |s: &mut RiverApp| {
        s.chat.close_shortcuts()
    })
    .body_key("shortcuts-body")
    .width(560.0)
    .child(
        col()
            .key("shortcuts-scroll")
            .w_full()
            .max_h(scroll_height(win_h))
            .scroll(rune::ScrollSpec {
                stick_bottom: false,
            })
            .child(list),
    )
    .footer(dk::close_footer(
        k,
        "shortcuts-close",
        |s: &mut RiverApp| s.chat.close_shortcuts(),
    ))
    .build(k, layer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    /// One scanned file: where it is, its live code (comments and everything from an inline
    /// `#[cfg(test)] mod name {` on removed),
    struct Source {
        path: PathBuf,
        code: String,
    }

    /// `text` up to an inline test module: a `#[cfg(test)]` line directly followed by
    /// `mod name {` with the brace on the same line. A declaration (`mod name;`) does not cut.
    /// Also whether everything after the cut is test code: once that module has closed (a `}`
    /// in column 0), each item at the top level must carry a `#[cfg(test)]` or `#[test]`
    /// attribute, so live handler code placed after a mid-file test module is not hidden.
    fn live_part(text: &str) -> (&str, bool) {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let mut at = 0;
        for (i, line) in lines.iter().enumerate() {
            if line.trim() == "#[cfg(test)]"
                && lines.get(i + 1).is_some_and(|n| {
                    let n = n.trim();
                    n.starts_with("mod ") && n.ends_with('{')
                })
            {
                return (&text[..at], tail_is_tests_only(&lines[i + 2..]));
            }
            at += line.len();
        }
        (text, true)
    }

    /// `rest` is what follows the first line of a test module. True when the module's contents
    /// are all indented and every top-level item after its closing brace is attributed as test
    /// code.
    fn tail_is_tests_only(rest: &[&str]) -> bool {
        let mut closed = false;
        let mut test_attr = false;
        for line in rest {
            let top = !line.is_empty() && !line.starts_with([' ', '\t', '\n', '\r']);
            if !top {
                continue;
            }
            if !closed {
                if line.starts_with('}') {
                    closed = true;
                }
                continue;
            }
            let t = line.trim();
            if t.starts_with("//") || t.starts_with('}') {
                continue;
            }
            if t.starts_with("#[cfg(test)]") || t.starts_with("#[test]") {
                test_attr = true;
                continue;
            }
            if t.starts_with("#[") {
                continue;
            }
            // An item: it must have been attributed.
            if !test_attr {
                return false;
            }
            test_attr = false;
        }
        true
    }

    /// The code of every `.rs` file under the app's and the desktop shell's `src`, minus test
    /// files. `\r\n` is normalised.
    fn sources() -> Vec<Source> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).expect("read src") {
                let p = e.expect("entry").path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        walk(&root.join("src"), &mut files);
        walk(&root.join("../desktop/src"), &mut files);
        files.sort();
        files
            .into_iter()
            .filter(|p| {
                let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                !(n == "tests.rs" || n.ends_with("_tests.rs"))
            })
            .map(|path| {
                let text = std::fs::read_to_string(&path)
                    .expect("read source")
                    .replace("\r\n", "\n");
                let (live, tail_ok) = live_part(&text);
                assert!(
                    tail_ok,
                    "{} has live code after its test module; the chord scan would not see it",
                    path.display()
                );
                let code = live
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("//"))
                    .collect::<Vec<_>>()
                    .join("\n");
                Source { path, code }
            })
            .collect()
    }

    /// What a scan found: the characters chords are matched by, and the arguments that were not
    /// character literals.
    #[derive(Default)]
    struct Scan {
        chords: BTreeSet<char>,
        opaque: Vec<String>,
    }

    /// Every `is_command(...)` and `Key::Command(...)` in `code`. An argument may be one
    /// character literal or several joined by `|` (a match arm).
    fn scan_code(code: &str, into: &mut Scan) {
        for needle in ["is_command(", "Key::Command("] {
            let mut rest = code;
            while let Some(i) = rest.find(needle) {
                rest = &rest[i + needle.len()..];
                let arg = rest.split(')').next().unwrap_or_default().trim();
                let mut parsed = Vec::new();
                for alt in arg.split('|') {
                    let alt = alt.trim();
                    let inner = alt.strip_prefix('\'').and_then(|a| a.strip_suffix('\''));
                    match inner.map(|i| i.trim_start_matches('\\')) {
                        Some(c) if c.chars().count() == 1 => {
                            parsed.push(c.chars().next().unwrap().to_ascii_lowercase());
                        }
                        _ => {
                            parsed.clear();
                            into.opaque.push(arg.to_owned());
                            break;
                        }
                    }
                }
                into.chords.extend(parsed);
            }
        }
    }

    fn scan(sources: &[Source]) -> Scan {
        let mut found = Scan::default();
        for s in sources {
            scan_code(&s.code, &mut found);
        }
        found
    }

    fn handled_chords() -> BTreeSet<char> {
        scan(&sources()).chords
    }

    fn listed_chords() -> BTreeSet<char> {
        SHORTCUTS
            .iter()
            .flat_map(|s| s.commands.iter().copied())
            .collect()
    }

    #[test]
    fn every_chord_a_handler_matches_is_listed() {
        let handled = handled_chords();
        let listed = listed_chords();
        let missing: Vec<_> = handled.difference(&listed).collect();
        assert!(
            missing.is_empty(),
            "chords matched in the code but missing from SHORTCUTS: {missing:?}. Add a row to \
             chat/shortcuts.rs."
        );
        // The scrape must really see the known handlers, or the test above checks nothing.
        for c in ['k', 'q', 'f', '/', '=', '-', '0'] {
            assert!(
                handled.contains(&c),
                "the scrape did not find the {c:?} chord"
            );
        }
    }

    #[test]
    fn every_chord_listed_is_matched_by_a_handler() {
        let handled = handled_chords();
        let stale: Vec<_> = listed_chords().difference(&handled).copied().collect();
        assert!(
            stale.is_empty(),
            "SHORTCUTS lists chords no handler matches: {stale:?}"
        );
    }

    #[test]
    fn a_row_names_its_chord_and_its_commands_agree_with_it() {
        for sc in SHORTCUTS {
            assert!(!sc.chord.is_empty() && !sc.action.is_empty(), "{sc:?}");
            if sc.commands.is_empty() {
                continue;
            }
            // A row with commands starts with Mod and ends in one of them (`Mod+= / Mod+-`
            // lists two, so check each alternative against the commands).
            for alt in sc.chord.split(" / ") {
                let (m, key) = alt.split_once('+').expect("Mod+key");
                assert_eq!(m, "Mod", "{sc:?}");
                let c = key.chars().next().expect("a key").to_ascii_lowercase();
                assert!(
                    key.chars().count() == 1 && sc.commands.contains(&c),
                    "{sc:?}"
                );
            }
        }
    }

    #[test]
    fn no_chord_is_matched_through_something_the_scan_cannot_read() {
        let found = scan(&sources());
        assert!(
            found.opaque.is_empty(),
            "is_command/Key::Command called with something other than character literals: {:?}. \
             Match by literal, or teach this scan.",
            found.opaque
        );
    }

    #[test]
    fn the_scan_sees_each_whole_file() {
        let all = sources();
        // A file cut short would hide its chords: chat/mod.rs has a `mod tests;` declaration
        // near the top that used to cut it after 44 lines.
        let mod_rs = all
            .iter()
            .find(|s| s.path.ends_with("chat/mod.rs"))
            .expect("chat/mod.rs is scanned");
        assert!(
            mod_rs.code.contains("pub fn escape("),
            "chat/mod.rs was cut short"
        );
        // Where a file does have an inline test module, what is cut is only its tail.
        let lib = all
            .iter()
            .find(|s| s.path.ends_with("river-app/src/lib.rs") || s.path.ends_with("src/lib.rs"))
            .expect("lib.rs is scanned");
        assert!(lib.code.contains("pub fn spec()"));
        assert!(!lib.code.contains("fn the_chord_opens_the_switcher"));
        assert!(all.iter().any(|s| s.path.ends_with("desktop/src/main.rs")));
    }

    #[test]
    fn the_real_scan_catches_a_chord_added_at_the_end_of_any_file() {
        // The same scan the checks above use, over the real sources with one extra chord
        // appended to each file in turn: it must be seen every time.
        let all = sources();
        let before = scan(&all).chords;
        assert!(!before.contains(&'j') && !before.contains(&'y'));
        for i in 0..all.len() {
            let mut one = Scan::default();
            for (k, s) in all.iter().enumerate() {
                let mut code = s.code.clone();
                if k == i {
                    code.push_str("\nfn x(ev: &KeyEvent) -> bool { ev.is_command('j') }\n");
                    code.push_str(
                        "\nfn y(k: Key) { match k { Key::Command('y' | 'Q') => {} _ => {} } }\n",
                    );
                }
                scan_code(&code, &mut one);
            }
            assert!(
                one.chords.contains(&'j') && one.chords.contains(&'y'),
                "{:?}",
                all[i].path
            );
        }
        // And a chord that is not a literal is reported.
        let mut one = Scan::default();
        scan_code("ev.is_command(c)", &mut one);
        assert_eq!(one.opaque, ["c"]);
        assert!(one.chords.is_empty());
        let mut one = Scan::default();
        scan_code("Key::Command('a' | 'B') ev.is_command('\\'')", &mut one);
        assert_eq!(one.chords, BTreeSet::from(['a', 'b', '\'']));
    }

    #[test]
    fn an_inline_test_module_cuts_but_a_declaration_does_not() {
        let decl = "a\n#[cfg(test)]\nmod tests;\nb\n";
        assert_eq!(live_part(decl), (decl, true));
        assert_eq!(
            live_part("a\n#[cfg(test)]\nmod tests {\nb\n"),
            ("a\n", true)
        );
        let f = "a\n#[cfg(test)]\nfn f() {}\nb\n";
        assert_eq!(live_part(f), (f, true));
    }

    #[test]
    fn live_code_after_a_test_module_is_noticed() {
        let tests_only =
            "a\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n#[cfg(test)]\nfn helper() {}\n";
        assert_eq!(live_part(tests_only), ("a\n", true));
        let live_after =
            "a\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\nfn live(ev: &KeyEvent) {}\n";
        assert_eq!(live_part(live_after), ("a\n", false));
        let impl_after = "a\n#[cfg(test)]\nmod tests {\n}\nimpl X {\n    fn f() {}\n}\n";
        assert!(!live_part(impl_after).1);
    }

    #[test]
    fn chords_show_for_the_platform() {
        assert_eq!(shown("Mod+K", false), "Ctrl+K");
        assert_eq!(shown("Mod+K", true), "Cmd+K");
        assert_eq!(shown("Mod+= / Mod+-", false), "Ctrl+= / Ctrl+-");
        assert_eq!(
            shown("Word+Left / Word+Right", false),
            "Ctrl+Left / Ctrl+Right"
        );
        assert_eq!(
            shown("Word+Left / Word+Right", true),
            "Option+Left / Option+Right"
        );
        assert_eq!(shown("Shift+Enter", true), "Shift+Enter");
        assert_eq!(shown("Esc", false), "Esc");
    }

    #[test]
    fn the_list_scrolls_in_what_a_short_window_leaves() {
        assert_eq!(scroll_height(800.0), 380.0);
        assert_eq!(scroll_height(400.0), 210.0);
        assert_eq!(scroll_height(240.0), 120.0);
    }

    #[test]
    fn the_page_is_built_from_the_table() {
        let ids: BTreeSet<_> = SHORTCUTS.iter().map(|s| (s.group, s.chord)).collect();
        assert_eq!(ids.len(), SHORTCUTS.len(), "no row twice");
        assert!(SHORTCUTS.iter().any(|s| s.group == GENERAL));
        assert!(SHORTCUTS.iter().any(|s| s.group == COMPOSER));
    }
}
