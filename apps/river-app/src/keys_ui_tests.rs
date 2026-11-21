//! Emoji shortcodes, zoom and the shortcuts page, driven through the window as a person would:
//! typing, the chords, and pressing what is drawn.

use rune::{Env, Key, KeyEvent, Mods, Runtime};

use crate::chat;
use crate::{RiverApp, Setup, WINDOW_SIZE, spec, view};

fn app() -> Runtime<RiverApp> {
    app_at(WINDOW_SIZE)
}

fn app_at(size: (f32, f32)) -> Runtime<RiverApp> {
    let mut s = RiverApp::detached();
    s.setup = Setup::Connected {
        version: "0.2.155".into(),
    };
    let env = Env {
        system_dark: true,
        scale: 1.0,
    };
    let mut rt = Runtime::new(s, view, size, env);
    rt.on_key = spec().on_key;
    rt.frame(0.0);
    rt.frame(1.0 / 60.0);
    rt
}

fn settle(rt: &mut Runtime<RiverApp>) {
    for i in 1..=30 {
        rt.frame(f64::from(i) / 60.0);
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

fn chord(rt: &mut Runtime<RiverApp>, c: char) {
    rt.key(KeyEvent {
        key: Key::Command(c),
        mods: Mods {
            cmd: true,
            ctrl: !cfg!(target_os = "macos"),
            logo: cfg!(target_os = "macos"),
            ..Mods::default()
        },
    });
    settle(rt);
}

fn type_text(rt: &mut Runtime<RiverApp>, text: &str) {
    press(rt, "composer-field");
    key(rt, Key::Text(text.into()));
}

fn composer(rt: &Runtime<RiverApp>) -> String {
    rt.state.chat.composer.text.clone()
}

fn listed(rt: &Runtime<RiverApp>) -> Vec<String> {
    rt.state
        .chat
        .shortcode_popup()
        .map(|p| p.entries.iter().map(|e| e.name.to_owned()).collect())
        .unwrap_or_default()
}

// ---- emoji shortcodes ----

#[test]
fn typing_a_colon_and_two_letters_lists_emoji_and_enter_inserts_the_chosen_one() {
    let mut rt = app();
    type_text(&mut rt, "nice :sm");
    assert!(
        rt.find("shortcode-popup-content").is_some(),
        "the popup is drawn"
    );
    let names = listed(&rt);
    assert!(
        !names.is_empty() && names.len() <= chat::shortcodes::LIMIT,
        "{names:?}"
    );
    assert!(names.iter().all(|n| n.contains("sm")), "{names:?}");
    // Down moves the highlight; Enter writes that row's emoji and sends nothing.
    key(&mut rt, Key::Down);
    let chosen = rt.state.chat.shortcode_popup().expect("open");
    assert_eq!(chosen.selected, 1 % names.len());
    let glyph = chosen.entries[chosen.selected].glyph;
    key(&mut rt, Key::Enter);
    assert_eq!(composer(&rt), format!("nice {glyph}"));
    assert!(rt.state.chat.shortcode_popup().is_none());
    assert!(
        rt.state.chat.outgoing.is_empty(),
        "Enter picked; it did not send"
    );
}

#[test]
fn tab_picks_the_first_row() {
    let mut rt = app();
    type_text(&mut rt, ":smi");
    let first = rt.state.chat.shortcode_popup().expect("open").entries[0].glyph;
    key(&mut rt, Key::Tab);
    assert_eq!(composer(&rt), first);
}

#[test]
fn clicking_a_row_inserts_it() {
    let mut rt = app();
    type_text(&mut rt, ":smi");
    let first = rt.state.chat.shortcode_popup().expect("open").entries[0].glyph;
    // Rows are keyed by the name they insert.
    let name = rt.state.chat.shortcode_popup().expect("open").entries[0].name;
    let row = rt
        .elements()
        .into_iter()
        .find_map(|e| e.key.filter(|k| k.contains(name)))
        .unwrap_or_else(|| panic!("no row for {name}"));
    press(&mut rt, &row);
    assert_eq!(composer(&rt), first);
}

#[test]
fn escape_closes_the_popup_and_keeps_the_text() {
    let mut rt = app();
    type_text(&mut rt, ":smi");
    key(&mut rt, Key::Escape);
    assert!(rt.state.chat.shortcode_popup().is_none());
    assert_eq!(composer(&rt), ":smi");
    // Typing on in the same word keeps it closed; a new word opens it again.
    key(&mut rt, Key::Text("l".into()));
    assert!(rt.state.chat.shortcode_popup().is_none());
    key(&mut rt, Key::Text(" :smi".into()));
    assert!(rt.state.chat.shortcode_popup().is_some());
}

#[test]
fn a_complete_name_with_its_closing_colon_becomes_the_emoji() {
    let mut rt = app();
    type_text(&mut rt, "ok :smile");
    key(&mut rt, Key::Text(":".into()));
    assert_eq!(composer(&rt), "ok 😄");
    assert!(rt.state.chat.shortcode_popup().is_none());
    // An unknown name keeps its colon: it is just text.
    rt.update(|s, _| s.chat.composer.replace_text(""));
    type_text(&mut rt, ":notanemoji");
    key(&mut rt, Key::Text(":".into()));
    assert_eq!(composer(&rt), ":notanemoji:");
}

#[test]
fn it_stays_out_of_times_urls_double_colons_and_code() {
    for text in [
        "meet at 10:30",
        "see http://example.com/a:sm",
        "a ::smile",
        "`:smile",
        "```\n:smile",
        ":s",
    ] {
        let mut rt = app();
        type_text(&mut rt, text);
        assert!(
            rt.state.chat.shortcode_popup().is_none()
                && rt.find("shortcode-popup-content").is_none(),
            "{text:?} opened the popup"
        );
        // And a closing colon after text that is not a shortcode leaves the text alone.
        key(&mut rt, Key::Text(":".into()));
        assert!(composer(&rt).ends_with(':'), "{text:?}");
    }
    // A complete name inside a code span is not converted either.
    let mut rt = app();
    type_text(&mut rt, "`x :smile");
    key(&mut rt, Key::Text(":".into()));
    assert_eq!(composer(&rt), "`x :smile:");
}

#[test]
fn the_mention_popup_still_wins_its_own_word() {
    let mut rt = app();
    type_text(&mut rt, "@");
    assert!(rt.state.chat.mention_popup().is_some());
    assert!(rt.state.chat.shortcode_popup().is_none());
}

// ---- zoom ----

fn zoom_percent(rt: &Runtime<RiverApp>) -> u16 {
    rt.state.chat.zoom.percent()
}

#[test]
fn the_zoom_keys_step_through_the_levels_from_the_composer_and_reset() {
    let mut rt = app();
    press(&mut rt, "composer-field");
    assert_eq!(zoom_percent(&rt), 100);
    assert!(
        rt.find("zoom-frame").is_some(),
        "the frame is there at 100% too"
    );
    assert!(rt.find("zoom-badge").is_none());
    chord(&mut rt, '=');
    assert_eq!(zoom_percent(&rt), 110);
    assert!(
        rt.find("zoom-badge").is_some(),
        "the level is shown when not 100%"
    );
    chord(&mut rt, '=');
    chord(&mut rt, '=');
    assert_eq!(zoom_percent(&rt), 150);
    chord(&mut rt, '-');
    assert_eq!(zoom_percent(&rt), 125);
    chord(&mut rt, '0');
    assert_eq!(zoom_percent(&rt), 100);
    assert!(rt.find("zoom-badge").is_none());
    assert_eq!(composer(&rt), "", "the keys were not typed");
    for _ in 0..12 {
        chord(&mut rt, '-');
    }
    assert_eq!(zoom_percent(&rt), 80, "the smallest level");
    for _ in 0..12 {
        chord(&mut rt, '=');
    }
    assert_eq!(zoom_percent(&rt), 200, "the largest level");
}

#[test]
fn a_zoom_change_is_queued_for_saving_once() {
    let mut rt = app();
    chord(&mut rt, '=');
    assert_eq!(rt.state.chat.zoom.take_dirty(), Some(110));
    assert_eq!(rt.state.chat.zoom.take_dirty(), None);
    // The app that runs in a test never writes the person's preferences file.
    chord(&mut rt, '=');
    rt.update(|s, tx| s.sync_dnd(tx));
    assert!(!rt.state.dnd_persist);
}

#[test]
fn what_is_drawn_zoomed_can_still_be_pressed() {
    let mut rt = app();
    chord(&mut rt, '=');
    chord(&mut rt, '=');
    chord(&mut rt, '=');
    chord(&mut rt, '=');
    chord(&mut rt, '=');
    chord(&mut rt, '=');
    assert_eq!(zoom_percent(&rt), 200);
    assert!(rt.find("zoom-frame").is_some());
    // Hit testing follows the scale: pressing the footer button where it is drawn opens it.
    press(&mut rt, "dnd");
    assert!(
        rt.state.chat.dnd.settings.manual,
        "the press landed on the zoomed row"
    );
}

#[test]
fn zoom_makes_the_layout_narrow_like_a_smaller_window() {
    let mut rt = app();
    assert!(
        rt.find("members").is_some(),
        "the members panel sits beside the conversation"
    );
    for _ in 0..6 {
        chord(&mut rt, '=');
    }
    assert_eq!(zoom_percent(&rt), 200);
    // 1280 / 2 = 640 points of layout: the narrow layout, as in a 640 wide window.
    assert!(rt.find("members").is_none(), "the panel gives way at 200%");
    chord(&mut rt, '0');
    assert!(rt.find("members").is_some(), "and comes back at 100%");
}

#[test]
fn pressing_the_badge_resets_the_zoom() {
    let mut rt = app();
    chord(&mut rt, '=');
    press(&mut rt, "zoom-badge");
    assert_eq!(zoom_percent(&rt), 100);
}

// ---- the shortcuts page ----

#[test]
fn the_chord_opens_the_page_even_from_the_composer_and_escape_closes_it() {
    let mut rt = app();
    press(&mut rt, "composer-field");
    chord(&mut rt, '/');
    assert!(rt.state.chat.shortcuts_open);
    assert!(rt.find("shortcuts-dialog").is_some());
    assert_eq!(composer(&rt), "", "the chord was not typed");
    // Every row of the table is on the page.
    for i in 0..chat::shortcuts::SHORTCUTS.len() {
        assert!(rt.find(&format!("shortcut-{i}")).is_some(), "row {i}");
    }
    // A held chord repeats: it must not close and reopen the page.
    chord(&mut rt, '/');
    assert!(rt.state.chat.shortcuts_open);
    key(&mut rt, Key::Escape);
    assert!(!rt.state.chat.shortcuts_open);
    assert!(rt.find("shortcuts-dialog").is_none());
}

#[test]
fn the_footer_entry_opens_it_and_close_closes_it() {
    let mut rt = app();
    press(&mut rt, "shortcuts-entry");
    assert!(rt.state.chat.shortcuts_open);
    press(&mut rt, "shortcuts-close");
    assert!(!rt.state.chat.shortcuts_open);
}

#[test]
fn it_does_not_open_over_another_dialog_or_the_switcher() {
    let mut rt = app();
    press(&mut rt, "settings");
    chord(&mut rt, '/');
    assert!(!rt.state.chat.shortcuts_open, "not over a dialog");
    rt.update(|s, _| s.chat.close_settings());
    settle(&mut rt);
    chord(&mut rt, 'k');
    assert!(rt.state.chat.switcher.is_some());
    chord(&mut rt, '/');
    assert!(!rt.state.chat.shortcuts_open, "not over the switcher");
}

#[test]
fn the_switcher_does_not_open_over_it() {
    let mut rt = app();
    chord(&mut rt, '/');
    assert!(rt.state.chat.shortcuts_open);
    chord(&mut rt, 'k');
    assert!(rt.state.chat.switcher.is_none());
    assert!(rt.state.chat.shortcuts_open);
}

#[test]
fn another_dialog_opening_closes_it_but_a_link_menu_does_not() {
    let mut rt = app();
    chord(&mut rt, '/');
    rt.update(|s, tx| {
        s.chat.link_menu = Some((
            "https://example.org".into(),
            rune::PointerPress { x: 5.0, y: 5.0 },
        ));
        s.sync_dnd(tx);
    });
    assert!(rt.state.chat.shortcuts_open, "a link menu leaves it open");
    rt.update(|s, tx| {
        s.chat.link_menu = None;
        s.chat.settings.open();
        s.sync_dnd(tx);
    });
    settle(&mut rt);
    assert!(!rt.state.chat.shortcuts_open, "a dialog closes it");
    assert!(rt.state.chat.settings.open, "and the dialog stays");
}

#[test]
fn the_zoom_keys_work_while_the_page_is_open() {
    let mut rt = app();
    chord(&mut rt, '/');
    chord(&mut rt, '=');
    assert_eq!(zoom_percent(&rt), 110);
    assert!(rt.state.chat.shortcuts_open);
    assert!(rt.find("shortcuts-dialog").is_some());
}

// ---- zoom keeps the screen's state, and sits well with search ----

fn rect_of(rt: &Runtime<RiverApp>, key: &str) -> [f32; 4] {
    rt.elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("no element {key}"))
        .rect
}

#[test]
fn the_badge_is_in_the_room_list_heading_and_covers_no_composer_control() {
    for size in [WINDOW_SIZE, (640.0, 480.0)] {
        let mut rt = app_at(size);
        chord(&mut rt, '=');
        let badge = rect_of(&rt, "zoom-badge");
        let heading_end = rect_of(&rt, "room-list")[1];
        assert!(
            badge[1] + badge[3] <= heading_end + 1.0,
            "{size:?}: above the room list"
        );
        for key in ["send", "emoji", "composer-field", "history"] {
            let r = rect_of(&rt, key);
            let apart = badge[0] + badge[2] <= r[0]
                || r[0] + r[2] <= badge[0]
                || badge[1] + badge[3] <= r[1]
                || r[1] + r[3] <= badge[1];
            assert!(apart, "{size:?}: the badge overlaps {key}");
        }
    }
}

#[test]
fn the_chord_is_not_typed_and_search_closes_under_the_page() {
    let mut rt = app();
    chord(&mut rt, 'f');
    assert!(rt.state.chat.search.is_some());
    chord(&mut rt, '/');
    rt.update(|s, tx| s.sync_unread(tx));
    assert!(rt.state.chat.shortcuts_open);
    assert!(
        rt.state.chat.search.is_none(),
        "search gives way to the dialog"
    );
}

thread_local! {
    /// The focused element's key as each key the window handler saw found it.
    static FOCUS_SEEN: std::cell::RefCell<Vec<Option<String>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The app's key handler, noting which element the runtime says has the keyboard first.
fn recording_on_key(s: &mut RiverApp, ev: &KeyEvent, tx: &mut rune::Tx) -> bool {
    FOCUS_SEEN.with(|f| f.borrow_mut().push(tx.focused().map(str::to_owned)));
    (spec().on_key.expect("the app handles keys"))(s, ev, tx)
}

/// Every element's full path, in tree order, without the zoom badge (which is there only while
/// the zoom is not 100%).
fn paths(rt: &Runtime<RiverApp>) -> Vec<String> {
    rt.elements()
        .into_iter()
        .map(|e| e.path)
        .filter(|p| !p.contains("zoom-badge"))
        .collect()
}

/// How far the history is scrolled: how far its content starts above its viewport, in window
/// points.
fn history_offset(rt: &Runtime<RiverApp>) -> f32 {
    rect_of(rt, "history-frame")[1] - rect_of(rt, "history")[1]
}

#[test]
fn crossing_100_percent_keeps_the_tree_the_scroll_offset_and_the_focused_element() {
    let mut rt = app();
    rt.on_key = Some(recording_on_key);
    press(&mut rt, "composer-field");
    let (x, y) = rt.find("history").expect("history");
    rt.wheel(x, y, 150.0);
    settle(&mut rt);
    assert_eq!(rt.scroll_at_bottom("history"), Some(false));
    let (paths0, offset0) = (paths(&rt), history_offset(&rt));
    assert!(
        offset0 > 10.0,
        "the history is scrolled part way: {offset0}"
    );
    // Up through 110% and back to 100%, then down to 90% and back, then once more up: each
    // key is seen by the window handler with the runtime's own focused element.
    for c in ['=', '0', '-', '0', '='] {
        chord(&mut rt, c);
        assert_eq!(rt.scroll_at_bottom("history"), Some(false), "after {c}");
        assert_eq!(paths(&rt), paths0, "node paths after {c}");
    }
    chord(&mut rt, '0');
    let seen = FOCUS_SEEN.with(|f| f.borrow().clone());
    assert_eq!(seen.len(), 6, "{seen:?}");
    assert!(
        seen.iter().all(|k| k.as_deref() == Some("composer-field")),
        "the focused element changed across the zoom keys: {seen:?}"
    );
    let offset1 = history_offset(&rt);
    assert!(
        (offset1 - offset0).abs() < 1.0,
        "scroll offset {offset0} became {offset1}"
    );
}

#[test]
fn a_zoom_round_trip_leaves_the_layout_as_a_fresh_start_has_it() {
    let fresh = app();
    let mut rt = app();
    for c in ['=', '=', '=', '-', '-', '0'] {
        chord(&mut rt, c);
    }
    assert_eq!(zoom_percent(&rt), 100);
    let shape = |rt: &Runtime<RiverApp>| -> Vec<(String, [f32; 4], f32)> {
        rt.elements()
            .into_iter()
            .map(|e| (e.path, e.rect, e.scale))
            .collect()
    };
    let (a, b) = (shape(&fresh), shape(&rt));
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.0, y.0);
        assert!(
            x.1.iter().zip(&y.1).all(|(p, q)| (p - q).abs() < 0.01) && (x.2 - y.2).abs() < 1e-4,
            "{}: {:?} scale {} became {:?} scale {}",
            x.0,
            x.1,
            x.2,
            y.1,
            y.2
        );
    }
}

