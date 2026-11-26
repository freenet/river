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

#[test]
fn a_place_whose_message_is_gone_falls_back_to_the_offset() {
    let mut rt = long_room(|_| {});
    scroll_up(&mut rt, 500.0);
    select(&mut rt, B);
    // Every message the anchor could be (the sampled ones) goes.
    rt.update(move |s, _| {
        s.chat.edit_rooms(|rooms| {
            let r = rooms.iter_mut().find(|r| r.key == A).unwrap();
            let mut i = 0;
            r.messages.retain(|_| {
                i += 1;
                (i - 1) % 8 != 0
            });
        });
    });
    select(&mut rt, A);
    assert!(!rt.state.chat.at_bottom, "still away from the end");
    assert_eq!(rt.scroll_at_bottom(WINDOW), Some(false));
    assert!(
        (offset(&rt) - 500.0).abs() < 1.0,
        "the saved offset came back: {}",
        offset(&rt)
    );
}

fn offset(rt: &Runtime<RiverApp>) -> f32 {
    rt.state.chat.history_pos.borrow().offset
}

fn max_offset(rt: &Runtime<RiverApp>) -> f32 {
    rt.state.chat.history_pos.borrow().max
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

#[test]
fn several_separate_scrolls_before_leaving_are_all_remembered() {
    let mut rt = long_room(|_| {});
    // Scrolls with no other commit between them: only the first one flips "at the end".
    for y in [60.0, 400.0, 900.0, 640.0] {
        scroll_up(&mut rt, y);
    }
    let (row, y) = reading_row(&rt);
    let at = offset(&rt);
    select(&mut rt, B);
    select(&mut rt, A);
    let again = y_of(&rt, &row).unwrap_or_else(|| panic!("{row} is drawn after reopening"));
    assert!((again - y).abs() < 1.0, "{row} moved from {y} to {again}");
    assert!((offset(&rt) - at).abs() < 1.0);
}

#[test]
fn reading_inside_a_tall_last_message_is_remembered() {
    let mut rt = long_room(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == A).unwrap();
        let m = r.messages.last_mut().unwrap();
        m.md = (0..120)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        m.text = m.md.clone();
    });
    rt.update(|_, tx| tx.scroll_to_bottom_instant(WINDOW));
    settle(&mut rt);
    let to = max_offset(&rt) - 700.0;
    scroll_up(&mut rt, to);
    scroll_up(&mut rt, to - 37.0);
    let at = offset(&rt);
    select(&mut rt, B);
    select(&mut rt, A);
    assert!(!rt.state.chat.at_bottom);
    assert!((offset(&rt) - at).abs() < 1.0, "{at} -> {}", offset(&rt));
}

#[test]
fn a_thread_over_a_scrolled_room_gives_it_back_where_it_was() {
    let mut rt = long_room(|_| {});
    rt.update(|s, _| s.chat.set_window_front(true));
    scroll_up(&mut rt, 500.0);
    scroll_up(&mut rt, 700.0);
    let (row, y) = reading_row(&rt);
    let peer = peer(&rt);
    arrive_in_a(&mut rt);
    read_reports(&mut rt);
    rt.update(move |s, _| assert!(s.chat.open_dm(A, &peer)));
    settle(&mut rt);
    run_frames(&mut rt);
    rt.update(|s, _| s.chat.close_dm());
    settle(&mut rt);
    run_frames(&mut rt);
    let again = y_of(&rt, &row).unwrap_or_else(|| panic!("{row} is drawn after the thread"));
    assert!((again - y).abs() < 1.0, "{row} moved from {y} to {again}");
    assert!(read_reports(&mut rt).is_empty(), "nothing unseen was read");
}

#[test]
fn a_thread_that_goes_away_gives_the_room_back_where_it_was() {
    let mut rt = long_room(|_| {});
    rt.update(|s, _| s.chat.set_window_front(true));
    scroll_up(&mut rt, 700.0);
    let (row, y) = reading_row(&rt);
    let peer = peer(&rt);
    arrive_in_a(&mut rt);
    read_reports(&mut rt);
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
    let again = y_of(&rt, &row).unwrap_or_else(|| panic!("{row} is drawn again"));
    assert!((again - y).abs() < 1.0, "{row} moved from {y} to {again}");
    assert!(read_reports(&mut rt).is_empty(), "nothing unseen was read");
}

#[test]
fn a_room_reopened_at_a_place_keeps_what_is_below_it_unread_until_the_end() {
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
    let unread = |rt: &Runtime<RiverApp>| {
        rt.state
            .chat
            .rooms()
            .iter()
            .find(|r| r.key == A)
            .unwrap()
            .unread
    };
    assert_eq!(unread(&rt), 1, "the arrival below the place is unread");
    let marked = rt.state.chat.read.get(A).cloned();
    assert!(
        marked.is_none_or(|m| Some(m) != super::ReadMark::of(rt.state.chat.room().unwrap())),
        "the local mark has not moved to the newest message"
    );
    rt.update(|_, tx| tx.scroll_to_bottom_instant(WINDOW));
    settle(&mut rt);
    run_frames(&mut rt);
    assert_eq!(unread(&rt), 0, "reaching the end reads it");
    assert_eq!(read_reports(&mut rt), ["arrival".to_owned()]);
}

#[test]
fn a_place_is_dropped_with_its_room() {
    let mut rt = long_room(|_| {});
    scroll_up(&mut rt, 500.0);
    select(&mut rt, B);
    assert!(rt.state.chat.scroll_memory.borrow().contains_key(A));
    rt.update(|s, _| {
        s.chat.full_list = true;
        s.chat.edit_rooms(|rooms| rooms.retain(|r| r.key != A));
    });
    settle(&mut rt);
    assert!(!rt.state.chat.scroll_memory.borrow().contains_key(A));
    assert_eq!(rt.state.chat.room().unwrap().key, B);
}

#[test]
fn a_room_opened_while_its_history_cannot_draw_is_not_read() {
    let mut rt = long_room(|_| {});
    rt.update(|s, _| s.chat.set_window_front(true));
    scroll_up(&mut rt, 500.0);
    select(&mut rt, B);
    arrive_in_a(&mut rt);
    read_reports(&mut rt);
    // Chosen, then covered by a thread before any frame ran: the history never draws.
    let peer = peer(&rt);
    rt.update(move |s, _| {
        s.chat.select(A);
        assert!(s.chat.open_dm(A, &peer));
    });
    settle(&mut rt);
    run_frames(&mut rt);
    assert!(read_reports(&mut rt).is_empty());
    assert!(!rt.state.chat.at_bottom);
}
