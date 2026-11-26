//! Each room remembers where the reader was in it, and opens there again (the web UI's
//! `remember_reading_position`).

use rune::Runtime;

use super::reply_jump_tests::{long_room, settle};
use crate::RiverApp;

const A: &str = "town-square";
const B: &str = "rust-help";
const WINDOW: &str = "history-scroll-town-square";

/// What the app's per-frame hook (`spec().poll`) does, until it has nothing staged.
fn run_frames(rt: &mut Runtime<RiverApp>) {
    let poll = crate::spec().poll.expect("a poll hook");
    for _ in 0..12 {
        rt.update(|s, tx| {
            poll(s, 0.0, tx);
        });
        settle(rt);
    }
}

fn select(rt: &mut Runtime<RiverApp>, room: &str) {
    let room = room.to_owned();
    rt.update(move |s, _| s.chat.select(&room));
    settle(rt);
    run_frames(rt);
}

fn scroll_up(rt: &mut Runtime<RiverApp>, y: f32) {
    rt.update(move |_, tx| tx.scroll_to_y(WINDOW, y));
    settle(rt);
}

/// The first message row whose top is inside the history, and where that top is in the window.
fn reading_row(rt: &Runtime<RiverApp>) -> (String, f32) {
    let (view, _) = rt.presented(WINDOW).expect("the history is drawn");
    rt.elements()
        .into_iter()
        .filter_map(|e| {
            let k = e.key?;
            (k.starts_with("row-") && e.rect[1] >= view[1]).then_some((k, e.rect[1]))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .expect("a message row is in view")
}

fn y_of(rt: &Runtime<RiverApp>, key: &str) -> Option<f32> {
    rt.elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key))
        .map(|e| e.rect[1])
}

fn arrive_in_a(rt: &mut Runtime<RiverApp>) {
    rt.update(|s, _| {
        s.chat.edit_rooms(|rooms| {
            let r = rooms.iter_mut().find(|r| r.key == A).unwrap();
            let mut m = r.messages.last().unwrap().clone();
            m.key = "arrival".into();
            m.id = r.messages.len() as u32 + 50;
            m.time += 500;
            m.text = "a new message".into();
            m.md = m.text.clone();
            m.reply_to = None;
            r.messages.push(m);
        });
    });
    settle(rt);
}

fn read_reports(rt: &mut Runtime<RiverApp>) -> Vec<String> {
    std::mem::take(&mut rt.state.chat.reported)
        .into_iter()
        .filter_map(|c| match c.action {
            super::Action::MarkRead { up_to } => Some(up_to),
            _ => None,
        })
        .collect()
}

#[test]
fn a_room_scrolled_up_reopens_at_the_same_message_in_the_same_place() {
    let mut rt = long_room(|_| {});
    scroll_up(&mut rt, 500.0);
    let (row, y) = reading_row(&rt);
    select(&mut rt, B);
    select(&mut rt, A);
    let again = y_of(&rt, &row).unwrap_or_else(|| panic!("{row} is drawn after reopening"));
    assert!((again - y).abs() < 1.0, "{row} moved from {y} to {again}");
    assert!(!rt.state.chat.at_bottom, "Latest is offered again");
}

#[test]
fn a_message_arriving_meanwhile_does_not_move_the_restored_place() {
    let mut rt = long_room(|_| {});
    scroll_up(&mut rt, 500.0);
    let (row, y) = reading_row(&rt);
    select(&mut rt, B);
    arrive_in_a(&mut rt);
    select(&mut rt, A);
    let again = y_of(&rt, &row).unwrap_or_else(|| panic!("{row} is drawn after reopening"));
    assert!((again - y).abs() < 1.0, "{row} moved from {y} to {again}");
}

#[test]
fn a_room_left_at_the_bottom_opens_at_the_bottom() {
    let mut rt = long_room(|_| {});
    // Up, then back down, so the memory has to be cleared and not merely never written.
    scroll_up(&mut rt, 500.0);
    rt.update(|_, tx| tx.scroll_to_bottom_instant(WINDOW));
    settle(&mut rt);
    select(&mut rt, B);
    arrive_in_a(&mut rt);
    select(&mut rt, A);
    assert_eq!(rt.scroll_at_bottom(WINDOW), Some(true));
    assert!(rt.state.chat.at_bottom);
    assert!(y_of(&rt, "row-arrival").is_some(), "the newest is drawn");
}

#[test]
fn a_restored_position_does_not_report_unseen_messages_read() {
    let mut rt = long_room(|_| {});
    // A window in front, so reading is reported at all.
    rt.update(|s, _| s.chat.set_window_front(true));
    settle(&mut rt);
    scroll_up(&mut rt, 500.0);
    select(&mut rt, B);
    arrive_in_a(&mut rt);
    read_reports(&mut rt);
    select(&mut rt, A);
    assert!(
        read_reports(&mut rt).is_empty(),
        "nothing was read: the newest is off screen"
    );
    // Reading on to the end does report it.
    rt.update(|_, tx| tx.scroll_to_bottom_instant(WINDOW));
    settle(&mut rt);
    assert_eq!(read_reports(&mut rt), ["arrival".to_owned()]);
}

#[test]
fn a_jump_to_a_message_beats_the_remembered_place() {
    let mut rt = long_room(|_| {});
    scroll_up(&mut rt, 500.0);
    select(&mut rt, B);
    rt.update(|s, _| {
        s.chat.select(A);
        s.chat.search_jump("long-50", std::time::Instant::now());
    });
    settle(&mut rt);
    run_frames(&mut rt);
    let (view, _) = rt.presented(WINDOW).unwrap();
    let y = y_of(&rt, "row-long-50").expect("the jump target is drawn");
    assert!(y > view[1] && y < view[1] + view[3], "target in view: {y}");
}

#[test]
fn each_room_keeps_its_own_place() {
    let mut rt = long_room(|_| {});
    scroll_up(&mut rt, 300.0);
    let (row, y) = reading_row(&rt);
    select(&mut rt, B);
    select(&mut rt, A);
    select(&mut rt, B);
    select(&mut rt, A);
    let again = y_of(&rt, &row).expect("drawn");
    assert!((again - y).abs() < 1.0, "twice over: {y} -> {again}");
}
