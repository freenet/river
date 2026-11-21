//! The Settings dialog: one dialog, opened from the gear (or Ctrl+, / Cmd+,), with a list of
//! sections on the left and the chosen section on the right.
//!
//! In a narrow window (the same width the rest of the app calls narrow, [`NARROW_WIDTH`]) the
//! list is a screen of its own: pressing a section opens it, and Back returns to the list.
//!
//! Sections so far: **Updates** (River's own updates, [`super::update_view::updates_section`])
//! and **Connection** (the Freenet node, [`connection_section`]). A section exists only when it
//! has content. A new section is an entry in [`Section::ALL`] and an arm in
//! [`section_content`]; the rest (list, keys, narrow screens, arrow keys) follows from those.
//! Which section was last shown is not remembered: the dialog opens on the first section, and
//! nothing about it is saved (the settings themselves keep their own saved choices).
//!
//! The dialog has no Save or Close button: every control in it acts at once, and the X closes
//! it. Keys are stable (`settings-section-{id}`, `settings-back`, and the keys of the controls
//! inside the sections), because tests and the end-to-end script find elements by key.

use rune::{El, Key, col, el, icon, row};
use rune_kit::{Button, ButtonVariant, Kit};

use super::dialog_kit::{DialogSpec, rule};
use super::icons;
use super::style::{self as st, Palette};
use super::view::label;
use crate::RiverApp;

/// A section of the dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Updates,
    Connection,
}

impl Section {
    /// Every section, in the order the list shows them.
    pub const ALL: &'static [Section] = &[Section::Updates, Section::Connection];

    /// The name in the list and the heading of the section (sentence case).
    pub fn title(self) -> &'static str {
        match self {
            Section::Updates => "Updates",
            Section::Connection => "Connection",
        }
    }

    /// The part of the key that names it: `settings-section-{id}`.
    pub fn id(self) -> &'static str {
        match self {
            Section::Updates => "updates",
            Section::Connection => "connection",
        }
    }

    fn key(self) -> String {
        format!("settings-section-{}", self.id())
    }

    fn index(self) -> usize {
        Section::ALL
            .iter()
            .position(|s| *s == self)
            .expect("every section is in ALL")
    }
}

/// Whether the dialog is open, and what it shows. Session-only: nothing here is saved.
#[derive(Debug)]
pub struct SettingsUi {
    pub open: bool,
    pub section: Section,
    /// Narrow window only: the section is open (Back returns to the list). A wide window shows
    /// the list and a section together and ignores this.
    pub in_section: bool,
}

impl Default for SettingsUi {
    fn default() -> Self {
        SettingsUi {
            open: false,
            section: Section::ALL[0],
            in_section: false,
        }
    }
}

impl SettingsUi {
    /// Open on the first section (on its list, in a narrow window).
    pub fn open(&mut self) {
        *self = SettingsUi {
            open: true,
            ..SettingsUi::default()
        };
    }