#[test]
fn the_zoomed_screen_fills_the_window_one_frame_after_the_key_not_after_settling() {
    let mut rt = app();
    settle(&mut rt);
    for c in ['=', '=', '=', '0'] {
        rt.key(KeyEvent {
            key: Key::Command(c),
            mods: Mods {
                cmd: true,
                ctrl: !cfg!(target_os = "macos"),
                logo: cfg!(target_os = "macos"),
                ..Mods::default()
            },
        });
        // One frame, no settling.
        rt.frame(1.0 / 60.0);
        let drawn = rect_of(&rt, "kit-root");
        assert!(
            (drawn[2] - 1280.0).abs() < 1.0 && (drawn[3] - 800.0).abs() < 1.0,
            "after {c} the screen is drawn at {drawn:?}, not the window's size"
        );
        assert!(
            !rt.elements().iter().any(|e| e.moving),
            "something is still animating one frame after {c}"
        );
    }
    // And the badge press, which is another route to the same change.
    chord(&mut rt, '=');
    press_one_frame(&mut rt, "zoom-badge");
    let drawn = rect_of(&rt, "kit-root");
    assert!(
        (drawn[2] - 1280.0).abs() < 1.0 && (drawn[3] - 800.0).abs() < 1.0,
        "{drawn:?}"
    );
}

