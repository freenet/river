//! The Settings dialog driven headlessly: opening and closing, the section list, the keyboard,
//! the narrow layout, and that what it holds still behaves as it did when it was spread over
//! a footer button and two dialogs.

use rune::{Env, Key, KeyEvent, Mods, Runtime};

use super::example;
use super::settings::{Section, is_chord};
use crate::{NARROW_SCENE_SIZE, RiverApp, Setup, WINDOW_SIZE, view};

fn connected() -> RiverApp {
    let mut s = RiverApp::detached();
    s.setup = Setup::Connected {
        version: "0.2.155".into(),
    };
    s
}

/// A shell with a tray (or without), for the Background and startup section.
struct Shell {
    tray: bool,
    close_to_tray: std::sync::Mutex<bool>,
    login: std::sync::Mutex<bool>,
}

impl super::BackgroundHost for Shell {
    fn tray_available(&self) -> bool {
        self.tray
    }
    fn close_to_tray(&self) -> bool {
        *self.close_to_tray.lock().unwrap()
    }
    fn set_close_to_tray(&self, on: bool) -> Result<(), String> {
        *self.close_to_tray.lock().unwrap() = on;
        Ok(())
    }
    fn start_at_login(&self) -> bool {
        *self.login.lock().unwrap()
    }
    fn set_start_at_login(&self, on: bool) -> Result<(), String> {
        *self.login.lock().unwrap() = on;
        Ok(())
    }
}

fn shell(tray: bool) -> std::sync::Arc<Shell> {
    std::sync::Arc::new(Shell {
        tray,
        close_to_tray: std::sync::Mutex::new(true),
        login: std::sync::Mutex::new(false),
    })
}

/// A connected app whose shell offers Background and startup, and keeps a log.
fn with_shell(tray: bool) -> (RiverApp, std::sync::Arc<Shell>) {
    let mut s = connected();
    let host = shell(tray);
    s.chat.background = Some(super::BackgroundUi::new(host.clone()));
    s.chat.diagnostics = Some(super::DiagnosticsUi::new(log_host()));
    (s, host)
}

/// A shell with a log file, which remembers how often its folder was opened and can refuse.
struct LogShell {
    opened: std::sync::atomic::AtomicUsize,
    refuse: bool,
    /// Keeps no file: stderr goes elsewhere.
    elsewhere: bool,
    /// Keeps no file and has nowhere to write.
    unavailable: bool,
    /// Was writing to its file and stopped.
    stopped: bool,
}

impl super::LogHost for LogShell {
    fn log_state(&self) -> super::LogState {
        if self.stopped {
            super::LogState::Stopped
        } else if self.unavailable {
            super::LogState::Unavailable
        } else if self.elsewhere {
            super::LogState::Elsewhere
        } else {
            super::LogState::File("/home/ada/.config/River/logs/river.log".into())
        }
    }
    fn open_log_folder(&self) -> Result<(), String> {
        self.opened
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.refuse {
            Err("couldn't open /home/ada/.config/River/logs: no file manager".into())
        } else {
            Ok(())
        }
    }
}

fn log_host() -> std::sync::Arc<LogShell> {
    std::sync::Arc::new(LogShell {
        opened: 0.into(),
        refuse: false,
        elsewhere: false,
        unavailable: false,
        stopped: false,
    })
}

fn render(state: RiverApp, size: (f32, f32)) -> Runtime<RiverApp> {
    let env = Env {
        system_dark: true,
        scale: 1.0,
    };
    let mut rt = Runtime::new(state, view, size, env);
    rt.on_key = crate::spec().on_key;
    rt.frame(0.0);
    rt.frame(1.0 / 60.0);
    rt
}

fn settle(rt: &mut Runtime<RiverApp>) {
    for i in 0..6 {
        rt.frame(1.0 + f64::from(i) / 60.0);
    }
}

fn press(rt: &mut Runtime<RiverApp>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no element {key}"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    settle(rt);
}

fn key(rt: &mut Runtime<RiverApp>, key: Key) {
    rt.key(KeyEvent {
        key,
        mods: Mods::default(),
    });
    settle(rt);
}

fn texts(rt: &Runtime<RiverApp>) -> Vec<String> {
    rt.elements().into_iter().filter_map(|e| e.text).collect()
}

fn shows(rt: &Runtime<RiverApp>, needle: &str) -> bool {
    texts(rt).iter().any(|t| t.contains(needle))
}

fn focused(rt: &Runtime<RiverApp>) -> Option<String> {
    rt.elements()
        .into_iter()
        .find(|e| e.focused)
        .and_then(|e| e.key)
}

fn open(size: (f32, f32)) -> Runtime<RiverApp> {
    let mut rt = render(connected(), size);
    press(&mut rt, "settings");
    rt
}

fn is_open(rt: &Runtime<RiverApp>) -> bool {
    rt.find("settings-dialog").is_some()
}

#[test]
fn the_gear_opens_settings_and_the_x_closes_it() {
    let mut rt = render(connected(), WINDOW_SIZE);
    assert!(!is_open(&rt));
    let gear = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("settings"))
        .expect("the gear");
    assert_eq!(gear.role, "button");
    assert_eq!(gear.label.as_deref(), Some("Settings"));
    press(&mut rt, "settings");
    assert!(is_open(&rt));
    assert!(shows(&rt, "Settings"));
    // One section at a time, the first on open.
    assert_eq!(rt.state.chat.settings.section, Section::Updates);
    assert!(rt.find("settings-section-updates").is_some());
    assert!(rt.find("settings-section-connection").is_some());
    press(&mut rt, "close");
    assert!(!is_open(&rt));
}

#[test]
fn escape_closes_settings_from_every_section() {
    for section in Section::ALL {
        let (s, _) = with_shell(true);
        let mut rt = render(s, WINDOW_SIZE);
        press(&mut rt, "settings");
        press(&mut rt, &format!("settings-section-{}", section.id()));
        assert!(is_open(&rt));
        key(&mut rt, Key::Escape);
        assert!(!is_open(&rt), "{section:?}");
    }
}

#[test]
fn the_keyboard_starts_on_the_section_list() {
    let rt = open(WINDOW_SIZE);
    assert_eq!(focused(&rt).as_deref(), Some("settings-section-updates"));
}

#[test]
fn pressing_a_section_shows_it_and_only_it() {
    let mut rt = open(WINDOW_SIZE);
    assert!(rt.find("settings-auto").is_some(), "Updates is first");
    assert!(rt.find("settings-node-change").is_none());
    press(&mut rt, "settings-section-connection");
    assert_eq!(rt.state.chat.settings.section, Section::Connection);
    assert!(rt.find("settings-node-change").is_some());
    assert!(rt.find("settings-auto").is_none());
    let heading = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("settings-heading"))
        .and_then(|e| e.text);
    assert_eq!(heading.as_deref(), Some("Connection"));
    press(&mut rt, "settings-section-updates");
    assert!(rt.find("settings-auto").is_some());
}

#[test]
fn every_section_has_content_and_a_name() {
    for section in Section::ALL {
        let (mut s, _) = with_shell(true);
        s.chat.settings.open_at(*section);
        let rt = render(s, WINDOW_SIZE);
        let name = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(&format!("settings-section-{}", section.id())))
            .unwrap_or_else(|| panic!("no row for {section:?}"));
        assert_eq!(name.role, "tab");
        assert_eq!(name.label.as_deref(), Some(section.title()));
        let pane = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("settings-pane"))
            .expect("the pane");
        assert_eq!(pane.role, "tabpanel");
        assert_eq!(pane.label.as_deref(), Some(section.title()));
        // A heading and at least one setting: an empty section would be a dead list entry.
        let rows = rt
            .elements()
            .into_iter()
            .filter(|e| e.key.as_deref().is_some_and(|k| k.ends_with("-row")))
            .count();
        assert!(rows >= 1, "{section:?} shows no setting");
    }
}

