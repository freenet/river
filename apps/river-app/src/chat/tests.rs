//! The chat screen driven headlessly, as a person would: pressing rows and buttons, typing.

use rune::{Env, Key, KeyEvent, Runtime};

use super::example::{EMPTY_ROOM, PRIVATE_ROOM};
use crate::rooms::{Action, Command};
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

/// Press a control that shows only while the pointer is over its message: hover first, as a
/// person does.
fn hover_press(rt: &mut Runtime<RiverApp>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no element {key}"));
    rt.pointer_move(x, y);
    for i in 1..30 {
        rt.frame(f64::from(i) * 0.05);
    }
    press(rt, key);
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
fn shift_return_adds_a_line_and_only_return_sends() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let before = rt.state.chat.room().unwrap().messages.len();
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Text("first".into()));
    rt.key(KeyEvent {
        key: Key::Enter,
        mods: rune::Mods {
            shift: true,
            ..rune::Mods::default()
        },
    });
    rt.frame(0.0);
    key(&mut rt, Key::Text("second".into()));
    assert_eq!(rt.state.chat.composer.text, "first\nsecond");
    assert_eq!(
        rt.state.chat.room().unwrap().messages.len(),
        before,
        "nothing sent yet"
    );
    key(&mut rt, Key::Enter);
    let room = rt.state.chat.room().unwrap();
    assert_eq!(room.messages.len(), before + 1);
    assert_eq!(room.last_message().unwrap().text, "first\nsecond");
    assert_eq!(rt.state.chat.composer.text, "");
}

#[test]
fn a_press_on_the_composer_boxs_padding_focuses_the_editor() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let boxed = rt.find("composer-input").expect("the input row");
    // The row's own top-left padding, outside the one-line editor.
    let el = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("composer-input"))
        .unwrap();
    let (x, y) = (el.rect[0] + 3.0, el.rect[1] + 2.0);
    let _ = boxed;
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.1);
    rt.frame(0.2);
    key(&mut rt, Key::Text("hi".into()));
    assert_eq!(rt.state.chat.composer.text, "hi");
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
    for key in ["create-room", "enter-invite", "invite"] {
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
    let first = format!("msg-{}", town.messages[0].key);
    let last = format!("msg-{}", town.last_message().unwrap().key);
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
        // Only a chat screen with a room open has a members panel.
        let room_open = matches!(
            rt.state.access,
            crate::Access::Example | crate::Access::Ready
        ) && rt.state.chat.room().is_some();
        assert_eq!(
            beside,
            !is_narrow && room_open && rt.state.chat.members_open,
            "{name}"
        );
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
    assert!(rt.elements().iter().any(|e| {
        e.key
            .as_deref()
            .is_some_and(|k| k.starts_with("reaction-") && k.ends_with("-🎉"))
    }));
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

// Rooms read from the node ------------------------------------------------------------------

fn node_chat() -> RiverApp {
    let mut s = connected();
    s.chat = super::Chat::new(
        super::Source::Node(super::LinkStatus::Live),
        Vec::new(),
        0,
        0,
        0,
    );
    s
}

fn node_rooms(edit: impl FnOnce(&mut Vec<super::Room>)) -> Runtime<RiverApp> {
    let mut s = node_chat();
    let mut rooms = super::example::data().rooms;
    edit(&mut rooms);
    s.chat
        .show_node_rooms(rooms, None, super::LinkStatus::Live, 29_000_000);
    render(s, WINDOW_SIZE)
}

fn sent(rt: &mut Runtime<RiverApp>) -> Vec<Action> {
    std::mem::take(&mut rt.state.chat.outgoing)
        .into_iter()
        .map(|c| c.action)
        .collect()
}

/// A message in the open room by its author: (key, is it mine).
fn message_of(rt: &Runtime<RiverApp>, mine: bool) -> String {
    let room = rt.state.chat.room().unwrap();
    room.messages
        .iter()
        .rev()
        .find(|m| (m.author == room.me) == mine)
        .unwrap()
        .key
        .clone()
}

#[test]
fn a_message_typed_in_a_node_room_goes_to_the_node_and_is_not_added_here() {
    let mut rt = node_rooms(|_| {});
    let room = rt.state.chat.room().unwrap().key.clone();
    let before = rt.state.chat.room().unwrap().messages.len();
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Text("hello node".into()));
    key(&mut rt, Key::Enter);
    let commands = std::mem::take(&mut rt.state.chat.outgoing);
    assert_eq!(
        commands,
        [Command {
            room,
            action: Action::Send {
                text: "hello node".into(),
                reply_to: None
            }
        }]
    );
    // The node's next update shows it; nothing is made up here meanwhile.
    assert_eq!(rt.state.chat.room().unwrap().messages.len(), before);
    assert_eq!(rt.state.chat.composer.text, "");
}

#[test]
fn a_room_the_user_cannot_send_in_says_why_and_takes_nothing() {
    let reason = "You have been banned from sending messages in this room.";
    let mut rt = node_rooms(|rooms| {
        for r in rooms {
            r.send_block = Some(reason.into());
        }
    });
    assert!(shows(&rt, reason));
    rt.state.chat.composer.replace_text("hello");
    assert!(!rt.state.chat.send());
    press(&mut rt, "send");
    let target = message_of(&rt, false);
    // No action bar either, and the actions do nothing if reached another way.
    assert!(rt.find(&format!("reply-{target}")).is_none());
    rt.state.chat.start_reply(&target);
    rt.state.chat.react(&target, "👍");
    assert!(rt.state.chat.outgoing.is_empty());
}

#[test]
fn reply_edit_delete_and_react_become_commands() {
    let mut rt = node_rooms(|_| {});
    let theirs = message_of(&rt, false);
    let mine = message_of(&rt, true);
    // Only your own messages offer Edit and Delete.
    assert!(rt.find(&format!("edit-{theirs}")).is_none());
    assert!(rt.find(&format!("delete-{theirs}")).is_none());

    hover_press(&mut rt, &format!("reply-{theirs}"));
    assert!(rt.find("replying").is_some());
    key(&mut rt, Key::Text("agreed".into()));
    key(&mut rt, Key::Enter);
    assert_eq!(
        sent(&mut rt),
        [Action::Send {
            text: "agreed".into(),
            reply_to: Some(theirs.clone())
        }]
    );
    assert!(rt.find("replying").is_none());

    // Editing takes the composer and gives the draft back afterwards.
    rt.state.chat.composer.replace_text("my draft");
    let old = rt
        .state
        .chat
        .room()
        .unwrap()
        .message_by_key(&mine)
        .unwrap()
        .text
        .clone();
    hover_press(&mut rt, &format!("edit-{mine}"));
    assert_eq!(rt.state.chat.composer.text, old);
    rt.state.chat.composer.replace_text("fixed text");
    key(&mut rt, Key::Enter);
    assert_eq!(
        sent(&mut rt),
        [Action::Edit {
            target: mine.clone(),
            text: "fixed text".into()
        }]
    );
    assert_eq!(rt.state.chat.composer.text, "my draft");

    hover_press(&mut rt, &format!("delete-{mine}"));
    assert!(sent(&mut rt).is_empty(), "deleting asks first");
    press(&mut rt, "confirm-delete");
    assert_eq!(
        sent(&mut rt),
        [Action::Delete {
            target: mine.clone()
        }]
    );

    hover_press(&mut rt, &format!("react-{theirs}"));
    press(&mut rt, &format!("pick-{theirs}-0"));
    assert_eq!(
        sent(&mut rt),
        [Action::React {
            target: theirs.clone(),
            emoji: "👍".into()
        }]
    );
    assert!(rt.state.chat.picker.is_none());
}

fn two_message_ids(rt: &Runtime<RiverApp>) -> (String, String) {
    let ms = &rt.state.chat.room().unwrap().messages;
    (ms[ms.len() - 2].key.clone(), ms[ms.len() - 1].key.clone())
}

#[test]
fn a_press_outside_the_reaction_picker_closes_it_and_reacts_to_nothing() {
    let mut rt = node_rooms(|_| {});
    let (_, last) = two_message_ids(&rt);
    hover_press(&mut rt, &format!("react-{last}"));
    assert!(rt.state.chat.picker.is_some());
    press(&mut rt, "composer");
    assert!(rt.state.chat.picker.is_none());
    assert!(sent(&mut rt).is_empty());
    assert!(rt.find(&format!("picker-{last}")).is_none());
}

#[test]
fn escape_closes_the_reaction_picker() {
    let mut rt = node_rooms(|_| {});
    let (_, last) = two_message_ids(&rt);
    hover_press(&mut rt, &format!("react-{last}"));
    assert!(rt.state.chat.picker.is_some());
    key(&mut rt, Key::Escape);
    assert!(rt.state.chat.picker.is_none());
    assert!(sent(&mut rt).is_empty());
}

#[test]
fn the_react_button_of_another_message_opens_its_picker_after_the_outside_press() {
    let mut rt = node_rooms(|_| {});
    let (prev, last) = two_message_ids(&rt);
    hover_press(&mut rt, &format!("react-{last}"));
    // The press outside only closes; a second press opens the other message's picker.
    hover_press(&mut rt, &format!("react-{prev}"));
    assert!(rt.state.chat.picker.is_none());
    hover_press(&mut rt, &format!("react-{prev}"));
    assert_eq!(rt.state.chat.picker.as_deref(), Some(prev.as_str()));
    assert!(sent(&mut rt).is_empty());
}

#[test]
fn pressing_a_reaction_toggles_it() {
    let mut rt = node_rooms(|rooms| {
        let room = &mut rooms[0];
        let me = room.me;
        let last = room.messages.len() - 1;
        room.messages[last].reactions = vec![super::Reaction {
            emoji: "🎉".into(),
            by: vec![me],
        }];
    });
    let target = rt
        .state
        .chat
        .room()
        .unwrap()
        .last_message()
        .unwrap()
        .key
        .clone();
    press(&mut rt, &format!("reaction-{target}-🎉"));
    assert_eq!(
        sent(&mut rt),
        [Action::React {
            target,
            emoji: "🎉".into()
        }]
    );
}

#[test]
fn a_message_on_its_way_and_one_that_failed_say_so() {
    let mut rt = node_rooms(|rooms| {
        let room = &mut rooms[0];
        let n = room.messages.len();
        room.messages[n - 1].author = room.me;
        room.messages[n - 1].pending = Some(super::model::Pending {
            entry: 7,
            failed: Some("Freenet refused it: too big".into()),
            retry: true,
        });
        room.messages[n - 2].author = room.me;
        room.messages[n - 2].pending = Some(super::model::Pending {
            entry: 6,
            failed: None,
            retry: false,
        });
        room.notices = vec![super::model::Notice {
            entry: 9,
            text: "Your reaction couldn't be sent. Freenet refused it: no".into(),
            retry: true,
        }];
    });
    assert!(shows(&rt, "Sending…"));
    assert!(shows(&rt, "Not sent. Freenet refused it: too big"));
    assert!(shows(&rt, "Your reaction couldn't be sent."));
    // A message on its way can't be acted on yet.
    let pending = rt
        .state
        .chat
        .room()
        .unwrap()
        .messages
        .last()
        .unwrap()
        .key
        .clone();
    assert!(rt.find(&format!("actions-{pending}")).is_none());
    press(&mut rt, "retry-7");
    press(&mut rt, "discard-7");
    press(&mut rt, "retry-9");
    press(&mut rt, "dismiss-9");
    assert_eq!(
        sent(&mut rt),
        [
            Action::Retry { entry: 7 },
            Action::Discard { entry: 7 },
            Action::Retry { entry: 9 },
            Action::Discard { entry: 9 },
        ]
    );
}

#[test]
fn an_edit_starts_from_the_text_as_written_so_mentions_survive() {
    let token = "@[Ada](rv:0011223344556677)";
    let mut rt = node_rooms(|rooms| {
        let room = &mut rooms[0];
        let me = room.me;
        let m = room
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.author == me)
            .unwrap();
        m.text = "hi @Ada".into();
        m.source = format!("hi {token}");
    });
    let mine = message_of(&rt, true);
    rt.state.chat.start_edit(&mine);
    assert_eq!(rt.state.chat.composer.text, format!("hi {token}"));
}

#[test]
fn switching_rooms_ends_a_reply_or_edit() {
    let mut rt = node_rooms(|_| {});
    let mine = message_of(&rt, true);
    rt.state.chat.composer.replace_text("draft A");
    rt.state.chat.start_edit(&mine);
    press(&mut rt, &format!("room-{PRIVATE_ROOM}"));
    assert!(matches!(rt.state.chat.mode, super::Mode::Write));
    press(
        &mut rt,
        &format!("room-{}", super::example::data().rooms[0].key),
    );
    assert_eq!(rt.state.chat.composer.text, "draft A");
}

