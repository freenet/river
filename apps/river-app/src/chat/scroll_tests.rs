//! Scrollbars on every pane that scrolls (#1015), and the arrow keys scrolling the
//! conversation (#1016).

use rune::{Key, KeyEvent, Mods, Runtime};

use super::dm_view_tests as dm;
use super::reply_jump_tests::{long_room, settle};
use crate::RiverApp;

const HISTORY: &str = "history-scroll-town-square";

fn key(rt: &mut Runtime<RiverApp>, key: Key) {
    rt.key(KeyEvent {
        key,
        mods: Mods::default(),
    });
    settle(rt);
}

fn click(rt: &mut Runtime<RiverApp>, x: f32, y: f32) {
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    settle(rt);
}

fn has(rt: &Runtime<RiverApp>, key: &str) -> bool {
    rt.elements().iter().any(|e| e.key.as_deref() == Some(key))
}

fn y_of(rt: &Runtime<RiverApp>, key: &str) -> f32 {
    rt.elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("{key} is drawn"))
        .rect[1]
}

fn focused_key(rt: &Runtime<RiverApp>) -> Option<String> {
    rt.elements()
        .into_iter()
        .find(|e| e.focused)
        .and_then(|e| e.key)
}

/// The centre of the first drawn text that contains `needle`.
fn text_at(rt: &Runtime<RiverApp>, needle: &str) -> (f32, f32) {
    // The history's own text: the rail and the header repeat what a thread says.
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.rect[0] > 300.0 && e.text.as_deref().is_some_and(|t| t.contains(needle)))
        .unwrap_or_else(|| panic!("no text {needle}"));
    (e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0)
}

/// A point inside the history that no message control covers: right of the text, left of the bar.
fn blank_spot(rt: &Runtime<RiverApp>, key: &str) -> (f32, f32) {
    let (r, _) = rt.presented(key).expect("the history is drawn");
    (r[0] + r[2] - 60.0, r[1] + r[3] * 0.5)
}

/// Every scroll view drawn has a scroll-bar area wrapped around it, each with its thumb.
fn assert_bar(rt: &Runtime<RiverApp>, viewport: &str) {
    assert!(has(rt, viewport), "{viewport} is drawn");
    assert!(
        has(rt, &format!("{viewport}-scroll-area")),
        "{viewport} has a scroll bar"
    );
}

#[test]
fn the_three_main_panes_have_a_scroll_bar() {
    let rt = long_room(|_| {});
    for pane in [HISTORY, "room-list", "member-list"] {
        assert_bar(&rt, pane);
    }
    let thumb = rt
        .elements()
        .into_iter()
        .filter(|e| e.key.as_deref() == Some("thumb"))
        .any(|e| e.rect[3] > 10.0);
    assert!(thumb, "a long history draws a thumb");
}

#[test]
fn the_emoji_panel_the_dialogs_and_a_direct_message_thread_have_a_scroll_bar() {
    let mut rt = long_room(|_| {});
    rt.update(|s, _| s.chat.emoji_open = true);
    settle(&mut rt);
    assert_bar(&rt, "emoji-scroll");
    rt.update(|s, _| s.chat.emoji_open = false);
    for (open, viewport) in [
        (
            (|s: &mut RiverApp| {
                s.chat.open_shortcuts();
            }) as fn(&mut RiverApp),
            "shortcuts-scroll",
        ),
        (
            |s: &mut RiverApp| {
                s.chat.toggle_overview();
            },
            "overview-rows",
        ),
        (|s: &mut RiverApp| s.chat.open_details(), "details-scroll"),
    ] {
        let mut rt = long_room(|_| {});
        rt.update(move |s, _| open(s));
        settle(&mut rt);
        assert_bar(&rt, viewport);
    }
    let (mut rt, _) = long_dm();
    assert_bar(&rt, &dm_scroll());
    let _ = &mut rt;
}

fn dm_scroll() -> String {
    super::dm_view::scroll_key(dm::ROOM, &dm::peer_key())
}

/// A direct-message thread long enough to scroll, open.
fn long_dm() -> (Runtime<RiverApp>, String) {
    long_dm_with(false)
}

/// The same, with the peer a member of the room (`member`): the box takes text, as in a live
/// thread. Otherwise it is read-only and says why.
fn long_dm_with(member: bool) -> (Runtime<RiverApp>, String) {
    let mut rt = dm::app();
    rt.update(|s, _| s.chat.source = super::Source::Node(super::LinkStatus::Live));
    if member {
        dm::make_peer_a_member(&mut rt);
    }
    let mut f = dm::Fixture::new();
    for i in 0..40u64 {
        let (from, to) = if i % 2 == 0 {
            (dm::PEER, dm::ME)
        } else {
            (dm::ME, dm::PEER)
        };
        f.compose(
            from,
            to,
            dm::T0 + i * 60,
            format!("dm line number {i}").as_bytes(),
        );
    }
    dm::give(&mut rt, f.snapshot(true));
    let peer = dm::peer_key();
    rt.update(move |s, _| assert!(s.chat.open_dm(dm::ROOM, &peer)));
    settle(&mut rt);
    (rt, dm_scroll())
}