#[test]
fn the_arrow_keys_choose_sections_and_move_the_keyboard_with_them() {
    let mut rt = open(WINDOW_SIZE);
    key(&mut rt, Key::Down);
    assert_eq!(rt.state.chat.settings.section, Section::Notifications);
    assert_eq!(
        focused(&rt).as_deref(),
        Some("settings-section-notifications")
    );
    assert!(rt.find("dnd-switch").is_some());
    // Without a shell there is no Background section to step onto.
    key(&mut rt, Key::Down);
    assert_eq!(rt.state.chat.settings.section, Section::Connection);
    assert!(rt.find("settings-node-change").is_some());
    key(&mut rt, Key::Down);
    assert_eq!(rt.state.chat.settings.section, Section::Identity);
    // The list stops at its end.
    key(&mut rt, Key::Down);
    assert_eq!(rt.state.chat.settings.section, Section::Appearance);
    key(&mut rt, Key::Down);
    assert_eq!(rt.state.chat.settings.section, Section::About);
    key(&mut rt, Key::Down);
    assert_eq!(rt.state.chat.settings.section, Section::About);
    for _ in 0..5 {
        key(&mut rt, Key::Up);
    }
    assert_eq!(rt.state.chat.settings.section, Section::Updates);
    assert_eq!(focused(&rt).as_deref(), Some("settings-section-updates"));
    key(&mut rt, Key::Up);
    assert_eq!(rt.state.chat.settings.section, Section::Updates);
    key(&mut rt, Key::End);
    assert_eq!(rt.state.chat.settings.section, Section::About);
    key(&mut rt, Key::Home);
    assert_eq!(rt.state.chat.settings.section, Section::Updates);
    assert!(is_open(&rt), "arrows never close it");
}

#[test]
fn tab_leaves_the_list_for_the_section_and_does_not_visit_each_row() {
    let mut s = connected();
    // A copy that updates itself, so the switch is a control and not a disabled one.
    s.chat.update.support = crate::update::UpdateSupport::Automatic;
    let mut rt = render(s, WINDOW_SIZE);
    press(&mut rt, "settings");
    assert_eq!(focused(&rt).as_deref(), Some("settings-section-updates"));
    key(&mut rt, Key::Tab);
    let now = focused(&rt).expect("Tab focuses something");
    assert!(
        !now.starts_with("settings-section-"),
        "Tab went to another list row: {now}"
    );
    assert_eq!(
        now, "settings-check-now",
        "the first control of the section"
    );
    key(&mut rt, Key::Tab);
    assert_eq!(focused(&rt).as_deref(), Some("settings-auto"));
}

#[test]
fn the_chord_opens_settings_once_and_never_over_another_dialog() {
    let ctrl_comma = || KeyEvent {
        key: Key::Command(','),
        mods: Mods {
            ctrl: true,
            cmd: true,
            ..Mods::default()
        },
    };
    assert!(is_chord(&ctrl_comma()));
    assert!(!is_chord(&KeyEvent {
        key: Key::Command('.'),
        mods: ctrl_comma().mods,
    }));
    let mut rt = render(connected(), WINDOW_SIZE);
    rt.key(ctrl_comma());
    settle(&mut rt);
    assert!(is_open(&rt));
    // A held key repeats the chord; the dialog stays as it is.
    press(&mut rt, "settings-section-connection");
    rt.key(ctrl_comma());
    settle(&mut rt);
    assert!(is_open(&rt));
    assert_eq!(rt.state.chat.settings.section, Section::Connection);

    // Not over the shortcuts page.
    let mut rt = render(connected(), WINDOW_SIZE);
    rt.update(|s, _| {
        s.chat.open_shortcuts();
    });
    settle(&mut rt);
    rt.key(ctrl_comma());
    settle(&mut rt);
    assert!(!is_open(&rt));
    assert!(rt.find("shortcuts-dialog").is_some());
}

#[test]
fn the_shortcuts_page_lists_the_chord_and_the_gear_names_it() {
    assert!(
        super::shortcuts::SHORTCUTS
            .iter()
            .any(|s| s.chord == "Mod+," && s.commands == [','])
    );
    assert_eq!(super::shortcuts::shown("Mod+,", false), "Ctrl+,");
    assert_eq!(super::shortcuts::shown("Mod+,", true), "Cmd+,");
    // The gear's tooltip is built from that chord, not from a typed copy of it.
    let view = include_str!("view.rs");
    assert!(
        view.contains("\"Settings ({})\"") && view.contains("shown(\"Mod+,\""),
        "the gear's help text names the chord through shortcuts::shown"
    );
}

#[test]
fn a_narrow_window_shows_the_list_first_then_a_section_with_back() {
    let mut rt = open(NARROW_SCENE_SIZE);
    // The list is a screen of its own.
    assert!(rt.find("settings-section-updates").is_some());
    assert!(rt.find("settings-section-connection").is_some());
    assert!(rt.find("settings-pane").is_none());
    assert!(rt.find("settings-back").is_none());
    press(&mut rt, "settings-section-connection");
    assert!(rt.find("settings-pane").is_some());
    assert!(rt.find("settings-node-change").is_some());
    assert!(rt.find("settings-section-updates").is_none());
    press(&mut rt, "settings-back");
    assert!(rt.find("settings-pane").is_none());
    assert!(rt.find("settings-section-updates").is_some());
    // The Updates content is reachable, and works, from its screen.
    press(&mut rt, "settings-section-updates");
    assert!(rt.find("settings-auto").is_some());
    assert!(rt.find("settings-check-now").is_some());
    key(&mut rt, Key::Escape);
    assert!(!is_open(&rt), "Escape closes from a section screen too");
}

#[test]
fn opening_again_starts_on_the_first_section_and_remembers_nothing() {
    let mut rt = open(WINDOW_SIZE);
    press(&mut rt, "settings-section-connection");
    press(&mut rt, "close");
    press(&mut rt, "settings");
    assert_eq!(rt.state.chat.settings.section, Section::Updates);
    assert!(rt.find("settings-auto").is_some());
}

#[test]
fn change_node_is_in_connection_and_opens_the_form() {
    let mut rt = open(WINDOW_SIZE);
    press(&mut rt, "settings-section-connection");
    let address = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("settings-node-address"))
        .and_then(|e| e.text)
        .expect("the node address");
    assert_eq!(address, rt.state.chat.node.choice.address.to_string());
    press(&mut rt, "settings-node-change");
    assert!(!is_open(&rt), "the form takes the screen");
    assert!(rt.find("node-dialog").is_some());
    assert!(rt.find("node-input").is_some());
}

#[test]
fn node_status_is_still_one_press_from_the_status_pill_and_from_settings() {
    let mut s = connected();
    s.chat = super::Chat::new(
        super::Source::Node(super::LinkStatus::Live),
        Vec::new(),
        0,
        0,
        0,
    );
    let rooms = example::data().rooms;
    s.chat
        .show_node_rooms(rooms, None, super::LinkStatus::Live, 29_000_000);
    let mut rt = render(s, WINDOW_SIZE);
    // The old entry points: the connection pill opens the status. While connected there is no
    // Change node link in the footer; it appears when the link is lost.
    assert!(rt.find("node-change").is_none());
    press(&mut rt, "connection-pill");
    assert!(rt.find("node-status-dialog").is_some());
    press(&mut rt, "ns-close");
    rt.update(|s, _| s.chat.source = super::Source::Node(super::LinkStatus::Reconnecting));
    settle(&mut rt);
    press(&mut rt, "node-change");
    assert!(rt.find("node-dialog").is_some());
    press(&mut rt, "node-cancel");
    // And Settings reaches the same two.
    press(&mut rt, "settings");
    press(&mut rt, "settings-section-connection");
    press(&mut rt, "settings-node-status");
    assert!(rt.find("node-status-dialog").is_some());
    assert!(!is_open(&rt));
}