#[test]
fn example_rooms_reply_edit_delete_and_react_in_memory() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let theirs = message_of(&rt, false);
    let mine = message_of(&rt, true);
    rt.state.chat.start_reply(&theirs);
    rt.state.chat.composer.replace_text("a reply");
    assert!(rt.state.chat.send());
    let room = rt.state.chat.room().unwrap();
    let reply = room.last_message().unwrap();
    assert_eq!(reply.text, "a reply");
    assert_eq!(
        reply.reply_to.as_ref().map(|r| r.message),
        room.message_by_key(&theirs).map(|m| m.id)
    );
    rt.state.chat.start_edit(&mine);
    rt.state.chat.composer.replace_text("edited");
    assert!(rt.state.chat.send());
    let m = rt.state.chat.room().unwrap().message_by_key(&mine).unwrap();
    assert!(m.edited && m.text == "edited");
    rt.state.chat.react(&theirs, "👍");
    assert_eq!(
        rt.state.chat.room().unwrap().own_reaction(&theirs),
        Some("👍")
    );
    rt.state.chat.react(&theirs, "👍");
    assert_eq!(rt.state.chat.room().unwrap().own_reaction(&theirs), None);
    rt.state.chat.ask_delete(&mine);
    rt.state.chat.confirm_delete();
    assert!(
        rt.state
            .chat
            .room()
            .unwrap()
            .message_by_key(&mine)
            .is_none()
    );
    assert!(rt.state.chat.outgoing.is_empty());
}

#[test]
fn node_rooms_open_the_web_uis_room_once_and_use_the_clock() {
    let mut chat = node_chat().chat;
    let rooms = super::example::data().rooms;
    chat.show_node_rooms(
        rooms.clone(),
        Some(PRIVATE_ROOM),
        super::LinkStatus::Loading,
        3 * 1440 + 5,
    );
    assert_eq!(chat.room().unwrap().key, PRIVATE_ROOM);
    assert_eq!((chat.today, chat.day_zero, chat.now), (3, 0, 3 * 1440 + 5));
    assert_eq!(chat.source, super::Source::Node(super::LinkStatus::Loading));
    // The user moves on; a later update does not pull them back.
    let other = rooms[0].key.clone();
    chat.select(&other);
    chat.show_node_rooms(
        rooms,
        Some(PRIVATE_ROOM),
        super::LinkStatus::Live,
        3 * 1440 + 6,
    );
    assert_eq!(chat.room().unwrap().key, other);
}

#[test]
fn a_room_read_here_counts_unread_from_where_it_was_read() {
    let mut chat = node_chat().chat;
    let mut rooms = super::example::data().rooms;
    let key = PRIVATE_ROOM.to_owned();
    let i = rooms.iter().position(|r| r.key == key).unwrap();
    // The node says 2 unread; reading the room here clears them.
    rooms[i].unread = 2;
    chat.show_node_rooms(rooms.clone(), None, super::LinkStatus::Live, 0);
    chat.select(&key);
    let other = rooms.iter().find(|r| r.key != key).unwrap().key.clone();
    chat.select(&other);
    // The next update still carries the node's stale count of 2: it is not shown again.
    chat.show_node_rooms(rooms.clone(), None, super::LinkStatus::Live, 0);
    let unread = |c: &super::Chat| c.rooms().iter().find(|r| r.key == key).unwrap().unread;
    assert_eq!(unread(&chat), 0);
    // Two new messages from someone else, one in the same minute as the last read one.
    let room = &mut rooms[i];
    let last = room.last_message().unwrap().clone();
    let someone = room.members.iter().find(|m| m.id != room.me).unwrap().id;
    for (n, time) in [(1, last.time), (2, last.time + 1)] {
        room.messages.push(super::Message {
            id: last.id + n,
            key: format!("k{n}"),
            author: someone,
            time,
            text: "new".into(),
            source: "new".into(),
            md: String::new(),
            reply_to: None,
            reactions: vec![],
            edited: false,
            pending: None,
        });
    }
    // And one of this user's own, which is never unread.
    room.messages.push(super::Message {
        id: last.id + 3,
        key: "k3".into(),
        author: room.me,
        time: last.time + 2,
        text: "mine".into(),
        source: "mine".into(),
        md: String::new(),
        reply_to: None,
        reactions: vec![],
        edited: false,
        pending: None,
    });
    chat.show_node_rooms(rooms, None, super::LinkStatus::Live, 0);
    assert_eq!(unread(&chat), 2);
}

#[test]
fn no_node_rooms_says_where_rooms_come_from() {
    let rt = render(node_chat(), WINDOW_SIZE);
    assert!(rt.find("no-rooms").is_some());
    assert!(rt.find("composer").is_none());
}

#[test]
fn what_the_node_rooms_are_missing_is_said_in_the_room_list() {
    let mut s = node_chat();
    s.chat.show_node_rooms(
        super::example::data().rooms,
        None,
        super::LinkStatus::Live,
        29_000_000,
    );
    let rt = render(s, WINDOW_SIZE);
    for key in ["not-updated", "not-following", "unreadable"] {
        assert!(rt.find(key).is_none(), "{key} with nothing to say");
    }
    let mut s = node_chat();
    s.chat.show_node_rooms(
        super::example::data().rooms,
        None,
        super::LinkStatus::Live,
        29_000_000,
    );
    s.chat.issues = crate::rooms::Issues {
        store_missing: false,
        not_updated: vec!["Town Square".into()],
        not_following: vec!["Family".into()],
        unreadable: 2,
        update_required: true,
        setting_up: false,
    };
    let rt = render(s, WINDOW_SIZE);
    for key in [
        "update-required",
        "not-updated",
        "not-following",
        "unreadable",
    ] {
        assert!(rt.find(key).is_some(), "{key}");
    }
}

/// An app River has moved past says so before anything else, even with no store.
#[test]
fn an_empty_list_asks_for_an_update_when_river_has_moved_on() {
    let mut chat = node_chat().chat;
    chat.issues.store_missing = true;
    chat.issues.update_required = true;
    let hint = super::view::no_rooms_hint(&chat);
    assert!(hint.contains("newer version of River"), "{hint}");
    assert!(hint.contains("Update this app"), "{hint}");
}

#[test]
fn an_empty_list_says_why_when_the_store_is_missing() {
    let mut chat = node_chat().chat;
    let usual = super::view::no_rooms_hint(&chat);
    chat.issues.store_missing = true;
    let missing = super::view::no_rooms_hint(&chat);
    assert_ne!(usual, missing);
    assert!(missing.contains("couldn't set up its storage"), "{missing}");
    // Nothing tells a new user to open River in the browser first: the app sets up the store.
    for hint in [usual, missing] {
        assert!(!hint.contains("browser first"), "{hint}");
    }
    let rt = render(
        {
            let mut s = node_chat();
            s.chat.issues.store_missing = true;
            s
        },
        WINDOW_SIZE,
    );
    assert!(rt.find("no-rooms-hint").is_some());
}

/// Markdown that `links::prepare` gives up on is shown as the plain text, end to end: the
/// message's own words, `[label](url)` brackets and all, not the unprepared markdown.
#[test]
fn a_message_that_cannot_be_made_safe_is_shown_as_plain_text() {
    let id = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";
    // More than the rewrite cap of links, then a spoofed one.
    let text = format!(
        "{}[freenet:{id}](https://evil.example/)",
        "<ab:c> ".repeat(60)
    );
    let rt = node_rooms(|rooms| {
        let m = rooms[0].messages.last_mut().unwrap();
        m.text = text.clone();
        m.md = text.clone();
    });
    assert!(shows(&rt, "](https://evil.example/)"), "{:?}", texts(&rt));
    // The same text, within the cap, is drawn as markdown: the label only, no brackets.
    let ok = format!("see [freenet:{id}](https://evil.example/)");
    let rt = node_rooms(|rooms| {
        let m = rooms[0].messages.last_mut().unwrap();
        m.text = ok.clone();
        m.md = ok.clone();
    });
    assert!(!shows(&rt, "](https://evil.example/)"), "{:?}", texts(&rt));
}

/// A message that is cheap as written but not once its line breaks are made hard breaks is
/// shown as plain text, never as unprepared markdown.
#[test]
fn a_message_that_stops_being_cheap_after_its_line_breaks_is_plain_text() {
    let id = "raAqMhMG7KUpXBU2SxgCQ3Vh4PYjttxdSWd9ftV7RLv";
    let text = format!(
        "{}[freenet:{id}](https://evil.example/)",
        "a\n".repeat(1400)
    );
    assert!(super::markdown::is_cheap(&text));
    let rt = node_rooms(|rooms| {
        let m = rooms[0].messages.last_mut().unwrap();
        m.text = text.clone();
        m.md = text.clone();
    });
    assert!(shows(&rt, "](https://evil.example/)"), "{:?}", texts(&rt));
}

#[test]
fn dragging_over_a_message_selects_it_and_copy_takes_the_text() {
    let board = rune::clipboard::memory();
    let mut rt = render(connected(), WINDOW_SIZE);
    rt.use_clipboard(&board);
    rt.frame(0.1);
    let room = rt.state.chat.room().unwrap();
    let m = room
        .messages
        .iter()
        .find(|m| m.text.contains("Updated, and the room list"))
        .unwrap();
    let key_name = format!("md-{}", m.key);
    let el = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key_name.as_str()))
        .unwrap_or_else(|| panic!("no {key_name}"));
    let (x0, y0) = (el.rect[0] + 1.0, el.rect[1] + 8.0);
    let (x1, y1) = (el.rect[0] + el.rect[2] - 1.0, el.rect[1] + el.rect[3] - 4.0);
    rt.pointer_down(x0, y0);
    rt.pointer_move((x0 + x1) / 2.0, y1);
    rt.pointer_move(x1, y1);
    rt.pointer_up(x1, y1);
    rt.frame(0.2);
    rt.key(KeyEvent {
        key: Key::Command('c'),
        mods: rune::Mods {
            cmd: true,
            ctrl: !cfg!(target_os = "macos"),
            ..rune::Mods::default()
        },
    });
    rt.frame(0.3);
    assert_eq!(
        board.text().as_deref().map(String::as_str),
        Some("Updated, and the room list loads noticeably faster for me.")
    );
}

#[test]
fn a_message_with_links_and_chips_can_be_selected() {
    let board = rune::clipboard::memory();
    let mut state = connected();
    let scene = SCENES
        .iter()
        .find(|(n, _)| *n == "chat-markdown")
        .unwrap()
        .1;
    scene(&mut state);
    let mut rt = render(state, WINDOW_SIZE);
    rt.use_clipboard(&board);
    rt.frame(0.1);
    let el = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("md-md-sample-0"))
        .expect("the sample message");
    eprintln!("rect {:?}", el.rect);
    let (x0, y0) = (el.rect[0] + 1.0, el.rect[1] + 8.0);
    let (x1, y1) = (el.rect[0] + el.rect[2] - 1.0, el.rect[1] + el.rect[3] - 4.0);
    rt.pointer_down(x0, y0);
    rt.pointer_move((x0 + x1) / 2.0, y1);
    rt.pointer_move(x1, y1);
    rt.pointer_up(x1, y1);
    rt.frame(0.2);
    rt.key(KeyEvent {
        key: Key::Command('c'),
        mods: rune::Mods {
            cmd: true,
            ctrl: !cfg!(target_os = "macos"),
            ..rune::Mods::default()
        },
    });
    rt.frame(0.3);
    let got = board.text().map(|s| s.to_string()).unwrap_or_default();
    eprintln!("copied: {got:?}");
    assert!(
        got.contains("Release notes") && got.contains("freenet.org"),
        "{got:?}"
    );
}