#[test]
fn a_click_on_blank_history_focuses_it_for_the_arrows() {
    // Fails if `.focusable(true)` is taken off the history: the click then focuses nothing.
    let mut rt = long_room(|_| {});
    let (x, y) = blank_spot(&rt, HISTORY);
    click(&mut rt, x, y);
    assert_eq!(focused_key(&rt).as_deref(), Some(HISTORY));
}

#[test]
fn up_and_down_scroll_the_conversation_after_a_click_on_blank_space_or_on_text() {
    for on_text in [false, true] {
        let mut rt = long_room(|_| {});
        let (x, y) = if on_text {
            text_at(&rt, "filler line number 58")
        } else {
            blank_spot(&rt, HISTORY)
        };
        click(&mut rt, x, y);
        let at_bottom = y_of(&rt, "row-long-59");
        key(&mut rt, Key::Up);
        let up = y_of(&rt, "row-long-59");
        assert!(
            up > at_bottom + 20.0,
            "Up scrolled back (text click: {on_text}): {at_bottom} -> {up}"
        );
        key(&mut rt, Key::Down);
        let down = y_of(&rt, "row-long-59");
        assert!(
            (down - at_bottom).abs() < 1.0,
            "Down came forward again (text click: {on_text}): {at_bottom} -> {up} -> {down}"
        );
    }
}

fn dm_line_y(rt: &Runtime<RiverApp>) -> f32 {
    rt.elements()
        .into_iter()
        .find(|e| {
            e.rect[0] > 300.0
                && e.text
                    .as_deref()
                    .is_some_and(|t| t.contains("dm line number 39"))
        })
        .map(|e| e.rect[1])
        .expect("the newest line is drawn")
}

#[test]
fn up_and_down_scroll_a_direct_message_thread_after_a_click_on_text() {
    for member in [true, false] {
        let (mut rt, _) = long_dm_with(member);
        let (x, y) = text_at(&rt, "dm line number 38");
        click(&mut rt, x, y);
        let before = dm_line_y(&rt);
        key(&mut rt, Key::Up);
        let up = dm_line_y(&rt);
        assert!(
            up > before + 20.0,
            "Up scrolled the thread (member: {member}): {before} -> {up}"
        );
        key(&mut rt, Key::Down);
        assert!(
            (dm_line_y(&rt) - before).abs() < 1.0,
            "Down came back (member: {member})"
        );
    }
}

#[test]
fn the_arrows_stay_in_a_direct_message_box_that_has_text() {
    let (mut rt, _) = long_dm_with(true);
    rt.update(|s, _| s.chat.dm.compose_edit().replace_text("hi"));
    settle(&mut rt);
    assert_eq!(rt.state.chat.dm.compose_edit().text, "hi");
    let before = dm_line_y(&rt);
    key(&mut rt, Key::Up);
    assert!((dm_line_y(&rt) - before).abs() < 1.0);
}

#[test]
fn held_arrow_repeats_add_up_until_the_view_reports_the_move() {
    let rt = long_room(|_| {});
    let mut pos = rt.state.chat.history_pos.borrow_mut();
    assert_eq!(pos.key_for_test(), HISTORY, "the history reported itself");
    let start = pos.offset_for_test();
    // Three presses before a frame reports the new offset.
    let last = (0..3).map(|_| pos.next_target(-40.0)).last().unwrap();
    assert!((last - (start - 120.0)).abs() < 0.01, "{start} -> {last}");
}

#[test]
fn presses_past_the_bottom_do_not_pile_up() {
    let mut rt = long_room(|_| {});
    // Nothing has focus, so the keys take the app's route (`history_arrow`).
    assert_eq!(focused_key(&rt), None);
    // The history opens at its newest message, the bottom.
    let bottom = y_of(&rt, "row-long-59");
    for _ in 0..5 {
        key(&mut rt, Key::Down);
    }
    assert!(
        (y_of(&rt, "row-long-59") - bottom).abs() < 1.0,
        "nothing below"
    );
    key(&mut rt, Key::Up);
    assert!(y_of(&rt, "row-long-59") > bottom + 20.0, "one Up moves it");
}