fn press_one_frame(rt: &mut Runtime<RiverApp>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no element {key}"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1.0 / 60.0);
}

#[test]
fn the_smallest_window_cannot_be_zoomed_in_and_the_first_zoomed_frame_knows_it() {
    let mut rt = app_at((640.0, 480.0));
    assert_eq!(rt.state.chat.zoom.drawn(), 100);
    rt.update(|s, _| s.chat.zoom.load(200));
    rt.frame(0.0);
    assert_eq!(
        rt.state.chat.zoom.drawn(),
        100,
        "capped on the first zoomed frame"
    );
    let frame = rect_of(&rt, "zoom-frame");
    assert_eq!([frame[2], frame[3]], [640.0, 480.0]);
    // A taller window allows more, up to its own cap.
    let mut rt = app();
    rt.update(|s, _| s.chat.zoom.load(200));
    rt.frame(0.0);
    assert_eq!(rt.state.chat.zoom.drawn(), 167);
}

#[test]
fn the_badge_is_in_the_room_list_heading_covers_no_composer_control_and_is_as_big_as_its_neighbours()
 {
    for size in [WINDOW_SIZE, (640.0, 480.0)] {
        let mut rt = app_at(size);
        // 80% is allowed in every window, so the badge shows in all of them.
        chord(&mut rt, '-');
        let badge = rect_of(&rt, "zoom-badge");
        let heading_end = rect_of(&rt, "room-list")[1];
        assert!(
            badge[1] + badge[3] <= heading_end + 1.0,
            "{size:?}: above the room list"
        );
        let scale = f32::from(rt.state.chat.zoom.drawn()) / 100.0;
        let neighbour = rect_of(&rt, "create-room");
        assert!(
            badge[3] + 0.5 >= neighbour[3],
            "{size:?}: badge {badge:?} vs {neighbour:?} ({scale})"
        );
        for key in ["send", "emoji", "composer-field", "history"] {
            let r = rect_of(&rt, key);
            let apart = badge[0] + badge[2] <= r[0]
                || r[0] + r[2] <= badge[0]
                || badge[1] + badge[3] <= r[1]
                || r[1] + r[3] <= badge[1];
            assert!(apart, "{size:?}: the badge overlaps {key}");
        }
    }
}