#[test]
fn dragging_across_messages_selects_across_them() {
    let board = rune::clipboard::memory();
    let mut rt = render(connected(), WINDOW_SIZE);
    rt.use_clipboard(&board);
    rt.frame(0.1);
    let find = |rt: &Runtime<RiverApp>, text: &str| {
        let room = rt.state.chat.room().unwrap();
        let m = room
            .messages
            .iter()
            .find(|m| m.text.contains(text))
            .unwrap();
        let k = format!("md-{}", m.key);
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(k.as_str()))
            .unwrap()
            .rect
    };
    let a = find(&rt, "Updated, and the room list");
    let b = find(&rt, "Small thing I noticed");
    rt.pointer_down(a[0] + 2.0, a[1] + 8.0);
    rt.pointer_move(b[0] + 50.0, b[1] + 8.0);
    rt.pointer_move(b[0] + b[2] - 2.0, b[1] + b[3] - 4.0);
    rt.pointer_up(b[0] + b[2] - 2.0, b[1] + b[3] - 4.0);
    rt.frame(0.2);
    rt.key(KeyEvent {
        key: Key::Command('c'),
        mods: rune::Mods {
            cmd: true,
            ctrl: !cfg!(target_os = "macos"),
            ..rune::Mods::default()
        },
    });
    rt.frame(0.3);
    let got = board.text().map(|s| s.to_string()).unwrap_or_default();
    eprintln!("copied: {got:?}");
    assert!(
        got.contains("Updated, and the room list") && got.contains("Small thing I noticed"),
        "{got:?}"
    );
}

/// What winit gives off macOS for Ctrl+<key>: rune's `cmd` and `ctrl` both set.
#[cfg(not(target_os = "macos"))]
fn ctrl(rt: &mut Runtime<RiverApp>, key: Key, shift: bool) {
    rt.key(KeyEvent {
        key,
        mods: rune::Mods {
            shift,
            cmd: true,
            ctrl: true,
            ..rune::Mods::default()
        },
    });
    rt.frame(0.3);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn ctrl_arrows_and_ctrl_backspace_work_by_word_in_the_composer() {
    let mut rt = render(connected(), WINDOW_SIZE);
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Text("one two three".into()));
    ctrl(&mut rt, Key::Left, false);
    key(&mut rt, Key::Text("X".into()));
    assert_eq!(rt.state.chat.composer.text, "one two Xthree");
    ctrl(&mut rt, Key::Left, false);
    ctrl(&mut rt, Key::Left, false);
    ctrl(&mut rt, Key::Left, false);
    key(&mut rt, Key::Text("Y".into()));
    assert_eq!(rt.state.chat.composer.text, "Yone two Xthree");
    ctrl(&mut rt, Key::Right, false);
    key(&mut rt, Key::Text("Z".into()));
    assert_eq!(rt.state.chat.composer.text, "YoneZ two Xthree");
    // Ctrl+Backspace deletes the word before the caret, not the line before it.
    ctrl(&mut rt, Key::Backspace, false);
    assert_eq!(rt.state.chat.composer.text, " two Xthree");
    // Ctrl+Shift+Right selects by word.
    ctrl(&mut rt, Key::Right, true);
    key(&mut rt, Key::Text("!".into()));
    assert_eq!(rt.state.chat.composer.text, "! Xthree");
}

#[test]
fn the_emoji_button_opens_a_picker_that_types_into_the_composer() {
    let mut rt = render(connected(), WINDOW_SIZE);
    assert!(rt.find("emoji-panel").is_none());
    press(&mut rt, "emoji");
    assert!(rt.find("emoji-panel").is_some());
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Text("hi ".into()));
    press(&mut rt, "emoji-0-1");
    assert!(
        rt.elements()
            .into_iter()
            .any(|e| e.key.as_deref() == Some("composer-field") && e.focused),
        "the composer has the focus again"
    );
    let text = rt.state.chat.composer.text.clone();
    assert!(
        text.starts_with("hi ") && text.chars().count() == 4,
        "{text:?}"
    );
    // Another category, and the button closes the picker.
    press(&mut rt, "emoji-tab-hearts");
    press(&mut rt, "emoji");
    assert!(rt.find("emoji-panel").is_none());
}

#[test]
fn a_description_with_a_link_is_markdown_on_one_line_with_no_more_button() {
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        rooms[0].description =
            "[Freenet](https://freenet.org) is the place.\nSecond line, with `code`.".into();
    });
    let rt = render(state, WINDOW_SIZE);
    let key = format!("description-{}", rt.state.chat.room().unwrap().key);
    assert!(
        rt.find(&key).is_some(),
        "the first line is drawn as markdown"
    );
    // The header never expands: the details dialog shows the whole description.
    assert!(rt.find("description-toggle").is_none());
    assert!(rt.find("description-scroll").is_none());
    assert!(!shows(&rt, "Second line"));
}

#[test]
fn a_description_that_starts_with_a_blank_line_still_shows() {
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        rooms[0].description = "\n\nThe actual description".into();
    });
    let rt = render(state, WINDOW_SIZE);
    assert!(shows(&rt, "The actual description"));
}

#[test]
fn escape_closes_the_emoji_picker() {
    let mut rt = render(connected(), WINDOW_SIZE);
    press(&mut rt, "emoji");
    assert!(rt.find("emoji-panel").is_some());
    press(&mut rt, "emoji-0-0");
    key(&mut rt, Key::Escape);
    assert!(rt.find("emoji-panel").is_none());
}

#[test]
fn the_reaction_picker_goes_when_the_room_stops_taking_reactions() {
    let mut rt = node_rooms(|_| {});
    let (_, last) = two_message_ids(&rt);
    hover_press(&mut rt, &format!("react-{last}"));
    assert!(rt.find(&format!("picker-{last}")).is_some());
    // The tests module is a child of `chat`, so it reaches the private room list.
    for r in &mut rt.state.chat.rooms {
        r.send_block = Some("You have been banned.".into());
    }
    // State changed outside an event: tell the runtime to build the view again.
    rt.invalidate();
    for i in 0..10 {
        rt.frame(1.0 + f64::from(i) * 0.1);
    }
    assert!(rt.find(&format!("picker-{last}")).is_none());
}

fn rect_of(rt: &Runtime<RiverApp>, key: &str) -> [f32; 4] {
    rt.elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key) && !e.exiting)
        .unwrap_or_else(|| panic!("no element {key}"))
        .rect
}

#[test]
fn the_reaction_picker_stays_in_the_window_and_off_its_own_toolbar() {
    for size in [WINDOW_SIZE, NARROW_SCENE_SIZE, (480.0, 720.0)] {
        let mut s = node_chat();
        let rooms = super::example::data().rooms;
        s.chat
            .show_node_rooms(rooms, None, super::LinkStatus::Live, 29_000_000);
        let mut rt = render(s, size);
        let (_, last) = two_message_ids(&rt);
        hover_press(&mut rt, &format!("react-{last}"));
        let pk = rect_of(&rt, &format!("picker-{last}"));
        let bar = rect_of(&rt, &format!("actions-{last}"));
        eprintln!("{size:?} picker {pk:?} bar {bar:?}");
        assert!(pk[0] >= 0.0 && pk[1] >= 0.0, "{size:?} {pk:?}");
        assert!(
            pk[0] + pk[2] <= size.0 + 0.5 && pk[1] + pk[3] <= size.1 + 0.5,
            "{size:?} {pk:?}"
        );
        let overlap = pk[0] < bar[0] + bar[2]
            && bar[0] < pk[0] + pk[2]
            && pk[1] < bar[1] + bar[3]
            && bar[1] < pk[1] + pk[3];
        assert!(!overlap, "{size:?} picker {pk:?} covers toolbar {bar:?}");
    }
}

// The member info dialog ----------------------------------------------------------------------

/// Run frames long enough for a dialog's opening or closing motion to finish.
fn settle(rt: &mut Runtime<RiverApp>) {
    for i in 1..=30 {
        rt.frame(10.0 + f64::from(i) * 0.05);
    }
}

/// The `uid` of the open room's member called `name`.
fn uid_of(rt: &Runtime<RiverApp>, name: &str) -> String {
    let room = rt.state.chat.room().unwrap();
    room.members
        .iter()
        .find(|m| m.nickname == name)
        .unwrap_or_else(|| panic!("no member {name}"))
        .uid
        .clone()
}

fn open_member(rt: &mut Runtime<RiverApp>, name: &str) {
    let id = rt
        .state
        .chat
        .room()
        .unwrap()
        .member_by_uid(&uid_of(rt, name))
        .unwrap()
        .id;
    press(rt, &format!("member-{id}"));
}

#[test]
fn clicking_a_member_opens_their_info() {
    let mut rt = render(connected(), WINDOW_SIZE);
    assert!(rt.find("member-info-dialog").is_none());
    open_member(&mut rt, "Jonas");
    assert!(rt.find("member-info-dialog").is_some());
    assert_eq!(rt.state.chat.member_modal, Some(uid_of(&rt, "Jonas")));
    assert!(shows(&rt, "Member Info"));
    assert!(shows(&rt, "Jonas"));
    assert!(shows(&rt, "Member ID"));
    // Jonas was invited by the owner, whose name opens the owner's own dialog.
    assert!(shows(&rt, "Invited by"));
    assert!(rt.find("member-inviter").is_some());
    assert!(rt.find("member-tag-owner").is_none());
    assert!(rt.find("member-tag-you").is_none());
    assert!(rt.find("member-tag-impersonation").is_none());
    press(&mut rt, "member-inviter");
    assert_eq!(rt.state.chat.member_modal, Some(uid_of(&rt, "Maya Chen")));
    assert!(rt.find("member-tag-owner").is_some());
}

#[test]
fn a_member_row_opens_from_the_keyboard() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let id = rt
        .state
        .chat
        .room()
        .unwrap()
        .member_by_uid(&uid_of(&rt, "Jonas"))
        .unwrap()
        .id;
    let want = format!("member-{id}");
    let mut reached = false;
    for _ in 0..80 {
        key(&mut rt, Key::Tab);
        if rt
            .elements()
            .into_iter()
            .any(|e| e.focused && e.key.as_deref() == Some(want.as_str()))
        {
            reached = true;
            break;
        }
    }
    assert!(reached, "Tab never reached {want}");
    // Space presses a focused button, as it does every button in the app.
    key(&mut rt, Key::Text(" ".into()));
    assert_eq!(rt.state.chat.member_modal, Some(uid_of(&rt, "Jonas")));
}

#[test]
fn escape_closes_the_member_dialog() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_member(&mut rt, "Jonas");
    assert!(rt.find("member-info-dialog").is_some());
    key(&mut rt, Key::Escape);
    assert!(rt.find("member-info-dialog").is_none());
    assert!(rt.state.chat.member_modal.is_none());
}

#[test]
fn the_owner_and_you_are_tagged() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_member(&mut rt, "Maya Chen");
    assert!(rt.find("member-tag-owner").is_some());
    assert!(shows(&rt, "Room Owner"));
    assert!(shows(&rt, "N/A (Room Owner)"));
    assert!(rt.find("member-tag-impersonation").is_none());
    key(&mut rt, Key::Escape);
    open_member(&mut rt, "Sam Rivers");
    assert!(rt.find("member-tag-you").is_some());
    assert!(rt.find("member-tag-owner").is_none());
    // Nothing to message or moderate in yourself.
    assert!(rt.find("member-dm").is_none());
}

#[test]
fn what_needs_commands_this_app_lacks_is_shown_unavailable() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_member(&mut rt, "Jonas");
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("member-dm"))
        .expect("no member-dm");
    assert_eq!(e.help.as_deref(), Some(super::view::NOT_YET));
    // The web dialog has no Mention button; mentions come from the composer.
    assert!(rt.find("member-mention").is_none());
    // Sam did not invite Jonas, so Sam has no say over him.
    assert!(rt.find("member-ban").is_none());
    key(&mut rt, Key::Escape);
    // The owner moderates everyone but themselves.
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        let me = rooms[0].me;
        for m in &mut rooms[0].members {
            m.role = if m.id == me {
                super::model::Role::Owner
            } else {
                super::model::Role::Member
            };
        }
    });
    let mut rt = render(state, WINDOW_SIZE);
    open_member(&mut rt, "Jonas");
    for k in ["member-ban", "member-deputize"] {
        let e = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(k))
            .unwrap_or_else(|| panic!("no {k}"));
        assert_eq!(e.help.as_deref(), Some(super::view::NOT_YET), "{k}");
    }
    assert!(rt.find("member-tag-invited-by-you").is_some());
}

#[test]
fn the_dialog_closes_when_the_room_changes() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_member(&mut rt, "Jonas");
    assert!(rt.find("member-info-dialog").is_some());
    rt.update(|s, _| s.chat.select(PRIVATE_ROOM));
    settle(&mut rt);
    assert!(rt.state.chat.member_modal.is_none());
    assert!(rt.find("member-info-dialog").is_none());
    assert_eq!(rt.state.chat.room().unwrap().key, PRIVATE_ROOM);
}