#[test]
fn the_arrows_do_not_scroll_while_the_composer_has_focus() {
    let mut rt = long_room(|_| {});
    let (x, y) = rt.find("composer-field").expect("the composer is drawn");
    click(&mut rt, x, y);
    key(&mut rt, Key::Text("hi".into()));
    let before = y_of(&rt, "row-long-59");
    key(&mut rt, Key::Up);
    assert!(
        (y_of(&rt, "row-long-59") - before).abs() < 1.0,
        "Up stays in the field"
    );
}

#[test]
fn the_arrows_do_not_scroll_behind_a_dialog() {
    type Open = fn(&mut RiverApp);
    let dialogs: [(&str, Open); 5] = [
        ("Keyboard shortcuts", |s| {
            s.chat.open_shortcuts();
        }),
        ("Unread and mentions", |s| {
            s.chat.toggle_overview();
        }),
        ("Settings", |s| {
            s.chat.open_settings();
        }),
        ("Export identity", |s| s.chat.open_export()),
        ("Room details", |s| s.chat.open_details()),
    ];
    for (title, open) in dialogs {
        let mut rt = long_room(|_| {});
        rt.update(move |s, _| open(s));
        settle(&mut rt);
        assert!(
            rt.state.chat.dialog_open_for_test(),
            "{title}: the dialog is open"
        );
        // Click the dialog's own text first, which leaves nothing focused.
        let (x, y) = text_at(&rt, title);
        click(&mut rt, x, y);
        let before = y_of(&rt, "row-long-59");
        key(&mut rt, Key::Up);
        assert!(
            (y_of(&rt, "row-long-59") - before).abs() < 1.0,
            "{title}: Up did not scroll the history behind it"
        );
    }
}

/// Nothing at the right edge of a pane sits under the bar's track: the content ends one track
/// width short of the scroll view's edge.
#[test]
fn the_content_leaves_room_for_the_scroll_bar_track() {
    let strip = rune_kit::ScrollBarStyle::from_kit(&rune_kit::Kit::neutral(true)).strip();
    let check = |rt: &Runtime<RiverApp>, viewport: &str| {
        let els = rt.elements();
        let v = els
            .iter()
            .find(|e| e.key.as_deref() == Some(viewport))
            .unwrap_or_else(|| panic!("{viewport} is drawn"));
        let inside = format!("{}/", v.path);
        let edge = v.rect[0] + v.rect[2];
        let mut n = 0;
        for e in els.iter().filter(|e| e.path.starts_with(&inside)) {
            n += 1;
            assert!(
                e.rect[0] + e.rect[2] <= edge - strip + 0.5,
                "{viewport}: {:?} reaches {} of {edge}",
                e.key,
                e.rect[0] + e.rect[2]
            );
        }
        assert!(n > 5, "{viewport} has content");
    };
    let rt = long_room(|_| {});
    for pane in [HISTORY, "room-list", "member-list"] {
        check(&rt, pane);
    }
    let (rt, scroll) = long_dm();
    check(&rt, &scroll);
}

#[test]
fn up_in_an_empty_composer_still_edits_the_last_message_after_the_history_was_clicked() {
    let mut rt = long_room(|_| {});
    let (x, y) = blank_spot(&rt, HISTORY);
    click(&mut rt, x, y);
    let (x, y) = rt.find("composer-field").expect("the composer is drawn");
    click(&mut rt, x, y);
    key(&mut rt, Key::Up);
    assert!(rt.find("editing").is_some(), "Up edits my last message");
}

#[test]
fn a_click_in_the_thread_blurs_the_direct_message_box() {
    // `open_dm` hands the box the keyboard by flag. A click on the thread takes it away, as it does
    // from the room composer (#1022): the box stops counting as focused and the thread has focus.
    // A box that cannot take text (the peer left the room, a send waits for its answer) gets the
    // same flag, and has to give it up the same way.
    for (member, on_text) in [(true, false), (true, true), (false, false), (false, true)] {
        let (mut rt, scroll) = long_dm_with(member);
        assert_eq!(rt.state.chat.dm_block().is_none(), member);
        if member {
            assert_eq!(focused_key(&rt).as_deref(), Some("dm-composer-field"));
        }
        let (x, y) = if on_text {
            text_at(&rt, "dm line number 38")
        } else {
            blank_spot(&rt, &scroll)
        };
        click(&mut rt, x, y);
        assert_eq!(
            focused_key(&rt).as_deref(),
            Some(scroll.as_str()),
            "member: {member}, text click: {on_text}"
        );
        assert!(
            !rt.state.chat.dm.compose_edit().focused,
            "the box's focus flag is cleared (member: {member}, text click: {on_text})"
        );
    }
}