#[test]
fn the_old_entry_points_are_still_in_the_source() {
    // Settings added a home for the node address, Change and Node status without taking the
    // old ones away. The behaviour is tested above; this pins the exact keys in both files.
    let view = include_str!("view.rs");
    let node_ui = include_str!("node_ui.rs");
    for (file, src, key) in [
        ("view.rs", view, ".key(\"connection-pill\")"),
        ("view.rs", view, "node_ui::change_link("),
        ("node_ui.rs", node_ui, ".key(\"node-change\")"),
    ] {
        assert!(src.contains(key), "{key} left {file}");
    }
    // The footer buttons that are still the only way to their dialogs.
    for footer in ["dnd_entry", "\"Enter invite code\""] {
        assert!(view.contains(footer), "{footer} left the room list");
    }
    // Moved into Settings > Identity and Appearance.
    assert!(
        !view.contains("shortcuts::entry("),
        "shortcuts::entry( is back in view.rs"
    );
}

#[test]
fn zoomed_in_the_dialog_still_fits_and_scrolls() {
    let mut s = connected();
    s.chat.settings.open_at(Section::Connection);
    let rt = render(s, (640.0, 480.0));
    let dialog = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("settings-dialog"))
        .expect("the dialog");
    assert!(
        dialog.rect[3] <= 480.0,
        "taller than the window: {:?}",
        dialog.rect
    );
    assert!(
        dialog.rect[2] <= 640.0,
        "wider than the window: {:?}",
        dialog.rect
    );
}

#[test]
fn the_update_switch_is_a_named_switch_and_the_toggle_is_unchanged() {
    let mut s = connected();
    s.chat.update.support = crate::update::UpdateSupport::Automatic;
    s.chat.update.auto = true;
    let mut rt = render(s, WINDOW_SIZE);
    press(&mut rt, "settings");
    let sw = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("settings-auto"))
        .expect("the switch");
    assert_eq!(sw.role, "switch");
    assert_eq!(sw.label.as_deref(), Some("Update River automatically"));
    press(&mut rt, "settings-auto");
    assert!(!rt.state.chat.update.auto);
}

fn role_of(rt: &Runtime<RiverApp>, key: &str) -> (&'static str, Option<String>) {
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("no element {key}"));
    (e.role, e.label)
}

#[test]
fn roles_are_tabs_only_where_they_are_tabs() {
    // Wide: a tab list of tabs, and the section is its panel.
    let rt = open(WINDOW_SIZE);
    assert_eq!(role_of(&rt, "settings-nav").0, "tablist");
    assert_eq!(role_of(&rt, "settings-section-updates").0, "tab");
    assert_eq!(role_of(&rt, "settings-pane").0, "tabpanel");
    // Narrow list screen: a group of buttons, no tabs.
    let mut rt = open(NARROW_SCENE_SIZE);
    assert_eq!(
        role_of(&rt, "settings-nav"),
        ("group", Some("Settings sections".into()))
    );
    assert_eq!(
        role_of(&rt, "settings-section-connection"),
        ("button", Some("Connection".into()))
    );
    // Narrow section screen: a plain group with the section's name, and Back is a button.
    press(&mut rt, "settings-section-connection");
    assert_eq!(
        role_of(&rt, "settings-pane"),
        ("group", Some("Connection".into()))
    );
    assert_eq!(role_of(&rt, "settings-back").0, "button");
    assert!(
        !rt.elements()
            .into_iter()
            .any(|e| e.role == "tab" || e.role == "tablist" || e.role == "tabpanel"),
        "no tab roles in the narrow layout"
    );
}

#[test]
fn a_node_note_marks_the_connection_section_and_the_update_notice_comes_first() {
    // No note: no marker, and the row's name is just its title.
    let rt = open(WINDOW_SIZE);
    assert!(rt.find("settings-section-connection-new").is_none());
    assert_eq!(
        role_of(&rt, "settings-section-connection").1.as_deref(),
        Some("Connection")
    );
    // A note: a dot on the row and "new" in its accessible name, wide and narrow.
    for size in [WINDOW_SIZE, NARROW_SCENE_SIZE] {
        let mut s = connected();
        s.chat.update.node_note =
            Some("Freenet updated itself from 0.2.158 to 0.2.160 while River was open.".into());
        s.chat.settings.open();
        let rt = render(s, size);
        assert!(
            rt.find("settings-section-connection-new").is_some(),
            "{size:?}"
        );
        assert_eq!(
            role_of(&rt, "settings-section-connection").1.as_deref(),
            Some("Connection, new")
        );
    }
    // The rollback notice is above the rows of Updates, so a short pane never hides it.
    let mut s = connected();
    s.chat.update.notice = Some("River 0.2.0 did not start, so River went back to 0.1.0.".into());
    s.chat.settings.open();
    let rt = render(s, WINDOW_SIZE);
    let y = |k: &str| rt.find(k).unwrap_or_else(|| panic!("no {k}")).1;
    assert!(y("settings-notice") < y("settings-version-row"));
    // And in a short window it is in view.
    let mut s = connected();
    s.chat.update.notice = Some("River 0.2.0 did not start, so River went back to 0.1.0.".into());
    s.chat.settings.open_at(Section::Updates);
    let rt = render(s, (640.0, 480.0));
    assert!(visible(&rt, "settings-notice"));
}

fn visible(rt: &Runtime<RiverApp>, key: &str) -> bool {
    rt.elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key))
        .is_some_and(|e| e.visible[2] > 0.0 && e.visible[3] > 0.0)
}

#[test]
fn closing_from_the_keyboard_chord_gives_the_composer_its_keyboard_back() {
    let mut rt = render(connected(), WINDOW_SIZE);
    press(&mut rt, "composer-field");
    assert!(rt.state.chat.composer.focused);
    rt.key(KeyEvent {
        key: Key::Command(','),
        mods: Mods {
            ctrl: true,
            cmd: true,
            ..Mods::default()
        },
    });
    settle(&mut rt);
    assert!(is_open(&rt));
    assert!(!rt.state.chat.composer.focused);
    key(&mut rt, Key::Escape);
    assert!(!is_open(&rt));
    assert!(rt.state.chat.composer.focused, "the composer had it");
    assert_eq!(focused(&rt).as_deref(), Some("composer-field"));
    // Opened with nothing focused in the composer, closing does not take the keyboard.
    let mut rt = open(WINDOW_SIZE);
    key(&mut rt, Key::Escape);
    assert!(!rt.state.chat.composer.focused);
}

#[test]
fn space_presses_the_switch_and_enter_or_space_opens_a_narrow_section() {
    let mut s = connected();
    s.chat.update.support = crate::update::UpdateSupport::Automatic;
    s.chat.update.auto = true;
    let mut rt = render(s, WINDOW_SIZE);
    press(&mut rt, "settings");
    key(&mut rt, Key::Tab);
    key(&mut rt, Key::Tab);
    assert_eq!(focused(&rt).as_deref(), Some("settings-auto"));
    key(&mut rt, Key::Text(" ".into()));
    assert!(!rt.state.chat.update.auto, "Space toggles the switch");
    key(&mut rt, Key::Text(" ".into()));
    assert!(rt.state.chat.update.auto);

    for open_key in [Key::Enter, Key::Text(" ".into())] {
        let mut rt = open(NARROW_SCENE_SIZE);
        assert_eq!(focused(&rt).as_deref(), Some("settings-section-updates"));
        key(&mut rt, Key::Down);
        assert_eq!(
            focused(&rt).as_deref(),
            Some("settings-section-notifications")
        );
        assert!(
            rt.find("settings-pane").is_none(),
            "arrows only move focus here"
        );
        key(&mut rt, open_key.clone());
        assert!(
            rt.find("settings-pane").is_some(),
            "{open_key:?} opens the section"
        );
        assert_eq!(rt.state.chat.settings.section, Section::Notifications);
    }
}