#[test]
fn a_name_that_imitates_the_owner_is_warned_about() {
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        let room = &mut rooms[0];
        let owner = room
            .members
            .iter()
            .find(|m| m.role == super::model::Role::Owner)
            .unwrap()
            .id;
        // Cyrillic "а" for the first "a", and an invisible character.
        for (id, name) in [
            (91, "M\u{0430}ya Chen"),
            (92, "Maya\u{200B} Chen"),
            (93, "Maya Chen2"),
        ] {
            let mut m = super::model::Member::new(id, name, super::model::Role::Member);
            m.invited_by = Some(owner);
            room.members.push(m);
        }
    });
    let mut rt = render(state, WINDOW_SIZE);
    for (uid, warned) in [("91", true), ("92", true), ("93", false)] {
        rt.state.chat.open_member(uid);
        rt.invalidate();
        settle(&mut rt);
        assert_eq!(
            rt.find("member-tag-impersonation").is_some(),
            warned,
            "member {uid}"
        );
        assert_eq!(
            rt.find("member-impersonation").is_some(),
            warned,
            "member {uid}"
        );
        if warned {
            assert!(shows(&rt, "NOT the room owner"));
            assert!(shows(&rt, "Resembles:"));
        }
        key(&mut rt, Key::Escape);
        settle(&mut rt);
    }
    // The real owner is not accused of imitating themselves.
    open_member(&mut rt, "Maya Chen");
    assert!(rt.find("member-tag-impersonation").is_none());
}

#[test]
fn a_member_who_leaves_closes_the_dialog_for_good() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_member(&mut rt, "Jonas");
    let jonas = rt
        .state
        .chat
        .room()
        .unwrap()
        .member_by_uid(&uid_of(&rt, "Jonas"))
        .unwrap()
        .clone();
    rt.update(|s, _| {
        s.chat
            .edit_rooms(|rooms| rooms[0].members.retain(|m| m.nickname != "Jonas"));
    });
    settle(&mut rt);
    assert!(rt.state.chat.member_modal.is_none());
    assert!(rt.find("member-info-dialog").is_none());
    // They come back, and the dialog does not.
    rt.update(|s, _| {
        s.chat.edit_rooms(|rooms| rooms[0].members.push(jonas));
    });
    settle(&mut rt);
    assert!(rt.state.chat.member_modal.is_none());
    assert!(rt.find("member-info-dialog").is_none());
}

#[test]
fn an_invite_cycle_or_a_missing_inviter_still_renders() {
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        let room = &mut rooms[0];
        let owner = room
            .members
            .iter()
            .find(|m| m.role == super::model::Role::Owner)
            .unwrap()
            .id;
        let mut a = super::model::Member::new(81, "Ann", super::model::Role::Member);
        let mut b = super::model::Member::new(82, "Bo", super::model::Role::Member);
        let mut c = super::model::Member::new(83, "Cy", super::model::Role::Member);
        a.invited_by = Some(82);
        b.invited_by = Some(81);
        c.invited_by = Some(83);
        let mut d = super::model::Member::new(84, "Di", super::model::Role::Member);
        d.invited_by = Some(9999);
        room.members.extend([a, b, c, d]);
        // The viewer is invited through the cycle, so the walk must end.
        room.members
            .iter_mut()
            .find(|m| m.id == room.me)
            .unwrap()
            .invited_by = Some(81);
        let _ = owner;
    });
    let mut rt = render(state, WINDOW_SIZE);
    for name in ["Ann", "Bo", "Cy", "Di"] {
        open_member(&mut rt, name);
        assert!(rt.find("member-info-dialog").is_some(), "{name}");
        key(&mut rt, Key::Escape);
        settle(&mut rt);
    }
    open_member(&mut rt, "Di");
    assert!(shows(&rt, "Unknown"), "a missing inviter is Unknown");
}

#[test]
fn the_copy_buttons_copy_the_id_and_the_key() {
    let board = rune::clipboard::memory();
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        for m in &mut rooms[0].members {
            m.key = format!("key-of-{}", m.uid);
        }
    });
    let env = Env {
        system_dark: true,
        scale: 1.0,
    };
    let mut rt = Runtime::new(state, view, WINDOW_SIZE, env);
    rt.use_clipboard(&board);
    rt.frame(0.0);
    rt.frame(1.0 / 60.0);
    open_member(&mut rt, "Jonas");
    let uid = uid_of(&rt, "Jonas");
    press(&mut rt, "member-id-copy");
    assert_eq!(
        board.text().as_deref().map(String::as_str),
        Some(uid.as_str())
    );
    press(&mut rt, "member-vk-copy");
    assert_eq!(
        board.text().as_deref().map(String::as_str),
        Some(format!("key-of-{uid}").as_str())
    );
}

#[test]
fn a_long_unbroken_name_stays_inside_the_dialog() {
    let long = "W".repeat(48);
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        let room = &mut rooms[0];
        let owner = room
            .members
            .iter()
            .find(|m| m.role == super::model::Role::Owner)
            .unwrap()
            .id;
        let mut m = super::model::Member::new(85, long.clone(), super::model::Role::Member);
        m.invited_by = Some(owner);
        m.key = "k".repeat(60);
        let mut kid = super::model::Member::new(86, "Kid", super::model::Role::Member);
        kid.invited_by = Some(85);
        kid.key = "k".repeat(60);
        room.members.extend([m, kid]);
    });
    let mut rt = render(state, WINDOW_SIZE);
    open_member(&mut rt, "Kid");
    let dialog = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("member-info-dialog"))
        .unwrap();
    let [dx, _, dw, _] = dialog.rect;
    for k in ["member-inviter", "member-vk-value"] {
        let e = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(k))
            .unwrap_or_else(|| panic!("no {k}"));
        assert!(
            e.rect[0] >= dx - 0.5 && e.rect[0] + e.rect[2] <= dx + dw + 0.5,
            "{k} {:?} outside {:?}",
            e.rect,
            dialog.rect
        );
    }
    key(&mut rt, Key::Escape);
    settle(&mut rt);
    open_member(&mut rt, &long);
    let name = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("member-name"))
        .unwrap();
    assert!(
        name.rect[0] + name.rect[2] <= dx + dw + 0.5,
        "{:?} outside {:?}",
        name.rect,
        dialog.rect
    );
}

#[test]
fn a_departed_member_is_not_one_you_invited() {
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        let room = &mut rooms[0];
        let me = room.me;
        for m in &mut room.members {
            m.role = if m.id == me {
                super::model::Role::Owner
            } else {
                super::model::Role::Member
            };
        }
        // Someone who wrote but is no longer a member: no inviter, no key, a stand-in name.
        let mut gone = super::model::Member::new(87, "Member 87", super::model::Role::Member);
        gone.claimed = false;
        room.members.push(gone);
    });
    let mut rt = render(state, WINDOW_SIZE);
    open_member(&mut rt, "Member 87");
    assert!(rt.find("member-info-dialog").is_some());
    assert!(rt.find("member-tag-invited-by-you").is_none());
    assert!(rt.find("member-ban").is_none());
    assert!(rt.find("member-vk").is_none());
}

#[test]
fn only_a_name_the_owner_chose_is_protected() {
    let imitate = |owner_name: &str, claimed: bool, copy: &str| {
        let mut state = connected();
        state.chat.edit_rooms(|rooms| {
            let room = &mut rooms[0];
            let owner = room
                .members
                .iter_mut()
                .find(|m| m.role == super::model::Role::Owner)
                .unwrap();
            owner.nickname = owner_name.into();
            owner.claimed = claimed;
            let oid = owner.id;
            let mut m = super::model::Member::new(88, copy, super::model::Role::Member);
            m.invited_by = Some(oid);
            room.members.push(m);
        });
        let mut rt = render(state, WINDOW_SIZE);
        open_member(&mut rt, copy);
        rt.find("member-tag-impersonation").is_some()
    };
    // The control: a chosen name is protected.
    assert!(imitate("Maya Chen", true, "M\u{0430}ya Chen"));
    // Not one the owner chose (shown as a stand-in), nor a placeholder or a generated handle.
    assert!(!imitate("Member 12", false, "Member 12 "));
    assert!(!imitate("Unknown", true, "Unkn0wn"));
    assert!(!imitate("Unnamed", true, "unnamed"));
    assert!(!imitate("Amber Worm", true, "AmberWorm"));
}

#[test]
fn following_the_inviter_keeps_the_keyboard_inside_the_dialog() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_member(&mut rt, "Jonas");
    press(&mut rt, "member-inviter");
    // The owner has no inviter, so the pressed button is gone.
    assert!(rt.find("member-inviter").is_none());
    key(&mut rt, Key::Tab);
    let focused: Vec<_> = rt.elements().into_iter().filter(|e| e.focused).collect();
    assert!(!focused.is_empty(), "Tab finds something to focus");
    for e in focused {
        assert!(
            e.path.contains("member-info-layer"),
            "focus left the dialog: {}",
            e.path
        );
    }
}

// Editing your own nickname -----------------------------------------------------------------

/// The node's rooms, with your own member info dialog open, opened by pressing your row.
fn own_dialog(edit: impl FnOnce(&mut Vec<super::Room>)) -> Runtime<RiverApp> {
    let mut rt = node_rooms(edit);
    let me = rt.state.chat.room().map(|r| r.me).unwrap();
    press(&mut rt, &format!("member-{me}"));
    settle(&mut rt);
    rt
}

/// Draw again after the test changed the state directly (the app does it through events).
fn redraw(rt: &mut Runtime<RiverApp>) {
    // A press on the dialog's own text changes nothing but draws again.
    press(rt, "member-name");
    settle(rt);
}

fn type_nickname(rt: &mut Runtime<RiverApp>, text: &str) {
    rt.state.chat.nickname.replace_text(text);
    redraw(rt);
}

#[test]
fn your_own_dialog_has_an_editable_nickname_with_save_and_cancel() {
    let rt = own_dialog(|_| {});

    for part in ["member-nickname-input", "nickname-save", "nickname-cancel"] {
        assert!(rt.find(part).is_some(), "no {part}");
    }
    assert_eq!(rt.state.chat.nickname.text, "Sam Rivers");
    assert!(rt.state.chat.nickname.focused);
    assert!(!shows(&rt, "Sending"));
}

#[test]
fn someone_elses_dialog_has_no_nickname_field() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_member(&mut rt, "Jonas");
    assert!(rt.find("member-nickname-input").is_none());
    assert!(rt.find("nickname-save").is_none());
    // And saving cannot reach their record from there.
    rt.state.chat.submit_nickname();
    assert!(sent(&mut rt).is_empty());
}

#[test]
fn saving_sends_the_trimmed_nickname_and_the_dialog_waits_for_the_node() {
    let mut rt = own_dialog(|_| {});
    type_nickname(&mut rt, "  Sam the Second  ");
    press(&mut rt, "nickname-save");
    assert_eq!(
        sent(&mut rt),
        [Action::SetNickname {
            nickname: "Sam the Second".into()
        }]
    );
    assert!(
        rt.state.chat.member_modal.is_some(),
        "stays to show its state"
    );

    // The background work reports it on its way: Sending, and Save waits for it.
    rt.state.chat.edit_rooms(|r| {
        r[0].own_nickname = Some(super::model::Pending {
            entry: 4,
            failed: None,
            retry: false,
        })
    });
    redraw(&mut rt);
    assert!(shows(&rt, "Sending"));
    press(&mut rt, "nickname-save");
    assert!(sent(&mut rt).is_empty(), "one change at a time");
}

#[test]
fn enter_in_the_field_saves() {
    let mut rt = own_dialog(|_| {});
    type_nickname(&mut rt, "Enter Sam");
    press(&mut rt, "member-nickname-input");
    key(&mut rt, Key::Enter);
    assert_eq!(
        sent(&mut rt),
        [Action::SetNickname {
            nickname: "Enter Sam".into()
        }]
    );
}

#[test]
fn a_failed_change_says_why_and_offers_retry_and_discard() {
    let mut rt = own_dialog(|r| {
        r[0].own_nickname = Some(super::model::Pending {
            entry: 7,
            failed: Some("Not saved. Freenet refused it: no room".into()),
            retry: true,
        })
    });
    assert!(shows(&rt, "Not saved. Freenet refused it: no room"));
    press(&mut rt, "nickname-retry");
    press(&mut rt, "nickname-discard");
    assert_eq!(
        sent(&mut rt),
        [Action::Retry { entry: 7 }, Action::Discard { entry: 7 }]
    );
    // A failed change does not stop a new one from being saved.
    type_nickname(&mut rt, "Again");
    press(&mut rt, "nickname-save");
    assert_eq!(
        sent(&mut rt),
        [Action::SetNickname {
            nickname: "Again".into()
        }]
    );
}

