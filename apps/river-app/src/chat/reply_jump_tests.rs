//! A press on a reply's quoted strip jumps to the message it quotes, as the web UI does
//! (`ui/src/components/conversation/reply_highlight.rs`).

use rune::{Env, Key, KeyEvent, Runtime};

use super::model::Message;
use crate::{RiverApp, Setup, WINDOW_SIZE, view};

const WINDOW: &str = "history-scroll-town-square";

/// The example Town Square with 60 filler messages and then three replies: to the oldest
/// message, to the newest filler, and (twice) to message 6.
pub(super) fn long_room(edit: impl FnOnce(&mut Vec<super::Room>)) -> Runtime<RiverApp> {
    let mut s = RiverApp::detached();
    s.setup = Setup::Connected {
        version: "0.2.155".into(),
    };
    s.chat = super::Chat::new(
        super::Source::Node(super::LinkStatus::Live),
        Vec::new(),
        0,
        0,
        0,
    );
    let mut rooms = super::example::data().rooms;
    let r = rooms.iter_mut().find(|r| r.key == "town-square").unwrap();
    let template: Message = r.messages.last().unwrap().clone();
    let base = r.messages.len() as u32;
    for i in 0..60u32 {
        let mut m = template.clone();
        m.key = format!("long-{i}");
        m.id = base + i + 1;
        m.time += i + 1;
        m.text = format!("filler line number {i}");
        m.md = m.text.clone();
        m.reactions.clear();
        m.reply_to = None;
        r.messages.push(m);
    }
    let first = r
        .messages
        .iter()
        .find(|m| m.key == "long-10")
        .unwrap()
        .clone();
    let newest_filler = r.messages.last().unwrap().clone();
    let author = template.author;
    let six = r.messages.iter().find(|m| m.id == 6).unwrap().clone();
    let mut add = |key: &str, to: &Message| {
        let mut m = template.clone();
        m.key = key.into();
        m.id = r.messages.len() as u32 + 1;
        m.time += 100 + m.id;
        m.text = format!("reply {key}");
        m.md = m.text.clone();
        m.reactions.clear();
        m.reply_to = Some(super::ReplyTo {
            message: to.id,
            author: to.author,
            preview: super::model::preview(&to.text, 100),
        });
        r.messages.push(m);
    };
    add("reply-first", &first);
    add("reply-six-a", &six);
    add("reply-six-b", &six);
    add("reply-newest", &newest_filler);
    let _ = author;
    edit(&mut rooms);
    s.chat
        .show_node_rooms(rooms, None, super::LinkStatus::Live, 29_000_000);
    let mut rt = Runtime::new(
        s,
        view,
        WINDOW_SIZE,
        Env {
            system_dark: true,
            scale: 1.0,
        },
    );
    rt.on_key = crate::spec().on_key;
    rt.set_tab_visits_controls(true);
    rt.frame(0.0);
    settle(&mut rt);
    rt
}

pub(super) fn settle(rt: &mut Runtime<RiverApp>) {
    for i in 1..=30 {
        rt.frame(10.0 + f64::from(i) * 0.05);
    }
}

/// What the app's per-frame hook does until the staged scroll goes out.
fn drive_reveal(rt: &mut Runtime<RiverApp>) {
    for _ in 0..10 {
        let mut again = false;
        rt.update(|s, tx| again = s.chat.apply_reveal(tx));
        settle(rt);
        if !again {
            break;
        }
    }
}

fn strip(msg: &str) -> String {
    format!("msg-{msg}/reply")
}

fn click(rt: &mut Runtime<RiverApp>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no {key}"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    settle(rt);
    drive_reveal(rt);
}

fn view_rect(rt: &Runtime<RiverApp>) -> [f32; 4] {
    rt.presented(WINDOW).expect("the history is drawn").0
}

fn centre_of(rt: &Runtime<RiverApp>, msg: &str) -> f32 {
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(format!("row-{msg}").as_str()))
        .unwrap_or_else(|| panic!("row-{msg} is not drawn"));
    e.rect[1] + e.rect[3] / 2.0
}