/// The part of the history that shows, in layout points (its window rect over the scale).
fn history_height(rt: &Runtime<RiverApp>) -> f32 {
    let shown = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("history"))
        .expect("history")
        .visible;
    shown[3] / (f32::from(rt.state.chat.zoom.drawn()) / 100.0)
}

#[test]
fn search_with_a_tall_draft_leaves_the_history_what_it_has_unzoomed_in_the_smallest_window() {
    let draft = "line\n".repeat(8);
    for size in [WINDOW_SIZE, (640.0, 480.0), (1280.0, 600.0), (960.0, 480.0)] {
        // Unzoomed, in the smallest layout the zoom allows.
        let mut base = app_at((640.0, 480.0));
        base.update(|s, _| s.chat.composer.replace_text(&draft));
        chord(&mut base, 'f');
        settle(&mut base);
        let floor = history_height(&base);

        let mut rt = app_at(size);
        rt.update(|s, _| s.chat.composer.replace_text(&draft));
        chord(&mut rt, 'f');
        for _ in 0..6 {
            chord(&mut rt, '=');
        }
        settle(&mut rt);
        let (scale, h) = (
            f32::from(rt.state.chat.zoom.drawn()) / 100.0,
            history_height(&rt),
        );
        eprintln!("{size:?} zoom {scale}: history {h} layout points (smallest unzoomed {floor})");
        assert!(h + 1.0 >= floor, "{size:?} at {scale}: {h} < {floor}");
        assert!(rt.state.chat.search.is_some());
    }
}