#[test]
fn resizing_across_the_narrow_width_keeps_the_section_and_the_keyboard() {
    let mut rt = render(connected(), WINDOW_SIZE);
    press(&mut rt, "settings");
    press(&mut rt, "settings-section-connection");
    assert!(rt.find("settings-pane").is_some() && rt.find("settings-nav").is_some());
    // Zoom lays the window out narrower, as a smaller window would.
    rt.update(|s, _| s.chat.zoom.load(150));
    settle(&mut rt);
    assert!(rt.state.chat.narrow);
    assert!(is_open(&rt));
    // Narrow: the section the person was on is what shows, with Back. (The wide list row
    // that had the keyboard is gone.)
    assert!(rt.find("settings-back").is_some());
    assert!(rt.find("settings-node-change").is_some());
    let f = focused(&rt);
    assert!(
        f.as_deref()
            .is_some_and(|k| k.starts_with("settings-") || k == "close"),
        "the keyboard is in the dialog, not lost: {f:?}"
    );
    // The keyboard still works: Escape closes.
    rt.update(|s, _| s.chat.zoom.load(100));
    settle(&mut rt);
    assert!(!rt.state.chat.narrow);
    assert!(rt.find("settings-section-connection").is_some());
    let f = focused(&rt);
    assert!(
        f.as_deref()
            .is_some_and(|k| k.starts_with("settings-") || k == "close"),
        "the keyboard is in the dialog, not lost: {f:?}"
    );
    key(&mut rt, Key::Escape);
    assert!(!is_open(&rt));
}

fn find(rt: &Runtime<RiverApp>, key: &str) -> rune::ElementInfo {
    rt.elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("no element {key}"))
}

fn state_line(rt: &Runtime<RiverApp>) -> String {
    find(rt, "dnd-state").text.expect("the state line")
}

fn notifications_at(minute: u16, manual: bool, schedule: Option<(u16, u16)>) -> RiverApp {
    let mut s = connected();
    s.chat.dnd.pinned = true;
    s.chat.dnd.now = minute;
    s.chat.dnd.settings.manual = manual;
    s.chat.dnd.settings.schedule = schedule.map(|(start, end)| super::DndSchedule { start, end });
    s.chat.settings.open_at(Section::Notifications);
    s.chat.track_settings_sections();
    s
}

#[test]
fn the_state_line_on_screen_is_true_for_off_switch_schedule_and_both() {
    let night = Some((22 * 60, 7 * 60));
    let cases = [
        (12 * 60, false, None, "Do not disturb is off."),
        (
            12 * 60,
            true,
            None,
            "Do not disturb is on until you turn it off.",
        ),
        (
            23 * 60,
            false,
            night,
            "Do not disturb is on until 07:00 (your daily schedule).",
        ),
        (
            12 * 60,
            false,
            night,
            "Do not disturb is off. Your daily schedule turns it on at 22:00.",
        ),
    ];
    for size in [WINDOW_SIZE, NARROW_SCENE_SIZE] {
        for (minute, manual, schedule, line) in cases {
            let rt = render(notifications_at(minute, manual, schedule), size);
            assert_eq!(state_line(&rt), line, "{size:?}");
            // The switch says what the switch is: the schedule being on never reads as the
            // switch being off in the same breath, because the line comes first and says so.
            let sw = find(&rt, "dnd-switch");
            assert_eq!(sw.role, "switch");
            // The room list's row agrees with the line.
            let row = find(&rt, "dnd-text");
            let on = line.contains("is on");
            assert_eq!(
                row.text.as_deref(),
                Some(if on {
                    "Do not disturb: on"
                } else {
                    "Do not disturb: off"
                }),
                "{line}"
            );
        }
    }
}

#[test]
fn the_state_line_changes_when_the_schedule_ends_while_the_section_is_open() {
    let mut rt = render(
        notifications_at(16 * 60 + 59, false, Some((9 * 60, 17 * 60))),
        WINDOW_SIZE,
    );
    assert!(state_line(&rt).contains("until 17:00"));
    rt.update(|s, _| s.chat.dnd.now = 17 * 60);
    settle(&mut rt);
    assert_eq!(
        state_line(&rt),
        "Do not disturb is off. Your daily schedule turns it on at 09:00."
    );
    assert_eq!(
        find(&rt, "dnd-text").text.as_deref(),
        Some("Do not disturb: off")
    );
}

#[test]
fn the_notifications_controls_work_and_keep_their_keys() {
    let mut rt = render(notifications_at(12 * 60, false, None), WINDOW_SIZE);
    for key in [
        "dnd-switch",
        "dnd-schedule",
        "dnd-mentions",
        "settings-previews",
    ] {
        assert_eq!(find(&rt, key).role, "switch", "{key}");
    }
    assert!(rt.find("dnd-start").is_none(), "times only with a schedule");
    press(&mut rt, "dnd-switch");
    assert!(rt.state.chat.dnd.settings.manual);
    assert_eq!(
        state_line(&rt),
        "Do not disturb is on until you turn it off."
    );
    press(&mut rt, "dnd-schedule");
    assert!(rt.find("dnd-start").is_some() && rt.find("dnd-end").is_some());
    assert_eq!(
        rt.state.chat.dnd.start.text, "22:00",
        "the default schedule is offered"
    );
    press(&mut rt, "dnd-mentions");
    assert!(rt.state.chat.dnd.settings.allow_mentions);
    // Each change is queued to be saved once.
    assert!(rt.state.chat.dnd.take_dirty().is_some());
}

#[test]
fn the_previews_switch_is_off_by_default_explains_itself_and_is_saved_once() {
    let mut rt = render(notifications_at(12 * 60, false, None), WINDOW_SIZE);
    assert!(!rt.state.chat.private_previews, "private by default");
    let sw = find(&rt, "settings-previews");
    assert_eq!(
        sw.label.as_deref(),
        Some("Show the sender and text in notifications")
    );
    assert!(shows(&rt, "Off by default"));
    assert!(rt.state.chat.take_previews_dirty().is_none());
    press(&mut rt, "settings-previews");
    assert!(rt.state.chat.private_previews);
    assert_eq!(rt.state.chat.take_previews_dirty(), Some(true));
    assert_eq!(rt.state.chat.take_previews_dirty(), None);
    press(&mut rt, "settings-previews");
    assert_eq!(rt.state.chat.take_previews_dirty(), Some(false));
}

/// The preference keeps the key and the plain true/false format it had when it had no switch.
#[test]
fn the_previews_preference_keeps_its_key_and_format() {
    assert_eq!(crate::chat::PREVIEWS_PREF, "notification_previews");
    let dir = std::env::temp_dir().join(format!("river-previews-test-{}", std::process::id()));
    let path = dir.join("prefs.json");
    rune::prefs::Prefs::at(&path)
        .set(crate::chat::PREVIEWS_PREF, &true)
        .unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    let back: bool = rune::prefs::Prefs::at(&path)
        .get(crate::chat::PREVIEWS_PREF)
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(back);
    assert!(raw.contains("\"notification_previews\""), "{raw}");
}

#[test]
fn the_notifications_section_points_to_the_rooms_bell_and_has_no_global_switch() {
    let rt = render(notifications_at(12 * 60, false, None), WINDOW_SIZE);
    assert!(shows(&rt, "set from the bell in that room's header"));
    // Three switches for do not disturb, one for previews and the room list's quick row:
    // nothing that turns every notification on or off as a whole.
    let switches = rt
        .elements()
        .into_iter()
        .filter(|e| e.role == "switch")
        .count();
    assert_eq!(switches, 5);
}

