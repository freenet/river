//! The Settings dialog: one dialog, opened from the gear (or Ctrl+, / Cmd+,), with a list of
//! sections on the left and the chosen section on the right.
//!
//! In a narrow window (the same width the rest of the app calls narrow, [`NARROW_WIDTH`]) the
//! list is a screen of its own: pressing a section opens it, and Back returns to the list.
//!
//! Sections so far: **Updates** (River's own updates, [`super::update_view::updates_section`]),
//! **Notifications** (do not disturb and message previews, [`notifications_section`]),
//! **Background and startup** (the tray and login item, only where the shell offers them,
//! [`super::background::background_section`]) and **Connection** (the Freenet node,
//! [`connection_section`]). A section exists only when it has content ([`Section::shown`]). A new
//! section is an entry in [`Section::ALL`] and an arm in [`section_content`]; the rest (list,
//! keys, narrow screens, arrow keys) follows from those.
//! Which section was last shown is not remembered: the dialog opens on the first section, and
//! nothing about it is saved (the settings themselves keep their own saved choices).
//!
//! Esc closes the whole dialog on every screen, including a section screen of a narrow window;
//! Back is the way to the list. Opening from the keyboard hands the keyboard back to the
//! composer on close when the composer had it ([`SettingsUi::restore_composer`]; the shortcuts
//! page does not do this). Not in Settings yet: Export and Import ID, zoom and the shortcuts
//! page, which are still the room list's footer button and their own dialogs. The room list keeps
//! one quiet Do not disturb row that flips the switch ([`super::dnd::Chat::quick_dnd`]).
//!
//! The dialog has no Save or Close button: every control in it acts at once, and the X closes
//! it. Keys are stable (`settings-section-{id}`, `settings-back`, and the keys of the controls
//! inside the sections), because tests and the end-to-end script find elements by key.

use std::rc::Rc;

use rune::{El, Key, col, el, icon, row};
use rune_kit::{Button, ButtonVariant, Kit, Switch};

use super::dialog_kit::{DialogSpec, rule};
use super::icons;
use super::style::{self as st, Palette};
use super::view::label;
use crate::RiverApp;

/// A section of the dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Updates,
    Notifications,
    Background,
    Connection,
}

impl Section {
    /// Every section, in the order the list shows them.
    pub const ALL: &'static [Section] = &[
        Section::Updates,
        Section::Notifications,
        Section::Background,
        Section::Connection,
    ];

    /// The sections the list shows: all of them, but Background and startup only where the
    /// shell offers it.
    pub fn shown(chat: &super::Chat) -> Vec<Section> {
        Section::ALL
            .iter()
            .copied()
            .filter(|s| *s != Section::Background || chat.background.is_some())
            .collect()
    }

    /// The name in the list and the heading of the section (sentence case).
    pub fn title(self) -> &'static str {
        match self {
            Section::Updates => "Updates",
            Section::Notifications => "Notifications",
            Section::Background => "Background and startup",
            Section::Connection => "Connection",
        }
    }

    /// The part of the key that names it: `settings-section-{id}`.
    pub fn id(self) -> &'static str {
        match self {
            Section::Updates => "updates",
            Section::Notifications => "notifications",
            Section::Background => "background",
            Section::Connection => "connection",
        }
    }

    fn key(self) -> String {
        format!("settings-section-{}", self.id())
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
    /// The composer had the keyboard when the dialog opened, so closing gives it back.
    pub restore_composer: bool,
}