#[test]
fn what_the_room_would_refuse_is_said_in_the_dialog_and_not_sent() {
    let mut rt = own_dialog(|r| r[0].nickname_limit = 12);
    // Empty: said when saving.
    type_nickname(&mut rt, "   ");
    press(&mut rt, "nickname-save");
    assert!(shows(&rt, "Nickname can't be empty."));
    // Too long: said as it is typed, with the room's limit.
    type_nickname(&mut rt, "0123456789abc");
    assert!(shows(
        &rt,
        "Nickname is 13 bytes; this room allows at most 12."
    ));
    press(&mut rt, "nickname-save");
    // Characters that don't show.
    type_nickname(&mut rt, "eve\u{202e}");
    assert!(shows(&rt, "Nicknames can't contain emoji"));
    press(&mut rt, "nickname-save");
    assert!(sent(&mut rt).is_empty());
    assert!(
        rt.state.chat.member_modal.is_some(),
        "the dialog stays open"
    );
    // Fixed, it saves and the message goes.
    type_nickname(&mut rt, "ok");
    assert!(!shows(&rt, "characters that don't show"));
    press(&mut rt, "nickname-save");
    assert_eq!(sent(&mut rt).len(), 1);
}

#[test]
fn the_same_nickname_closes_the_dialog_and_sends_nothing() {
    let mut rt = own_dialog(|_| {});
    press(&mut rt, "nickname-save");
    assert!(sent(&mut rt).is_empty());
    assert!(rt.state.chat.member_modal.is_none());
}

#[test]
fn cancel_closes_the_dialog_and_sends_nothing() {
    let mut rt = own_dialog(|_| {});
    type_nickname(&mut rt, "Never Sent");
    press(&mut rt, "nickname-cancel");
    assert!(sent(&mut rt).is_empty());
    assert!(rt.state.chat.member_modal.is_none());
    // Reopened, it starts from the real nickname, not the abandoned one.
    let me = {
        let room = rt.state.chat.room().unwrap();
        room.member(room.me).unwrap().uid.clone()
    };
    rt.state.chat.open_member(&me);
    assert_eq!(rt.state.chat.nickname.text, "Sam Rivers");
}

#[test]
fn without_a_nickname_the_field_starts_empty_and_says_so() {
    let mut rt = own_dialog(|r| {
        let room = &mut r[0];
        let me = room.me;
        let m = room.members.iter_mut().find(|m| m.id == me).unwrap();
        m.nickname = format!("Member {me}");
        m.claimed = false;
    });
    assert_eq!(rt.state.chat.nickname.text, "");
    assert!(rt.find("member-nickname-unset").is_some());
    // The stand-in name is not a nickname to keep: saving nothing is refused.
    press(&mut rt, "nickname-save");
    assert!(shows(&rt, "Nickname can't be empty."));
    assert!(sent(&mut rt).is_empty());
}

#[test]
fn a_room_you_cannot_write_in_says_why_before_save_and_sends_nothing() {
    for (block, mutate) in [
        (
            "You have been banned from sending messages in this room.",
            (|r: &mut super::Room| {
                r.send_block =
                    Some("You have been banned from sending messages in this room.".into())
            }) as fn(&mut super::Room),
        ),
        (
            crate::rooms::outbox::PRUNED_MESSAGE,
            |r: &mut super::Room| {
                r.nickname_block = Some(crate::rooms::outbox::PRUNED_MESSAGE.into())
            },
        ),
    ] {
        let mut rt = own_dialog(|r| mutate(&mut r[0]));
        assert!(shows(&rt, block), "the reason is shown before Save");
        let save = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("nickname-save"))
            .unwrap();
        assert!(save.opacity < 0.9, "Save is drawn disabled");
        type_nickname(&mut rt, "Nope");
        press(&mut rt, "nickname-save");
        rt.state.chat.submit_nickname();
        assert!(sent(&mut rt).is_empty(), "nothing is sent: {block}");
    }
}

#[test]
fn in_the_example_rooms_the_change_is_made_in_memory() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_member(&mut rt, "Sam Rivers");
    type_nickname(&mut rt, "Sam R.");
    press(&mut rt, "nickname-save");
    assert!(rt.state.chat.member_modal.is_none());
    let room = rt.state.chat.room().unwrap();
    assert_eq!(room.nickname(room.me), "Sam R.");
    assert!(sent(&mut rt).is_empty());
}

#[test]
fn a_nickname_change_updates_the_members_list_and_messages() {
    // What the view shows once the room holds the new name: the same member, new name,
    // everywhere it is drawn.
    let mut rt = own_dialog(|_| {});
    rt.state.chat.edit_rooms(|r| {
        let me = r[0].me;
        r[0].members
            .iter_mut()
            .find(|m| m.id == me)
            .unwrap()
            .nickname = "Brand New".into();
    });
    press(&mut rt, "nickname-cancel");
    rt.frame(0.7);
    assert!(
        texts(&rt)
            .iter()
            .filter(|t| t.as_str() == "Brand New")
            .count()
            >= 2
    );
    assert!(!shows(&rt, "Sam Rivers"));
}

#[test]
fn a_rename_shows_in_the_room_list_preview() {
    let mut rt = node_rooms(|r| {
        let room = &mut r[0];
        let who = room.last_message().unwrap().author;
        room.members
            .iter_mut()
            .find(|m| m.id == who)
            .unwrap()
            .nickname = "Renamed Rae".into();
    });
    settle(&mut rt);
    assert!(shows(&rt, "Renamed Rae: "), "{:?}", texts(&rt));
}

#[test]
fn the_same_name_with_other_spacing_is_the_same_name() {
    let mut rt = own_dialog(|r| {
        let room = &mut r[0];
        let me = room.me;
        room.members
            .iter_mut()
            .find(|m| m.id == me)
            .unwrap()
            .nickname = "Bob Smith".into();
    });
    assert_eq!(rt.state.chat.nickname.text, "Bob Smith");
    type_nickname(&mut rt, "  Bob   Smith ");
    press(&mut rt, "nickname-save");
    assert!(sent(&mut rt).is_empty());
    assert!(rt.state.chat.member_modal.is_none());
}

#[test]
fn members_whose_names_are_only_symbols_are_all_unnamed_with_their_id_to_tell_them_apart() {
    // As the web UI: every such member is "Unnamed" in the list; the Member ID in the dialog
    // (and here, the row's hint) is what distinguishes them.
    let rt = node_rooms(|r| {
        let room = &mut r[0];
        for m in room.members.iter_mut().filter(|m| m.id != room.me).take(2) {
            m.nickname = super::visual_fold::UNNAMED.to_owned();
        }
    });
    let hinted: Vec<_> = rt
        .elements()
        .into_iter()
        .filter(|e| {
            e.help
                .as_deref()
                .is_some_and(|h| h.starts_with("Member ID "))
        })
        .collect();
    assert_eq!(hinted.len(), 2);
    assert_ne!(hinted[0].help, hinted[1].help);
}

// Notification modes ---------------------------------------------------------------------------

use crate::rooms::modes::NotificationMode;

fn open_bell(rt: &mut Runtime<RiverApp>) {
    press(rt, "notification-bell");
}

/// The mode radio rows are `item0..item2`, in the web UI's order.
fn pick(rt: &mut Runtime<RiverApp>, mode: NotificationMode) {
    let i = NotificationMode::ALL
        .iter()
        .position(|m| *m == mode)
        .unwrap();
    press(rt, &format!("item{i}"));
}

#[test]
fn the_bell_opens_the_dialog_with_the_three_modes_and_the_current_one_checked() {
    let mut rt = render(connected(), WINDOW_SIZE);
    assert!(rt.find("notify-dialog").is_none());
    open_bell(&mut rt);
    assert!(rt.find("notify-dialog").is_some());
    for m in NotificationMode::ALL {
        assert!(shows(&rt, m.label()), "{}", m.label());
        assert!(shows(&rt, m.help()), "{}", m.help());
    }
    assert!(shows(
        &rt,
        "Applies to this room and syncs across your devices."
    ));
    // The default is checked: exactly the first radio.
    let checked: Vec<bool> = (0..3)
        .map(|i| {
            rt.elements()
                .into_iter()
                .find(|e| e.key.as_deref() == Some(format!("item{i}").as_str()))
                .map(|e| e.item_selected)
                .expect("a radio row")
        })
        .collect();
    assert_eq!(checked, [true, false, false]);
}

#[test]
fn choosing_a_mode_applies_it_closes_the_dialog_and_sends_it_to_be_stored() {
    let mut rt = node_rooms(|_| {});
    let room = rt.state.chat.room().unwrap().key.clone();
    open_bell(&mut rt);
    pick(&mut rt, NotificationMode::MentionsAndReplies);
    assert!(rt.find("notify-dialog").is_none());
    assert_eq!(
        rt.state.chat.mode_of(&room),
        NotificationMode::MentionsAndReplies
    );
    assert_eq!(
        rt.state.chat.outgoing,
        [Command {
            room: room.clone(),
            action: Action::SetNotificationMode {
                mode: NotificationMode::MentionsAndReplies
            }
        }]
    );
    // The bell names the mode, and the dialog shows it checked on the next open.
    assert!(
        rt.elements()
            .iter()
            .any(|e| e.key.as_deref() == Some("notification-bell")
                && e.label.as_deref() == Some("Notifications: Mentions & replies only"))
    );
    open_bell(&mut rt);
    let selected = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("item1"))
        .unwrap()
        .item_selected;
    assert!(selected);
}

#[test]
fn choosing_the_current_mode_stores_nothing() {
    let mut rt = node_rooms(|_| {});
    open_bell(&mut rt);
    pick(&mut rt, NotificationMode::All);
    assert!(rt.find("notify-dialog").is_none());
    assert!(rt.state.chat.outgoing.is_empty());
}

#[test]
fn example_rooms_change_in_memory_and_send_nothing() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_bell(&mut rt);
    pick(&mut rt, NotificationMode::Muted);
    assert!(rt.state.chat.outgoing.is_empty());
    let room = rt.state.chat.room().unwrap().key.clone();
    assert_eq!(rt.state.chat.mode_of(&room), NotificationMode::Muted);
}

#[test]
fn escape_and_a_press_outside_close_the_dialog_without_changing_anything() {
    let mut rt = node_rooms(|_| {});
    open_bell(&mut rt);
    key(&mut rt, Key::Escape);
    assert!(rt.find("notify-dialog").is_none());
    open_bell(&mut rt);
    // The window's corner is the scrim, outside the dialog.
    rt.pointer_down(4.0, 4.0);
    rt.pointer_up(4.0, 4.0);
    rt.frame(0.1);
    rt.frame(0.2);
    assert!(rt.find("notify-dialog").is_none());
    open_bell(&mut rt);
    press(&mut rt, "close");
    assert!(rt.find("notify-dialog").is_none());
    assert!(rt.state.chat.outgoing.is_empty());
    let room = rt.state.chat.room().unwrap().key.clone();
    assert_eq!(rt.state.chat.mode_of(&room), NotificationMode::All);
}

#[test]
fn the_dialog_belongs_to_its_room() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_bell(&mut rt);
    rt.update(|s, _| s.chat.select("rust-help"));
    rt.frame(0.1);
    assert!(rt.find("notify-dialog").is_none());
}

#[test]
fn the_dialog_says_when_the_master_switch_is_off() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_bell(&mut rt);
    assert!(rt.find("notify-master-off").is_none());
    let mut off = connected();
    off.chat.notifications = false;
    let mut rt = render(off, WINDOW_SIZE);
    open_bell(&mut rt);
    assert!(rt.find("notify-master-off").is_some());
    assert!(shows(&rt, "switched off for every room"));
}

#[test]
fn a_muted_room_shows_no_unread_count_in_the_list() {
    let mut rt = render(connected(), WINDOW_SIZE);
    // Design Crew and Rust Help each have two unread.
    assert!(rt.find("room-rust-help").is_some());
    let badges = |rt: &Runtime<RiverApp>| {
        rt.elements()
            .into_iter()
            .filter(|e| e.key.as_deref() == Some("unread"))
            .count()
    };
    assert_eq!(badges(&rt), 2);
    rt.update(|s, _| s.chat.set_mode("rust-help", NotificationMode::Muted));
    for i in 1..20 {
        rt.frame(f64::from(i) * 0.1);
    }
    assert_eq!(badges(&rt), 1);
}