fn assert_centred(rt: &Runtime<RiverApp>, msg: &str) {
    let v = view_rect(rt);
    let c = centre_of(rt, msg);
    let mid = v[1] + v[3] / 2.0;
    assert!(
        (c - mid).abs() < 6.0,
        "{msg} is centred: centre {c}, view middle {mid}"
    );
}

fn id_of(rt: &Runtime<RiverApp>, key: &str) -> u32 {
    rt.state
        .chat
        .room()
        .unwrap()
        .message_by_key(key)
        .unwrap()
        .id
}

#[test]
fn pressing_the_strip_centres_the_original_and_marks_it() {
    let mut rt = long_room(|_| {});
    let target = "long-10".to_owned();
    let (_, y) = rt.find(&format!("row-{target}")).unwrap_or((0.0, -1e6));
    let v = view_rect(&rt);
    assert!(
        !(y > v[1] && y < v[1] + v[3]),
        "the original starts off screen"
    );
    assert_eq!(rt.state.chat.flashing(), None);
    click(&mut rt, &strip("reply-first"));
    assert_eq!(rt.state.chat.flashing(), Some(target.as_str()));
    assert_centred(&rt, &target);
    // It stays there: the bottom-sticking history does not pull it back.
    let before = centre_of(&rt, &target);
    settle(&mut rt);
    assert!((centre_of(&rt, &target) - before).abs() < 1.0);
    // The mark ends by the clock it is given.
    let t0 = std::time::Instant::now();
    let id = id_of(&rt, "long-10");
    rt.update(move |s, _| {
        s.chat.reply_jump(id, t0);
        assert!(s.chat.tick_flash(t0 + super::search::REPLY_FLASH_FOR / 2));
        assert!(!s.chat.tick_flash(t0 + super::search::REPLY_FLASH_FOR));
    });
}

#[test]
fn the_strip_is_a_labelled_button() {
    let rt = long_room(|_| {});
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.path.ends_with("msg-reply-first/reply"))
        .expect("the strip");
    assert_eq!(e.role, "button");
    assert!(
        e.label
            .as_deref()
            .is_some_and(|l| l.starts_with("Jump to the message from ")),
        "{:?}",
        e.label
    );
}

#[test]
fn enter_and_space_on_the_focused_strip_jump() {
    for k in [Key::Enter, Key::Text(" ".into())] {
        let mut rt = long_room(|_| {});
        let target = "long-10".to_owned();
        // Tab until the strip has the keyboard.
        let mut found = false;
        let mut seen = Vec::new();
        for _ in 0..80 {
            rt.key(KeyEvent {
                key: Key::Tab,
                mods: Default::default(),
            });
            rt.frame(0.3);
            seen.extend(
                rt.elements()
                    .into_iter()
                    .filter(|e| e.focused)
                    .map(|e| e.path),
            );
            if rt
                .elements()
                .into_iter()
                .any(|e| e.focused && e.path.ends_with("msg-reply-first/reply"))
            {
                found = true;
                break;
            }
        }
        if !found {
            let f: Vec<_> = rt
                .elements()
                .into_iter()
                .filter(|e| e.focused)
                .map(|e| e.path)
                .collect();
            panic!("focused: {f:?} seen {seen:?}");
        }
        rt.key(KeyEvent {
            key: k.clone(),
            mods: Default::default(),
        });
        settle(&mut rt);
        drive_reveal(&mut rt);
        assert_eq!(rt.state.chat.flashing(), Some(target.as_str()), "{k:?}");
        assert_centred(&rt, &target);
    }
}

#[test]
fn dragging_across_the_strip_does_not_jump() {
    let mut rt = long_room(|_| {});
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.path.ends_with("msg-reply-first/reply"))
        .unwrap();
    let (x0, y) = (e.rect[0] + 20.0, e.rect[1] + e.rect[3] / 2.0);
    rt.pointer_down(x0, y);
    rt.pointer_move(x0 + 40.0, y);
    rt.pointer_move(x0 + 120.0, y);
    rt.pointer_up(x0 + 120.0, y);
    settle(&mut rt);
    drive_reveal(&mut rt);
    assert_eq!(rt.state.chat.flashing(), None);
    assert!(rt.state.chat.pending_reveal().is_none());
}

