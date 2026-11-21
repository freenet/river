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
fn escape_closes_settings_from_either_section() {
    for section in Section::ALL {
        let mut rt = open(WINDOW_SIZE);
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
        let mut s = connected();
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
    assert_eq!(rt.state.chat.settings.section, Section::Connection);
    assert_eq!(focused(&rt).as_deref(), Some("settings-section-connection"));
    assert!(rt.find("settings-node-change").is_some());
    // The list stops at its end.
    key(&mut rt, Key::Down);
    assert_eq!(rt.state.chat.settings.section, Section::Connection);
    key(&mut rt, Key::Up);
    assert_eq!(rt.state.chat.settings.section, Section::Updates);
    assert_eq!(focused(&rt).as_deref(), Some("settings-section-updates"));
    key(&mut rt, Key::Up);
    assert_eq!(rt.state.chat.settings.section, Section::Updates);
    key(&mut rt, Key::End);
    assert_eq!(rt.state.chat.settings.section, Section::Connection);
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
    for footer in ["dnd_entry", "\"Enter invite code\"", "\"Import ID\""] {
        assert!(view.contains(footer), "{footer} left the room list");
    }
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
        assert_eq!(focused(&rt).as_deref(), Some("settings-section-connection"));
        assert!(
            rt.find("settings-pane").is_none(),
            "arrows only move focus here"
        );
        key(&mut rt, open_key.clone());
        assert!(
            rt.find("settings-pane").is_some(),
            "{open_key:?} opens the section"
        );
        assert_eq!(rt.state.chat.settings.section, Section::Connection);
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
