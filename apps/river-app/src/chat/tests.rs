//! The chat screen driven headlessly, as a person would: pressing rows and buttons, typing.

use rune::{Env, Key, KeyEvent, Runtime};

use super::example::{EMPTY_ROOM, PRIVATE_ROOM};
use crate::{NARROW_SCENE_SIZE, RiverApp, SCENES, Setup, WINDOW_SIZE, scene_size, view};

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
    rt.frame(0.0);
    rt.frame(1.0 / 60.0);
    rt
}

fn press(rt: &mut Runtime<RiverApp>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no element {key}"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.1);
    rt.frame(0.2);
}

fn key(rt: &mut Runtime<RiverApp>, key: Key) {
    rt.key(KeyEvent {
        key,
        mods: Default::default(),
    });
    rt.frame(0.3);
}

/// Every piece of text on screen, in tree order.
fn texts(rt: &Runtime<RiverApp>) -> Vec<String> {
    rt.elements().into_iter().filter_map(|e| e.text).collect()
}

fn shows(rt: &Runtime<RiverApp>, needle: &str) -> bool {
    texts(rt).iter().any(|t| t.contains(needle))
}

/// Whether the element keyed `key` shows at least partly inside its scroll viewport.
fn visible(rt: &Runtime<RiverApp>, key: &str) -> bool {
    rt.elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key))
        .is_some_and(|e| e.visible[2] > 0.0 && e.visible[3] > 0.0)
}

#[test]
fn connecting_shows_the_chat_screen() {
    let rt = render(connected(), WINDOW_SIZE);
    for part in ["rooms", "room-header", "history", "composer", "members"] {
        assert!(rt.find(part).is_some(), "missing {part}");
    }
    assert!(shows(&rt, "Town Square"));
    // Example rooms say so, and do not claim a connection.
    assert!(shows(&rt, "Example rooms, not connected to Freenet"));
    assert!(!shows(&rt, "Connected to Freenet"));
}

#[test]
fn selecting_a_room_opens_its_conversation() {
    let mut rt = render(connected(), WINDOW_SIZE);
    assert!(!shows(&rt, "pushed the new mockups"));
    press(&mut rt, &format!("room-{PRIVATE_ROOM}"));
    assert_eq!(rt.state.chat.room().unwrap().key, PRIVATE_ROOM);
    assert_eq!(rt.state.chat.room().unwrap().unread, 0);
    assert!(shows(&rt, "pushed the new mockups"));
    assert!(rt.find("private-badge").is_some());
    assert!(!shows(&rt, "Release notes are at"));
    // Two days back is written as a date, so the example's calendar reaches the view.
    assert!(shows(&rt, "Wednesday, November 4"));
}

#[test]
fn only_the_open_room_loses_its_unread_badge() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let badge = |room: &str| format!("room-{room}/unread");
    assert!(rt.find(&badge(PRIVATE_ROOM)).is_some());
    assert!(rt.find(&badge("rust-help")).is_some());
    press(&mut rt, &format!("room-{PRIVATE_ROOM}"));
    assert!(rt.find(&badge(PRIVATE_ROOM)).is_none());
    assert!(rt.find(&badge("rust-help")).is_some());
}

#[test]
fn the_empty_room_invites_a_first_message() {
    let mut rt = render(connected(), WINDOW_SIZE);
    press(&mut rt, &format!("room-{EMPTY_ROOM}"));
    assert!(rt.find("history-empty").is_some());
    assert!(shows(&rt, "No messages yet"));
}

#[test]
fn typing_and_pressing_return_sends_and_clears_the_composer() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let before = rt.state.chat.room().unwrap().messages.len();
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Text("Hello from the desktop".into()));
    assert_eq!(rt.state.chat.composer.text, "Hello from the desktop");
    key(&mut rt, Key::Enter);
    let room = rt.state.chat.room().unwrap();
    assert_eq!(room.messages.len(), before + 1);
    let last = room.last_message().unwrap();
    assert_eq!(last.text, "Hello from the desktop");
    assert_eq!(last.author, room.me);
    assert_eq!(rt.state.chat.composer.text, "");
    assert!(shows(&rt, "Hello from the desktop"));
}

#[test]
fn a_draft_stays_in_the_room_it_was_typed_in() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let texts_in = |rt: &Runtime<RiverApp>, key: &str| {
        rt.state
            .chat
            .rooms()
            .iter()
            .find(|r| r.key == key)
            .unwrap()
            .messages
            .iter()
            .map(|m| m.text.clone())
            .collect::<Vec<_>>()
    };
    let town_before = texts_in(&rt, "town-square");
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Text("for Town Square only".into()));
    press(&mut rt, &format!("room-{PRIVATE_ROOM}"));
    assert_eq!(rt.state.chat.composer.text, "");
    assert!(!shows(&rt, "for Town Square only"));
    // Type and send in the other room: only its own text goes, and only to it. A leaked
    // draft would arrive here as "for Town Square onlyfor Design Crew", or in Town Square.
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Text("for Design Crew".into()));
    key(&mut rt, Key::Enter);
    assert_eq!(
        texts_in(&rt, PRIVATE_ROOM).last().map(String::as_str),
        Some("for Design Crew")
    );
    assert_eq!(texts_in(&rt, "town-square"), town_before);
    press(&mut rt, "room-town-square");
    assert_eq!(rt.state.chat.composer.text, "for Town Square only");
    assert!(shows(&rt, "for Town Square only"));
}

