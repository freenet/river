//! Scrollbars on every pane that scrolls (#1015), and the arrow keys scrolling the
//! conversation (#1016).

use rune::{Key, KeyEvent, Mods, Runtime};

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

/// The y of a message row near the bottom of the history, which moves as the history scrolls.
fn row_y(rt: &Runtime<RiverApp>) -> f32 {
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("row-long-59"))
        .expect("the newest filler row is drawn");
    e.rect[1]
}

/// A point inside the history that no message control covers: right of the text, left of the bar.
fn blank_spot(rt: &Runtime<RiverApp>) -> (f32, f32) {
    let (r, _) = rt.presented(HISTORY).expect("the history is drawn");
    (r[0] + r[2] - 60.0, r[1] + r[3] * 0.5)
}

#[test]
fn every_scroll_pane_has_a_scroll_bar() {
    let rt = long_room(|_| {});
    for area in [
        // The conversation, the room list and the members list.
        "history-scroll-town-square-scroll-area",
        "room-list-scroll-area",
        "member-list-scroll-area",
    ] {
        assert!(has(&rt, area), "{area} has a scroll bar area");
    }
    // Each area holds the bar, its thumb slot and its thumb.
    assert!(has(&rt, "scroll-bar") && has(&rt, "thumb-slot") && has(&rt, "thumb"));
    // The history overflows, so its thumb is drawn with a length.
    let thumb = rt
        .elements()
        .into_iter()
        .filter(|e| e.key.as_deref() == Some("thumb"))
        .any(|e| e.rect[3] > 10.0);
    assert!(thumb, "a long history draws a thumb");
}

#[test]
fn every_scroll_view_in_the_app_opts_into_a_scroll_bar() {
    // Views not on screen in the default scene (dialogs, the direct-message thread, the emoji
    // panel): each `.scroll(` in these files is followed by a `.scroll_bar(k)`.
    for (name, src) in [
        ("conversation.rs", include_str!("conversation.rs")),
        ("dm_view.rs", include_str!("dm_view.rs")),
        ("view.rs", include_str!("view.rs")),
        ("overview.rs", include_str!("overview.rs")),
        ("shortcuts.rs", include_str!("shortcuts.rs")),
        ("details.rs", include_str!("details.rs")),
        ("identity.rs", include_str!("identity.rs")),
        ("settings.rs", include_str!("settings.rs")),
    ] {
        let code = src.split("#[cfg(test)]").next().unwrap();
        let views = code.matches("ScrollSpec {").count();
        let bars = code.matches(".scroll_bar(k)").count();
        assert_eq!(
            views, bars,
            "{name}: {views} scroll views, {bars} scroll bars"
        );
    }
}

#[test]
fn both_conversation_views_take_keyboard_focus_for_the_arrows() {
    for src in [include_str!("conversation.rs"), include_str!("dm_view.rs")] {
        assert!(
            src.contains(".label(\"Conversation history\")"),
            "the history takes focus"
        );
    }
}

#[test]
fn down_and_up_scroll_the_conversation_after_a_click_on_it() {
    let mut rt = long_room(|_| {});
    let (x, y) = blank_spot(&rt);
    click(&mut rt, x, y);
    let at_bottom = row_y(&rt);
    key(&mut rt, Key::Up);
    let up = row_y(&rt);
    assert!(
        up > at_bottom + 20.0,
        "Up scrolled the history back: {at_bottom} -> {up}"
    );
    key(&mut rt, Key::Down);
    let down = row_y(&rt);
    assert!(
        (down - at_bottom).abs() < 1.0,
        "Down scrolled it forward again: {at_bottom} -> {up} -> {down}"
    );
}

#[test]
fn the_arrows_do_not_scroll_while_the_composer_has_focus() {
    let mut rt = long_room(|_| {});
    let (x, y) = rt.find("composer-field").expect("the composer is drawn");
    click(&mut rt, x, y);
    key(&mut rt, Key::Text("hi".into()));
    let before = row_y(&rt);
    key(&mut rt, Key::Up);
    assert!((row_y(&rt) - before).abs() < 1.0, "Up stays in the field");
}

#[test]
fn up_in_an_empty_composer_still_edits_the_last_message_after_the_history_was_focused() {
    let mut rt = long_room(|_| {});
    let (x, y) = blank_spot(&rt);
    click(&mut rt, x, y);
    let (x, y) = rt.find("composer-field").expect("the composer is drawn");
    click(&mut rt, x, y);
    key(&mut rt, Key::Up);
    assert!(rt.find("editing").is_some(), "Up edits my last message");
}