#[test]
fn space_and_tab_work_the_notification_switches() {
    let mut rt = render(notifications_at(12 * 60, false, None), WINDOW_SIZE);
    assert_eq!(
        focused(&rt).as_deref(),
        Some("settings-section-notifications")
    );
    key(&mut rt, Key::Tab);
    assert_eq!(focused(&rt).as_deref(), Some("dnd-switch"));
    key(&mut rt, Key::Text(" ".into()));
    assert!(rt.state.chat.dnd.settings.manual);
    key(&mut rt, Key::Tab);
    assert_eq!(focused(&rt).as_deref(), Some("dnd-schedule"));
    key(&mut rt, Key::Escape);
    assert!(!is_open(&rt));
}

#[test]
fn the_notifications_section_in_a_narrow_window() {
    let mut rt = open(NARROW_SCENE_SIZE);
    press(&mut rt, "settings-section-notifications");
    assert!(rt.find("dnd-state").is_some());
    assert!(rt.find("settings-previews").is_some());
    press(&mut rt, "settings-back");
    assert!(rt.find("dnd-state").is_none());
    // Typed schedule times are kept when going back.
    press(&mut rt, "settings-section-notifications");
    press(&mut rt, "dnd-schedule");
    rt.update(|s, _| {
        s.chat.dnd.start.replace_text("21:15");
        s.chat.dnd.sync_inputs();
    });
    press(&mut rt, "settings-back");
    rt.update(|s, _| {
        s.chat.select_settings_section(Section::Connection);
    });
    settle(&mut rt);
    assert_eq!(
        rt.state.chat.dnd.settings.schedule.unwrap().start,
        21 * 60 + 15
    );
}

#[test]
fn the_room_lists_row_toggles_and_a_schedule_sends_you_to_settings() {
    let mut rt = render(notifications_at(12 * 60, false, None), WINDOW_SIZE);
    press(&mut rt, "close");
    press(&mut rt, "dnd");
    assert!(rt.state.chat.dnd.settings.manual && !is_open(&rt));
    press(&mut rt, "dnd");
    assert!(!rt.state.chat.dnd.settings.manual && !is_open(&rt));
    // On because of the schedule: the row opens the section that can change it.
    rt.update(|s, _| {
        s.chat.dnd.settings.schedule = Some(super::DndSchedule {
            start: 22 * 60,
            end: 7 * 60,
        });
        s.chat.dnd.now = 23 * 60;
    });
    settle(&mut rt);
    assert_eq!(
        find(&rt, "dnd-text").text.as_deref(),
        Some("Do not disturb: on")
    );
    press(&mut rt, "dnd");
    assert!(is_open(&rt));
    assert_eq!(rt.state.chat.settings.section, Section::Notifications);
    assert!(
        !rt.state.chat.dnd.settings.manual,
        "the switch is left as it was"
    );
}

#[test]
fn background_and_startup_is_listed_only_where_the_shell_offers_it() {
    let rt = open(WINDOW_SIZE);
    assert!(rt.find("settings-section-background").is_none());
    let (s, _) = with_shell(true);
    let mut rt = render(s, WINDOW_SIZE);
    press(&mut rt, "settings");
    assert!(rt.find("settings-section-background").is_some());
    assert_eq!(
        find(&rt, "settings-section-background").label.as_deref(),
        Some("Background and startup")
    );
    // The footer button for it is gone.
    assert!(rt.find("background").is_none());
}

#[test]
fn the_background_switches_reach_the_shell_and_quit_is_set_apart_as_danger() {
    let (s, host) = with_shell(true);
    let mut rt = render(s, WINDOW_SIZE);
    press(&mut rt, "settings");
    press(&mut rt, "settings-section-background");
    assert_eq!(find(&rt, "background-close-to-tray").role, "switch");
    press(&mut rt, "background-start-at-login");
    press(&mut rt, "background-close-to-tray");
    assert!(*host.login.lock().unwrap());
    assert!(!*host.close_to_tray.lock().unwrap());
    // The hint names the platform's own chord (Cmd+Q on a Mac), the one `app_key` binds as
    // `Mod+Q` through `is_command('q')`.
    let chord = if cfg!(target_os = "macos") {
        "Cmd+Q"
    } else {
        "Ctrl+Q"
    };
    assert_eq!(
        super::shortcuts::shown("Mod+Q", cfg!(target_os = "macos")),
        chord
    );
    assert!(shows(&rt, chord));
    // Quit River is below every switch and no switch is its neighbour.
    let quit_y = find(&rt, "background-quit").rect[1];
    for k in ["background-close-to-tray", "background-start-at-login"] {
        assert!(find(&rt, k).rect[1] < quit_y, "{k}");
    }
    let src = include_str!("background.rs");
    assert!(src.contains("danger_outline_button_tx"), "styled as danger");
    // Space reaches a switch from the keyboard.
    let first = find(&rt, "background-close-to-tray");
    assert_eq!(first.role, "switch");
}

#[test]
fn without_a_tray_there_is_no_tray_control_and_closing_is_explained() {
    let (s, host) = with_shell(false);
    let mut rt = render(s, WINDOW_SIZE);
    press(&mut rt, "settings");
    press(&mut rt, "settings-section-background");
    assert!(rt.find("background-close-to-tray").is_none());
    assert!(shows(&rt, "closing the window quits River"));
    assert!(shows(&rt, "there is no tray"));
    assert!(rt.find("background-quit").is_some(), "Quit stays reachable");
    // Login still works without a tray.
    press(&mut rt, "background-start-at-login");
    assert!(*host.login.lock().unwrap());
}

#[test]
fn the_background_section_in_a_narrow_window_and_reopening_reads_the_shell_again() {
    let (s, host) = with_shell(true);
    let mut rt = render(s, NARROW_SCENE_SIZE);
    press(&mut rt, "settings");
    press(&mut rt, "settings-section-background");
    assert!(rt.find("background-quit").is_some());
    press(&mut rt, "close");
    // The shell's answer changes behind the app's back (another instance, the OS).
    *host.login.lock().unwrap() = true;
    press(&mut rt, "settings");
    press(&mut rt, "settings-section-background");
    assert!(rt.state.chat.background.as_ref().unwrap().start_at_login);
}

#[test]
fn diagnostics_is_listed_only_where_the_shell_keeps_a_log() {
    let rt = open(WINDOW_SIZE);
    assert!(rt.find("settings-section-diagnostics").is_none());
    let mut s = connected();
    s.chat.diagnostics = Some(super::DiagnosticsUi::new(log_host()));
    let mut rt = render(s, WINDOW_SIZE);
    press(&mut rt, "settings");
    assert_eq!(
        find(&rt, "settings-section-diagnostics").label.as_deref(),
        Some("Diagnostics")
    );
}