#[test]
fn the_send_button_sends_too() {
    let mut state = connected();
    state.chat.composer.replace_text("Pressed, not typed");
    let mut rt = render(state, WINDOW_SIZE);
    press(&mut rt, "send");
    assert_eq!(
        rt.state.chat.room().unwrap().last_message().unwrap().text,
        "Pressed, not typed"
    );
    assert!(rt.state.chat.composer.text.is_empty());
}

#[test]
fn controls_that_do_nothing_yet_say_so() {
    let rt = render(connected(), WINDOW_SIZE);
    for key in ["create-room", "enter-invite", "invite", "emoji"] {
        let e = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(key))
            .unwrap_or_else(|| panic!("no {key}"));
        assert_eq!(e.help.as_deref(), Some(super::view::NOT_YET), "{key}");
    }
}

#[test]
fn each_room_opens_at_its_newest_message() {
    // Two long rooms: Town Square and a copy of it.
    let mut state = connected();
    let mut copy = state.chat.rooms()[0].clone();
    copy.key = "town-square-copy".into();
    copy.name = "Town Square copy".into();
    state.chat.edit_rooms(|rooms| rooms.push(copy));
    let town = &state.chat.rooms()[0];
    let first = format!("msg-{}", town.messages[0].id);
    let last = format!("msg-{}", town.last_message().unwrap().id);
    let mut rt = render(state, WINDOW_SIZE);
    assert!(visible(&rt, &last) && !visible(&rt, &first));
    // Scroll Town Square to its top.
    let (x, y) = rt.find("history").unwrap();
    let mut t = 1.0;
    for _ in 0..40 {
        rt.wheel(x, y, 200.0);
        t += 1.0 / 60.0;
        rt.frame(t);
    }
    for _ in 0..120 {
        t += 1.0 / 60.0;
        rt.frame(t);
    }
    assert!(visible(&rt, &first), "the wheel scrolled the history up");
    // The other long room opens at its end, not at Town Square's offset.
    let (x, y) = rt.find("room-town-square-copy").unwrap();
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    for _ in 0..30 {
        t += 1.0 / 60.0;
        rt.frame(t);
    }
    assert_eq!(rt.state.chat.room().unwrap().key, "town-square-copy");
    assert!(visible(&rt, &last), "the newest message shows");
    assert!(!visible(&rt, &first));
}

#[test]
fn the_members_panel_toggles() {
    let mut rt = render(connected(), WINDOW_SIZE);
    assert!(rt.find("members").is_some());
    press(&mut rt, "toggle-members");
    assert!(!rt.state.chat.members_open);
    assert!(rt.find("members").is_none());
    press(&mut rt, "toggle-members");
    assert!(rt.find("members").is_some());
}

#[test]
fn a_narrow_window_leaves_out_the_members_panel() {
    let rt = render(connected(), NARROW_SCENE_SIZE);
    assert!(rt.state.chat.members_open);
    assert!(rt.find("members").is_none());
    assert!(rt.find("composer").is_some());
}

#[test]
fn a_narrow_window_opens_members_over_the_conversation() {
    let mut rt = render(connected(), NARROW_SCENE_SIZE);
    press(&mut rt, "toggle-members");
    assert!(rt.state.chat.members_overlay);
    assert!(rt.find("members-overlay").is_some());
    // The wide layout's choice is untouched.
    assert!(rt.state.chat.members_open);
    // The overlay covers the header's toggle, so it has its own close button.
    press(&mut rt, "close-members");
    assert!(!rt.state.chat.members_overlay);
    assert!(rt.find("members").is_none());
    // Pressing the dimmed conversation closes it too.
    press(&mut rt, "toggle-members");
    press(&mut rt, "members-scrim");
    assert!(!rt.state.chat.members_overlay);
    // Tab stays inside it, and Escape closes it.
    press(&mut rt, "toggle-members");
    assert!(rt.state.chat.members_overlay);
    for i in 0..8 {
        key(&mut rt, Key::Tab);
        let focused: Vec<_> = rt.elements().into_iter().filter(|e| e.focused).collect();
        if i == 0 {
            assert!(!focused.is_empty(), "Tab focuses something in the panel");
        }
        for e in focused {
            assert!(e.path.contains("members-overlay"), "Tab reached {}", e.path);
        }
    }
    key(&mut rt, Key::Escape);
    assert!(!rt.state.chat.members_overlay);
    // The room list is behind it too: a press there closes the panel and opens nothing.
    press(&mut rt, "toggle-members");
    press(&mut rt, &format!("room-{PRIVATE_ROOM}"));
    assert!(!rt.state.chat.members_overlay);
    assert_eq!(rt.state.chat.room().unwrap().key, "town-square");
}