#[test]
fn the_press_does_not_open_a_menu_or_start_a_reply() {
    let mut rt = long_room(|_| {});
    click(&mut rt, &strip("reply-first"));
    assert!(
        matches!(rt.state.chat.mode, super::Mode::Write),
        "no reply or edit mode"
    );
    assert!(rt.state.chat.outgoing.is_empty());
}

#[test]
fn a_reply_to_a_missing_message_is_not_a_button_and_does_not_scroll() {
    let mut rt = long_room(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == "town-square").unwrap();
        let gone = r.messages.iter().position(|m| m.id == 6).unwrap();
        r.messages.remove(gone);
    });
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.path.ends_with("msg-reply-six-a/reply"))
        .expect("the strip is still drawn");
    assert_ne!(e.role, "button");
    assert!(
        rt.elements()
            .into_iter()
            .any(|e| e.text.as_deref() == Some("Original message unavailable"))
    );
    let before = centre_of(&rt, "reply-newest");
    click(&mut rt, &strip("reply-six-a"));
    assert_eq!(rt.state.chat.flashing(), None);
    assert!((centre_of(&rt, "reply-newest") - before).abs() < 1.0);
    // The model call refuses too.
    rt.update(|s, _| s.chat.reply_jump(6, std::time::Instant::now()));
    assert!(rt.state.chat.pending_reveal().is_none());
}