#[test]
fn a_mode_picked_here_survives_an_older_update_until_the_node_echoes_it() {
    let mut s = node_chat();
    s.chat.set_mode("rust-help", NotificationMode::Muted);
    // An update the node sent before it took the pick: still the old mode.
    s.chat.apply_modes(Default::default(), &[]);
    assert_eq!(s.chat.mode_of("rust-help"), NotificationMode::Muted);
    // The echo ends the hold; after it the node's word is taken, so a change made in the
    // browser shows here.
    s.chat.apply_modes(
        [("rust-help".to_owned(), NotificationMode::Muted)].into(),
        &[],
    );
    s.chat.apply_modes(
        [("rust-help".to_owned(), NotificationMode::All)].into(),
        &[],
    );
    assert_eq!(s.chat.mode_of("rust-help"), NotificationMode::All);
}

/// The pick is the screen's until the node says it ended; then the node's word is shown.
#[test]
fn a_failed_or_dismissed_save_reverts_the_display_and_a_stored_one_follows_the_store() {
    let room = "rust-help".to_owned();
    let settled = [room.clone()];
    // Failed: the node settles it with the mode the store still holds (none: All).
    let mut s = node_chat();
    s.chat.set_mode(&room, NotificationMode::Muted);
    s.chat.apply_modes(Default::default(), &settled);
    assert_eq!(s.chat.mode_of(&room), NotificationMode::All);
    // It does not come back with a later update either.
    s.chat.apply_modes(Default::default(), &[]);
    assert_eq!(s.chat.mode_of(&room), NotificationMode::All);

    // Stored, then a web tab reverts the store: the display follows the store.
    s.chat.set_mode(&room, NotificationMode::Muted);
    s.chat
        .apply_modes([(room.clone(), NotificationMode::Muted)].into(), &settled);
    assert_eq!(s.chat.mode_of(&room), NotificationMode::Muted);
    s.chat.apply_modes(Default::default(), &[]);
    assert_eq!(s.chat.mode_of(&room), NotificationMode::All);

    // Stored, and the update that settles it still carries the old mode: the node's word wins.
    s.chat.set_mode(&room, NotificationMode::Muted);
    s.chat.apply_modes(Default::default(), &settled);
    assert_eq!(s.chat.mode_of(&room), NotificationMode::All);
}

// The room details dialog ----------------------------------------------------------------------

use super::LinkStatus;
use super::model::Pending;

/// The example rooms as rooms from the node, with `room` open.
fn from_node(room: &str) -> RiverApp {
    let mut state = connected();
    let rooms = super::example::data().rooms;
    state
        .chat
        .show_node_rooms(rooms, None, LinkStatus::Live, 29_000_000);
    state.chat.select(room);
    state
}

/// The example room the viewer owns.
const OWNED: &str = "book-club";

fn details(rt: &Runtime<RiverApp>) -> &super::details::DetailsForm {
    rt.state.chat.details.as_ref().expect("the dialog is open")
}

/// Draw again after the test changed the state itself, as an event would have.
fn redraw_now(rt: &mut Runtime<RiverApp>) {
    rt.invalidate();
    rt.frame(0.4);
}

fn type_into(rt: &mut Runtime<RiverApp>, field: &str, text: &str) {
    press(rt, field);
    // The field's text is replaced, as a person selects all and types.
    if let Some(form) = rt.state.chat.details.as_mut() {
        match field {
            "details-name-input" => form.name.replace_text(""),
            "details-description-input" => form.description.replace_text(""),
            other => panic!("no field {other}"),
        }
    }
    redraw_now(rt);
    key(rt, Key::Text(text.into()));
}

/// Whether Save is drawn dimmed, as a disabled button is.
fn save_dimmed(rt: &Runtime<RiverApp>) -> bool {
    rt.elements()
        .into_iter()
        .any(|e| e.key.as_deref() == Some("details-save") && e.opacity < 0.9)
}

#[test]
fn clicking_the_room_name_or_description_opens_the_details() {
    for target in ["room-title", "description-open"] {
        let mut rt = render(connected(), WINDOW_SIZE);
        assert!(rt.find("room-details-dialog").is_none());
        press(&mut rt, target);
        assert!(rt.find("room-details-dialog").is_some(), "{target}");
        assert_eq!(details(&rt).room, rt.state.chat.room().unwrap().key);
    }
}

#[test]
fn the_room_name_opens_the_details_from_the_keyboard() {
    let mut rt = render(connected(), WINDOW_SIZE);
    // The name is the first thing Tab reaches; Space presses it, as it does any button.
    key(&mut rt, Key::Tab);
    assert!(
        rt.elements()
            .into_iter()
            .any(|e| e.key.as_deref() == Some("room-title") && e.focused),
        "the name takes the keyboard focus"
    );
    key(&mut rt, Key::Text(" ".into()));
    assert!(rt.find("room-details-dialog").is_some());
}

#[test]
fn escape_and_switching_rooms_close_the_details() {
    let mut rt = render(connected(), WINDOW_SIZE);
    press(&mut rt, "room-title");
    key(&mut rt, Key::Escape);
    assert!(rt.find("room-details-dialog").is_none());
    assert!(rt.state.chat.details.is_none());
    press(&mut rt, "room-title");
    press(&mut rt, &format!("room-{PRIVATE_ROOM}"));
    assert!(rt.state.chat.details.is_none(), "it belongs to its room");
    assert!(rt.find("room-details-dialog").is_none());
}

#[test]
fn a_member_sees_the_room_but_cannot_edit_it() {
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    press(&mut rt, "room-title");
    assert!(rt.find("room-details-dialog").is_some());
    assert!(shows(&rt, "Town Square"));
    // The description is markdown, shown in full.
    let key = format!("details-description-{}", rt.state.chat.room().unwrap().key);
    assert!(rt.find(&key).is_some() || shows(&rt, "The public room for everyone"));
    assert!(shows(&rt, "Members"));
    assert!(shows(&rt, "Max Message Size (bytes)"));
    assert!(shows(&rt, "Maya Chen"), "the owner is named");
    for edit in [
        "details-name-input",
        "details-description-input",
        "details-save",
    ] {
        assert!(rt.find(edit).is_none(), "a member has no {edit}");
    }
    // Keys can be copied.
    assert!(rt.find("details-contract-id-copy").is_some());
    assert!(rt.find("details-owner-key-copy").is_some());
    assert!(rt.find("details-private").is_none());
}

#[test]
fn a_private_room_says_so() {
    let mut rt = render(from_node(PRIVATE_ROOM), WINDOW_SIZE);
    press(&mut rt, "room-title");
    assert!(rt.find("details-private").is_some());
    assert!(rt.find("details-secret").is_some());
}

#[test]
fn the_owner_name_opens_the_owners_dialog() {
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    press(&mut rt, "room-title");
    press(&mut rt, "details-owner-open");
    assert!(rt.find("room-details-dialog").is_none());
    assert!(rt.find("member-info-dialog").is_some());
    assert!(shows(&rt, "Room Owner"));
}

#[test]
fn the_owner_edits_the_room_and_it_shows_everywhere() {
    let mut state = connected();
    state.chat.select(OWNED);
    let mut rt = render(state, WINDOW_SIZE);
    press(&mut rt, "room-title");
    for edit in [
        "details-name-input",
        "details-description-input",
        "details-save",
        "details-cancel",
    ] {
        assert!(rt.find(edit).is_some(), "the owner has {edit}");
    }
    type_into(&mut rt, "details-name-input", "Reading Circle");
    assert_eq!(details(&rt).name.text, "Reading Circle");
    rt.state
        .chat
        .details
        .as_mut()
        .unwrap()
        .description
        .replace_text("Now **reading** *Ursula*");
    rt.state.chat.details.as_mut().unwrap().limits[0].replace_text("12");
    redraw_now(&mut rt);
    press(&mut rt, "details-save");
    assert!(rt.find("room-details-dialog").is_none(), "saved, so closed");
    let room = rt.state.chat.room().unwrap();
    assert_eq!(room.name, "Reading Circle");
    assert_eq!(room.description, "Now **reading** *Ursula*");
    assert_eq!(room.details.limits.max_members, 12);
    // The header, the room list and the composer all say the new name.
    assert!(shows(&rt, "Reading Circle"));
    assert!(!shows(&rt, "Book Club"));
    assert!(
        rt.find("room-book-club").is_some(),
        "the room's key is stable"
    );
}

#[test]
fn cancel_changes_nothing() {
    let mut state = connected();
    state.chat.select(OWNED);
    let mut rt = render(state, WINDOW_SIZE);
    press(&mut rt, "room-title");
    type_into(&mut rt, "details-name-input", "Scratch");
    press(&mut rt, "details-cancel");
    assert!(rt.find("room-details-dialog").is_none());
    assert_eq!(rt.state.chat.room().unwrap().name, "Book Club");
}

#[test]
fn an_unchanged_dialog_saves_nothing() {
    let mut rt = render(from_node(OWNED), WINDOW_SIZE);
    press(&mut rt, "room-title");
    press(&mut rt, "details-save");
    assert!(rt.find("room-details-dialog").is_none());
    assert!(rt.state.chat.outgoing.is_empty(), "nothing was sent");
}

#[test]
fn only_what_changed_goes_to_the_node() {
    let mut rt = render(from_node(OWNED), WINDOW_SIZE);
    press(&mut rt, "room-title");
    type_into(&mut rt, "details-name-input", "  Reading Circle ");
    rt.state.chat.details.as_mut().unwrap().limits[3].replace_text("4");
    redraw_now(&mut rt);
    press(&mut rt, "details-save");
    let sent = std::mem::take(&mut rt.state.chat.outgoing);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].room, OWNED);
    let Action::SetRoomDetails { edit } = &sent[0].action else {
        panic!("{:?}", sent[0].action);
    };
    assert_eq!(edit.name.as_deref(), Some("Reading Circle"));
    assert_eq!(edit.max_user_bans, Some(4));
    assert_eq!(edit.description, None);
    assert_eq!(edit.max_members, None);
    // Waiting on the node: said, and the dialog stays until the room shows it.
    assert!(rt.find("room-details-dialog").is_some());
}

#[test]
fn the_dialog_shows_a_change_on_its_way_and_closes_when_the_room_has_it() {
    let mut rt = render(from_node(OWNED), WINDOW_SIZE);
    press(&mut rt, "room-title");
    type_into(&mut rt, "details-name-input", "Reading Circle");
    press(&mut rt, "details-save");
    let pending = |name: &str, failed: Option<&str>| {
        let (name, failed) = (name.to_owned(), failed.map(str::to_owned));
        move |rooms: &mut Vec<super::model::Room>| {
            let r = rooms.iter_mut().find(|r| r.key == OWNED).unwrap();
            r.name = name.clone();
            r.details.name_source = name.clone();
            r.details.pending = Some(Pending {
                entry: 7,
                failed: failed.clone(),
                retry: true,
            });
        }
    };
    rt.state.chat.edit_rooms(pending("Reading Circle", None));
    redraw_now(&mut rt);
    assert!(rt.find("room-details-dialog").is_some());
    assert!(shows(&rt, "Sending"));
    assert!(save_dimmed(&rt), "one change at a time");
    // The node refuses it: said with Retry and Discard, and the dialog stays.
    rt.state.chat.edit_rooms(pending(
        "Book Club",
        Some("Not saved. Freenet refused it: the room is full."),
    ));
    redraw_now(&mut rt);
    assert!(rt.find("room-details-dialog").is_some());
    assert!(shows(&rt, "the room is full"));
    assert!(rt.find("details-retry").is_some());
    assert!(rt.find("details-discard").is_some());
    press(&mut rt, "details-retry");
    let sent = std::mem::take(&mut rt.state.chat.outgoing);
    assert!(
        sent.iter().any(|c| c.action == Action::Retry { entry: 7 }),
        "{sent:?}"
    );
    press(&mut rt, "details-discard");
    assert!(
        rt.state
            .chat
            .outgoing
            .iter()
            .any(|c| c.action == Action::Discard { entry: 7 })
    );
    // Taken: the room shows the name and has nothing on its way, so the dialog is done.
    rt.state.chat.edit_rooms(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == OWNED).unwrap();
        r.name = "Reading Circle".into();
        r.details.name_source = "Reading Circle".into();
        r.details.pending = None;
    });
    rt.state.chat.details.as_mut().unwrap().saving = true;
    rt.state.chat.edit_rooms(|_| {});
    redraw_now(&mut rt);
    assert!(rt.find("room-details-dialog").is_none());
}