    /// Open showing `section` (scenes and tests).
    pub fn open_at(&mut self, section: Section) {
        *self = SettingsUi {
            open: true,
            section,
            in_section: true,
        };
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// Show `section`: in a narrow window this opens it from the list.
    pub fn select(&mut self, section: Section) {
        self.section = section;
        self.in_section = true;
    }

    /// Back to the list (narrow window).
    pub fn back(&mut self) {
        self.in_section = false;
    }
}

/// Whether `ev` is the chord that opens the dialog: Ctrl+, (Cmd+, on a Mac).
pub fn is_chord(ev: &rune::KeyEvent) -> bool {
    ev.is_command(',') && !ev.mods.alt && !ev.mods.shift
}

impl super::Chat {
    /// Open the dialog from a button or the chord. It does not open over another dialog or the
    /// switcher (the guard the shortcuts page uses), and when it is open already the chord does
    /// nothing, so a held key does not flicker it. Returns whether it is open now.
    pub fn open_settings(&mut self) -> bool {
        if self.settings.open {
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
        self.settings.open();
        true
    }
}

/// A row of a section: the name and a line saying what the setting does on the left, its
/// control on the right.
pub(super) fn setting_row(key: &str, title: El, note: Option<El>, control: El, _p: &Palette) -> El {
    let mut words = col().grow(1.0).min_w(0.0).gap(3.0).child(title);
    if let Some(note) = note {
        words = words.child(note);
    }
    row()
        .key(key.to_owned())
        .w_full()
        .gap(16.0)
        .center_y()
        .child(words)
        .child(row().child(control))
}

/// The rows of a section with a thin rule between each.
pub(super) fn section_rows(key: &'static str, rows: Vec<El>, p: &Palette) -> El {
    let mut s = col().key(key).w_full().gap(14.0);
    for (i, r) in rows.into_iter().enumerate() {
        if i > 0 {
            s = s.child(rule(p));
        }
        s = s.child(r);
    }
    s
}

/// The Connection section: the node River talks to, the node's version, and where to see more.
fn connection_section(chat: &super::Chat, p: &Palette, k: &Kit) -> El {
    let ui = &chat.update;
    let node = &chat.node;
    let mut address_note = col().gap(2.0).child(
        label(node.choice.address.to_string(), st::BODY, p.text_secondary)
            .wrap()
            .w_full()
            .key("settings-node-address"),
    );
    address_note = address_note.child(
        label(
            if node.choice.address.is_loopback() {
                "The Freenet node on this computer.".to_owned()
            } else {
                "A Freenet node on another computer.".to_owned()
            },
            st::SMALL,
            p.text_muted,
        )
        .wrap()
        .w_full()
        .key("settings-node-where"),
    );
    if let Some(warning) = node.insecure() {
        address_note = address_note.child(
            label(warning, st::SMALL, p.warning_text)
                .wrap()
                .w_full()
                .key("settings-node-insecure"),
        );
    }
    let about = label(
        "Freenet updates itself in the background. River does not replace it, and never stops \
         a Freenet it did not start. River needs Freenet 0.2.156 or newer.",
        st::SMALL,
        p.text_muted,
    )
    .wrap()
    .w_full()
    .key("settings-node-note");
    let version_note = ui.node_note.as_ref().map(|note| {
        label(note.clone(), st::SMALL, p.text_secondary)
            .wrap()
            .w_full()
            .key("settings-node-changed")
    });
    section_rows(
        "settings-freenet",
        vec![
            setting_row(
                "settings-node-row",
                label("Node address", st::BODY, p.text),
                Some(address_note),
                Button::new("Change node")
                    .variant(ButtonVariant::Outline)
                    .on_press(|s: &mut RiverApp| {
                        s.chat.settings.close();
                        s.node_open();
                    })
                    .build(k)
                    .key("settings-node-change"),
                p,
            ),
            setting_row(
                "settings-version-node-row",
                label("Freenet version", st::BODY, p.text),
                version_note,
                label(
                    match &ui.node_version {
                        Some(v) => super::node_status::safe_version(v),
                        None => "Not connected".to_owned(),
                    },
                    st::BODY,
                    p.text,
                )
                .key("settings-node-version"),
                p,
            ),
            setting_row(
                "settings-node-status-row",
                label("Node status", st::BODY, p.text),
                Some(
                    label(
                        "Whether River is connected, and details you can copy into a bug report.",
                        st::SMALL,
                        p.text_muted,
                    )
                    .wrap()
                    .w_full(),
                ),
                Button::new("Node status")
                    .variant(ButtonVariant::Outline)
                    .on_press(|s: &mut RiverApp| {
                        s.chat.settings.close();
                        s.chat.status.open();
                    })
                    .build(k)
                    .key("settings-node-status"),
                p,
            ),
        ],
        p,
    )
    .child(about)
}

/// The body of `section`.
fn section_content(section: Section, chat: &super::Chat, p: &Palette, k: &Kit) -> El {
    match section {
        Section::Updates => super::update_view::updates_section(&chat.update, p, k),
        Section::Connection => connection_section(chat, p, k),
    }
}

/// A key a section row is moved by, as the step it takes in the list.
fn step_of(key: &Key) -> Option<Step> {
    match *key {
        Key::Up | Key::Left => Some(Step::Prev),
        Key::Down | Key::Right => Some(Step::Next),
        Key::Home => Some(Step::First),
        Key::End => Some(Step::Last),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum Step {
    Prev,
    Next,
    First,
    Last,
}

/// The section a step from `from` lands on. The list stops at its ends.
fn step_to(from: Section, step: Step) -> Section {
    let last = Section::ALL.len() - 1;
    let i = from.index();
    Section::ALL[match step {
        Step::Prev => i.saturating_sub(1),
        Step::Next => (i + 1).min(last),
        Step::First => 0,
        Step::Last => last,
    }]
}

/// One row of the list. `wide`: the row shows which section is open, and the arrow keys choose
/// a section as they move. Narrow: the row is a button that opens the section (arrow keys only
/// move between rows, and a chevron says it opens a screen).
fn section_row(section: Section, current: Section, wide: bool, p: &Palette) -> El {
    let on = wide && section == current;
    let ink = if on { p.text } else { p.text_secondary };
    let mut r = row()
        .key(section.key())
        .role(if wide { "tab" } else { "button" })
        .label(section.title())
        .focusable(true)
        .w_full()
        .h(if wide { 32.0 } else { 46.0 })
        .px(if wide { 10.0 } else { 4.0 })
        .gap(8.0)
        .center_y()
        .radius(6.0)
        .hover(p.row_hover)
        .cursor(rune::Cursor::Pointer)
        .child(label(
            section.title(),
            if on { st::ROOM_STRONG } else { st::ROOM },
            ink,
        ));
    if wide {
        r = r
            .focus_selected(on)
            // Only the open section is a Tab stop; the arrow keys move among the others.
            .tab_index(if on { 0 } else { -1 });
        if on {
            // The dialog opens with the keyboard on the open section's row.
            r = r.fill(p.surface).autofocus();
        }
    } else {
        if section == Section::ALL[0] {
            r = r.autofocus();
        }
        r = r
            .child(el().grow(1.0))
            .child(icon(&icons::CHEVRON_RIGHT, 14.0).color(p.text_muted))
            .border_sides([0.0, 0.0, 1.0, 0.0], p.line);
    }
    r.on_press(move |s: &mut RiverApp| s.chat.settings.select(section))
        .on_key(move |s: &mut RiverApp, ev, tx| {
            let Some(step) = step_of(&ev.key) else {
                return false;
            };
            // The arrow keys move one list; a modified arrow belongs to something else.
            if ev.mods.ctrl || ev.mods.alt || ev.mods.shift || ev.mods.logo {
                return false;
            }
            let to = step_to(section, step);
            if wide {
                // The list is automatic: the section shows as the row gets the keyboard.
                s.chat.settings.section = to;
            }
            tx.focus(to.key(), true);
            true
        })
}

/// The dialog, when open. `narrow`: the window is narrow, so the list and a section are
/// screens of their own. `win_h` is the window's height in layout points.
pub(super) fn dialog(chat: &super::Chat, narrow: bool, win_h: f32, k: &Kit, layer: &mut Vec<El>) {
    let ui = &chat.settings;
    if !ui.open {
        return;
    }
    let p = st::palette();
    let close = |s: &mut RiverApp| s.chat.settings.close();
    let list = |wide: bool| {
        let mut nav = col()
            .key("settings-nav")
            .role("tablist")
            .label("Settings sections")
            .gap(2.0);
        for section in Section::ALL {
            nav = nav.child(section_row(*section, ui.section, wide, &p));
        }
        nav
    };
    let heading = label(ui.section.title(), st::HEADER, p.text).key("settings-heading");
    if narrow && !ui.in_section {
        DialogSpec::new("settings", "Settings", close)
            .body_key("settings-body")
            .width(460.0)
            .gap(0.0)
            .child(list(false).w_full())
            .build(k, layer);
        return;
    }
    let content = section_content(ui.section, chat, &p, k);
    // What the dialog leaves of the window: its title and margins.
    let pane_h = (win_h - 170.0).clamp(240.0, 340.0);
    if narrow {
        DialogSpec::new("settings", "Settings", close)
            .body_key("settings-body")
            .width(460.0)
            .gap(14.0)
            .child(
                row().w_full().child(
                    Button::new("Back")
                        .variant(ButtonVariant::Ghost)
                        .on_press(|s: &mut RiverApp| s.chat.settings.back())
                        .build(k)
                        .key("settings-back")
                        .autofocus(),
                ),
            )
            .child(
                col()
                    .key("settings-pane")
                    .role("tabpanel")
                    .label(ui.section.title())
                    .w_full()
                    .gap(14.0)
                    .max_h(pane_h - 50.0)
                    .scroll(rune::ScrollSpec {
                        stick_bottom: false,
                    })
                    .child(heading)
                    .child(content),
            )
            .build(k, layer);
        return;
    }
    DialogSpec::new("settings", "Settings", close)
        .body_key("settings-body")
        .width(700.0)
        .flush()
        .child(
            row()
                .key("settings-layout")
                .w_full()
                .h(pane_h)
                .child(col().w(190.0).h_full().px(12.0).py(4.0).child(list(true)))
                .child(el().w(1.0).h_full().fill(p.line))
                .child(
                    col()
                        .key("settings-pane")
                        .role("tabpanel")
                        .label(ui.section.title())
                        .grow(1.0)
                        .min_w(0.0)
                        .h_full()
                        .px(24.0)
                        .py(8.0)
                        .gap(14.0)
                        .scroll(rune::ScrollSpec {
                            stick_bottom: false,
                        })
                        .child(heading)
                        .child(content),
                ),
        )
        .build(k, layer);
}