#[test]
fn an_arrival_after_the_jump_does_not_pull_the_reader_to_the_bottom() {
    let mut rt = long_room(|_| {});
    let target = "long-10".to_owned();
    click(&mut rt, &strip("reply-first"));
    let before = centre_of(&rt, &target);
    rt.update(|s, _| {
        s.chat.edit_rooms(|rooms| {
            let r = rooms.iter_mut().find(|r| r.key == "town-square").unwrap();
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
    settle(&mut rt);
    assert!(!rt.state.chat.at_bottom, "Latest stays offered");
    assert!(
        (centre_of(&rt, &target) - before).abs() < 1.0,
        "the list did not move"
    );
}

#[test]
fn two_replies_to_one_message_both_jump_to_it() {
    let mut rt = long_room(|_| {});
    let six = rt
        .state
        .chat
        .room()
        .unwrap()
        .messages
        .iter()
        .find(|m| m.id == 6)
        .unwrap()
        .key
        .clone();
    assert_eq!(id_of(&rt, &six), 6);
    for reply in ["reply-six-a", "reply-six-b"] {
        // Back to the end first, so each press has to travel.
        rt.update(|_, tx| tx.scroll_to_bottom_instant(WINDOW));
        settle(&mut rt);
        click(&mut rt, &strip(reply));
        assert_eq!(rt.state.chat.flashing(), Some(six.as_str()));
        let v = view_rect(&rt);
        let c = centre_of(&rt, &six);
        assert!(c >= v[1] && c <= v[1] + v[3], "{six} is in view");
    }
}

#[test]
fn a_reply_to_the_newest_message_jumps_without_error() {
    let mut rt = long_room(|_| {});
    click(&mut rt, &strip("reply-newest"));
    assert_eq!(rt.state.chat.flashing(), Some("long-59"));
    let v = view_rect(&rt);
    let c = centre_of(&rt, "long-59");
    assert!(c >= v[1] && c <= v[1] + v[3], "visible");
}

fn focus_strip(rt: &mut Runtime<RiverApp>, msg: &str) {
    for _ in 0..80 {
        rt.key(KeyEvent {
            key: Key::Tab,
            mods: Default::default(),
        });
        rt.frame(0.3);
        if rt
            .elements()
            .into_iter()
            .any(|e| e.focused && e.path.ends_with(&format!("msg-{msg}/reply")))
        {
            return;
        }
    }
    panic!("Tab never reached the strip");
}

#[test]
fn tabbing_to_the_strip_shows_the_focus_ring() {
    let mut rt = long_room(|_| {});
    focus_strip(&mut rt, "reply-first");
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.focused && e.path.ends_with("msg-reply-first/reply"))
        .unwrap();
    assert!(e.focus_visible, "keyboard focus is drawn");
}

#[test]
fn a_reply_to_the_very_first_message_jumps_there() {
    let mut rt = long_room(|rooms| {
        // Quote the oldest message from the first reply.
        let r = rooms.iter_mut().find(|r| r.key == "town-square").unwrap();
        let first = r.messages.first().unwrap().clone();
        let reply = r
            .messages
            .iter_mut()
            .find(|m| m.key == "reply-first")
            .unwrap();
        reply.reply_to = Some(super::ReplyTo {
            message: first.id,
            author: first.author,
            preview: super::model::preview(&first.text, 100),
        });
    });
    let first = rt.state.chat.room().unwrap().messages[0].key.clone();
    click(&mut rt, &strip("reply-first"));
    assert_eq!(rt.state.chat.flashing(), Some(first.as_str()));
    let v = view_rect(&rt);
    let c = centre_of(&rt, &first);
    assert!(
        c >= v[1] && c <= v[1] + v[3],
        "the oldest message is in view"
    );
}

#[test]
fn a_target_taller_than_the_view_is_brought_into_view_from_its_top() {
    let mut rt = long_room(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == "town-square").unwrap();
        let m = r.messages.iter_mut().find(|m| m.key == "long-10").unwrap();
        m.text = (0..80)
            .map(|i| format!("tall line {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        m.md = m.text.clone();
    });
    click(&mut rt, &strip("reply-first"));
    let v = view_rect(&rt);
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("row-long-10"))
        .expect("drawn");
    assert!(e.rect[3] > v[3], "the message is taller than the view");
    // As the web UI: the top of the message is in view, not its middle.
    assert!(
        e.rect[1] >= v[1] - 8.0 && e.rect[1] < v[1] + v[3] / 2.0,
        "its top ({}) is in the upper half of the view {v:?}",
        e.rect[1]
    );
}

#[test]
fn an_arrival_after_a_jump_near_the_end_leaves_the_target_in_view() {
    let mut rt = long_room(|_| {});
    click(&mut rt, &strip("reply-newest"));
    rt.update(|s, _| {
        s.chat.edit_rooms(|rooms| {
            let r = rooms.iter_mut().find(|r| r.key == "town-square").unwrap();
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
    settle(&mut rt);
    let v = view_rect(&rt);
    let c = centre_of(&rt, "long-59");
    assert!(c >= v[1] && c <= v[1] + v[3], "the target is still in view");
}

#[test]
fn a_press_with_three_points_of_travel_still_jumps() {
    let mut rt = long_room(|_| {});
    let target = "long-10".to_owned();
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.path.ends_with("msg-reply-first/reply"))
        .unwrap();
    let (x, y) = (e.rect[0] + 20.0, e.rect[1] + e.rect[3] / 2.0);
    rt.pointer_down(x, y);
    rt.pointer_move(x + 3.0, y);
    rt.pointer_up(x + 3.0, y);
    settle(&mut rt);
    drive_reveal(&mut rt);
    assert_eq!(rt.state.chat.flashing(), Some(target.as_str()));
}

#[test]
fn a_cancelled_drag_does_not_swallow_the_next_key_press() {
    let mut rt = long_room(|_| {});
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.path.ends_with("msg-reply-first/reply"))
        .unwrap();
    let (x, y) = (e.rect[0] + 20.0, e.rect[1] + e.rect[3] / 2.0);
    rt.pointer_down(x, y);
    rt.pointer_move(x + 60.0, y);
    // Escape and losing the window's focus both cancel a held drag.
    rt.key(KeyEvent {
        key: Key::Escape,
        mods: Default::default(),
    });
    rt.window_focus(false);
    rt.window_focus(true);
    rt.pointer_up(x + 60.0, y);
    settle(&mut rt);
    assert_eq!(rt.state.chat.flashing(), None, "a drag is not a click");
    focus_strip(&mut rt, "reply-first");
    rt.key(KeyEvent {
        key: Key::Text(" ".into()),
        mods: Default::default(),
    });
    settle(&mut rt);
    drive_reveal(&mut rt);
    assert_eq!(rt.state.chat.flashing(), Some("long-10"));
}