#[test]
fn values_the_room_would_refuse_are_said_and_not_saved() {
    let mut rt = render(from_node(OWNED), WINDOW_SIZE);
    press(&mut rt, "room-title");
    for (typed, said) in [
        ("", "Give the room a name."),
        ("   ", "Give the room a name."),
    ] {
        rt.state
            .chat
            .details
            .as_mut()
            .unwrap()
            .name
            .replace_text(typed);
        redraw_now(&mut rt);
        assert!(shows(&rt, said), "{typed:?}");
        press(&mut rt, "details-save");
        assert!(rt.state.chat.outgoing.is_empty());
        assert!(rt.find("room-details-dialog").is_some());
    }
    rt.state
        .chat
        .details
        .as_mut()
        .unwrap()
        .name
        .replace_text("Book Club");
    for (typed, said) in [
        ("abc", "Members must be a whole number of at least 1."),
        ("0", "Members must be at least 1."),
        ("-3", "Members must be a whole number of at least 1."),
    ] {
        rt.state.chat.details.as_mut().unwrap().limits[0].replace_text(typed);
        redraw_now(&mut rt);
        assert!(shows(&rt, said), "{typed:?}: {:?}", texts(&rt));
        assert!(save_dimmed(&rt), "Save is off while the values are refused");
        press(&mut rt, "details-save");
        assert!(
            rt.state.chat.outgoing.is_empty(),
            "{typed:?} must not be sent"
        );
    }
    // A name longer than the room's limit.
    rt.state.chat.details.as_mut().unwrap().limits[0].replace_text("200");
    rt.state
        .chat
        .details
        .as_mut()
        .unwrap()
        .name
        .replace_text("n".repeat(101));
    redraw_now(&mut rt);
    assert!(shows(&rt, "Max Room Name Size"));
    press(&mut rt, "details-save");
    assert!(rt.state.chat.outgoing.is_empty());
}

#[test]
fn an_owner_who_cannot_sign_here_is_told_why() {
    let mut state = from_node(OWNED);
    state.chat.edit_rooms(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == OWNED).unwrap();
        r.details.editable =
            super::model::Editable::Blocked(crate::rooms::room::NO_KEY_TO_EDIT.into());
    });
    let mut rt = render(state, WINDOW_SIZE);
    press(&mut rt, "room-title");
    assert!(rt.find("details-blocked").is_some());
    assert!(shows(&rt, "doesn't hold your key"));
    assert!(rt.find("details-name-input").is_none());
}

#[test]
fn the_details_fit_a_narrow_window() {
    let mut state = connected();
    state.chat.select(OWNED);
    let mut rt = render(state, NARROW_SCENE_SIZE);
    press(&mut rt, "room-title");
    assert!(rt.find("room-details-dialog").is_some());
    assert!(rt.find("details-save").is_some());
}

#[test]
fn return_in_the_name_field_saves() {
    let mut state = connected();
    state.chat.select(OWNED);
    let mut rt = render(state, WINDOW_SIZE);
    press(&mut rt, "room-title");
    type_into(&mut rt, "details-name-input", "Reading Circle");
    key(&mut rt, Key::Enter);
    assert!(rt.find("room-details-dialog").is_none());
    assert_eq!(rt.state.chat.room().unwrap().name, "Reading Circle");
}

#[test]
fn a_field_changed_elsewhere_while_the_dialog_is_open_is_not_signed_back() {
    let mut rt = render(from_node(OWNED), WINDOW_SIZE);
    press(&mut rt, "room-title");
    // Another device raises the member cap while the dialog is open.
    rt.state.chat.edit_rooms(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == OWNED).unwrap();
        r.details.limits.max_members = 12;
    });
    redraw_now(&mut rt);
    type_into(&mut rt, "details-name-input", "Reading Circle");
    press(&mut rt, "details-save");
    let sent = std::mem::take(&mut rt.state.chat.outgoing);
    let Some(Action::SetRoomDetails { edit }) = sent.last().map(|c| &c.action) else {
        panic!("{sent:?}");
    };
    assert_eq!(edit.name.as_deref(), Some("Reading Circle"));
    assert_eq!(
        edit.max_members, None,
        "the other device's value is left alone"
    );
}

#[test]
fn an_unrelated_update_does_not_close_the_dialog_and_a_second_save_does_nothing() {
    let mut rt = render(from_node(OWNED), WINDOW_SIZE);
    press(&mut rt, "room-title");
    type_into(&mut rt, "details-name-input", "Reading Circle");
    // A new message arrives: the dialog stays, unsaved.
    rt.state.chat.edit_rooms(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == OWNED).unwrap();
        r.send("hello", 1);
    });
    assert!(rt.state.chat.details.is_some());
    // Save twice: one command only, and the dialog waits for the room to show it.
    rt.state.chat.submit_details();
    rt.state.chat.submit_details();
    let sent: Vec<_> = rt
        .state
        .chat
        .outgoing
        .iter()
        .filter(|c| matches!(c.action, Action::SetRoomDetails { .. }))
        .collect();
    assert_eq!(sent.len(), 1, "{sent:?}");
    // An update that does not show the change yet does not close it either.
    rt.state.chat.edit_rooms(|_| {});
    assert!(rt.state.chat.details.is_some());
    // Once the room shows the name, it closes.
    rt.state.chat.edit_rooms(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == OWNED).unwrap();
        r.details.name_source = "Reading Circle".into();
    });
    assert!(rt.state.chat.details.is_none());
}

#[test]
fn lowering_a_limit_below_what_is_used_asks_first_in_plain_words() {
    let mut rt = render(from_node(OWNED), WINDOW_SIZE);
    rt.state.chat.edit_rooms(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == OWNED).unwrap();
        r.details.usage.members = 7;
    });
    press(&mut rt, "room-title");
    rt.state.chat.details.as_mut().unwrap().limits[0].replace_text("3");
    redraw_now(&mut rt);
    press(&mut rt, "details-save");
    assert!(
        rt.state.chat.outgoing.is_empty(),
        "not sent before it is confirmed"
    );
    assert!(shows(
        &rt,
        "Lowering Members to 3 removes 4 members, starting with the longest invite chains. This cannot be undone."
    ));
    // Back returns to the fields, unsent.
    press(&mut rt, "details-back");
    assert!(rt.find("details-confirm").is_none());
    assert!(rt.state.chat.outgoing.is_empty());
    press(&mut rt, "details-save");
    press(&mut rt, "details-confirm-save");
    let sent = std::mem::take(&mut rt.state.chat.outgoing);
    let Some(Action::SetRoomDetails { edit }) = sent.last().map(|c| &c.action) else {
        panic!("{sent:?}");
    };
    assert_eq!(edit.max_members, Some(3));
}

#[test]
fn the_dialog_shows_how_much_of_a_limit_is_used() {
    let mut rt = render(from_node(OWNED), WINDOW_SIZE);
    rt.state.chat.edit_rooms(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == OWNED).unwrap();
        r.details.usage.bans = 4;
        r.details.usage.messages = 61;
    });
    press(&mut rt, "room-title");
    assert!(shows(&rt, "Max Recent Messages (61 held now)"));
    assert!(shows(&rt, "Max User Bans (4 in force now)"));
}

#[test]
fn the_dialog_says_what_the_room_would_refuse() {
    let mut rt = render(from_node(OWNED), WINDOW_SIZE);
    press(&mut rt, "room-title");
    let set = |rt: &mut Runtime<RiverApp>, f: &dyn Fn(&mut super::details::DetailsForm)| {
        f(rt.state.chat.details.as_mut().unwrap());
        redraw_now(rt);
    };
    // A nickname limit over the ceiling, a limit over u32, a description over the limit, and a
    // name over a limit lowered in the same edit.
    set(&mut rt, &|f| f.limits[4].replace_text("1001"));
    assert!(shows(&rt, "Max nickname size can be at most"));
    set(&mut rt, &|f| {
        f.limits[4].replace_text("50");
        f.limits[2].replace_text("4294967296");
    });
    assert!(shows(&rt, "at most 4294967295"));
    set(&mut rt, &|f| {
        f.limits[2].replace_text("1000");
        f.description.replace_text("d".repeat(501));
    });
    assert!(shows(&rt, "Max Room Description Size"));
    set(&mut rt, &|f| {
        f.description.replace_text("short");
        f.limits[5].replace_text("3");
    });
    assert!(shows(&rt, "Max Room Name Size"));
    assert!(save_dimmed(&rt));
    assert!(rt.state.chat.outgoing.is_empty());
}

// ---- @mention autocomplete, and opening a member's dialog from a name, avatar or chip ----

fn type_text(rt: &mut Runtime<RiverApp>, text: &str) {
    press(rt, "composer-field");
    key(rt, Key::Text(text.into()));
}

fn mention_names(rt: &Runtime<RiverApp>) -> Vec<String> {
    rt.state
        .chat
        .mention_popup()
        .map(|p| p.candidates.into_iter().map(|c| c.name).collect())
        .unwrap_or_default()
}

#[test]
fn typing_an_at_lists_the_members_and_filters_as_you_type() {
    let mut rt = render(connected(), WINDOW_SIZE);
    assert!(rt.find("mention-popup-content").is_none());
    type_text(&mut rt, "hi @");
    assert!(rt.find("mention-popup-content").is_some());
    let all = mention_names(&rt);
    // Everyone with a name but you (Sam Rivers), by name.
    assert_eq!(
        all,
        [
            "Elena Petrova",
            "Jonas",
            "Kwame Mensah",
            "Maya Chen",
            "Priya Raman",
            "Tomás"
        ]
    );
    key(&mut rt, Key::Text("jo".into()));
    assert_eq!(mention_names(&rt), ["Jonas"]);
    // A name that only contains the query comes after those that start with it.
    key(&mut rt, Key::Backspace);
    key(&mut rt, Key::Backspace);
    key(&mut rt, Key::Text("e".into()));
    let names = mention_names(&rt);
    assert_eq!(names, ["Elena Petrova", "Kwame Mensah", "Maya Chen"]);
    key(&mut rt, Key::Text("zzz".into()));
    assert!(
        rt.find("mention-popup-content").is_none(),
        "no match, no popup"
    );
}

#[test]
fn an_email_address_is_not_a_mention() {
    let mut rt = render(connected(), WINDOW_SIZE);
    type_text(&mut rt, "write to ian@");
    assert!(rt.find("mention-popup-content").is_none());
}

#[test]
fn enter_picks_the_highlighted_member_and_writes_the_token() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let jonas = uid_of(&rt, "Jonas");
    type_text(&mut rt, "hi @jo");
    let before = rt.state.chat.room().unwrap().messages.len();
    key(&mut rt, Key::Enter);
    assert_eq!(
        rt.state.chat.composer.text,
        format!("hi @[Jonas](rv:{jonas}) ")
    );
    assert_eq!(
        rt.state.chat.room().unwrap().messages.len(),
        before,
        "Enter picked, it did not send"
    );
    assert!(rt.find("mention-popup-content").is_none());
}

#[test]
fn arrow_keys_move_the_highlight_and_tab_picks() {
    let mut rt = render(connected(), WINDOW_SIZE);
    type_text(&mut rt, "@");
    key(&mut rt, Key::Down);
    key(&mut rt, Key::Down);
    assert_eq!(rt.state.chat.mention_popup().unwrap().selected, 2);
    key(&mut rt, Key::Up);
    assert_eq!(rt.state.chat.mention_popup().unwrap().selected, 1);
    key(&mut rt, Key::Up);
    key(&mut rt, Key::Up);
    assert_eq!(
        rt.state.chat.mention_popup().unwrap().selected,
        5,
        "wraps to the last"
    );
    key(&mut rt, Key::Down);
    key(&mut rt, Key::Down);
    key(&mut rt, Key::Tab);
    let text = rt.state.chat.composer.text.clone();
    assert!(
        text.starts_with("@[Jonas](rv:") && text.ends_with(") "),
        "{text}"
    );
    assert!(rt.find("mention-popup-content").is_none());
}

#[test]
fn clicking_a_row_picks_that_member() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let priya = uid_of(&rt, "Priya Raman");
    type_text(&mut rt, "cc @pr");
    assert!(rt.find(&format!("mention-popup-option-{priya}")).is_some());
    press(&mut rt, &format!("mention-popup-option-{priya}"));
    assert_eq!(
        rt.state.chat.composer.text,
        format!("cc @[Priya Raman](rv:{priya}) ")
    );
    assert!(rt.find("mention-popup-content").is_none());
    assert!(
        rt.elements()
            .into_iter()
            .any(|e| e.key.as_deref() == Some("composer-field") && e.focused),
        "the composer keeps the focus"
    );
}

