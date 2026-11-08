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
    };
    let rt = render(s, WINDOW_SIZE);
    for key in ["not-updated", "not-following", "unreadable"] {
        assert!(rt.find(key).is_some(), "{key}");
    }
}

#[test]
fn an_empty_list_says_why_when_the_store_is_missing() {
    let mut chat = node_chat().chat;
    let usual = super::view::no_rooms_hint(&chat);
    chat.issues.store_missing = true;
    let missing = super::view::no_rooms_hint(&chat);
    assert_ne!(usual, missing);
    assert!(missing.contains("older than River on the web"), "{missing}");
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
