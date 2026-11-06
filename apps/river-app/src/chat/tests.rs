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
    let counts = |rt: &Runtime<RiverApp>| {
        rt.state
            .chat
            .rooms
            .iter()
            .map(|r| r.messages.len())
            .collect::<Vec<_>>()
    };
    let before = counts(&rt);
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Text("for Town Square only".into()));
    press(&mut rt, &format!("room-{PRIVATE_ROOM}"));
    assert_eq!(rt.state.chat.composer.text, "");
    assert!(!shows(&rt, "for Town Square only"));
    // Return in the other room sends nothing anywhere.
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Enter);
    assert_eq!(counts(&rt), before);
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
    let mut copy = state.chat.rooms[0].clone();
    copy.key = "town-square-copy".into();
    copy.name = "Town Square copy".into();
    state.chat.rooms.push(copy);
    let first = format!("msg-{}", state.chat.rooms[0].messages[0].id);
    let last = format!("msg-{}", state.chat.rooms[0].last_message().unwrap().id);
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