#[test]
fn search_keeps_the_history_its_minimum_height_when_zoomed_with_a_short_draft() {
    for size in [WINDOW_SIZE, (1280.0, 600.0)] {
        let mut rt = app_at(size);
        chord(&mut rt, 'f');
        for _ in 0..6 {
            chord(&mut rt, '=');
        }
        let h = history_height(&rt);
        assert!(
            h + 1.0 >= chat::search::HISTORY_MIN_H,
            "{size:?}: {h} < {}",
            chat::search::HISTORY_MIN_H
        );
    }
}

#[test]
fn the_node_status_dialog_and_the_shortcuts_page_exclude_each_other() {
    // The page does not open over the node status dialog.
    let mut rt = app();
    rt.update(|s, _| s.chat.status.open());
    settle(&mut rt);
    assert!(rt.state.chat.status.open);
    chord(&mut rt, '/');
    assert!(
        !rt.state.chat.shortcuts_open,
        "not over the node status dialog"
    );
    // And the node status dialog opening closes the page.
    let mut rt = app();
    chord(&mut rt, '/');
    assert!(rt.state.chat.shortcuts_open);
    rt.update(|s, tx| {
        s.chat.status.open();
        s.sync_dnd(tx);
    });
    settle(&mut rt);
    assert!(!rt.state.chat.shortcuts_open, "the page gave way");
    assert!(rt.state.chat.status.open, "and the status dialog stays");
}
