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

fn offset(rt: &Runtime<RiverApp>) -> f32 {
    rt.state.chat.history_pos.borrow().offset
}

fn y_of(rt: &Runtime<RiverApp>, key: &str) -> Option<f32> {
    rt.elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key))
        .map(|e| e.rect[1])
}

/// A member of room A other than you, as a thread's peer key.
fn peer(rt: &Runtime<RiverApp>) -> String {
    let r = rt.state.chat.rooms().iter().find(|r| r.key == A).unwrap();
    r.members
        .iter()
        .find(|m| m.id != r.me)
        .unwrap()
        .full_id
        .to_string()
}

fn unread(rt: &Runtime<RiverApp>) -> usize {
    rt.state
        .chat
        .rooms()
        .iter()
        .find(|r| r.key == A)
        .unwrap()
        .unread
}

fn assert_offset(rt: &Runtime<RiverApp>, want: f32) {
    assert!(
        (offset(rt) - want).abs() < 1.0,
        "wanted {want}, at {}",
        offset(rt)
    );
}

#[test]
fn a_room_scrolled_up_reopens_at_the_same_offset() {
    let mut rt = long_room(|_| {});
    scroll_up(&mut rt, 500.0);
    select(&mut rt, B);
    select(&mut rt, A);
    assert_offset(&rt, 500.0);
    assert_eq!(rt.scroll_at_bottom(WINDOW), Some(false));
}

/// Leaving again after the history drew at its end but before the restore ran keeps the
/// remembered offset: that end position was never the reader's.
#[test]
fn leaving_before_the_restore_ran_keeps_the_offset() {
    let mut rt = long_room(|_| {});
    scroll_up(&mut rt, 500.0);
    select(&mut rt, B);
    // Reopen and draw, without the per-frame hook that applies the restore.
    rt.update(|s, _| s.chat.select(A));
    settle(&mut rt);
    assert_eq!(
        rt.scroll_at_bottom(WINDOW),
        Some(true),
        "premise: drawn at its end"
    );
    select(&mut rt, B);
    select(&mut rt, A);
    assert_offset(&rt, 500.0);
}

#[test]
fn several_separate_scrolls_before_leaving_are_all_remembered() {
    let mut rt = long_room(|_| {});
    for y in [60.0, 400.0, 900.0, 640.0] {
        scroll_up(&mut rt, y);
    }
    let at = offset(&rt);
    select(&mut rt, B);
    select(&mut rt, A);
    assert_offset(&rt, at);
}

#[test]
fn each_room_keeps_its_own_offset_across_repeated_visits() {
    let mut rt = long_room(|_| {});
    scroll_up(&mut rt, 300.0);
    for _ in 0..2 {
        select(&mut rt, B);
        select(&mut rt, A);
    }
    assert_offset(&rt, 300.0);
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
fn a_jump_to_a_message_beats_the_remembered_offset() {
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
fn closing_a_thread_gives_the_room_back_at_its_offset() {
    let mut rt = long_room(|_| {});
    scroll_up(&mut rt, 500.0);
    scroll_up(&mut rt, 700.0);
    let peer = peer(&rt);
    rt.update(move |s, _| assert!(s.chat.open_dm(A, &peer)));
    settle(&mut rt);
    run_frames(&mut rt);
    rt.update(|s, _| s.chat.close_dm());
    settle(&mut rt);
    run_frames(&mut rt);
    assert_offset(&rt, 700.0);
}

#[test]
fn a_thread_that_goes_away_gives_the_room_back_at_its_offset() {
    let mut rt = long_room(|_| {});
    scroll_up(&mut rt, 700.0);
    let peer = peer(&rt);
    rt.update(move |s, _| assert!(s.chat.open_dm(A, &peer)));
    settle(&mut rt);
    // The peer leaves the room: the empty thread has nothing to show.
    rt.update(|s, _| {
        s.chat.edit_rooms(|rooms| {
            let r = rooms.iter_mut().find(|r| r.key == A).unwrap();
            let me = r.me;
            r.members.retain(|m| m.id == me);
        });
    });
    settle(&mut rt);
    run_frames(&mut rt);
    assert!(!rt.state.chat.dm.is_open());
    assert_offset(&rt, 700.0);
}

#[test]
fn reopening_reads_the_room_exactly_as_without_the_memory() {
    let mut rt = long_room(|_| {});
    rt.update(|s, _| s.chat.set_window_front(true));
    scroll_up(&mut rt, 500.0);
    select(&mut rt, B);
    arrive_in_a(&mut rt);
    rt.update(|s, _| {
        s.chat.edit_rooms(|rooms| {
            rooms.iter_mut().find(|r| r.key == A).unwrap().unread = 1;
        });
    });
    read_reports(&mut rt);
    select(&mut rt, A);
    assert_eq!(unread(&rt), 0, "opening the room reads it");
    assert_eq!(
        rt.state.chat.read.get(A).cloned(),
        super::ReadMark::of(rt.state.chat.room().unwrap()),
        "the local mark is at the newest message"
    );
    assert_offset(&rt, 500.0);
}

#[test]
fn an_offset_is_dropped_with_its_room() {
    let mut rt = long_room(|_| {});
    scroll_up(&mut rt, 500.0);
    select(&mut rt, B);
    assert!(rt.state.chat.scroll_memory.offsets.contains_key(A));
    rt.update(|s, _| {
        s.chat.full_list = true;
        s.chat.edit_rooms(|rooms| rooms.retain(|r| r.key != A));
    });
    settle(&mut rt);
    assert!(!rt.state.chat.scroll_memory.offsets.contains_key(A));
}