#[test]
fn escape_closes_the_popup_until_the_caret_leaves_the_word() {
    let mut rt = render(connected(), WINDOW_SIZE);
    type_text(&mut rt, "@ma");
    assert!(rt.find("mention-popup-content").is_some());
    key(&mut rt, Key::Escape);
    assert!(rt.find("mention-popup-content").is_none());
    assert_eq!(
        rt.state.chat.composer.text, "@ma",
        "Escape leaves the text alone"
    );
    key(&mut rt, Key::Text("y".into()));
    assert!(
        rt.find("mention-popup-content").is_none(),
        "still closed in the same word"
    );
    // A new word opens it again.
    key(&mut rt, Key::Text(" @".into()));
    assert!(rt.find("mention-popup-content").is_some());
}

#[test]
fn the_popup_closes_when_the_caret_leaves_the_word() {
    let mut rt = render(connected(), WINDOW_SIZE);
    type_text(&mut rt, "@ma");
    assert!(rt.find("mention-popup-content").is_some());
    key(&mut rt, Key::Text(" ".into()));
    assert!(rt.find("mention-popup-content").is_none());
    // Moving the caret back into the word brings it back.
    key(&mut rt, Key::Left);
    assert!(rt.find("mention-popup-content").is_some());
    rt.state.chat.composer.set_caret(0);
    rt.invalidate();
    rt.frame(1.0);
    assert!(rt.find("mention-popup-content").is_none());
}

#[test]
fn there_is_no_popup_in_a_room_you_cannot_write_to() {
    let mut rt = render(connected(), WINDOW_SIZE);
    type_text(&mut rt, "@");
    assert!(rt.find("mention-popup-content").is_some());
    for r in &mut rt.state.chat.rooms {
        r.send_block = Some("You have been banned.".into());
    }
    rt.invalidate();
    settle(&mut rt);
    assert!(rt.find("mention-popup-content").is_none());
    assert!(rt.state.chat.mention_popup().is_none());
}

#[test]
fn unnamed_members_and_you_are_not_offered() {
    let mut rt = render(connected(), WINDOW_SIZE);
    rt.state.chat.edit_rooms(|rooms| {
        let m = rooms[0]
            .members
            .iter_mut()
            .find(|m| m.nickname == "Tomás")
            .unwrap();
        m.claimed = false;
    });
    type_text(&mut rt, "@");
    let names = mention_names(&rt);
    assert!(!names.contains(&"Tomás".to_owned()));
    assert!(!names.contains(&"Sam Rivers".to_owned()));
}

#[test]
fn members_with_one_name_are_told_apart_by_their_id() {
    let mut rt = render(connected(), WINDOW_SIZE);
    rt.state.chat.edit_rooms(|rooms| {
        let m = rooms[0]
            .members
            .iter_mut()
            .find(|m| m.nickname == "Tomás")
            .unwrap();
        m.nickname = "jonas".into();
    });
    type_text(&mut rt, "@jonas");
    let p = rt.state.chat.mention_popup().unwrap();
    assert_eq!(p.candidates.len(), 2);
    assert!(p.candidates.iter().all(|c| c.duplicate));
}

#[test]
fn the_token_is_the_one_the_web_ui_writes() {
    use freenet_scaffold::util::FastHash;
    use river_core::mention::{MentionSegment, encode_mention, parse_segments};
    use river_core::room_state::member::MemberId;
    let id = MemberId(FastHash(0x1234_5678_9abc_def0));
    let mut rt = render(connected(), WINDOW_SIZE);
    rt.state.chat.edit_rooms(|rooms| {
        let m = rooms[0]
            .members
            .iter_mut()
            .find(|m| m.nickname == "Jonas")
            .unwrap();
        m.uid = id.to_string();
        m.nickname = "Jo [x](y)".into();
    });
    type_text(&mut rt, "hey @jo");
    key(&mut rt, Key::Tab);
    let text = rt.state.chat.composer.text.clone();
    let token = encode_mention(id, "Jo [x](y)");
    assert_eq!(text, format!("hey {token} "));
    // It reads back as a real mention of that member.
    let segs = parse_segments(&text);
    let mentions: Vec<_> = segs
        .iter()
        .filter_map(|s| match s {
            MentionSegment::Mention(m) => Some(m),
            _ => None,
        })
        .collect();
    assert_eq!(mentions.len(), 1);
    assert!(mentions[0].member_ref.matches(id));
    // And sending it sends the token as written.
    key(&mut rt, Key::Text("there".into()));
    key(&mut rt, Key::Enter);
    let last = rt.state.chat.room().unwrap().last_message().unwrap();
    assert_eq!(last.text, format!("hey {token} there"));
}

fn chat_with_chip(chip: impl Fn(&str) -> String) -> (Runtime<RiverApp>, String) {
    let mut state = connected();
    let jonas = {
        let rt = render(connected(), WINDOW_SIZE);
        uid_of(&rt, "Jonas")
    };
    state.chat.edit_rooms(|rooms| {
        let room = &mut rooms[0];
        let me = room.me;
        let mid = room.messages.last().map_or(0, |m| m.id) + 1;
        let md = chip(&jonas);
        room.messages.push(super::model::Message {
            id: mid,
            key: format!("chip-{mid}"),
            author: me,
            time: room.messages.last().map_or(0, |m| m.time),
            text: "@Jonas".into(),
            source: String::new(),
            md,
            reply_to: None,
            reactions: Vec::new(),
            edited: false,
            pending: None,
        });
    });
    (render(state, WINDOW_SIZE), jonas)
}

/// Press on the chip of the last message, which is a message of nothing else.
fn press_chip(rt: &mut Runtime<RiverApp>) {
    let id = rt
        .state
        .chat
        .room()
        .unwrap()
        .last_message()
        .unwrap()
        .key
        .clone();
    let [x, y, _, h] = rect_of(rt, &format!("md-{id}"));
    let (x, y) = (x + 10.0, y + h / 2.0);
    rt.pointer_move(x, y);
    rt.frame(0.1);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    settle(rt);
}

#[test]
fn clicking_a_mention_chip_opens_the_member_dialog() {
    let scheme = super::model::mention_scheme();
    let (mut rt, jonas) = chat_with_chip(|uid| format!("[@Jonas]({scheme}:member:{uid})"));
    assert!(rt.state.chat.member_modal.is_none());
    press_chip(&mut rt);
    assert_eq!(rt.state.chat.member_modal, Some(jonas));
    assert!(rt.find("member-info-dialog").is_some());
}

#[test]
fn a_forged_chip_opens_nothing() {
    let (mut rt, _) = chat_with_chip(|uid| format!("[@Jonas](rvm-0000000000000000:member:{uid})"));
    press_chip(&mut rt);
    assert!(rt.state.chat.member_modal.is_none());
}

#[test]
fn a_chip_of_someone_not_in_the_room_opens_nothing() {
    let scheme = super::model::mention_scheme();
    let (mut rt, _) = chat_with_chip(|_| format!("[@Jonas]({scheme}:member:NOSUCHID)"));
    press_chip(&mut rt);
    assert!(rt.state.chat.member_modal.is_none());
}

#[test]
fn clicking_an_author_name_or_avatar_opens_their_dialog() {
    for part in ["author-name", "author-avatar"] {
        let mut rt = render(connected(), WINDOW_SIZE);
        let room = rt.state.chat.room().unwrap();
        // The newest run of one author's messages, which is on screen.
        let n = room.messages.len();
        let at = (1..n)
            .rev()
            .find(|&i| room.messages[i - 1].author != room.messages[i].author)
            .unwrap();
        let first = &room.messages[at];
        let uid = room.member(first.author).unwrap().uid.clone();
        let key = format!("{part}-{}", first.key);
        assert!(rt.find("member-info-dialog").is_none());
        press(&mut rt, &key);
        assert_eq!(rt.state.chat.member_modal, Some(uid), "{part}");
        assert!(rt.find("member-info-dialog").is_some(), "{part}");
    }
}

fn mods(m: rune::Mods) -> KeyEvent {
    KeyEvent {
        key: Key::Enter,
        mods: m,
    }
}

#[test]
fn escape_then_send_then_a_new_at_sign_opens_the_popup_again() {
    let mut rt = render(connected(), WINDOW_SIZE);
    type_text(&mut rt, "@ma");
    key(&mut rt, Key::Escape);
    assert!(rt.find("mention-popup-content").is_none());
    key(&mut rt, Key::Enter);
    assert_eq!(rt.state.chat.composer.text, "");
    key(&mut rt, Key::Text("@".into()));
    assert!(rt.find("mention-popup-content").is_some());
}

#[test]
fn escape_then_deleting_the_word_then_an_at_sign_opens_the_popup_again() {
    let mut rt = render(connected(), WINDOW_SIZE);
    type_text(&mut rt, "@ma");
    key(&mut rt, Key::Escape);
    for _ in 0..3 {
        key(&mut rt, Key::Backspace);
    }
    key(&mut rt, Key::Text("@".into()));
    assert!(rt.find("mention-popup-content").is_some());
}

#[test]
fn modified_enter_and_word_keys_belong_to_the_composer() {
    let mut rt = render(connected(), WINDOW_SIZE);
    type_text(&mut rt, "@jo");
    for m in [
        rune::Mods {
            alt: true,
            ..Default::default()
        },
        rune::Mods {
            ctrl: true,
            ..Default::default()
        },
        rune::Mods {
            shift: true,
            ..Default::default()
        },
    ] {
        rt.key(mods(m));
        rt.frame(0.3);
        assert!(
            !rt.state.chat.composer.text.contains("rv:"),
            "{m:?} picked a member"
        );
    }
    let before = rt.state.chat.composer.focus;
    rt.key(KeyEvent {
        key: Key::Left,
        mods: rune::Mods {
            ctrl: true,
            ..Default::default()
        },
    });
    rt.frame(0.3);
    assert!(
        rt.state.chat.composer.focus < before,
        "Ctrl+Left moved the caret"
    );
}

#[test]
fn opening_a_member_closes_other_overlays_and_keeps_a_nickname_being_typed() {
    let mut rt = render(connected(), WINDOW_SIZE);
    rt.state.chat.emoji_open = true;
    let me = rt
        .state
        .chat
        .room()
        .unwrap()
        .member(rt.state.chat.room().unwrap().me)
        .unwrap()
        .uid
        .clone();
    rt.state.chat.open_member(&me);
    assert!(!rt.state.chat.emoji_open);
    rt.state.chat.nickname.replace_text("half typed");
    rt.state.chat.open_member(&me);
    assert_eq!(rt.state.chat.nickname.text, "half typed");
}

#[test]
fn a_name_with_hidden_characters_is_listed_cleaned_and_an_empty_one_not_at_all() {
    let mut rt = render(connected(), WINDOW_SIZE);
    rt.state.chat.edit_rooms(|rooms| {
        let ms = &mut rooms[0].members;
        ms.iter_mut()
            .find(|m| m.nickname == "Jonas")
            .unwrap()
            .nickname = "Jo\u{202e}nas".into();
        ms.iter_mut()
            .find(|m| m.nickname == "Tomás")
            .unwrap()
            .nickname = "\u{200b}".into();
    });
    type_text(&mut rt, "@");
    let names = mention_names(&rt);
    assert!(names.contains(&"Jonas".to_owned()), "{names:?}");
    assert!(!names.iter().any(|n| n.is_empty() || n.contains('\u{200b}')));
    assert_eq!(names.len(), 5);
}

#[test]
fn duplicates_are_judged_over_every_match_not_only_the_first_eight() {
    let mut rt = render(connected(), WINDOW_SIZE);
    rt.state.chat.edit_rooms(|rooms| {
        let ms = &mut rooms[0].members;
        for i in 0..9u32 {
            let mut m =
                super::model::Member::new(500 + i, format!("Aaa{i}"), super::model::Role::Member);
            m.uid = format!("U{i}");
            ms.push(m);
        }
        // Ties with the eighth listed and sorts after it, so it is not listed.
        let mut m = super::model::Member::new(600, "aaa7", super::model::Role::Member);
        m.uid = "U600".into();
        ms.push(m);
    });
    type_text(&mut rt, "@aaa");
    let p = rt.state.chat.mention_popup().unwrap();
    assert_eq!(p.candidates.len(), 8);
    assert!(
        p.candidates[7].duplicate,
        "Aaa7 shares its name with a member past the eighth"
    );
}