#[test]
fn diagnostics_shows_the_log_path_and_the_button_opens_its_folder() {
    let host = log_host();
    let mut s = connected();
    s.chat.diagnostics = Some(super::DiagnosticsUi::new(host.clone()));
    let mut rt = render(s, WINDOW_SIZE);
    press(&mut rt, "settings");
    press(&mut rt, "settings-section-diagnostics");
    assert!(shows(&rt, "/home/ada/.config/River/logs/river.log"));
    assert_eq!(find(&rt, "settings-log-open").role, "button");
    assert!(rt.find("settings-log-error").is_none());
    press(&mut rt, "settings-log-open");
    assert_eq!(host.opened.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(rt.find("settings-log-error").is_none());
}

#[test]
fn without_a_log_file_diagnostics_says_where_the_output_goes_and_has_no_button() {
    let host = std::sync::Arc::new(LogShell {
        opened: 0.into(),
        refuse: false,
        elsewhere: true,
        unavailable: false,
        stopped: false,
    });
    let mut s = connected();
    s.chat.diagnostics = Some(super::DiagnosticsUi::new(host));
    s.chat.settings.open_at(Section::Diagnostics);
    let rt = render(s, WINDOW_SIZE);
    assert!(shows(&rt, "output going elsewhere"));
    assert!(rt.find("settings-log-open").is_none());
    assert!(rt.find("settings-log-path").is_none());
}

#[test]
fn a_log_that_could_not_be_made_says_diagnostics_are_lost() {
    let host = std::sync::Arc::new(LogShell {
        opened: 0.into(),
        refuse: false,
        elsewhere: false,
        unavailable: true,
        stopped: false,
    });
    let mut s = connected();
    s.chat.diagnostics = Some(super::DiagnosticsUi::new(host));
    s.chat.settings.open_at(Section::Diagnostics);
    let rt = render(s, WINDOW_SIZE);
    assert!(shows(&rt, "could not open its log file"));
    assert!(!shows(&rt, "output going elsewhere"));
    assert!(rt.find("settings-log-open").is_none());
}

#[test]
fn a_log_that_stopped_says_so_and_is_not_shown_as_live() {
    let host = std::sync::Arc::new(LogShell {
        opened: 0.into(),
        refuse: false,
        elsewhere: false,
        unavailable: false,
        stopped: true,
    });
    let mut s = connected();
    s.chat.diagnostics = Some(super::DiagnosticsUi::new(host));
    s.chat.settings.open_at(Section::Diagnostics);
    let rt = render(s, WINDOW_SIZE);
    assert!(shows(&rt, "log file stopped"));
    assert!(rt.find("settings-log-open").is_none());
    assert!(rt.find("settings-log-path").is_none());
}

#[test]
fn a_folder_that_will_not_open_says_why() {
    let host = std::sync::Arc::new(LogShell {
        opened: 0.into(),
        refuse: true,
        elsewhere: false,
        unavailable: false,
        stopped: false,
    });
    let mut s = connected();
    s.chat.diagnostics = Some(super::DiagnosticsUi::new(host));
    s.chat.settings.open_at(Section::Diagnostics);
    let mut rt = render(s, WINDOW_SIZE);
    press(&mut rt, "settings-log-open");
    assert!(shows(&rt, "no file manager"));
}

#[test]
fn every_scene_of_these_sections_exists() {
    let names: Vec<&str> = crate::SCENES.iter().map(|(n, _)| *n).collect();
    for want in [
        "chat-settings-notifications",
        "chat-settings-notifications-narrow",
        "chat-settings-background",
        "chat-settings-background-narrow",
        "chat-settings-background-no-tray",
        "chat-settings-background-no-tray-narrow",
        "chat-settings-identity",
        "chat-settings-identity-narrow",
        "chat-settings-appearance",
        "chat-settings-appearance-narrow",
        "chat-settings-diagnostics",
        "chat-settings-about",
        "chat-settings-about-narrow",
        "chat-dnd-on",
    ] {
        assert!(names.contains(&want), "{want}");
    }
    for gone in ["chat-dnd", "chat-background", "chat-background-no-tray"] {
        assert!(
            !names.contains(&gone),
            "{gone} is a dialog that no longer exists"
        );
    }
}

#[test]
fn the_room_lists_row_is_a_named_switch_with_readable_off_text() {
    let mut s = connected();
    s.chat.dnd.pinned = true;
    s.chat.dnd.now = 12 * 60;
    let rt = render(s, WINDOW_SIZE);
    let row = find(&rt, "dnd");
    assert_eq!(row.role, "switch");
    assert_eq!(row.label.as_deref(), Some("Do not disturb"));
    assert!(row.focusable);
    let p = super::style::palette();
    assert_eq!(
        find(&rt, "dnd-text").text_color,
        Some(p.text_secondary),
        "off text is at least the secondary ink"
    );
}

/// Types something that is not a time into the schedule's From field of an open section.
fn type_bad_time(rt: &mut Runtime<RiverApp>) {
    rt.update(|s, _| {
        s.chat.dnd.start.replace_text("late");
        s.chat.dnd.sync_inputs();
    });
    settle(rt);
    assert!(
        rt.find("dnd-error").is_some(),
        "the reason shows while typing"
    );
}

fn schedule_night() -> RiverApp {
    notifications_at(12 * 60, false, Some((22 * 60, 7 * 60)))
}

#[test]
fn leaving_with_times_that_are_not_times_says_so_however_you_leave() {
    for leave in ["section", "escape", "x"] {
        let mut rt = render(schedule_night(), WINDOW_SIZE);
        type_bad_time(&mut rt);
        match leave {
            "section" => {
                press(&mut rt, "settings-section-connection");
                assert!(is_open(&rt));
                assert!(
                    shows(&rt, "Do not disturb schedule not changed"),
                    "said inside the dialog"
                );
                assert!(rt.find("settings-dnd-notice").is_some());
                press(&mut rt, "close");
            }
            "escape" => key(&mut rt, Key::Escape),
            _ => press(&mut rt, "close"),
        }
        assert!(!is_open(&rt), "{leave}");
        // The room list says it, with a way to dismiss it.
        assert!(rt.find("dnd-notice").is_some(), "{leave}");
        assert_eq!(
            find(&rt, "dnd-notice").text.as_deref(),
            Some("Do not disturb schedule not changed: the times were not valid.")
        );
        assert_eq!(
            rt.state.chat.dnd.settings.schedule.unwrap().start,
            22 * 60,
            "{leave}: the schedule in force is unchanged"
        );
        // Back in the section the invalid text and the error are still there.
        press(&mut rt, "settings");
        press(&mut rt, "settings-section-notifications");
        assert_eq!(rt.state.chat.dnd.start.text, "late", "{leave}");
        assert!(rt.find("dnd-error").is_some(), "{leave}");
        // Fixing it applies it and clears the notice.
        rt.update(|s, _| {
            s.chat.dnd.start.replace_text("21:30");
            s.chat.dnd.sync_inputs();
        });
        settle(&mut rt);
        assert!(rt.find("dnd-error").is_none(), "{leave}");
        press(&mut rt, "close");
        assert!(rt.find("dnd-notice").is_none(), "{leave}");
        assert_eq!(
            rt.state.chat.dnd.settings.schedule.unwrap().start,
            21 * 60 + 30
        );
    }
}

#[test]
fn the_dnd_notice_can_be_dismissed() {
    let mut rt = render(schedule_night(), WINDOW_SIZE);
    type_bad_time(&mut rt);
    key(&mut rt, Key::Escape);
    assert!(rt.find("dnd-notice").is_some());
    press(&mut rt, "dnd-notice-dismiss");
    assert!(rt.find("dnd-notice").is_none());
    assert!(rt.state.chat.dnd.settings.schedule.is_some());
}

#[test]
fn the_privacy_switch_is_in_view_at_the_default_size_and_tab_reveals_it_in_a_short_window() {
    // Default window: every control of the section is visible without scrolling.
    let mut s = notifications_at(12 * 60, false, Some((22 * 60, 7 * 60)));
    s.chat.dnd.start.replace_text("22:00");
    let rt = render(s, WINDOW_SIZE);
    assert!(visible(&rt, "settings-previews"), "no scrolling needed");
    // A short window scrolls; Tab to the last control brings it into view.
    let mut rt = render(
        notifications_at(12 * 60, false, Some((22 * 60, 7 * 60))),
        (640.0, 480.0),
    );
    for _ in 0..12 {
        if focused(&rt).as_deref() == Some("settings-previews") {
            break;
        }
        key(&mut rt, Key::Tab);
    }
    assert_eq!(focused(&rt).as_deref(), Some("settings-previews"));
    assert!(
        visible(&rt, "settings-previews"),
        "Tab scrolled the last control into view"
    );
}

#[test]
fn the_notifications_pane_has_a_scroll_bar_and_is_taller_in_a_tall_window() {
    let rt = render(schedule_night(), WINDOW_SIZE);
    let pane = find(&rt, "settings-pane");
    assert!(
        pane.rect[3] > 520.0,
        "uses the window's height: {:?}",
        pane.rect
    );
    let src = include_str!("settings.rs");
    assert!(
        src.matches(".scroll_bar(k)").count() >= 2,
        "wide and narrow"
    );
}

#[test]
fn identity_actions_are_unavailable_for_the_example_rooms() {
    let mut rt = render(connected(), WINDOW_SIZE);
    press(&mut rt, "settings");
    press(&mut rt, "settings-section-identity");
    press(&mut rt, "settings-import-id");
    assert!(is_open(&rt), "a disabled button does nothing");
    assert!(rt.state.chat.form.open.is_none());
}

/// A shell on the node with the example rooms, so identity actions are available.
fn node_state(edit: fn(&mut Vec<crate::chat::model::Room>)) -> RiverApp {
    let mut s = connected();
    let mut rooms = crate::chat::example::data().rooms;
    edit(&mut rooms);
    s.chat.source = crate::chat::Source::Node(crate::chat::LinkStatus::Live);
    // The first live list is catch-up; the second is the whole list.
    for _ in 0..2 {
        s.chat.show_node_rooms(
            rooms.clone(),
            None,
            crate::chat::LinkStatus::Live,
            29_000_000,
        );
    }
    s
}

fn export_button_keys(rt: &Runtime<RiverApp>) -> Vec<String> {
    rt.elements()
        .into_iter()
        .filter_map(|e| e.key)
        .filter(|k| k.starts_with("settings-export-row-"))
        .collect()
}

#[test]
fn the_export_dialog_names_the_room_it_is_for() {
    let mut rt = render(node_state(|_| {}), WINDOW_SIZE);
    let (key, name) = {
        let r = &rt.state.chat.rooms()[1];
        (r.key.clone(), r.name.clone())
    };
    press(&mut rt, "settings");
    press(&mut rt, "settings-section-identity");
    press(&mut rt, &format!("settings-export-{key}"));
    assert!(rt.find("export-identity-dialog").is_some());
    assert!(
        texts(&rt)
            .iter()
            .any(|t| t.contains(&name) && t.starts_with("Room:")),
        "the dialog shows {name:?}: {:?}",
        texts(&rt)
    );
}

#[test]
fn rooms_with_the_same_or_no_name_are_told_apart() {
    let mut rt = render(
        node_state(|rooms| {
            rooms[0].name = "Same".into();
            rooms[1].name = "Same".into();
            rooms[2].name = String::new();
        }),
        WINDOW_SIZE,
    );
    press(&mut rt, "settings");
    press(&mut rt, "settings-section-identity");
    let rooms = rt.state.chat.rooms().to_vec();
    let labels: Vec<String> = rooms
        .iter()
        .map(|r| crate::chat::identity::room_label(&rooms, &r.key))
        .collect();
    for (i, a) in labels.iter().enumerate() {
        for b in &labels[i + 1..] {
            assert_ne!(a, b, "{labels:?}");
        }
    }
    for l in &labels[..3] {
        assert!(texts(&rt).iter().any(|t| t == l), "row {l:?} is shown");
    }
    assert!(labels[2].starts_with("Unnamed room"));
    // A unique name stays as it is.
    let mut solo = rooms.clone();
    solo.truncate(1);
    assert_eq!(
        crate::chat::identity::room_label(&solo, &solo[0].key),
        solo[0].name
    );
    // The dialog uses the same label.
    let key = rooms[1].key.clone();
    press(&mut rt, &format!("settings-export-{key}"));
    assert!(shows(&rt, &format!("Room: {}", labels[1])));
}

#[test]
fn the_export_dialog_closes_when_its_room_leaves_the_list() {
    let mut rt = render(node_state(|_| {}), WINDOW_SIZE);
    let key = rt.state.chat.rooms()[1].key.clone();
    press(&mut rt, "settings");
    press(&mut rt, "settings-section-identity");
    press(&mut rt, &format!("settings-export-{key}"));
    assert!(rt.state.chat.export.is_some());
    let rest: Vec<_> = rt
        .state
        .chat
        .rooms()
        .iter()
        .filter(|r| r.key != key)
        .cloned()
        .collect();
    // Another room going leaves the dialog alone.
    let other: Vec<_> = rt.state.chat.rooms().iter().skip(1).cloned().collect();
    rt.update(move |s, _| s.chat.set_rooms(other));
    assert!(rt.state.chat.export.is_some());
    rt.update(move |s, _| s.chat.set_rooms(rest));
    settle(&mut rt);
    assert!(rt.state.chat.export.is_none(), "dialog and token dropped");
    assert!(rt.find("export-identity-dialog").is_none());
}

#[test]
fn a_partial_list_while_reconnecting_keeps_the_export_dialog_and_a_full_one_closes_it() {
    let mut rt = render(node_state(|_| {}), WINDOW_SIZE);
    let key = rt.state.chat.rooms()[1].key.clone();
    press(&mut rt, "settings");
    press(&mut rt, "settings-section-identity");
    press(&mut rt, &format!("settings-export-{key}"));
    assert!(rt.state.chat.export.is_some());
    let rest: Vec<_> = rt
        .state
        .chat
        .rooms()
        .iter()
        .filter(|r| r.key != key)
        .cloned()
        .collect();
    // Reconnecting: the list lacks the room only because it is partial.
    let partial = rest.clone();
    rt.update(move |s, _| {
        s.chat.show_node_rooms(
            partial,
            None,
            crate::chat::LinkStatus::Reconnecting,
            29_000_000,
        );
    });
    assert!(rt.state.chat.export.is_some(), "a partial list keeps it");
    // The first live list after that is still catch-up, not the whole list.
    let catch_up = rest.clone();
    rt.update(move |s, _| {
        s.chat
            .show_node_rooms(catch_up, None, crate::chat::LinkStatus::Live, 29_000_000);
    });
    assert!(rt.state.chat.export.is_some(), "catch-up keeps it");
    // A full live list without the room closes it.
    rt.update(move |s, _| {
        s.chat
            .show_node_rooms(rest, None, crate::chat::LinkStatus::Live, 29_000_000);
    });
    assert!(rt.state.chat.export.is_none(), "a full list closes it");
}

#[test]
fn identity_buttons_are_disabled_with_a_reason_for_the_example_rooms() {
    let mut rt = render(connected(), WINDOW_SIZE);
    press(&mut rt, "settings");
    press(&mut rt, "settings-section-identity");
    assert!(rt.find("settings-identity-unavailable").is_some());
    assert!(shows(&rt, "example rooms"));
    let key = rt.state.chat.rooms()[0].key.clone();
    press(&mut rt, &format!("settings-export-{key}"));
    assert!(is_open(&rt), "a disabled button does nothing");
    assert!(rt.state.chat.export.is_none());
    assert!(rt.state.chat.outgoing.is_empty());
    // With the node there is nothing to explain.
    let rt = {
        let mut rt = render(node_state(|_| {}), WINDOW_SIZE);
        press(&mut rt, "settings");
        press(&mut rt, "settings-section-identity");
        rt
    };
    assert!(rt.find("settings-identity-unavailable").is_none());
    assert_eq!(export_button_keys(&rt).len(), rt.state.chat.rooms().len());
}

#[test]
fn the_new_sections_are_listed_and_reachable_in_a_narrow_window() {
    let mut rt = render(connected(), NARROW_SCENE_SIZE);
    press(&mut rt, "settings");
    for id in ["identity", "appearance"] {
        assert!(rt.find(&format!("settings-section-{id}")).is_some(), "{id}");
    }
    press(&mut rt, "settings-section-appearance");
    assert!(rt.find("settings-size-100").is_some());
    press(&mut rt, "settings-back");
    press(&mut rt, "settings-section-identity");
    assert!(rt.find("settings-import-id").is_some());
}

// ---- Light and dark themes, and the About section ----

fn render_in(state: RiverApp, system_dark: bool) -> Runtime<RiverApp> {
    let env = Env {
        system_dark,
        scale: 1.0,
    };
    let mut rt = Runtime::new(state, view, WINDOW_SIZE, env);
    rt.on_key = crate::spec().on_key;
    rt.frame(0.0);
    rt.frame(1.0 / 60.0);
    rt
}

/// WCAG relative luminance: a `Color` holds linear light, which is what it is computed from.
fn luminance(c: rune::Color) -> f32 {
    c.luminance()
}

fn contrast(a: rune::Color, b: rune::Color) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[test]
fn the_light_and_dark_palettes_are_the_web_uis_values() {
    use super::style::palette_for;
    let (l, d) = (palette_for(false), palette_for(true));
    assert_eq!(l.bg, rune::color::hex("#ffffff").unwrap());
    assert_eq!(l.rail, rune::color::hex("#f4f4f6").unwrap());
    assert_eq!(l.text, rune::color::hex("#1a1a1a").unwrap());
    assert_eq!(l.warning_text, rune::color::hex("#8a5800").unwrap());
    assert_eq!(d.bg, rune::color::hex("#141416").unwrap());
    assert_eq!(d.text, rune::color::hex("#f0f0f2").unwrap());
}

#[test]
fn text_is_readable_on_the_surfaces_it_sits_on_in_both_themes() {
    use super::style::palette_for;
    for dark in [false, true] {
        let p = palette_for(dark);
        let min = 4.5;
        for (name, fg) in [
            ("text", p.text),
            ("text_secondary", p.text_secondary),
            ("text_muted", p.text_muted),
            ("quote", p.quote),
            ("accent", p.accent),
            ("danger", p.danger),
            ("warning_text", p.warning_text),
        ] {
            for (surface, bg) in [("bg", p.bg), ("rail", p.rail), ("surface", p.surface)] {
                let c = contrast(fg, bg);
                // The web UI's own muted and secondary inks are 4.47:1 and 4.48:1 on the
                // lifted surface (light and dark); everything else clears 4.5:1.
                let min = if surface == "surface" { 4.4 } else { min };
                assert!(c >= min, "dark={dark}: {name} on {surface} is {c:.2}:1");
            }
        }
        for (name, fg, bg) in [
            ("warning_text on warning_bg", p.warning_text, p.warning_bg),
            ("danger on danger_bg", p.danger, p.danger_bg),
            ("accent on accent_soft", p.accent, p.accent_soft),
            ("accent on own_row", p.accent, p.own_row),
        ] {
            let c = contrast(fg, bg);
            assert!(c >= min, "dark={dark}: {name} is {c:.2}:1");
        }
    }
}

#[test]
fn the_appearance_mode_resolves_against_the_system() {
    use super::style::Mode;
    assert!(Mode::System.dark(true));
    assert!(!Mode::System.dark(false));
    assert!(!Mode::Light.dark(true));
    assert!(Mode::Dark.dark(false));
    assert_eq!(Mode::default(), Mode::System);
    for m in Mode::ALL {
        assert_eq!(Mode::from_name(m.name()), m);
    }
    assert_eq!(Mode::from_name("nonsense"), Mode::System);
    assert_eq!(Mode::from_name(""), Mode::System);
}

#[test]
fn the_screen_follows_the_systems_appearance() {
    let want = |dark: bool| super::style::palette_for(dark).bg;
    let light = render_in(connected(), false);
    assert_eq!(find(&light, "chat").fill, want(false), "light system");
    let dark = render_in(connected(), true);
    assert_eq!(find(&dark, "chat").fill, want(true), "dark system");
}

#[test]
fn the_screen_changes_theme_when_the_system_does() {
    let mut rt = render_in(connected(), true);
    assert_eq!(find(&rt, "chat").fill, super::style::palette_for(true).bg);
    rt.set_system_dark(false);
    settle(&mut rt);
    assert_eq!(find(&rt, "chat").fill, super::style::palette_for(false).bg);
    rt.set_system_dark(true);
    settle(&mut rt);
    assert_eq!(find(&rt, "chat").fill, super::style::palette_for(true).bg);
}

#[test]
fn a_chosen_theme_overrides_the_system() {
    use super::style::{Mode, palette_for};
    let mut s = connected();
    s.chat.load_theme("light");
    let rt = render_in(s, true);
    assert_eq!(find(&rt, "chat").fill, palette_for(false).bg);
    let mut s = connected();
    s.chat.load_theme("dark");
    let rt = render_in(s, false);
    assert_eq!(find(&rt, "chat").fill, palette_for(true).bg);
    assert_eq!(Mode::from_name("light"), Mode::Light);
}

#[test]
fn the_theme_buttons_set_and_save_the_choice() {
    use super::style::Mode;
    let mut s = connected();
    s.chat.settings.open_at(Section::Appearance);
    let mut rt = render_in(s, true);
    assert_eq!(rt.state.chat.theme(), Mode::System, "system is the default");
    assert_eq!(rt.state.chat.take_theme_dirty(), None);
    press(&mut rt, "settings-theme-light");
    assert_eq!(rt.state.chat.theme(), Mode::Light);
    assert_eq!(
        find(&rt, "chat").fill,
        super::style::palette_for(false).bg,
        "light on a dark system"
    );
    assert_eq!(rt.state.chat.take_theme_dirty(), Some("light"));
    assert_eq!(rt.state.chat.take_theme_dirty(), None);
    press(&mut rt, "settings-theme-system");
    assert_eq!(rt.state.chat.take_theme_dirty(), Some("system"));
    assert_eq!(find(&rt, "chat").fill, super::style::palette_for(true).bg);
    press(&mut rt, "settings-theme-light");
    // Choosing what is already chosen saves nothing.
    rt.state.chat.take_theme_dirty();
    press(&mut rt, "settings-theme-light");
    assert_eq!(rt.state.chat.take_theme_dirty(), None);
}

#[test]
fn the_dialog_is_light_on_a_light_system() {
    let mut s = connected();
    s.chat.settings.open_at(Section::Appearance);
    let rt = render_in(s, false);
    let p = super::style::palette_for(false);
    assert_eq!(
        find(&rt, "settings-theme-note").text_color,
        Some(p.text_muted)
    );
}

#[test]
fn the_about_section_shows_the_versions_the_source_and_the_licence() {
    let mut s = connected();
    s.chat.update.node_version = Some("0.2.156".into());
    s.chat.settings.open_at(Section::About);
    let rt = render(s, WINDOW_SIZE);
    assert_eq!(
        find(&rt, "settings-about-version").text.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        find(&rt, "settings-about-node-version").text.as_deref(),
        Some("0.2.156")
    );
    assert_eq!(
        find(&rt, "settings-about-source-url").text.as_deref(),
        Some("https://github.com/freenet/river")
    );
    assert!(super::settings::SOURCE_URL.starts_with("https://"));
    assert_eq!(find(&rt, "settings-about-source").role, "button");
    assert_eq!(
        find(&rt, "settings-about-licence").text.as_deref(),
        Some("LGPL-3.0-only")
    );
}

#[test]
fn the_about_section_says_when_there_is_no_node() {
    let mut s = connected();
    s.chat.update.node_version = None;
    s.chat.settings.open_at(Section::About);
    let rt = render(s, WINDOW_SIZE);
    assert_eq!(
        find(&rt, "settings-about-node-version").text.as_deref(),
        Some("Not connected")
    );
}