#[test]
fn every_scene_renders() {
    for (name, apply) in SCENES {
        let mut state = RiverApp::detached();
        apply(&mut state);
        let rt = render(state, scene_size(name));
        assert!(
            rt.find("root").is_some() || rt.find("chat").is_some(),
            "{name}"
        );
        assert!(!texts(&rt).is_empty(), "{name} shows no text");
    }
}

/// The narrow scenes are rendered at the narrow size only because their names say "narrow"
/// (`scene_size`, and the matching glob in CI's render step). Check that names and layouts
/// agree, so a renamed scene cannot quietly render in the wrong layout.
#[test]
fn narrow_scenes_are_named_narrow() {
    let mut narrow = 0;
    for (name, apply) in SCENES {
        let mut state = RiverApp::detached();
        apply(&mut state);
        if !matches!(state.setup, Setup::Connected { .. }) {
            continue;
        }
        // The members overlay only exists in a narrow window.
        if state.chat.members_overlay {
            assert!(name.contains("narrow"), "{name} needs the narrow size");
        }
        let rt = render(state, scene_size(name));
        // A narrow window never puts the members panel beside the conversation.
        let beside = rt.find("members").is_some() && rt.find("members-overlay").is_none();
        let is_narrow = name.contains("narrow");
        assert_eq!(beside, !is_narrow && rt.state.chat.members_open, "{name}");
        narrow += usize::from(is_narrow);
    }
    assert!(narrow > 0, "no scene shows the narrow layout");
}

#[test]
fn the_town_square_history_renders_its_parts() {
    let rt = render(connected(), WINDOW_SIZE);
    assert!(shows(&rt, "Yesterday"));
    assert!(shows(&rt, "Today"));
    // The release notes message's address is a link (`find` also finds link boxes).
    assert!(rt.find("https://freenet.org/news").is_some());
    assert!(rt.find("reply").is_some());
    assert!(rt.find("reaction-🎉").is_some());
    assert!(rt.find("tag-owner").is_some());
}

#[test]
fn removing_the_open_room_shows_another_not_an_empty_screen() {
    let mut rt = render(connected(), WINDOW_SIZE);
    rt.state
        .chat
        .edit_rooms(|rooms| rooms.retain(|r| r.key != "town-square"));
    // State changed outside a handler, so ask for the view to run again.
    rt.invalidate();
    rt.frame(0.5);
    rt.frame(0.6);
    assert!(rt.find("no-rooms").is_none());
    assert!(rt.find("room-header").is_some());
    let open = rt.state.chat.room().expect("a room is open").key.clone();
    assert_eq!(open, PRIVATE_ROOM);
    assert_eq!(rt.state.chat.room().unwrap().unread, 0);
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Text("still here".into()));
    key(&mut rt, Key::Enter);
    let last = rt.state.chat.room().unwrap().last_message().unwrap();
    assert_eq!(last.text, "still here");
    // With no rooms at all, the empty screen shows.
    rt.state.chat.set_rooms(Vec::new());
    rt.invalidate();
    rt.frame(0.7);
    rt.frame(0.8);
    assert!(rt.find("no-rooms").is_some());
}

/// Opening members in a narrow window, widening, typing, then narrowing again must not bring
/// the modal panel back and take the keyboard from the composer.
#[test]
fn widening_closes_the_narrow_members_panel() {
    let mut rt = render(connected(), NARROW_SCENE_SIZE);
    press(&mut rt, "toggle-members");
    assert!(rt.find("members-overlay").is_some());
    let mut t = 1.0;
    let mut settle = |rt: &mut Runtime<RiverApp>| {
        for _ in 0..4 {
            t += 1.0 / 60.0;
            rt.frame(t);
        }
    };
    rt.set_viewport(WINDOW_SIZE, 1.0);
    settle(&mut rt);
    assert!(!rt.state.chat.members_overlay);
    let (x, y) = rt.find("composer-field").unwrap();
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    settle(&mut rt);
    assert!(rt.state.chat.composer.focused);
    rt.set_viewport(NARROW_SCENE_SIZE, 1.0);
    settle(&mut rt);
    assert!(rt.find("members-overlay").is_none());
    assert!(rt.state.chat.composer.focused);
    rt.key(KeyEvent {
        key: Key::Text("still typing".into()),
        mods: Default::default(),
    });
    settle(&mut rt);
    assert_eq!(rt.state.chat.composer.text, "still typing");
}