impl Default for SettingsUi {
    fn default() -> Self {
        SettingsUi {
            open: false,
            section: Section::ALL[0],
            in_section: false,
            restore_composer: false,
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
            restore_composer: false,
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
        // The dialog takes the keyboard; the composer gets it back when the dialog closes.
        // (Opening also closes the emoji picker, a reaction picker and the narrow members
        // panel, which would otherwise sit behind or beside the dialog.)
        let had_keyboard = self.composer.focused;
        self.composer.focused = false;
        self.settings.open();
        self.settings.restore_composer = had_keyboard;
        self.track_settings_sections();
        true
    }

    /// Close the dialog (X, Esc, a press outside), giving the keyboard back to the composer if
    /// it had it when the dialog opened.
    pub fn close_settings(&mut self) {
        if std::mem::take(&mut self.settings.restore_composer) {
            self.composer.focused = true;
        }
        self.settings.close();
        self.track_settings_sections();
    }

    /// Show `section` (a press, Enter or an arrow key on its row) and bring what it shows up to
    /// date at once.
    pub fn select_settings_section(&mut self, section: Section) {
        self.settings.select(section);
        self.track_settings_sections();
    }

    /// Keep what a section shows in step with whether it is on screen: the schedule's text
    /// fields (Notifications) and what the shell says about the tray and login item
    /// (Background and startup, read again each time it comes on screen). Run every frame and
    /// whenever the dialog opens or closes.
    pub fn track_settings_sections(&mut self) {
        self.track_dnd_editor();
        let showing = self.settings.open && self.settings.section == Section::Background;
        if let Some(b) = &mut self.background {
            if showing && !b.open {
                b.opened();
            } else if !showing && b.open {
                b.open = false;
            }
        }
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
                    .w_full()
                    .key("settings-node-status-note"),
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
        Section::Notifications => notifications_section(chat, p, k),
        Section::Background => super::background::background_section(chat, p, k),
        Section::Connection => connection_section(chat, p, k),
    }
}

/// The Notifications section: do not disturb, whether notifications show who wrote and what,
/// and where the choices for one room live. There is no switch for notifications as a whole:
/// each room has its own, and do not disturb is the way to silence all of them at once.
fn notifications_section(chat: &super::Chat, p: &Palette, k: &Kit) -> El {
    let previews = setting_row(
        "settings-previews-row",
        label(
            "Show the sender and text in notifications",
            st::BODY,
            p.text,
        ),
        Some(
            label(
                "Off by default, so nothing private shows on a locked screen or in a shared \
                 screen. Off: a private room's notification shows only who wrote, and a direct \
                 message shows no name or text.",
                st::SMALL,
                p.text_muted,
            )
            .wrap()
            .w_full()
            .key("settings-previews-note"),
        ),
        Switch::new(chat.private_previews)
            .label("Show the sender and text in notifications")
            .hide_label()
            .on_toggle(|s: &mut RiverApp| s.chat.toggle_previews())
            .build(k)
            .key("settings-previews"),
        p,
    );
    col()
        .key("settings-notifications")
        .w_full()
        .gap(18.0)
        .child(super::dnd::dnd_group(chat, p, k))
        .child(
            col()
                .key("settings-previews-group")
                .w_full()
                .gap(12.0)
                .child(label("Message text", st::ROOM_STRONG, p.text))
                .child(section_rows("settings-previews-rows", vec![previews], p)),
        )
        .child(
            label(
                "Notifications for one room (all messages, mentions only, or muted) are set from \
                 the bell in that room's header. Do not disturb is the one setting that silences \
                 every room at once.",
                st::SMALL,
                p.text_muted,
            )
            .wrap()
            .w_full()
            .key("settings-notifications-note"),
        )
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

/// The section a step from `from` lands on in the list `order`. The list stops at its ends.
fn step_to(order: &[Section], from: Section, step: Step) -> Section {
    let last = order.len() - 1;
    let i = order.iter().position(|s| *s == from).unwrap_or(0);
    order[match step {
        Step::Prev => i.saturating_sub(1),
        Step::Next => (i + 1).min(last),
        Step::First => 0,
        Step::Last => last,
    }]
}

/// One row of the list. `wide`: the row shows which section is open, and the arrow keys choose
/// a section as they move. Narrow: the row is a button that opens the section (arrow keys only
/// move between rows, and a chevron says it opens a screen).
fn section_row(
    order: Rc<Vec<Section>>,
    section: Section,
    current: Section,
    wide: bool,
    new: bool,
    p: &Palette,
) -> El {
    let on = wide && section == current;
    let ink = if on { p.text } else { p.text_secondary };
    let mut r = row()
        .key(section.key())
        .role(if wide { "tab" } else { "button" })
        // A section with something new says so to assistive technology as well as with a dot.
        .label(if new {
            format!("{}, new", section.title())
        } else {
            section.title().to_owned()
        })
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
    if new {
        r = r.child(
            el().key(format!("{}-new", section.key()))
                .size(7.0, 7.0)
                .radius(3.5)
                .fill(p.accent)
                .a11y_hidden(),
        );
    }
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
        if section == order[0] {
            r = r.autofocus();
        }
        r = r
            .child(el().grow(1.0))
            .child(icon(&icons::CHEVRON_RIGHT, 14.0).color(p.text_muted))
            .border_sides([0.0, 0.0, 1.0, 0.0], p.line);
    }
    r.on_press(move |s: &mut RiverApp| s.chat.select_settings_section(section))
        .on_key(move |s: &mut RiverApp, ev, tx| {
            if ev.key == Key::Enter {
                // Space presses a focused row; Enter does too, as on a button.
                s.chat.select_settings_section(section);
                return true;
            }
            let Some(step) = step_of(&ev.key) else {
                return false;
            };
            // The arrow keys move one list; a modified arrow belongs to something else.
            if ev.mods.ctrl || ev.mods.alt || ev.mods.shift || ev.mods.logo {
                return false;
            }
            let to = step_to(&order, section, step);
            if wide {
                // The list is automatic: the section shows as the row gets the keyboard.
                s.chat.select_settings_section(to);
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
    let close = |s: &mut RiverApp| s.chat.close_settings();
    let order = Rc::new(Section::shown(chat));
    let list = |wide: bool| {
        // Tabs only where they are tabs: the wide list. The narrow list is a screen of buttons.
        let mut nav = col()
            .key("settings-nav")
            .role(if wide { "tablist" } else { "group" })
            .label("Settings sections")
            .gap(2.0);
        for section in order.iter() {
            nav = nav.child(section_row(
                order.clone(),
                *section,
                ui.section,
                wide,
                *section == Section::Connection && chat.update.node_note.is_some(),
                &p,
            ));
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
    let pane_h = (win_h - 170.0).clamp(240.0, 460.0);
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
                    .role("group")
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
