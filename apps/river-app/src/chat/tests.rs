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
    rt.on_key = crate::spec().on_key;
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
    assert!(!shows(&rt, "Connected"));
    assert!(
        rt.find("notifications").is_none(),
        "the footer has no notifications switch"
    );
}

#[test]
fn the_connection_dot_follows_the_link_status() {
    use super::view::connection_pill;
    use super::{LinkStatus::*, Source};
    let p = super::style::palette();
    let words = |s| connection_pill(s, None, &p).text;
    assert_eq!(words(Source::Node(Live)), "Connected");
    assert_eq!(words(Source::Node(Loading)), "Connecting...");
    assert_eq!(words(Source::Node(Reconnecting)), "Disconnected");
    assert_eq!(connection_pill(Source::Node(Live), None, &p).dot, p.success);
    assert_eq!(
        connection_pill(Source::Node(Loading), None, &p).dot,
        p.warning
    );
    assert_eq!(
        connection_pill(Source::Node(Reconnecting), None, &p).dot,
        p.error
    );
    assert_eq!(connection_pill(Source::Example, None, &p).dot, p.text_muted);
    // Each state is on screen, with its dot and a tooltip.
    for (status, text) in [
        (Live, "Connected"),
        (Loading, "Connecting..."),
        (Reconnecting, "Disconnected"),
    ] {
        let mut s = connected();
        s.chat.source = Source::Node(status);
        let rt = render(s, WINDOW_SIZE);
        assert!(shows(&rt, text), "{text}");
        assert!(rt.find("connection-dot").is_some());
    }
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
        // A zoomed scene lays out at the window's size divided by the zoom, so it goes narrow
        // at a window size that is not (`zoom_makes_the_layout_narrow_like_a_smaller_window`).
        if state.chat.zoom.percent() != 100 {
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
        // Only a chat screen with a room's conversation in front has a members panel: not one
        // with a direct-message thread open.
        let room_open = matches!(
            rt.state.access,
            crate::Access::Example | crate::Access::Ready
        ) && rt.state.chat.visible_room().is_some();
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
    sent_all(rt)
        .into_iter()
        .filter(|a| !matches!(a, Action::PreviewBan { .. }))
        .collect()
}

/// Everything the screen asked for, including what a ban confirmation asks the room.
fn sent_all(rt: &mut Runtime<RiverApp>) -> Vec<Action> {
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
            reply_gone: false,
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
        reply_gone: false,
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
    // Forward by word goes to the start of the next word off macOS (see
    // `rune_bump::FORWARD_STOPS`), so Z lands before "two". macOS is excluded above.
    assert_eq!(rt.state.chat.composer.text, "Yone Ztwo Xthree");
    // Ctrl+Backspace deletes the word before the caret, not the line before it.
    ctrl(&mut rt, Key::Backspace, false);
    assert_eq!(rt.state.chat.composer.text, "Yone two Xthree");
    ctrl(&mut rt, Key::Backspace, false);
    assert_eq!(rt.state.chat.composer.text, "two Xthree");
    // Ctrl+Shift+Right selects by word, up to the next word's start.
    ctrl(&mut rt, Key::Right, true);
    key(&mut rt, Key::Text("!".into()));
    assert_eq!(rt.state.chat.composer.text, "!Xthree");
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
    assert!(shows(&rt, "Member info"));
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
fn the_members_dialog_offers_message_and_leaves_moderation_to_the_rooms_own_gate() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_member(&mut rt, "Jonas");
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("member-dm"))
        .expect("no member-dm");
    // The Message button opens the member's conversation, so it is not shown unavailable.
    assert_ne!(e.help.as_deref(), Some(super::view::NOT_YET));
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
    // The room offers no Ban here (example rooms carry no authority), so none is shown.
    assert!(rt.find("member-ban").is_none());
    // Deputize is real: it works, so it does not say it is not available yet.
    let e = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("member-deputize"))
        .expect("no member-deputize");
    assert_ne!(e.help.as_deref(), Some(super::view::NOT_YET));
    assert!(!dimmed(&rt, "member-deputize"), "Deputize is usable");
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
fn the_dialog_has_no_master_switch_note() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_bell(&mut rt);
    assert!(rt.find("notify-master-off").is_none());
    assert!(!shows(&rt, "switched off for every room"));
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
fn a_link_in_the_header_description_opens_the_page_not_the_dialog() {
    // Descriptions that start with bold text, a heading or a list item used to be shown as
    // plain words, so their links could not be followed and a press opened the dialog.
    for desc in [
        "Read [Freenet](https://freenet.org/news) first.",
        "**Welcome!** Read [Freenet](https://freenet.org/news) first.",
        "# Welcome to [Freenet](https://freenet.org/news)\n\nMore below.",
        "- [Freenet](https://freenet.org/news) news\n- other",
        "\nRead https://freenet.org/news first.",
        // A pipe in the text (a separator) must not turn the links off.
        "Official room | [Freenet](https://freenet.org/news) | be kind",
        "Official room - https://freenet.org/news | be kind",
        "Docs: <https://freenet.org/news> | Matrix: ask",
    ] {
        let mut state = connected();
        state
            .chat
            .edit_rooms(|rooms| rooms[0].description = desc.into());
        let mut rt = render(state, WINDOW_SIZE);
        let key = format!("description-{}", rt.state.chat.room().unwrap().key);
        assert!(rt.find(&key).is_some(), "{desc:?} is drawn as markdown");
        let link = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("https://freenet.org/news") && e.rect[1] < 60.0)
            .unwrap_or_else(|| panic!("{desc:?}: no link in the header"));
        let (x, y) = (
            link.rect[0] + link.rect[2] / 2.0,
            link.rect[1] + link.rect[3] / 2.0,
        );
        rt.pointer_move(x, y);
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(0.1);
        rt.frame(0.2);
        assert_eq!(
            rt.take_url_requests(),
            ["https://freenet.org/news"],
            "{desc:?}"
        );
        assert!(rt.find("room-details-dialog").is_none(), "{desc:?}");
        // The words around it still open the dialog.
        press(&mut rt, "description-open");
        assert!(rt.find("room-details-dialog").is_some(), "{desc:?}");
    }
}

#[test]
fn the_room_name_opens_the_details_from_the_keyboard() {
    let mut rt = render(connected(), WINDOW_SIZE);
    // Tab reaches the name (after the connection dot in the room list); Space presses it, as
    // it does any button.
    for _ in 0..3 {
        key(&mut rt, Key::Tab);
        if rt
            .elements()
            .into_iter()
            .any(|e| e.key.as_deref() == Some("room-title") && e.focused)
        {
            break;
        }
    }
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
    assert!(shows(&rt, "Up to 1000 bytes each"));
    assert!(shows(&rt, "Maya Chen"), "the owner is named");
    for edit in [
        "details-name-input",
        "details-description-input",
        "details-save",
    ] {
        assert!(rt.find(edit).is_none(), "a member has no {edit}");
    }
    // Keys can be copied, once the technical details are open.
    press(&mut rt, "details-technical-toggle");
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

/// The top edge of the element keyed `key`.
fn top(rt: &Runtime<RiverApp>, key: &str) -> f32 {
    rt.elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("no {key}"))
        .rect[1]
}

#[test]
fn a_member_sees_what_matters_first_and_the_rest_is_folded_away() {
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    press(&mut rt, "room-title");
    // The owner by name, the member count and the message size in plain words.
    assert!(shows(&rt, "Maya Chen"));
    assert!(!shows(&rt, "Room Owner"));
    assert!(shows(&rt, "6 of 200 members (not counting the owner)"));
    assert!(shows(&rt, "Up to 1000 bytes each"));
    // No "Description" label over the description (the fold is closed, so the byte limit's
    // own row is not there to be mistaken for one), and no jargon labels.
    assert!(!rt.state.chat.details.as_ref().unwrap().limits_open);
    assert!(!shows(&rt, "Description"));
    for jargon in ["Max Recent Messages", "Max User Bans", "Max Nickname Size"] {
        assert!(!shows(&rt, jargon), "{jargon}");
    }
    // The limits and the identifiers are folded away.
    for fold in ["details-limits-fold", "details-technical"] {
        assert!(rt.find(&format!("{fold}-toggle")).is_some(), "{fold}");
        assert!(
            rt.find(&format!("{fold}-body")).is_none(),
            "{fold} starts closed"
        );
    }
    assert!(rt.find("details-limit-1").is_none());
    assert!(rt.find("details-contract-id-copy").is_none());
    assert!(!rt.state.chat.details.as_ref().unwrap().limits_open);
    assert!(!rt.state.chat.details.as_ref().unwrap().technical_open);
}

#[test]
fn the_folds_open_and_close_with_a_click_enter_and_space() {
    for fold in ["details-limits-fold", "details-technical"] {
        let mut rt = render(from_node("town-square"), WINDOW_SIZE);
        press(&mut rt, "room-title");
        let body = format!("{fold}-body");
        press(&mut rt, &format!("{fold}-toggle"));
        assert!(rt.find(&body).is_some(), "{fold}: a click opens it");
        // The button keeps the keyboard after the click.
        key(&mut rt, Key::Enter);
        assert!(rt.find(&body).is_none(), "{fold}: Enter closes it");
        key(&mut rt, Key::Text(" ".into()));
        assert!(rt.find(&body).is_some(), "{fold}: Space opens it");
        assert!(rt.find("room-details-dialog").is_some());
    }
}

#[test]
fn the_limits_fold_lists_the_rest_in_plain_words() {
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    press(&mut rt, "room-title");
    press(&mut rt, "details-limits-fold-toggle");
    assert!(shows(&rt, "Messages kept"));
    assert!(shows(&rt, "Latest 100 (11 held now)"));
    assert!(shows(&rt, "Ban list"));
    assert!(shows(&rt, "Description length"));
    assert!(shows(&rt, "Nickname"));
    assert!(shows(&rt, "Up to 50 bytes"));
    assert!(shows(&rt, "Up to 100 bytes"));
    assert!(shows(&rt, "Up to 500 bytes"));
}

#[test]
fn technical_details_say_what_each_identifier_is_for_and_can_be_copied() {
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    press(&mut rt, "room-title");
    press(&mut rt, "details-technical-toggle");
    assert!(shows(&rt, "Room ID"));
    assert!(shows(&rt, "Contract ID"));
    assert!(shows(
        &rt,
        "give it to anyone who needs to refer to this room"
    ));
    assert!(shows(&rt, "address on the Freenet network"));
    let d = &rt.state.chat.room().unwrap().details;
    assert!(shows(&rt, &d.owner_key.clone()));
    assert!(shows(&rt, &d.contract_id.clone()));
    assert!(rt.find("details-owner-key-copy").is_some());
    assert!(rt.find("details-contract-id-copy").is_some());
}

#[test]
fn a_full_room_and_a_full_ban_list_are_said_up_front() {
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    rt.state.chat.edit_rooms(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == "town-square").unwrap();
        r.details.usage.members = r.details.limits.max_members - 1;
        r.details.usage.bans = 0;
    });
    press(&mut rt, "room-title");
    assert!(rt.find("details-room-full").is_none());
    assert!(rt.find("details-bans-full").is_none());
    rt.state.chat.edit_rooms(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == "town-square").unwrap();
        r.details.usage.members = r.details.limits.max_members;
        r.details.usage.bans = r.details.limits.max_user_bans;
    });
    redraw_now(&mut rt);
    assert!(rt.find("details-room-full").is_some());
    assert!(rt.find("details-bans-full").is_some());
    assert!(shows(
        &rt,
        "Someone joining may cause the most recently invited member"
    ));
    assert!(!shows(&rt, "no new members can join"));
    // With the fold still closed.
    assert!(rt.find("details-limits-fold-body").is_none());
    assert!(shows(&rt, "200 of 200 members (not counting the owner)"));
}

#[test]
fn the_owner_is_told_when_the_room_or_its_ban_list_is_full() {
    let mut rt = render(from_node(OWNED), WINDOW_SIZE);
    rt.state.chat.edit_rooms(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == OWNED).unwrap();
        r.details.usage.members = r.details.limits.max_members;
        r.details.usage.bans = r.details.limits.max_user_bans;
    });
    press(&mut rt, "room-title");
    assert!(rt.find("details-room-full").is_some());
    assert!(rt.find("details-bans-full").is_some());
    assert!(shows(&rt, "so it is full"));
    assert!(shows(&rt, "Adding a ban may drop an older one"));
    assert!(shows(&rt, "most recently invited member"));
}

#[test]
fn the_owner_edits_every_limit_with_nothing_folded_away() {
    let mut rt = render(from_node(OWNED), WINDOW_SIZE);
    press(&mut rt, "room-title");
    for i in 0..super::details::LIMIT_COUNT {
        assert!(
            rt.find(&format!("details-limit-{i}")).is_some(),
            "limit {i}"
        );
        assert!(
            rt.find(&format!("details-limit-help-{i}")).is_some(),
            "help {i}"
        );
    }
    assert!(rt.find("details-limits-fold-toggle").is_none());
    assert!(rt.find("details-name-input").is_some());
    assert!(shows(&rt, "Largest message, in bytes."));
    // Technical details are not editable, so they may fold.
    assert!(rt.find("details-technical-toggle").is_some());
}

#[test]
fn leave_room_is_set_apart_below_a_rule_and_still_asks_first() {
    for room in ["town-square", OWNED] {
        let mut rt = render(from_node(room), WINDOW_SIZE);
        press(&mut rt, "room-title");
        let leave = top(&rt, "details-leave");
        assert!(
            top(&rt, "details-leave-rule") < leave,
            "{room}: a rule above it"
        );
        if room == OWNED {
            assert!(top(&rt, "details-save") < top(&rt, "details-leave-rule"));
        }
        let before = rt.state.chat.rooms.len();
        press(&mut rt, "details-leave");
        assert!(rt.find("details-leave-ask").is_some(), "{room}");
        assert!(rt.find("details-leave").is_none());
        assert_eq!(rt.state.chat.rooms.len(), before, "nothing left yet");
        press(&mut rt, "details-leave-cancel");
        assert!(rt.find("details-leave").is_some());
    }
}

/// How many times the leave question has been asked, counted by its own state.
fn leave_asked(rt: &Runtime<RiverApp>) -> bool {
    rt.state.chat.details.as_ref().is_some_and(|f| f.leaving)
}

#[test]
fn enter_on_leave_room_asks_once_and_a_fold_toggle_never_does() {
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    press(&mut rt, "room-title");
    // A fold toggle with the keyboard (Enter and Space) never opens the leave question.
    press(&mut rt, "details-technical-toggle");
    key(&mut rt, Key::Enter);
    key(&mut rt, Key::Text(" ".into()));
    key(&mut rt, Key::Enter);
    assert!(!leave_asked(&rt));
    assert!(rt.find("details-leave-ask").is_none());
    // Enter on the focused Leave Room asks, once: a second Enter would be the confirm button
    // only if it had focus, and it has not.
    press(&mut rt, "details-leave");
    assert!(leave_asked(&rt));
    assert!(rt.find("details-leave-ask").is_some());
    assert!(rt.find("details-leave").is_none());
    assert!(rt.state.chat.details.is_some(), "the room is not left yet");
}

#[test]
fn enter_on_the_focused_leave_button_opens_the_question_exactly_once() {
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    press(&mut rt, "room-title");
    // Focus it without activating it: Tab until it has the keyboard.
    for _ in 0..80 {
        if rt
            .elements()
            .iter()
            .any(|e| e.focused && e.key.as_deref() == Some("details-leave"))
        {
            break;
        }
        key(&mut rt, Key::Tab);
    }
    assert!(
        rt.elements()
            .iter()
            .any(|e| e.focused && e.key.as_deref() == Some("details-leave")),
        "Tab reaches Leave Room"
    );
    assert!(!leave_asked(&rt));
    key(&mut rt, Key::Enter);
    assert!(leave_asked(&rt));
    assert!(rt.state.chat.details.is_some());
    // The question toggled once, not twice: it is still showing.
    assert!(rt.find("details-leave-ask").is_some());
}

#[test]
fn enter_in_the_name_and_a_limit_field_saves_and_never_reaches_leave() {
    // The name field.
    let mut state = connected();
    state.chat.select(OWNED);
    let mut rt = render(state, WINDOW_SIZE);
    press(&mut rt, "room-title");
    type_into(&mut rt, "details-name-input", "Reading Circle");
    key(&mut rt, Key::Enter);
    assert!(!leave_asked(&rt));
    assert!(rt.find("room-details-dialog").is_none(), "Enter saved it");
    assert_eq!(rt.state.chat.room().unwrap().name, "Reading Circle");
    // A limit field.
    let mut state = connected();
    state.chat.select(OWNED);
    let mut rt = render(state, WINDOW_SIZE);
    press(&mut rt, "room-title");
    press(&mut rt, "details-limit-0");
    rt.state.chat.details.as_mut().unwrap().limits[0].replace_text("12");
    redraw_now(&mut rt);
    key(&mut rt, Key::Enter);
    assert!(!leave_asked(&rt));
    assert!(rt.find("room-details-dialog").is_none(), "Enter saved it");
    assert_eq!(rt.state.chat.room().unwrap().details.limits.max_members, 12);
}

#[test]
fn escape_closes_the_dialog_with_a_fold_open() {
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    press(&mut rt, "room-title");
    press(&mut rt, "details-limits-fold-toggle");
    press(&mut rt, "details-technical-toggle");
    assert!(rt.find("details-limits-fold-body").is_some());
    key(&mut rt, Key::Escape);
    assert!(rt.find("room-details-dialog").is_none());
    assert!(rt.state.chat.details.is_none());
}

#[test]
fn the_owner_link_opens_the_owner_dialog_once_from_the_keyboard() {
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    press(&mut rt, "room-title");
    for _ in 0..80 {
        if rt
            .elements()
            .iter()
            .any(|e| e.focused && e.key.as_deref() == Some("details-owner-open"))
        {
            break;
        }
        key(&mut rt, Key::Tab);
    }
    key(&mut rt, Key::Enter);
    assert!(rt.find("room-details-dialog").is_none());
    assert!(
        rt.find("member-info-dialog").is_some(),
        "the owner's dialog is open"
    );
    assert!(rt.state.chat.details.is_none());
}

#[test]
fn a_room_without_its_owner_in_the_member_list_still_has_an_owner_row() {
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    rt.state.chat.edit_rooms(|rooms| {
        let r = rooms.iter_mut().find(|r| r.key == "town-square").unwrap();
        r.members.retain(|m| m.role != super::model::Role::Owner);
    });
    press(&mut rt, "room-title");
    assert!(rt.find("details-owner").is_some());
    assert!(rt.find("details-owner-open").is_none());
    assert!(rt.find("details-owner-unknown").is_some());
    assert!(shows(&rt, "townsqua"));
}

#[test]
fn the_owner_name_opens_the_owners_dialog() {
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    press(&mut rt, "room-title");
    // The link is the owner's name, not a generic label.
    assert!(shows(&rt, "Maya Chen"));
    assert!(!shows(&rt, "Room Owner"));
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
        "details-description-edit",
        "details-save",
        "details-cancel",
    ] {
        assert!(rt.find(edit).is_some(), "the owner has {edit}");
    }
    // The description is rendered until the pencil turns it into the text area.
    assert!(rt.find("details-description-input").is_none());
    press(&mut rt, "details-description-edit");
    assert!(rt.find("details-description-input").is_some());
    assert!(rt.find("details-description-edit").is_none());
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
fn the_dialog_shows_the_description_rendered_with_links_that_open() {
    for owner in [false, true] {
        let mut state = from_node(if owner { OWNED } else { "town-square" });
        state.chat.edit_rooms(|rooms| {
            for r in rooms
                .iter_mut()
                .filter(|r| r.key == OWNED || r.key == "town-square")
            {
                r.description =
                    "Some **bold** words and a [link](https://freenet.org/rules).\n\n- one\n- two"
                        .into();
                r.details.description_source = r.description.clone();
            }
        });
        let mut rt = render(state, WINDOW_SIZE);
        press(&mut rt, "room-title");
        let key = format!("details-description-{}", rt.state.chat.room().unwrap().key);
        assert!(
            rt.find(&key).is_some(),
            "rendered as markdown, owner {owner}"
        );
        assert!(!shows(&rt, "**bold**"), "no raw markdown, owner {owner}");
        assert!(rt.find("details-description-input").is_none());
        let link = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("https://freenet.org/rules") && e.rect[1] > 80.0)
            .expect("the link is drawn in the dialog");
        let (x, y) = (
            link.rect[0] + link.rect[2] / 2.0,
            link.rect[1] + link.rect[3] / 2.0,
        );
        rt.pointer_move(x, y);
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        rt.frame(0.1);
        assert_eq!(rt.take_url_requests(), ["https://freenet.org/rules"]);
        assert_eq!(rt.find("details-description-edit").is_some(), owner);
    }
}

#[test]
fn cancelling_the_description_edit_returns_the_form_to_the_room() {
    let mut rt = render(from_node(OWNED), WINDOW_SIZE);
    let before = rt.state.chat.room().unwrap().description.clone();
    press(&mut rt, "room-title");
    press(&mut rt, "details-description-edit");
    assert!(
        !details(&rt).name.focused,
        "only the text area has the keyboard"
    );
    type_into(&mut rt, "details-description-input", "Scratch");
    assert_eq!(details(&rt).description.text, "Scratch");
    press(&mut rt, "details-cancel");
    assert!(rt.find("room-details-dialog").is_none());
    assert_eq!(rt.state.chat.room().unwrap().description, before);
    assert!(rt.state.chat.outgoing.is_empty(), "nothing was queued");
    // Reopened, the form starts from the room again, rendered, not left in the editor.
    press(&mut rt, "room-title");
    assert_eq!(details(&rt).description.text, before);
    assert!(!details(&rt).editing_description);
    assert!(rt.find("details-description-input").is_none());
}

#[test]
fn text_in_the_header_description_opens_the_dialog_and_a_link_follows() {
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        rooms[0].description = "Read the [Freenet](https://freenet.org/news) page first.".into();
    });
    let mut rt = render(state, WINDOW_SIZE);
    let key = format!("description-{}", rt.state.chat.room().unwrap().key);
    let md = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(&key))
        .expect("the header description");
    // A point on the first words of the line, well inside the text and left of the link.
    let (x, y) = (md.rect[0] + 12.0, md.rect[1] + md.rect[3] / 2.0);
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.1);
    rt.frame(0.2);
    assert!(
        rt.find("room-details-dialog").is_some(),
        "text opens the dialog"
    );
    assert!(rt.take_url_requests().is_empty());
    key_escape(&mut rt);
    let link = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("https://freenet.org/news") && e.rect[1] < 60.0)
        .expect("the link");
    let (x, y) = (
        link.rect[0] + link.rect[2] / 2.0,
        link.rect[1] + link.rect[3] / 2.0,
    );
    rt.pointer_move(x, y);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.1);
    rt.frame(0.2);
    assert_eq!(rt.take_url_requests(), ["https://freenet.org/news"]);
    assert!(rt.find("room-details-dialog").is_none());
}

fn key_escape(rt: &mut Runtime<RiverApp>) {
    key(rt, Key::Escape);
}

#[test]
fn hidden_characters_in_link_labels_never_reach_the_screen() {
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        rooms[0].description = "See [evil\u{202e}gnp](https://freenet.org/a) now".into();
        if let Some(m) = rooms[0].messages.last_mut() {
            m.md = "a [fake\u{202e}txt](https://freenet.org/b) label".into();
        }
    });
    let rt = render(state, WINDOW_SIZE);
    assert!(shows(&rt, "evilgnp"), "the header label is drawn");
    assert!(shows(&rt, "faketxt"), "the message label is drawn");
    assert!(!texts(&rt).iter().any(|t| t.contains('\u{202e}')));
}

/// A room with a long, multi-paragraph description still fits a normal window without
/// scrolling, and a small window scrolls inside the dialog instead of past the screen.
#[test]
fn the_dialog_fits_the_window() {
    let long = "A long description, with [a link](https://freenet.org).\n\n".repeat(12);
    let build = |size: (f32, f32)| {
        let mut state = from_node(OWNED);
        state.chat.edit_rooms(|rooms| {
            for r in rooms.iter_mut().filter(|r| r.key == OWNED) {
                r.description = long.clone();
                r.details.description_source = long.clone();
            }
        });
        let mut rt = render(state, size);
        press(&mut rt, "room-title");
        rt
    };
    let rect = |rt: &Runtime<RiverApp>, key: &str| {
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(key))
            .unwrap_or_else(|| panic!("no {key}"))
            .rect
    };
    for size in [
        WINDOW_SIZE,
        NARROW_SCENE_SIZE,
        (640.0, 480.0),
        (640.0, 300.0),
    ] {
        let rt = build(size);
        let dialog = rect(&rt, "room-details-dialog");
        assert!(
            dialog[1] >= 0.0 && dialog[1] + dialog[3] <= size.1,
            "{size:?}: the dialog {dialog:?} is inside the window"
        );
        assert!(rt.find("details-save").is_some());
    }
    // The room-size limits are three to a row in a wide window and one to a row in a narrow
    // one; the byte-size limits are two to a row in a wide window.
    let wide = build(WINDOW_SIZE);
    let y = |rt: &Runtime<RiverApp>, i: usize| rect(rt, &format!("details-limit-{i}"))[1];
    assert_eq!(y(&wide, 0), y(&wide, 1));
    assert_eq!(y(&wide, 0), y(&wide, 3));
    assert_ne!(y(&wide, 0), y(&wide, 2));
    assert_eq!(y(&wide, 2), y(&wide, 4));
    let narrow = build(NARROW_SCENE_SIZE);
    assert_ne!(y(&narrow, 0), y(&narrow, 1));
    assert_ne!(y(&narrow, 2), y(&narrow, 4));
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
    assert!(shows(&rt, "61 held now"));
    assert!(shows(&rt, "4 on the list now"));
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
            reply_gone: false,
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

fn help_of(rt: &Runtime<RiverApp>, key: &str) -> Option<String> {
    rt.elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("no {key}"))
        .help
}

#[test]
fn up_in_an_empty_composer_edits_your_last_message() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let mine = message_of(&rt, true);
    let text = rt
        .state
        .chat
        .room()
        .unwrap()
        .message_by_key(&mine)
        .unwrap()
        .text
        .clone();
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Up);
    assert_eq!(rt.state.chat.composer.text, text);
    assert!(rt.find("editing").is_some());
}

#[test]
fn up_does_nothing_with_text_typed_or_while_replying() {
    let mut rt = render(connected(), WINDOW_SIZE);
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Text("hi".into()));
    key(&mut rt, Key::Up);
    assert!(rt.find("editing").is_none());
    assert_eq!(rt.state.chat.composer.text, "hi");
    rt.state.chat.composer.replace_text("");
    let theirs = message_of(&rt, false);
    rt.state.chat.start_reply(&theirs);
    rt.frame(0.5);
    key(&mut rt, Key::Up);
    assert!(rt.find("editing").is_none());
    assert!(rt.find("replying").is_some());
}

#[test]
fn up_with_no_message_of_yours_does_nothing() {
    let mut rt = render(connected(), WINDOW_SIZE);
    press(&mut rt, &format!("room-{EMPTY_ROOM}"));
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Up);
    assert!(rt.find("editing").is_none());
}

#[test]
fn escape_cancels_an_edit_and_a_reply() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let mine = message_of(&rt, true);
    rt.state.chat.composer.replace_text("my draft");
    hover_press(&mut rt, &format!("edit-{mine}"));
    assert!(rt.find("editing").is_some());
    key(&mut rt, Key::Escape);
    assert!(rt.find("editing").is_none());
    assert_eq!(rt.state.chat.composer.text, "my draft");

    let theirs = message_of(&rt, false);
    hover_press(&mut rt, &format!("reply-{theirs}"));
    assert!(rt.find("replying").is_some());
    key(&mut rt, Key::Escape);
    assert!(rt.find("replying").is_none());
}

fn type_bytes(rt: &mut Runtime<RiverApp>, n: usize) {
    let text = "a".repeat(n);
    rt.update(|s, _| s.chat.composer.replace_text(&text));
    rt.frame(0.5);
}

/// Type 'a's until the draft measures exactly `target` encoded bytes. Fails if the size steps
/// over `target` (it would then not be the size the test claims to check).
fn fit(rt: &mut Runtime<RiverApp>, target: usize) {
    let size_of = |rt: &mut Runtime<RiverApp>, n: usize| {
        rt.state.chat.composer.replace_text("a".repeat(n));
        rt.state.chat.draft_size().unwrap().0
    };
    let mut n = 0;
    while size_of(rt, n) < target {
        n += 1;
    }
    assert_eq!(size_of(rt, n), target, "the size steps over {target}");
    type_bytes(rt, n);
    assert_eq!(rt.state.chat.draft_size().unwrap().0, target);
}

#[test]
fn the_byte_counter_shows_from_eighty_percent_and_blocks_sending_over_the_limit() {
    let mut rt = node_rooms(|_| {});
    assert_eq!(rt.state.chat.draft_size().unwrap().1, 1000);
    fit(&mut rt, 100);
    assert!(rt.find("byte-counter").is_none(), "far from the limit");
    fit(&mut rt, 800);
    assert!(
        rt.find("byte-counter").is_none(),
        "exactly 80% is not past it"
    );
    fit(&mut rt, 801);
    assert!(
        texts(&rt).contains(&"801/1000".to_owned()),
        "{:?}",
        texts(&rt)
    );
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Enter);
    assert_eq!(sent(&mut rt).len(), 1, "near the limit still sends");

    // Over the limit: said in full, and refused with the draft kept.
    rt.update(|s, _| s.chat.composer.replace_text("a".repeat(1000)));
    rt.frame(0.5);
    let size = rt.state.chat.draft_size().unwrap().0;
    assert!(size > 1000, "{size}");
    assert!(shows(
        &rt,
        &format!("Message too long \u{2014} {size}/1000 bytes")
    ));
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Enter);
    assert!(sent(&mut rt).is_empty(), "over the limit is refused");
    assert_eq!(rt.state.chat.composer.text.len(), 1000, "the draft is kept");
    press(&mut rt, "send");
    assert!(sent(&mut rt).is_empty(), "the Send button too");

    fit(&mut rt, 1000);
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Enter);
    assert_eq!(sent(&mut rt).len(), 1, "exactly at the limit sends");
}

#[test]
fn the_byte_counter_counts_utf8_bytes_and_replies_carry_overhead() {
    let mut rt = node_rooms(|_| {});
    let plain = |rt: &Runtime<RiverApp>| rt.state.chat.draft_size().unwrap().0;
    rt.state.chat.composer.replace_text("a");
    let one = plain(&rt);
    rt.state.chat.composer.replace_text("\u{20ac}");
    assert_eq!(plain(&rt), one + 2, "a euro sign is three bytes");
    let theirs = message_of(&rt, false);
    rt.state.chat.start_reply(&theirs);
    assert!(
        plain(&rt) > one + 2,
        "a reply embeds who and what it answers"
    );
}

#[test]
fn a_message_time_shows_the_full_date_on_hover() {
    let rt = render(connected(), WINDOW_SIZE);
    let room = rt.state.chat.room().unwrap();
    let first = &room.messages[0];
    let help = help_of(&rt, &format!("time-{}", first.key)).unwrap();
    assert_eq!(
        help,
        super::model::full_time(rt.state.chat.day_zero, first.time)
    );
    assert!(
        help.contains("2026") && help.contains(&super::model::clock(first.time)),
        "{help}"
    );
    // A continuation row's gutter time says it too.
    let cont = room
        .messages
        .windows(2)
        .find(|w| w[0].author == w[1].author && w[0].time / 1440 == w[1].time / 1440)
        .map(|w| w[1].key.clone())
        .unwrap();
    assert!(help_of(&rt, &format!("time-{cont}")).is_some());
}

#[test]
fn the_full_time_is_the_web_uis_format() {
    // 2026-11-19 is a Thursday.
    let day = super::model::days_from_civil(2026, 11, 19);
    assert_eq!(
        super::model::full_time(day, 14 * 60 + 5),
        "Thu, Nov 19, 2026 14:05"
    );
    assert_eq!(
        super::model::full_time(day, 1440 + 9),
        "Fri, Nov 20, 2026 00:09"
    );
    assert_eq!(super::model::full_time(day, 0), "Thu, Nov 19, 2026 00:00");
}

#[test]
fn hovering_a_reaction_names_who_reacted() {
    let rt = node_rooms(|rooms| {
        let room = &mut rooms[0];
        let me = room.me;
        let other = room.members.iter().find(|m| m.id != me).unwrap().id;
        let last = room.messages.len() - 1;
        room.messages[last].reactions = vec![
            super::Reaction {
                emoji: "🎉".into(),
                by: vec![other, me],
            },
            super::Reaction {
                emoji: "👍".into(),
                by: vec![other],
            },
        ];
    });
    let room = rt.state.chat.room().unwrap();
    let me = room.nickname(room.me).to_owned();
    let other = room
        .members
        .iter()
        .find(|m| m.id != room.me)
        .unwrap()
        .nickname
        .clone();
    let target = room.last_message().unwrap().key.clone();
    assert_eq!(
        help_of(&rt, &format!("reaction-{target}-🎉")).unwrap(),
        format!("{other}, {me} (click to remove)")
    );
    assert_eq!(
        help_of(&rt, &format!("reaction-{target}-👍")).unwrap(),
        other
    );
}

#[test]
fn latest_shows_when_scrolled_away_and_jumps_to_the_end() {
    let mut rt = render(connected(), WINDOW_SIZE);
    assert!(rt.find("latest").is_none(), "at the newest message");
    let (x, y) = rt.find("history-frame").unwrap();
    rt.wheel(x, y, 2000.0);
    for i in 0..10 {
        rt.frame(1.0 + f64::from(i) * 0.05);
    }
    assert!(rt.find("latest").is_some(), "scrolled up");
    press(&mut rt, "latest");
    for i in 0..60 {
        rt.frame(3.0 + f64::from(i) * 0.05);
    }
    assert!(rt.find("latest").is_none(), "back at the end");
}

#[test]
fn the_apps_title_has_the_room_and_a_count_only_once_known_to_be_unfocused() {
    let mut s = connected();
    s.chat.select(EMPTY_ROOM);
    let name = s.chat.room().unwrap().name.clone();
    // No focus event yet: treated as focused, so no prefix on a window that has focus.
    assert_eq!(s.window_title(4), format!("River - {name}"));
    s.focus_changed(false);
    assert_eq!(s.window_title(4), format!("(4) River - {name}"));
    assert_eq!(s.window_title(0), format!("River - {name}"));
    s.focus_changed(true);
    assert_eq!(s.window_title(4), format!("River - {name}"));
}

/// Mark the open room's newest message of mine as on its way, failed, or unreadable.
fn spoil_newest_of_mine(rt: &mut Runtime<RiverApp>, how: &str) {
    rt.update(|s, _| {
        let how = how.to_owned();
        s.chat.edit_rooms(move |rooms| {
            let room = &mut rooms[0];
            let me = room.me;
            let m = room
                .messages
                .iter_mut()
                .rev()
                .find(|m| m.author == me)
                .unwrap();
            match how.as_str() {
                "pending" => {
                    m.pending = Some(super::model::Pending {
                        entry: 1,
                        failed: None,
                        retry: false,
                    })
                }
                "failed" => {
                    m.pending = Some(super::model::Pending {
                        entry: 1,
                        failed: Some("no".into()),
                        retry: true,
                    })
                }
                _ => m.source.clear(),
            }
        })
    });
    rt.frame(0.5);
}

#[test]
fn up_refuses_when_your_newest_message_cannot_be_edited_and_does_not_pick_an_older_one() {
    for how in ["pending", "failed", "unreadable"] {
        let mut rt = node_rooms(|_| {});
        // An older message of mine that could be edited.
        let room = rt.state.chat.room().unwrap();
        let mine = room.messages.iter().filter(|m| m.author == room.me).count();
        assert!(mine >= 2, "the example room has older messages of mine");
        spoil_newest_of_mine(&mut rt, how);
        press(&mut rt, "composer-field");
        key(&mut rt, Key::Up);
        assert!(rt.find("editing").is_none(), "{how}");
        assert_eq!(rt.state.chat.composer.text, "", "{how}");
        assert!(!rt.state.chat.edit_last(), "{how}");
    }
}

#[test]
fn the_composers_keys_run_in_order_escape_closes_the_picker_before_the_reply() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let theirs = message_of(&rt, false);
    hover_press(&mut rt, &format!("reply-{theirs}"));
    rt.update(|s, _| s.chat.emoji_open = true);
    rt.frame(0.5);
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Escape);
    assert!(!rt.state.chat.emoji_open, "the picker closes first");
    assert!(rt.find("replying").is_some(), "and the reply stays");
    key(&mut rt, Key::Escape);
    assert!(
        rt.find("replying").is_none(),
        "the next Escape abandons the reply"
    );
    // Up comes after Escape and only acts on an empty composer.
    key(&mut rt, Key::Up);
    assert!(rt.find("editing").is_some());
}

#[test]
fn escape_cancels_a_reply_when_the_composer_does_not_have_focus() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let theirs = message_of(&rt, false);
    hover_press(&mut rt, &format!("reply-{theirs}"));
    assert!(rt.find("replying").is_some());
    assert!(
        rt.state.chat.composer.focused,
        "a reply focuses the composer"
    );
    // Focus moves off the composer, onto a control elsewhere in the conversation.
    press(&mut rt, "emoji");
    press(&mut rt, "emoji");
    rt.update(|s, _| s.chat.composer.focused = false);
    rt.frame(0.5);
    key(&mut rt, Key::Escape);
    assert!(rt.find("replying").is_none());
}

#[test]
fn a_blocked_send_says_why_and_a_room_without_a_limit_blocks_nothing() {
    let mut rt = node_rooms(|_| {});
    rt.update(|s, _| s.chat.composer.replace_text("a".repeat(1000)));
    rt.frame(0.5);
    assert_eq!(
        help_of(&rt, "send").as_deref(),
        Some("Message exceeds the 1000 byte limit")
    );
    let mut rt = node_rooms(|rooms| rooms[0].details.limits.max_message_size = 0);
    rt.update(|s, _| s.chat.composer.replace_text("a".repeat(5000)));
    rt.frame(0.5);
    assert!(rt.state.chat.draft_size().is_none() && !rt.state.chat.over_limit());
    assert!(rt.find("byte-counter").is_none());
    press(&mut rt, "composer-field");
    key(&mut rt, Key::Enter);
    assert_eq!(sent(&mut rt).len(), 1, "no known limit, nothing to refuse");
}

#[test]
fn the_mention_popup_gets_escape_and_up_before_the_reply_edit_and_picker_handlers() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let theirs = message_of(&rt, false);
    hover_press(&mut rt, &format!("reply-{theirs}"));
    type_text(&mut rt, "@");
    assert!(rt.state.chat.mention_popup().is_some());
    rt.update(|s, _| s.chat.emoji_open = true);
    rt.frame(0.5);
    key(&mut rt, Key::Escape);
    assert!(
        rt.state.chat.mention_popup().is_none(),
        "the popup took Escape"
    );
    assert!(rt.find("replying").is_some(), "the reply is still there");
    assert!(rt.state.chat.emoji_open, "and so is the picker");
    // Up moves within the popup; it does not start an edit (the composer is not empty).
    rt.state.chat.composer.replace_text("");
    key(&mut rt, Key::Escape);
    assert!(!rt.state.chat.emoji_open, "then the picker");
    key(&mut rt, Key::Escape);
    assert!(rt.find("replying").is_none(), "then the reply");
}

// Deputies, as the web UI shows them. In the example Town Square Jonas is the owner's deputy
// (he can ban you) and Elena is your own.

#[test]
fn a_deputy_has_a_tag_on_their_messages_and_a_shield_in_the_list() {
    let rt = render(connected(), WINDOW_SIZE);
    // Elena's messages carry DEPUTY and the owner's OWNER.
    assert!(rt.find("tag-deputy").is_some());
    assert!(rt.find("tag-owner").is_some());
    assert!(shows(&rt, "DEPUTY"));
    let room = rt.state.chat.room().unwrap();
    let elena = room
        .members
        .iter()
        .find(|m| m.nickname == "Elena Petrova")
        .unwrap();
    let jonas = room.members.iter().find(|m| m.nickname == "Jonas").unwrap();
    let kwame = room
        .members
        .iter()
        .find(|m| m.nickname == "Kwame Mensah")
        .unwrap();
    let (elena, jonas, kwame) = (elena.id, jonas.id, kwame.id);
    assert!(rt.find(&format!("member-deputy-{elena}")).is_some());
    assert!(rt.find(&format!("member-deputy-{jonas}")).is_some());
    assert!(rt.find(&format!("member-deputy-{kwame}")).is_none());
    // The shield says who appointed them and what that means for you.
    let tip = |rt: &Runtime<RiverApp>, id: u32| {
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(format!("member-deputy-{id}").as_str()))
            .and_then(|e| e.help)
    };
    assert_eq!(
        tip(&rt, jonas).as_deref(),
        Some("Deputy (appointed by the room owner). Can ban you.")
    );
    assert_eq!(
        tip(&rt, elena).as_deref(),
        Some("Deputy (appointed by you). Cannot ban you.")
    );
    // The owner keeps the crown, not a shield.
    let maya = rt
        .state
        .chat
        .room()
        .unwrap()
        .members
        .iter()
        .find(|m| m.role == super::model::Role::Owner)
        .unwrap()
        .id;
    assert!(rt.find(&format!("member-deputy-{maya}")).is_none());
}

#[test]
fn a_deputys_dialog_has_the_tag_and_who_deputized_them() {
    let mut rt = render(connected(), WINDOW_SIZE);
    open_member(&mut rt, "Jonas");
    assert!(rt.find("member-tag-deputy").is_some());
    assert!(shows(&rt, "Deputy"));
    assert!(shows(&rt, "Deputized by:"));
    assert!(shows(&rt, "the room owner"));
    // The appointer opens their own dialog.
    press(&mut rt, "member-appointer-0");
    assert_eq!(rt.state.chat.member_modal, Some(uid_of(&rt, "Maya Chen")));
    assert!(rt.find("member-tag-deputy").is_none());
    assert!(rt.find("member-deputized-by").is_none());
    key(&mut rt, Key::Escape);
    // A plain member has neither.
    open_member(&mut rt, "Kwame Mensah");
    assert!(rt.find("member-tag-deputy").is_none());
    assert!(rt.find("member-deputized-by").is_none());
}

#[test]
fn a_name_that_imitates_a_deputy_is_warned_about() {
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        let room = &mut rooms[0];
        let owner = room
            .members
            .iter()
            .find(|m| m.role == super::model::Role::Owner)
            .unwrap()
            .id;
        // A Cyrillic "о" for Jonas's first "o"; a name of their own; Elena's name with a
        // zero-width character; and the owner's.
        for (id, name) in [
            (91, "J\u{043e}nas"),
            (92, "Jonas2"),
            (93, "Elena\u{200B} Petrova"),
        ] {
            let mut m = super::model::Member::new(id, name, super::model::Role::Member);
            m.invited_by = Some(owner);
            room.members.push(m);
        }
    });
    let mut rt = render(state, WINDOW_SIZE);
    for (uid, warned, text) in [
        ("91", true, "NOT a moderator"),
        ("92", false, ""),
        ("93", true, "NOT a moderator"),
    ] {
        rt.state.chat.open_member(uid);
        rt.invalidate();
        settle(&mut rt);
        assert_eq!(
            rt.find("member-tag-impersonation").is_some(),
            warned,
            "member {uid}"
        );
        if warned {
            assert!(shows(&rt, text), "member {uid}");
            assert!(shows(&rt, "Resembles:"));
        }
        key(&mut rt, Key::Escape);
        settle(&mut rt);
    }
    // The real deputies are not accused of imitating themselves.
    for name in ["Jonas", "Elena Petrova"] {
        open_member(&mut rt, name);
        assert!(rt.find("member-tag-impersonation").is_none(), "{name}");
        key(&mut rt, Key::Escape);
        settle(&mut rt);
    }
}

#[test]
fn deputy_names_with_nothing_chosen_are_not_protected() {
    use super::model::{Member, Role};
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        let room = &mut rooms[0];
        // A deputy who never chose a name (shown as a stand-in), and one whose name is a
        // placeholder. Neither protects the name it wears.
        let jonas = room
            .members
            .iter_mut()
            .find(|m| m.nickname == "Jonas")
            .unwrap();
        jonas.claimed = false;
        let elena = room
            .members
            .iter_mut()
            .find(|m| m.nickname == "Elena Petrova")
            .unwrap();
        elena.nickname = "Unknown".into();
        for (id, name) in [(91, "Jonas"), (92, "unknown")] {
            let mut m = Member::new(id, name, Role::Member);
            m.invited_by = Some(12);
            room.members.push(m);
        }
    });
    let mut rt = render(state, WINDOW_SIZE);
    for uid in ["91", "92"] {
        rt.state.chat.open_member(uid);
        rt.invalidate();
        settle(&mut rt);
        assert!(
            rt.find("member-tag-impersonation").is_none(),
            "member {uid}"
        );
        key(&mut rt, Key::Escape);
        settle(&mut rt);
    }
}

#[test]
fn two_deputies_with_the_same_name_get_a_sentence_true_of_both() {
    use super::model::{Appointer, Member, Role};
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        let room = &mut rooms[0];
        let mut twin = Member::new(91, "J\u{043e}nas", Role::Deputy);
        twin.invited_by = Some(12);
        twin.deputized_by = vec![Appointer::Owner];
        twin.can_ban_you = Some(true);
        room.members.push(twin);
    });
    let mut rt = render(state, WINDOW_SIZE);
    rt.state.chat.open_member("91");
    rt.invalidate();
    settle(&mut rt);
    assert!(rt.find("member-tag-impersonation").is_some());
    assert!(shows(&rt, "Both are real"));
    assert!(!shows(&rt, "NOT a moderator"));
}

#[test]
fn colliding_deputy_names_are_taken_in_member_id_order_not_list_order() {
    use super::model::{Appointer, Member, Role};
    let mut state = connected();
    state.chat.edit_rooms(|rooms| {
        let room = &mut rooms[0];
        // Real uids are truncated base32, which a number parse cannot order. The listed-first
        // twin has the LATER member id, so the other must be the one named.
        for (id, uid, name, order) in [
            (91, "q7xk3m2f", "Zed", 5),
            (92, "b4tn8w6c", "Z\u{0435}d", 1),
        ] {
            let mut m = Member::new(id, name, Role::Deputy);
            m.uid = uid.into();
            m.order = order;
            m.invited_by = Some(12);
            m.deputized_by = vec![Appointer::Owner];
            m.can_ban_you = Some(true);
            room.members.push(m);
        }
        let mut imp = Member::new(93, "ZE\u{0044}", Role::Member);
        imp.uid = "h2p5v9ra".into();
        imp.invited_by = Some(12);
        room.members.push(imp);
    });
    let room = state.chat.room().unwrap();
    let imp = room.member_by_uid("h2p5v9ra").unwrap();
    let hit = super::member_modal::imitation(room, imp).expect("imitation");
    assert_eq!(hit.who.nickname, "Z\u{0435}d");
    // And the dialog shows the warning.
    let mut rt = render(state, WINDOW_SIZE);
    rt.state.chat.open_member("h2p5v9ra");
    rt.invalidate();
    settle(&mut rt);
    assert!(rt.find("member-tag-impersonation").is_some());
}

// Deputize and Revoke deputy -------------------------------------------------------------------

/// Node rooms where Sam (the viewer) is the owner of the first room and every other member is a
/// plain member, after `edit`.
fn as_owner(edit: impl FnOnce(&mut super::Room)) -> Runtime<RiverApp> {
    node_rooms(|rooms| {
        let room = &mut rooms[0];
        let me = room.me;
        for m in &mut room.members {
            m.role = if m.id == me {
                super::model::Role::Owner
            } else {
                super::model::Role::Member
            };
            m.my_deputy = false;
            m.deputized_by.clear();
            m.can_ban_you = None;
        }
        edit(room);
    })
}

fn dialog_of(rt: &mut Runtime<RiverApp>, name: &str) {
    open_member(rt, name);
    settle(rt);
}

#[test]
fn the_owner_is_offered_deputize_for_any_other_member_and_it_asks_first() {
    let mut rt = as_owner(|_| {});
    dialog_of(&mut rt, "Jonas");
    assert!(rt.find("member-deputize").is_some());
    assert!(rt.find("member-revoke-deputy").is_none());
    assert!(shows(&rt, "Let Jonas help moderate people you invited."));
    press(&mut rt, "member-deputize");
    // Nothing is sent by the first press: it asks.
    assert!(sent(&mut rt).is_empty());
    assert!(rt.find("member-deputy-confirm").is_some());
    assert!(shows(&rt, "Deputize this member?"));
    press(&mut rt, "member-deputy-cancel");
    assert!(sent(&mut rt).is_empty());
    assert!(rt.find("member-deputy-confirm").is_none());
    assert!(rt.find("member-deputize").is_some());
    press(&mut rt, "member-deputize");
    press(&mut rt, "member-deputy-confirm");
    assert_eq!(
        sent(&mut rt),
        [Action::SetDeputy {
            target: uid_of(&rt, "Jonas"),
            grant: true
        }]
    );
    assert!(rt.state.chat.deputy_confirm.is_none());
    assert!(
        rt.state.chat.member_modal.is_some(),
        "stays to show its state"
    );
}

#[test]
fn one_of_my_deputies_is_offered_revoke_even_when_i_invited_nobody() {
    // Sam invited nobody in the example, and is not the owner; Elena is Sam's deputy.
    let mut rt = node_rooms(|_| {});
    dialog_of(&mut rt, "Elena Petrova");
    assert!(rt.find("member-deputize").is_none());
    assert!(rt.find("member-revoke-deputy").is_some());
    assert!(shows(
        &rt,
        "Elena Petrova can currently help moderate people you invited."
    ));
    press(&mut rt, "member-revoke-deputy");
    assert!(sent(&mut rt).is_empty());
    assert!(shows(&rt, "Revoke this deputy?"));
    press(&mut rt, "member-deputy-confirm");
    assert_eq!(
        sent(&mut rt),
        [Action::SetDeputy {
            target: uid_of(&rt, "Elena Petrova"),
            grant: false
        }]
    );
}

#[test]
fn a_member_who_invited_nobody_is_not_offered_deputize() {
    let mut rt = node_rooms(|_| {});
    dialog_of(&mut rt, "Jonas");
    assert!(rt.find("member-deputize").is_none());
    assert!(rt.find("member-revoke-deputy").is_none());
}

#[test]
fn a_member_with_someone_in_their_invite_subtree_is_offered_deputize() {
    // Sam did not invite Tomas in the example (Jonas did): make Sam his inviter.
    let mut rt = node_rooms(|rooms| {
        let room = &mut rooms[0];
        let me = room.me;
        let tomas = room
            .members
            .iter_mut()
            .find(|m| m.nickname == "Tomás")
            .unwrap();
        tomas.invited_by = Some(me);
    });
    dialog_of(&mut rt, "Jonas");
    assert!(rt.find("member-deputize").is_some());
}

#[test]
fn nobody_offers_deputize_for_the_owner_or_yourself() {
    let mut rt = as_owner(|_| {});
    dialog_of(&mut rt, "Sam Rivers");
    assert!(rt.find("member-deputize").is_none());
    assert!(rt.find("member-revoke-deputy").is_none());
    key(&mut rt, Key::Escape);
    // A member who is not the owner cannot make the owner a deputy, nor is Revoke shown for them.
    let mut rt = node_rooms(|_| {});
    dialog_of(&mut rt, "Maya Chen");
    assert!(rt.find("member-deputize").is_none());
    assert!(rt.find("member-revoke-deputy").is_none());
}

#[test]
fn a_change_on_its_way_waits_and_a_failed_one_says_why_with_retry_and_discard() {
    let mut rt = as_owner(|room| {
        let uid = room
            .members
            .iter()
            .find(|m| m.nickname == "Jonas")
            .unwrap()
            .uid
            .clone();
        room.members
            .iter_mut()
            .find(|m| m.uid == uid)
            .unwrap()
            .my_deputy = true;
        room.deputy_changes.push(super::model::DeputyChange {
            uid,
            grant: true,
            pending: super::model::Pending {
                entry: 3,
                failed: None,
                retry: false,
            },
        });
    });
    dialog_of(&mut rt, "Jonas");
    assert!(shows(&rt, "Sending"));
    press(&mut rt, "member-revoke-deputy");
    assert!(
        rt.find("member-deputy-confirm").is_none(),
        "one change at a time"
    );
    assert!(sent(&mut rt).is_empty());
    // Failed: why, Retry and Discard; the button is usable again.
    rt.state.chat.edit_rooms(|r| {
        let c = &mut r[0].deputy_changes[0];
        c.pending.failed = Some("Not saved. Freenet refused it: no".into());
        c.pending.retry = true;
        for m in &mut r[0].members {
            m.my_deputy = false;
        }
    });
    redraw(&mut rt);
    assert!(shows(&rt, "Not saved. Freenet refused it: no"));
    press(&mut rt, "deputy-retry");
    press(&mut rt, "deputy-discard");
    assert_eq!(
        sent(&mut rt),
        [Action::Retry { entry: 3 }, Action::Discard { entry: 3 }]
    );
    press(&mut rt, "member-deputize");
    assert!(rt.find("member-deputy-confirm").is_some());
}

#[test]
fn what_the_room_would_not_take_is_said_and_the_button_is_off() {
    let mut rt = as_owner(|room| room.deputy_block = Some("The room says no.".into()));
    dialog_of(&mut rt, "Jonas");
    assert!(shows(&rt, "The room says no."));
    press(&mut rt, "member-deputize");
    assert!(rt.find("member-deputy-confirm").is_none());
    assert!(sent(&mut rt).is_empty());

    // Full: Deputize is off and says so, Revoke is not.
    let mut rt = as_owner(|room| room.deputies_full = true);
    dialog_of(&mut rt, "Jonas");
    assert!(shows(&rt, "most deputies a member can have"));
    press(&mut rt, "member-deputize");
    assert!(rt.find("member-deputy-confirm").is_none());
}

#[test]
fn a_confirmation_does_not_outlive_its_dialog() {
    let mut rt = as_owner(|_| {});
    dialog_of(&mut rt, "Jonas");
    press(&mut rt, "member-deputize");
    assert!(rt.state.chat.deputy_confirm.is_some());
    key(&mut rt, Key::Escape);
    assert!(rt.state.chat.deputy_confirm.is_none());
    dialog_of(&mut rt, "Jonas");
    assert!(rt.find("member-deputy-confirm").is_none());
    // A confirmation made stale (the member is already a deputy by another device) sends nothing.
    press(&mut rt, "member-deputize");
    rt.state.chat.edit_rooms(|r| {
        for m in &mut r[0].members {
            if m.nickname == "Jonas" {
                m.my_deputy = true;
            }
        }
    });
    rt.state.chat.confirm_deputy();
    assert!(sent(&mut rt).is_empty());
}

#[test]
fn in_the_example_rooms_deputizing_changes_only_the_screen() {
    let mut rt = render(connected(), WINDOW_SIZE);
    dialog_of(&mut rt, "Elena Petrova");
    press(&mut rt, "member-revoke-deputy");
    press(&mut rt, "member-deputy-confirm");
    assert!(sent(&mut rt).is_empty());
    let room = rt.state.chat.room().unwrap();
    assert!(
        !room
            .member_by_uid(&uid_of(&rt, "Elena Petrova"))
            .unwrap()
            .my_deputy
    );
}

/// Whether the control keyed `key` is drawn dimmed, as a disabled one is.
fn dimmed(rt: &Runtime<RiverApp>, key: &str) -> bool {
    rt.elements()
        .into_iter()
        .any(|e| e.key.as_deref() == Some(key) && e.opacity < 0.9)
}

#[test]
fn the_offered_buttons_are_usable_and_the_blocked_ones_are_dimmed() {
    let mut rt = as_owner(|_| {});
    dialog_of(&mut rt, "Jonas");
    assert!(!dimmed(&rt, "member-deputize"));
    let mut rt = as_owner(|r| r.deputy_block = Some("no".into()));
    dialog_of(&mut rt, "Jonas");
    assert!(dimmed(&rt, "member-deputize"));
    // A revoke is not held back by what stops a grant.
    let mut rt = node_rooms(|rooms| {
        rooms[0].deputy_grant_block = Some("no nickname".into());
    });
    dialog_of(&mut rt, "Elena Petrova");
    assert!(!dimmed(&rt, "member-revoke-deputy"));
    // Nor is a grant held back by nothing: only by its own block.
    let mut rt = as_owner(|r| r.deputy_grant_block = Some("no nickname".into()));
    dialog_of(&mut rt, "Jonas");
    assert!(dimmed(&rt, "member-deputize"));
    assert!(shows(&rt, "no nickname"));
}

#[test]
fn deputizing_in_a_private_room_sends_to_that_room() {
    let mut rt = node_rooms(|_| {});
    rt.update(|s, _| s.chat.select(PRIVATE_ROOM));
    settle(&mut rt);
    // Sam owns Design Crew.
    dialog_of(&mut rt, "Priya Raman");
    assert!(!dimmed(&rt, "member-deputize"));
    press(&mut rt, "member-deputize");
    press(&mut rt, "member-deputy-confirm");
    let sent: Vec<_> = std::mem::take(&mut rt.state.chat.outgoing);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].room, PRIVATE_ROOM);
    assert_eq!(
        sent[0].action,
        Action::SetDeputy {
            target: uid_of(&rt, "Priya Raman"),
            grant: true
        }
    );
}

// Leaving a room -----------------------------------------------------------------------------------

use crate::rooms::{Action as RoomAction, Command as RoomCommand};

/// The dialog of the room `room` from the node, open.
fn details_open(room: &str) -> Runtime<RiverApp> {
    let mut rt = render(from_node(room), WINDOW_SIZE);
    press(&mut rt, "room-title");
    assert!(rt.find("room-details-dialog").is_some());
    rt
}

/// An owner's room where the viewer is offered Ban for Jonas.
fn ban_offered(offer: super::model::BanOffer) -> Runtime<RiverApp> {
    let mut rt = as_owner(|_| {});
    let jonas = uid_of(&rt, "Jonas");
    rt.state.chat.edit_rooms(|r| {
        r[0].ban_offers.insert(jonas, offer);
    });
    dialog_of(&mut rt, "Jonas");
    rt
}

#[test]
fn the_details_dialog_offers_leave_room_to_a_member_and_to_the_owner() {
    for room in ["town-square", OWNED] {
        let rt = details_open(room);
        assert!(rt.find("details-leave").is_some(), "{room}");
        assert!(shows(&rt, "Leave room"));
        // Nothing is asked until it is pressed.
        assert!(rt.find("details-leave-confirm").is_none());
    }
}

#[test]
fn pressing_leave_room_asks_with_the_web_uis_wording_and_sends_nothing() {
    let mut rt = details_open("town-square");
    press(&mut rt, "details-leave");
    assert!(details(&rt).leaving);
    assert!(shows(
        &rt,
        "Are you sure you want to leave this room? This action cannot be undone."
    ));
    // The signing key going with the room is said.
    assert!(shows(&rt, "signing key"));
    assert!(rt.find("details-leave-confirm").is_some());
    assert!(rt.find("details-leave-cancel").is_some());
    // The save controls are out of the way while asking.
    assert!(rt.find("details-leave").is_none());
    assert!(rt.state.chat.outgoing.is_empty());
}

#[test]
fn the_owner_is_warned_that_leaving_deletes_the_room_for_them() {
    let mut rt = details_open(OWNED);
    press(&mut rt, "details-leave");
    assert!(shows(
        &rt,
        "Warning: You are the owner of this room. Leaving will permanently delete it for you."
    ));
    assert!(shows(
        &rt,
        "Other members might retain access if they have the contract key, but coordination will be lost."
    ));
    assert!(shows(&rt, "signing key"));
}

#[test]
fn cancelling_leave_changes_nothing() {
    let mut rt = details_open("town-square");
    let before = rt.state.chat.rooms().len();
    press(&mut rt, "details-leave");
    press(&mut rt, "details-leave-cancel");
    assert!(!details(&rt).leaving);
    assert!(rt.find("details-leave").is_some());
    assert!(rt.find("details-leave-confirm").is_none());
    assert!(rt.state.chat.outgoing.is_empty());
    assert_eq!(rt.state.chat.rooms().len(), before);
    // Escape out of the question closes the dialog, and the next time it asks again.
    press(&mut rt, "details-leave");
    key(&mut rt, Key::Escape);
    assert!(rt.state.chat.details.is_none());
    assert!(rt.state.chat.outgoing.is_empty());
    press(&mut rt, "room-title");
    assert!(!details(&rt).leaving);
}

#[test]
fn confirming_leave_sends_it_closes_the_dialogs_and_the_next_room_opens_when_the_list_drops_it() {
    for room in ["town-square", OWNED] {
        let mut rt = details_open(room);
        // Unsent text in the room goes with it.
        rt.state.chat.composer.replace_text("half a thought");
        redraw_now(&mut rt);
        press(&mut rt, "details-leave");
        assert!(details(&rt).leaving, "{room}: asking");
        press(&mut rt, "details-leave-confirm");
        assert_eq!(
            std::mem::take(&mut rt.state.chat.outgoing),
            vec![RoomCommand {
                room: room.to_owned(),
                action: RoomAction::LeaveRoom
            }]
        );
        assert!(rt.state.chat.details.is_none());
        assert!(rt.find("room-details-dialog").is_none());
        assert!(rt.state.chat.composer.text.is_empty());
        // The node side no longer lists it: another room opens, and it is not this one.
        let rest: Vec<super::Room> = rt
            .state
            .chat
            .rooms()
            .iter()
            .filter(|r| r.key != room)
            .cloned()
            .collect();
        rt.state
            .chat
            .show_node_rooms(rest, None, LinkStatus::Live, 29_000_000);
        redraw_now(&mut rt);
        let now = rt.state.chat.room().expect("another room is open");
        assert_ne!(now.key, room);
        assert!(rt.state.chat.rooms().iter().all(|r| r.key != room));
        // Its half-written text did not follow to the next room.
        assert!(rt.state.chat.composer.text.is_empty());
    }
}

#[test]
fn an_owner_whose_details_are_blocked_is_still_warned_as_the_owner() {
    let mut s = from_node(OWNED);
    s.chat.edit_rooms(|rooms| {
        if let Some(r) = rooms.iter_mut().find(|r| r.key == OWNED) {
            r.details.editable = super::model::Editable::Blocked("no secret yet".into());
        }
    });
    let mut rt = render(s, WINDOW_SIZE);
    press(&mut rt, "room-title");
    press(&mut rt, "details-leave");
    assert!(shows(&rt, "You are the owner of this room"));
    assert!(shows(&rt, "owner cannot get ownership back"));
}

#[test]
fn confirming_leave_twice_asks_the_node_once() {
    let mut rt = details_open("town-square");
    press(&mut rt, "details-leave");
    press(&mut rt, "details-leave-confirm");
    // The dialog is closed: a second confirm has nothing to confirm.
    rt.state.chat.confirm_leave();
    assert_eq!(rt.state.chat.outgoing.len(), 1);
}

#[test]
fn leaving_an_example_room_just_removes_it() {
    let mut s = connected();
    s.chat.select("town-square");
    s.chat.open_details();
    s.chat.ask_leave();
    s.chat.confirm_leave();
    assert!(s.chat.outgoing.is_empty(), "nothing goes to a node");
    assert!(s.chat.rooms().iter().all(|r| r.key != "town-square"));
    assert!(s.chat.room().is_some());
}

/// The room answers what banning `uid` does to its other bans.
fn answer_preview(rt: &mut Runtime<RiverApp>, uid: &str, p: super::model::BanPreview) {
    rt.state.chat.edit_rooms(|r| {
        r[0].ban_preview.insert(uid.to_owned(), p);
    });
    redraw(rt);
}

#[test]
fn ban_is_offered_where_the_room_offers_it_asks_first_and_sends_one_command() {
    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    assert!(rt.find("member-ban").is_some());
    assert!(!dimmed(&rt, "member-ban"));
    assert_ne!(
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("member-ban"))
            .unwrap()
            .help
            .as_deref(),
        Some(super::view::NOT_YET)
    );
    press(&mut rt, "member-ban");
    // The web UI's words, and nothing has been sent.
    assert!(shows(&rt, "Confirm ban"));
    assert!(shows(&rt, "Are you sure you want to ban Jonas"));
    assert!(shows(&rt, "This action cannot be undone."));
    // Asking asks the room what the ban does; nothing is banned, and Confirm waits for the answer.
    let jonas = uid_of(&rt, "Jonas");
    assert_eq!(
        sent_all(&mut rt),
        [Action::PreviewBan {
            target: jonas.clone()
        }]
    );
    assert!(dimmed(&rt, "member-ban-confirm-yes"));
    press(&mut rt, "member-ban-confirm-yes");
    assert!(sent(&mut rt).is_empty(), "not before the room has answered");
    answer_preview(&mut rt, &jonas, super::model::BanPreview::default());
    assert!(!dimmed(&rt, "member-ban-confirm-yes"));
    // Cancel backs out.
    press(&mut rt, "member-ban-cancel");
    assert!(rt.find("member-ban-confirm").is_none());
    assert!(sent(&mut rt).is_empty());
    press(&mut rt, "member-ban");
    sent(&mut rt);
    answer_preview(&mut rt, &jonas, super::model::BanPreview::default());
    press(&mut rt, "member-ban-confirm-yes");
    assert_eq!(sent(&mut rt), [Action::Ban { target: jonas }]);
    // The dialog closes, as the web UI's does.
    assert!(rt.state.chat.member_modal.is_none());
    assert!(rt.state.chat.ban_confirm.is_none());
}

#[test]
fn a_private_room_ban_says_the_secret_changes() {
    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    rt.state.chat.edit_rooms(|r| r[0].private = true);
    redraw(&mut rt);
    press(&mut rt, "member-ban");
    assert!(shows(&rt, "also changes the room's secret"));
}

#[test]
fn a_ban_that_is_withheld_says_why_and_offers_no_button() {
    let rt = ban_offered(super::model::BanOffer::Withheld("Not from here.".into()));
    assert!(rt.find("member-ban").is_none());
    assert!(shows(&rt, "Not from here."));
}

#[test]
fn no_offer_means_no_button_and_nothing_said() {
    let mut rt = as_owner(|_| {});
    dialog_of(&mut rt, "Jonas");
    assert!(rt.find("member-ban").is_none());
    assert!(rt.find("member-ban-withheld").is_none());
}

#[test]
fn a_ban_confirmation_does_not_outlive_its_dialog_or_its_offer() {
    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    press(&mut rt, "member-ban");
    assert!(rt.state.chat.ban_confirm.is_some());
    key(&mut rt, Key::Escape);
    assert!(rt.state.chat.ban_confirm.is_none());
    dialog_of(&mut rt, "Jonas");
    assert!(rt.find("member-ban-confirm").is_none());
    // The room stopped offering it (the member left, or authority changed): nothing is sent.
    press(&mut rt, "member-ban");
    sent(&mut rt);
    rt.state.chat.edit_rooms(|r| r[0].ban_offers.clear());
    rt.state.chat.confirm_ban();
    assert!(sent(&mut rt).is_empty());
}

#[test]
fn the_confirmation_says_how_many_invitees_go_with_the_member() {
    let mut rt = as_owner(|_| {});
    let jonas = uid_of(&rt, "Jonas");
    rt.state.chat.edit_rooms(|r| {
        r[0].ban_offers
            .insert(jonas.clone(), super::model::BanOffer::Allowed);
        let jid = r[0].member_by_uid(&jonas).unwrap().id;
        for m in &mut r[0].members {
            m.invited_by = None;
        }
        let other = r[0]
            .members
            .iter()
            .position(|m| m.id != jid && m.id != r[0].me)
            .unwrap();
        r[0].members[other].invited_by = Some(jid);
    });
    dialog_of(&mut rt, "Jonas");
    press(&mut rt, "member-ban");
    assert!(shows(&rt, "also removes 1 member they invited"));
}

#[test]
fn the_owner_of_a_private_room_can_rotate_its_secret_and_nobody_else() {
    // Sam owns Design Crew (private).
    let mut rt = render(from_node(PRIVATE_ROOM), WINDOW_SIZE);
    press(&mut rt, "room-title");
    assert!(rt.find("details-rotate").is_some());
    assert!(!dimmed(&rt, "details-rotate"));
    press(&mut rt, "details-rotate");
    assert_eq!(sent(&mut rt), [Action::RotateSecret]);
    // One at a time: with one on its way the button does nothing more.
    rt.state.chat.edit_rooms(|r| {
        for room in r.iter_mut().filter(|r| r.key == PRIVATE_ROOM) {
            room.details.rotation = Some(super::model::Pending {
                entry: 1,
                failed: None,
                retry: false,
            });
        }
    });
    press(&mut rt, "details-secret");
    assert!(dimmed(&rt, "details-rotate"));
    press(&mut rt, "details-rotate");
    assert!(sent(&mut rt).is_empty());
    assert!(shows(&rt, "Rotating the secret"));
    // Failed: why, Retry and Discard.
    rt.state.chat.edit_rooms(|r| {
        for room in r.iter_mut().filter(|r| r.key == PRIVATE_ROOM) {
            room.details.rotation = Some(super::model::Pending {
                entry: 7,
                failed: Some("Not saved. Freenet refused it: no".into()),
                retry: true,
            });
        }
    });
    press(&mut rt, "details-secret");
    assert!(shows(&rt, "Not saved. Freenet refused it: no"));
    press(&mut rt, "rotation-retry");
    press(&mut rt, "rotation-discard");
    assert_eq!(
        sent(&mut rt),
        [Action::Retry { entry: 7 }, Action::Discard { entry: 7 }]
    );
}

#[test]
fn rotate_is_absent_for_members_and_public_rooms_and_dimmed_without_the_key() {
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    press(&mut rt, "room-title");
    assert!(rt.find("details-rotate").is_none());
    let mut rt = node_rooms(|rooms| {
        for r in rooms.iter_mut().filter(|r| r.key == PRIVATE_ROOM) {
            r.details.rotate = super::model::RotateOffer::No;
        }
    });
    rt.update(|s, _| s.chat.select(PRIVATE_ROOM));
    settle(&mut rt);
    press(&mut rt, "room-title");
    assert!(rt.find("details-rotate").is_none());
    let mut rt = node_rooms(|rooms| {
        for r in rooms.iter_mut().filter(|r| r.key == PRIVATE_ROOM) {
            r.details.rotate = super::model::RotateOffer::Blocked("no key".into());
        }
    });
    rt.update(|s, _| s.chat.select(PRIVATE_ROOM));
    settle(&mut rt);
    press(&mut rt, "room-title");
    assert!(dimmed(&rt, "details-rotate"));
    press(&mut rt, "details-rotate");
    assert!(sent(&mut rt).is_empty());
}

#[test]
fn the_confirmation_says_whose_ban_the_room_would_lift_and_refuses_one_it_would_drop() {
    let mut rt = as_owner(|_| {});
    let jonas = uid_of(&rt, "Jonas");
    rt.state.chat.edit_rooms(|r| {
        r[0].ban_offers
            .insert(jonas.clone(), super::model::BanOffer::Allowed);
        r[0].details.limits.max_user_bans = 3;
    });
    dialog_of(&mut rt, "Jonas");
    press(&mut rt, "member-ban");
    assert!(shows(&rt, "Checking what this ban does"));
    answer_preview(
        &mut rt,
        &jonas,
        super::model::BanPreview {
            lifts: vec!["Maya".into()],
            refused: false,
        },
    );
    assert!(shows(
        &rt,
        "This will lift the ban on Maya because the room keeps at most 3 bans."
    ));
    // The ban is still allowed.
    sent(&mut rt);
    press(&mut rt, "member-ban-confirm-yes");
    assert_eq!(sent(&mut rt).len(), 1);
    // One the room would not keep is said and cannot be confirmed.
    dialog_of(&mut rt, "Jonas");
    rt.state.chat.edit_rooms(|r| {
        r[0].ban_offers
            .insert(jonas.clone(), super::model::BanOffer::Allowed);
    });
    press(&mut rt, "member-ban");
    sent(&mut rt);
    answer_preview(
        &mut rt,
        &jonas,
        super::model::BanPreview {
            lifts: Vec::new(),
            refused: true,
        },
    );
    assert!(shows(&rt, "it would not keep this ban"));
    assert!(dimmed(&rt, "member-ban-confirm-yes"));
    press(&mut rt, "member-ban-confirm-yes");
    assert!(sent(&mut rt).is_empty());
}

#[test]
fn a_preview_that_changes_before_confirming_cannot_be_confirmed_by_a_double_press() {
    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    let jonas = uid_of(&rt, "Jonas");
    press(&mut rt, "member-ban");
    sent(&mut rt);
    answer_preview(&mut rt, &jonas, super::model::BanPreview::default());
    // The room's bans change: now this ban would lift someone's.
    answer_preview(
        &mut rt,
        &jonas,
        super::model::BanPreview {
            lifts: vec!["Maya".into()],
            refused: false,
        },
    );
    assert!(shows(&rt, "This changed while you were looking"));
    assert!(shows(&rt, "lift the ban on Maya"));
    let since = rt.state.chat.ban_changed.expect("flagged");
    // A double click, or a held key, within the reading time sends nothing.
    for ms in [0, 30, 120, 399] {
        rt.state
            .chat
            .confirm_ban_at(since + std::time::Duration::from_millis(ms));
    }
    assert!(sent(&mut rt).is_empty(), "shown again, not sent");
    assert!(rt.state.chat.ban_confirm.is_some());
    // A press after the new text has been on screen long enough confirms it.
    rt.state
        .chat
        .confirm_ban_at(since + std::time::Duration::from_millis(450));
    assert_eq!(sent(&mut rt), [Action::Ban { target: jonas }]);
}

#[test]
fn closing_a_ban_confirmation_tells_the_room_to_stop_working_out_its_effect() {
    // Cancel.
    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    press(&mut rt, "member-ban");
    sent_all(&mut rt);
    press(&mut rt, "member-ban-cancel");
    assert_eq!(
        sent_all(&mut rt),
        [Action::PreviewBan {
            target: String::new()
        }]
    );
    // Closing the dialog, switching room, and leaving each do the same.
    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    press(&mut rt, "member-ban");
    sent_all(&mut rt);
    rt.state.chat.close_member();
    assert!(
        sent_all(&mut rt)
            .iter()
            .any(|a| matches!(a, Action::PreviewBan { target } if target.is_empty()))
    );
    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    press(&mut rt, "member-ban");
    sent_all(&mut rt);
    rt.update(|s, _| s.chat.select(PRIVATE_ROOM));
    assert!(
        sent_all(&mut rt)
            .iter()
            .any(|a| matches!(a, Action::PreviewBan { target } if target.is_empty()))
    );
    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    press(&mut rt, "member-ban");
    sent_all(&mut rt);
    rt.state.chat.set_rooms(Vec::new());
    assert!(
        sent_all(&mut rt)
            .iter()
            .any(|a| matches!(a, Action::PreviewBan { target } if target.is_empty()))
    );
}

#[test]
fn a_ban_confirmation_is_cleared_whenever_the_dialog_goes() {
    // Changing room, opening the details, and the room losing the member each close the
    // dialog; none may leave a confirmation to come back with the next dialog.
    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    press(&mut rt, "member-ban");
    assert!(rt.state.chat.ban_confirm.is_some());
    rt.state.chat.open_details();
    assert!(rt.state.chat.member_modal.is_none());
    assert!(rt.state.chat.ban_confirm.is_none());

    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    press(&mut rt, "member-ban");
    rt.update(|s, _| s.chat.select(PRIVATE_ROOM));
    assert!(rt.state.chat.ban_confirm.is_none());

    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    press(&mut rt, "member-ban");
    let jonas = uid_of(&rt, "Jonas");
    rt.state
        .chat
        .edit_rooms(|r| r[0].members.retain(|m| m.uid != jonas));
    assert!(rt.state.chat.member_modal.is_none());
    assert!(rt.state.chat.ban_confirm.is_none());

    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    press(&mut rt, "member-ban");
    rt.state.chat.set_rooms(Vec::new());
    assert!(rt.state.chat.ban_confirm.is_none());
}

#[test]
fn rotate_sits_beside_leave_and_does_nothing_while_leaving_is_being_confirmed() {
    let mut rt = render(from_node(PRIVATE_ROOM), WINDOW_SIZE);
    press(&mut rt, "room-title");
    assert!(rt.find("details-rotate").is_some());
    assert!(rt.find("details-leave").is_some());
    press(&mut rt, "details-leave");
    assert!(dimmed(&rt, "details-rotate"));
    press(&mut rt, "details-rotate");
    assert!(sent(&mut rt).is_empty());
    rt.state.chat.rotate_secret();
    assert!(sent(&mut rt).is_empty());
}

#[test]
fn a_ban_dialog_open_when_the_room_is_left_goes_with_it() {
    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    press(&mut rt, "member-ban");
    sent(&mut rt);
    assert!(rt.state.chat.ban_confirm.is_some());
    // The list no longer has the room (left, here or in another window).
    let key = rt.state.chat.room().unwrap().key.clone();
    rt.state
        .chat
        .edit_rooms(|r| r.retain(|room| room.key != key));
    assert!(rt.state.chat.member_modal.is_none());
    assert!(rt.state.chat.ban_confirm.is_none());
    rt.state.chat.confirm_ban();
    assert!(sent(&mut rt).is_empty());
}

/// Probe: the composer is a Rune editor, so an input method's events reach it. A pre-edit
/// (the underlined text before it is chosen) is not in the text; the commit is. This is the
/// layer River controls; whether the operating system's input method (ibus, fcitx, the macOS
/// and Windows ones) sends these events to a Rune window is Rune's winit layer, which needs
/// a real input method to test.
#[test]
fn an_input_method_can_compose_into_the_composer() {
    let mut rt = render(connected(), WINDOW_SIZE);
    press(&mut rt, "composer-field");
    rt.ime(rune::Ime::Enabled);
    rt.ime(rune::Ime::Preedit("かん".into(), Some((0, 6)), Vec::new()));
    assert_eq!(
        rt.state.chat.composer.text, "",
        "a pre-edit is not yet text"
    );
    rt.ime(rune::Ime::Preedit(String::new(), None, Vec::new()));
    rt.ime(rune::Ime::Commit("漢字".into()));
    rt.frame(0.3);
    assert_eq!(rt.state.chat.composer.text, "漢字");
    // A dead key (é from ' then e) arrives as a commit too.
    rt.ime(rune::Ime::Commit("é".into()));
    rt.frame(0.4);
    assert_eq!(rt.state.chat.composer.text, "漢字é");
}

// Notification actions ------------------------------------------------------------------------

/// Rooms where the second one has a long history, so its oldest message starts far above the
/// newest.
fn with_long_history(rooms: &mut [super::Room]) {
    let base = rooms[1].messages.clone();
    let mut long = Vec::new();
    for i in 0..40 {
        for m in &base {
            let mut m = m.clone();
            m.key = format!("{}-{i}", m.key);
            long.push(m);
        }
    }
    rooms[1].messages = long;
}

fn room_keys(rt: &Runtime<RiverApp>) -> Vec<String> {
    rt.state
        .chat
        .rooms()
        .iter()
        .map(|r| r.key.clone())
        .collect()
}

#[test]
fn a_click_on_a_notification_opens_its_room_and_shows_the_message_in_one_update() {
    let mut rt = node_rooms(|r| with_long_history(r));
    let keys = room_keys(&rt);
    let (a, b) = (keys[0].clone(), keys[1].clone());
    assert_eq!(rt.state.chat.room().unwrap().key, a);
    let oldest = rt.state.chat.rooms()[1].messages[0].key.clone();
    let el = format!("msg-{oldest}");
    // Control: opening room B alone leaves its oldest message off screen, so the check below
    // tells a reveal from a room that is short.
    let plain = b.clone();
    rt.update(move |s, _| {
        s.open_room(&plain);
    });
    settle(&mut rt);
    assert!(!visible(&rt, &el), "room too short for the test");
    // From room A, in ONE update, as the notification's click does.
    let back = a.clone();
    rt.update(move |s, _| {
        s.open_room(&back);
    });
    settle(&mut rt);
    let (room, message) = (b.clone(), oldest.clone());
    rt.update(move |s, _| {
        assert!(s.open_message(&room, &message));
    });
    assert!(rt.state.chat.pending_reveal().is_some());
    // What the per-frame sync does (the test runtime has no poll hook of its own).
    for _ in 0..10 {
        let mut again = false;
        rt.update(|s, tx| again = s.chat.apply_reveal(tx));
        settle(&mut rt);
        if !again {
            break;
        }
    }
    assert_eq!(rt.state.chat.room().unwrap().key, b);
    assert!(visible(&rt, &el), "the message is brought into view");
}

#[test]
fn a_click_on_a_room_that_was_left_or_a_message_that_is_gone() {
    let mut rt = node_rooms(|_| {});
    let b = room_keys(&rt)[1].clone();
    let open = rt.state.chat.room().unwrap().key.clone();
    assert!(!rt.state.open_message("no-such-room", "m"), "room gone");
    assert_eq!(
        rt.state.chat.room().unwrap().key,
        open,
        "nothing else opens"
    );
    assert!(rt.state.open_message(&b, "no-such-message"));
    assert_eq!(rt.state.chat.room().unwrap().key, b);
    assert!(rt.state.chat.pending_reveal().is_none());
}

#[test]
fn a_reply_from_a_notification_sends_what_the_composer_would_and_leaves_the_screen_alone() {
    let mut rt = node_rooms(|_| {});
    let keys = room_keys(&rt);
    rt.state.chat.composer.replace_text("half typed");
    let result = rt
        .state
        .reply_from_notification(&keys[1], "  on my way\u{202e}  ");
    assert_eq!(result, Ok(()));
    assert_eq!(
        std::mem::take(&mut rt.state.chat.outgoing),
        [Command {
            room: keys[1].clone(),
            action: Action::Send {
                text: "on my way".into(),
                reply_to: None
            }
        }]
    );
    assert_eq!(
        rt.state.chat.room().unwrap().key,
        keys[0],
        "the open room stays"
    );
    assert_eq!(rt.state.chat.composer.text, "half typed");
    assert!(rt.state.chat.reply_notices(&keys[1]).is_empty());
}

#[test]
fn a_refused_reply_is_kept_as_that_rooms_draft_and_touches_nothing_else() {
    use super::ReplyRefusal;
    let reason = "You have been banned from sending messages in this room.";
    let mut rt = node_rooms(|rooms| {
        rooms[1].details.limits.max_message_size = 40;
        rooms[2].send_block = Some(reason.into());
    });
    let keys = room_keys(&rt);
    let open = rt.state.chat.room().unwrap().key.clone();
    rt.state.chat.composer.replace_text("typing here");
    let state = &mut rt.state;
    assert_eq!(
        state.reply_from_notification(&keys[1], " \n "),
        Err(ReplyRefusal::Empty)
    );
    assert!(
        state.chat.reply_notices(&keys[1]).is_empty(),
        "blank text needs no word"
    );
    // A room that was left: nothing to keep it in, and the line says so (in any room).
    assert_eq!(
        state.reply_from_notification("nope", "hi"),
        Err(ReplyRefusal::NoRoom)
    );
    assert!(
        state.chat.reply_notices(&open)[0]
            .1
            .contains("could not be kept")
    );
    // A blocked room with a draft: the text follows it, in the stored draft.
    state
        .chat
        .drafts
        .insert(keys[2].clone(), rune::TextEdit::new("earlier"));
    assert_eq!(
        state.reply_from_notification(&keys[2], "hi"),
        Err(ReplyRefusal::Blocked)
    );
    assert_eq!(state.chat.drafts[&keys[2]].text, "earlier\nhi");
    // Too long for another room: kept in its draft; the first room's line is still there.
    let long = "x".repeat(200);
    assert_eq!(
        state.reply_from_notification(&keys[1], &long),
        Err(ReplyRefusal::TooLong)
    );
    assert_eq!(state.chat.drafts[&keys[1]].text, long);
    assert!(
        state.chat.reply_notices(&keys[2])[0]
            .1
            .contains("cannot send")
    );
    assert!(state.chat.reply_notices(&keys[1])[0].1.contains("too long"));
    assert!(
        state.chat.reply_notices(&keys[1])[0]
            .1
            .contains(&state.chat.rooms()[1].name)
    );
    // Nothing else moved: same open room, same composer, nothing sent.
    assert_eq!(state.chat.room().unwrap().key, open);
    assert_eq!(state.chat.composer.text, "typing here");
    assert!(state.chat.outgoing.is_empty());
    // A reply that works in one room does not clear another room's line.
    assert_eq!(state.reply_from_notification(&keys[0], "ok"), Ok(()));
    assert!(!state.chat.reply_notices(&keys[1]).is_empty());
    state.chat.dismiss_reply_notice(&keys[1]);
    assert!(
        state
            .chat
            .reply_notices(&keys[1])
            .iter()
            .all(|(k, _)| *k != keys[1])
    );
    // The draft is there when the room is opened.
    state.open_room(&keys[1]);
    assert_eq!(state.chat.composer.text, long);
}

#[test]
fn a_refused_reply_to_the_open_room_survives_leaving_it_and_never_joins_a_reply_in_progress() {
    let mut rt = node_rooms(|rooms| rooms[0].details.limits.max_message_size = 40);
    let keys = room_keys(&rt);
    let target = message_of(&rt, false);
    rt.state.chat.start_reply(&target);
    rt.state.chat.composer.replace_text("mine");
    assert!(
        rt.state
            .reply_from_notification(&keys[0], &"y".repeat(100))
            .is_err()
    );
    let said = rt.state.chat.reply_notices(&keys[0])[0].1.to_owned();
    assert!(said.contains("leave this room and come back"), "{said}");
    // The reply in progress and its text are untouched.
    assert!(matches!(rt.state.chat.mode, super::Mode::Reply(_)));
    assert_eq!(rt.state.chat.composer.text, "mine");
    // Leaving the room keeps both: what was typed, then the refused text.
    rt.state.open_room(&keys[1]);
    assert_eq!(
        rt.state.chat.drafts[&keys[0]].text,
        format!("mine\n{}", "y".repeat(100))
    );
}

#[test]
fn a_draft_is_never_grown_past_its_cap() {
    let mut rt = node_rooms(|rooms| rooms[1].details.limits.max_message_size = 40);
    let keys = room_keys(&rt);
    let big = "z".repeat(super::quick_reply::MAX_DRAFT_CHARS - 10);
    rt.state
        .chat
        .drafts
        .insert(keys[1].clone(), rune::TextEdit::new(big.clone()));
    assert!(
        rt.state
            .reply_from_notification(&keys[1], &"x".repeat(100))
            .is_err()
    );
    assert_eq!(rt.state.chat.drafts[&keys[1]].text, big, "not grown");
    assert!(
        rt.state.chat.reply_notices(&keys[1])[0]
            .1
            .contains("draft is full")
    );
}

#[test]
fn a_click_when_the_window_is_not_in_front_waits_a_frame_even_for_the_open_room() {
    let mut rt = node_rooms(|r| with_long_history(r));
    let b = room_keys(&rt)[1].clone();
    let oldest = rt.state.chat.rooms()[1].messages[0].key.clone();
    rt.state.open_room(&b);
    settle(&mut rt);
    // In front: as the overview does, the scroll goes out on the first apply.
    rt.state.focus_changed(true);
    assert!(rt.state.open_message(&b, &oldest));
    let mut again = true;
    rt.update(|s, tx| again = s.chat.apply_reveal(tx));
    assert!(!again, "focused: no wait");
    // Not in front (hidden, minimised, another window): it waits one frame first.
    rt.state.focus_changed(false);
    assert!(rt.state.open_message(&b, &oldest));
    rt.update(|s, tx| again = s.chat.apply_reveal(tx));
    assert!(again, "unfocused: waits");
    rt.update(|s, tx| again = s.chat.apply_reveal(tx));
    assert!(!again);
}

#[test]
fn an_example_room_takes_no_reply_from_a_notification() {
    let mut rt = render(connected(), WINDOW_SIZE);
    let key = rt.state.chat.room().unwrap().key.clone();
    assert_eq!(
        rt.state.reply_from_notification(&key, "hi"),
        Err(super::ReplyRefusal::NoRoom)
    );
    assert!(rt.state.chat.outgoing.is_empty());
}

#[test]
fn mark_as_read_reads_only_up_to_the_notifications_message() {
    let mut rt = node_rooms(|rooms| rooms[1].unread = 0);
    let b = room_keys(&rt)[1].clone();
    let me = rt.state.chat.rooms()[1].me;
    let theirs: Vec<(usize, String)> = rt.state.chat.rooms()[1]
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.author != me)
        .map(|(i, m)| (i, m.key.clone()))
        .collect();
    assert!(
        theirs.len() >= 3,
        "the example room has enough to tell apart"
    );
    // The notification showed the first of their messages; the rest arrived after it.
    let (shown_at, shown) = theirs[0].clone();
    let newer = rt.state.chat.rooms()[1].messages[shown_at + 1..]
        .iter()
        .filter(|m| m.author != me)
        .count();
    rt.state.chat.edit_rooms(|r| r[1].unread = newer + 1);
    rt.state.mark_read_through(&b, &shown);
    assert_eq!(
        rt.state.chat.rooms()[1].unread,
        newer,
        "what came after stays unread"
    );
    // The mark moved, and survives the node's next update of the rooms.
    assert!(rt.state.chat.read.contains_key(&b));
    let rooms = rt.state.chat.rooms().to_vec();
    rt.state
        .chat
        .show_node_rooms(rooms, None, super::LinkStatus::Live, 29_000_000);
    assert_eq!(rt.state.chat.rooms()[1].unread, newer);
    // A stale click on an older message never moves it back.
    let before = rt.state.chat.read[&b];
    let older = rt.state.chat.rooms()[1].messages[0].key.clone();
    rt.state.mark_read_through(&b, &older);
    assert_eq!(rt.state.chat.read[&b], before);
    assert_eq!(rt.state.chat.rooms()[1].unread, newer);
    // A message that is gone marks nothing.
    rt.state.mark_read_through(&b, "gone");
    assert_eq!(rt.state.chat.rooms()[1].unread, newer);
}

#[test]
fn reply_opens_the_room_with_the_composer_ready() {
    let mut rt = node_rooms(|_| {});
    let other = room_keys(&rt)[1].clone();
    rt.state.open_room_to_reply(&other);
    assert_eq!(rt.state.chat.room().unwrap().key, other);
    assert!(rt.state.chat.composer.focused);
}

#[test]
fn the_connection_dot_is_reachable_with_tab_and_enter() {
    let mut s = connected();
    s.chat.source = super::Source::Node(super::LinkStatus::Live);
    let mut rt = render(s, WINDOW_SIZE);
    assert!(!rt.state.chat.status.open);
    let mut reached = false;
    for _ in 0..80 {
        key(&mut rt, Key::Tab);
        if rt
            .elements()
            .into_iter()
            .any(|e| e.focused && e.path.contains("connection-pill"))
        {
            reached = true;
            break;
        }
    }
    assert!(reached, "Tab never reached the connection dot");
    key(&mut rt, Key::Enter);
    assert!(rt.state.chat.status.open, "Enter opens the node status");
    assert!(rt.find("node-status-dialog").is_some());
}

#[test]
fn node_status_stacks_with_the_overview_and_escape_leaves_both_alone() {
    let mut chat = super::Chat::new(
        super::Source::Node(super::LinkStatus::Live),
        Vec::new(),
        0,
        0,
        0,
    );
    chat.overview = true;
    chat.status.open();
    // A dialog is open: the window-level Escape does nothing, the dialog takes its own.
    assert!(!chat.escape());
    assert!(chat.overview);
    // The switcher must not open over it.
    assert!(chat.other_dialog_open());
    chat.close_node_status();
    assert!(chat.overview, "closing the status leaves the overview");
}

// The shared dialog scaffold -------------------------------------------------------------------

/// The dialogs built on the scaffold, as a scene opens them: the scene, the dialog's key, the
/// dismiss button (Cancel, Back or Close) and the primary or danger button when it has one.
const KIT_DIALOGS: &[(&str, &str, &str, Option<&str>)] = &[
    (
        "chat-join-invite",
        "join-room",
        "form-cancel",
        Some("form-submit"),
    ),
    (
        "chat-export-id",
        "export-identity",
        "export-close",
        Some("export-copy"),
    ),
    (
        "chat-import-id",
        "import-identity",
        "form-cancel",
        Some("form-submit"),
    ),
    (
        "chat-import-id-replace",
        "import-identity",
        "import-identity-replace-cancel",
        Some("import-identity-replace-confirm"),
    ),
    (
        "chat-node-status",
        "node-status",
        "ns-close",
        Some("ns-copy"),
    ),
    (
        "chat-node-change",
        "node",
        "node-cancel",
        Some("node-connect"),
    ),
    (
        "chat-node-change-confirm-remote",
        "node",
        "node-back",
        Some("node-confirm"),
    ),
    ("chat-dnd", "dnd", "dnd-close", None),
    ("chat-background", "background", "background-close", None),
    ("chat-shortcuts", "shortcuts", "shortcuts-close", None),
    (
        "chat-dm-purge-confirm",
        "dm-purge",
        "dm-purge-cancel",
        Some("dm-purge-confirm"),
    ),
    (
        "chat-dm-invite-picker",
        "dm-invite-picker",
        "dm-invite-cancel",
        Some("dm-invite-send"),
    ),
    (
        "chat-ban-confirm",
        "member-info",
        "member-ban-cancel",
        Some("member-ban-confirm-yes"),
    ),
    (
        "chat-deputize-confirm",
        "member-info",
        "member-deputy-cancel",
        Some("member-deputy-confirm"),
    ),
    (
        "chat-room-leave-member",
        "room-details",
        "details-leave-cancel",
        Some("details-leave-confirm"),
    ),
    (
        "chat-room-details-owner",
        "room-details",
        "details-cancel",
        Some("details-save"),
    ),
];

/// Dialogs with nothing to press: only the X and Escape close them.
const KIT_DIALOGS_NO_FOOTER: &[(&str, &str)] = &[
    ("chat-notification-modes", "notify"),
    ("chat-overview", "overview"),
    ("chat-switcher", "switcher"),
    ("chat-settings", "settings"),
];

fn scene_runtime(scene: &str) -> Runtime<RiverApp> {
    let (_, apply) = SCENES
        .iter()
        .find(|(name, _)| *name == scene)
        .unwrap_or_else(|| panic!("no scene {scene}"));
    let mut state = RiverApp::detached();
    apply(&mut state);
    render(state, scene_size(scene))
}

fn focused_key(rt: &Runtime<RiverApp>) -> Option<String> {
    rt.elements()
        .into_iter()
        .find(|e| e.focused)
        .and_then(|e| e.key)
}

/// Tab until the element keyed `want` has the keyboard.
fn tab_to(rt: &mut Runtime<RiverApp>, want: &str) -> bool {
    for _ in 0..80 {
        if focused_key(rt).as_deref() == Some(want) {
            return true;
        }
        key(rt, Key::Tab);
    }
    focused_key(rt).as_deref() == Some(want)
}

#[test]
fn escape_closes_every_scaffold_dialog() {
    let all = KIT_DIALOGS
        .iter()
        .map(|(scene, dialog, ..)| (*scene, *dialog))
        .chain(KIT_DIALOGS_NO_FOOTER.iter().copied());
    for (scene, dialog) in all {
        let mut rt = scene_runtime(scene);
        let panel = format!("{dialog}-dialog");
        assert!(rt.find(&panel).is_some(), "{scene} shows {panel}");
        key(&mut rt, Key::Escape);
        assert!(
            rt.find(&panel).is_none(),
            "Escape closes {panel} in {scene}"
        );
    }
}

#[test]
fn every_scaffold_dialog_puts_dismiss_left_of_the_primary_button() {
    for (scene, _, dismiss, primary) in KIT_DIALOGS {
        let rt = scene_runtime(scene);
        let (dx, dy) = rt
            .find(dismiss)
            .unwrap_or_else(|| panic!("{scene}: no {dismiss}"));
        let Some(primary) = primary else { continue };
        let (px, py) = rt
            .find(primary)
            .unwrap_or_else(|| panic!("{scene}: no {primary}"));
        assert!(dx < px, "{scene}: {dismiss} is left of {primary}");
        assert!((dy - py).abs() < 4.0, "{scene}: they share a row");
    }
}

#[test]
fn quit_river_is_apart_at_the_left_and_close_is_rightmost() {
    let rt = scene_runtime("chat-background");
    let (qx, _) = rt.find("background-quit").expect("Quit River");
    let (cx, _) = rt.find("background-close").expect("Close");
    let (bx, _) = rt.find("background-body").expect("the body");
    assert!(qx < cx, "Quit River is left of Close");
    assert!(qx < bx + 120.0, "Quit River sits at the left edge");
}

/// The dismiss button of every dialog is reachable with Tab, and pressing it closes the dialog.
#[test]
fn tab_reaches_each_dismiss_button_and_space_presses_it() {
    for (scene, dialog, dismiss, _) in KIT_DIALOGS {
        // Escape is tested above; a confirm inside a dialog only goes back a step.
        if ["member-info", "room-details"].contains(dialog)
            || ["chat-import-id-replace", "chat-node-change-confirm-remote"].contains(scene)
        {
            continue;
        }
        let mut rt = scene_runtime(scene);
        assert!(tab_to(&mut rt, dismiss), "{scene}: Tab reaches {dismiss}");
        key(&mut rt, Key::Text(" ".into()));
        let panel = format!("{dialog}-dialog");
        assert!(
            rt.find(&panel).is_none(),
            "{scene}: {dismiss} closes {panel}"
        );
    }
}

/// The primary action is in the Tab order and Space presses it, as every button.
#[test]
fn tab_reaches_the_primary_action_of_the_export_and_node_status_dialogs() {
    let mut rt = scene_runtime("chat-export-id");
    assert!(tab_to(&mut rt, "export-copy"));
    key(&mut rt, Key::Text(" ".into()));
    assert!(rt.state.chat.export.as_ref().is_some_and(|e| e.copied));

    let mut rt = scene_runtime("chat-node-status");
    assert!(tab_to(&mut rt, "ns-copy"));
    key(&mut rt, Key::Text(" ".into()));
    assert!(rt.state.chat.status.copied);

    let mut rt = scene_runtime("chat-import-id");
    assert!(tab_to(&mut rt, "form-submit"));
    key(&mut rt, Key::Text(" ".into()));
    assert!(
        rt.state.chat.form.error.is_some(),
        "Import with nothing pasted says why"
    );
}

#[test]
fn the_danger_buttons_are_in_the_tab_order() {
    for (scene, want) in [
        ("chat-import-id-replace", "import-identity-replace-confirm"),
        ("chat-dm-purge-confirm", "dm-purge-confirm"),
        ("chat-room-leave-member", "details-leave-confirm"),
        ("chat-ban-confirm", "member-ban-confirm-yes"),
    ] {
        let mut rt = scene_runtime(scene);
        assert!(tab_to(&mut rt, want), "{scene}: Tab reaches {want}");
    }
}

/// The Create room description is a wrapped line in the body, so a long one cannot run past
/// the dialog's right edge (it did, in one line under the title).
#[test]
fn the_create_room_description_stays_inside_the_dialog() {
    let mut rt = scene_runtime("chat-connection-live");
    press(&mut rt, "create-room");
    let rects = rt.elements();
    let panel = rects
        .iter()
        .find(|e| e.key.as_deref() == Some("create-room-dialog"))
        .expect("the dialog");
    let sub = rects
        .iter()
        .find(|e| e.key.as_deref() == Some("dialog-subtitle"))
        .expect("the subtitle");
    assert!(
        sub.rect[0] + sub.rect[2] <= panel.rect[0] + panel.rect[2] + 0.5,
        "the subtitle {:?} ends inside the dialog {:?}",
        sub.rect,
        panel.rect
    );
}

// Confirms start safe ---------------------------------------------------------------------------

/// A confirm that has just opened: Return and Space on its first Tab stop must not fire the
/// action it confirms, and the keyboard must not be on the danger button (the opener that had
/// it has just left the tree). `probe` reads what the confirmed action would change.
fn assert_confirm_starts_safe(
    mut rt: Runtime<RiverApp>,
    name: &str,
    danger: &str,
    probe: impl Fn(&mut Runtime<RiverApp>) -> String,
) {
    assert!(rt.find(danger).is_some(), "{name}: the confirm is showing");
    assert_ne!(
        focused_key(&rt).as_deref(),
        Some(danger),
        "{name}: focus starts on the danger button"
    );
    let before = probe(&mut rt);
    key(&mut rt, Key::Enter);
    assert_eq!(probe(&mut rt), before, "{name}: Return fired the action");
    key(&mut rt, Key::Tab);
    assert_ne!(
        focused_key(&rt).as_deref(),
        Some(danger),
        "{name}: the first Tab stop is the danger button"
    );
    key(&mut rt, Key::Text(" ".into()));
    assert_eq!(probe(&mut rt), before, "{name}: Space fired the action");
}

fn actions_probe(rt: &mut Runtime<RiverApp>) -> String {
    format!("{:?}", sent_all(rt))
}

#[test]
fn every_confirm_opens_with_the_danger_button_unfocused_and_unfired() {
    // Ban.
    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    let jonas = uid_of(&rt, "Jonas");
    press(&mut rt, "member-ban");
    answer_preview(&mut rt, &jonas, super::model::BanPreview::default());
    let _ = sent_all(&mut rt);
    assert_confirm_starts_safe(rt, "ban", "member-ban-confirm-yes", actions_probe);

    // Leave room.
    let mut rt = render(from_node("town-square"), WINDOW_SIZE);
    press(&mut rt, "room-title");
    press(&mut rt, "details-leave");
    let _ = sent_all(&mut rt);
    assert_confirm_starts_safe(rt, "leave", "details-leave-confirm", actions_probe);

    // Replace identity.
    let mut rt = scene_runtime("chat-import-id-replace");
    press(&mut rt, "import-identity-replace-cancel");
    press(&mut rt, "form-submit");
    let _ = sent_all(&mut rt);
    assert_confirm_starts_safe(
        rt,
        "replace identity",
        "import-identity-replace-confirm",
        actions_probe,
    );

    // Delete their messages.
    let mut rt = scene_runtime("chat-dm-purge");
    press(&mut rt, "dm-purge");
    let _ = sent_all(&mut rt);
    assert_confirm_starts_safe(rt, "delete messages", "dm-purge-confirm", actions_probe);

    // Change node (the confirm's primary is not red, but it replaces the rooms showing).
    let mut rt = scene_runtime("chat-node-change");
    rt.state.chat.node.form.as_mut().expect("the form").input =
        rune::TextEdit::new("home.example.org:7509");
    press(&mut rt, "node-connect");
    assert_confirm_starts_safe(rt, "change node", "node-confirm", |rt| {
        rt.state.chat.node.choice.address.to_string()
    });
}

/// Return on a focused "Ban user" asks once: repeated Return does not stack a second ask, and
/// the opener leaves the dialog once the confirmation shows.
#[test]
fn enter_on_ban_user_asks_once_and_the_opener_goes() {
    let mut rt = ban_offered(super::model::BanOffer::Allowed);
    assert!(tab_to(&mut rt, "member-ban"), "Tab reaches Ban user");
    assert!(rt.state.chat.ban_confirm.is_none());
    key(&mut rt, Key::Enter);
    assert!(rt.state.chat.ban_confirm.is_some(), "Return asks");
    assert!(rt.find("member-ban").is_none(), "the opener is gone");
    for _ in 0..3 {
        key(&mut rt, Key::Enter);
    }
    assert!(rt.state.chat.ban_confirm.is_some());
    let previews = sent_all(&mut rt)
        .into_iter()
        .filter(|a| matches!(a, Action::PreviewBan { .. }))
        .count();
    assert_eq!(previews, 1, "the room is asked what the ban does once");
}

// Behaviour a Rune bump can change -------------------------------------------------------------
//
// Each test here pins what the app does through a Rune feature (word motion, input method,
// undo, modal focus, the direction chord), driven with the events a window delivers. They are
// the ratchet for the next bump: a changed line means Rune changed, so read its changelog.
mod rune_bump {
    use super::*;
    #[cfg(not(target_os = "macos"))]
    use rune::{PhysicalInput, PhysicalKeyCode};

    /// The key of the element Rune says has the keyboard (`Tx::focused`), not River's mirror
    /// of it (`composer.focused`).
    fn rune_focus(rt: &mut Runtime<RiverApp>) -> Option<String> {
        let mut out = None;
        rt.update(|_, tx| out = tx.focused().map(str::to_owned));
        out
    }

    /// River's flag and Rune's focus say the same about the composer.
    fn assert_composer_focus_agrees(rt: &mut Runtime<RiverApp>, what: &str) {
        let rune = rune_focus(rt);
        let flag = rt.state.chat.composer.focused;
        assert_eq!(
            flag,
            rune.as_deref() == Some("composer-field"),
            "{what}: composer.focused={flag} but Rune's focus is {rune:?}"
        );
    }

    /// What a Ctrl+key is off macOS: rune's `cmd` and `ctrl` both set (see `ctrl` above).
    #[cfg(not(target_os = "macos"))]
    fn word_key(rt: &mut Runtime<RiverApp>, key: Key, shift: bool) {
        ctrl(rt, key, shift);
    }

    /// What the same key is on macOS: Option.
    #[cfg(target_os = "macos")]
    fn word_key(rt: &mut Runtime<RiverApp>, key: Key, shift: bool) {
        rt.key(KeyEvent {
            key,
            mods: rune::Mods {
                shift,
                alt: true,
                ..rune::Mods::default()
            },
        });
        rt.frame(0.3);
    }

    fn command(rt: &mut Runtime<RiverApp>, c: char) {
        rt.key(KeyEvent {
            key: Key::Command(c),
            mods: rune::Mods {
                cmd: true,
                ctrl: !cfg!(target_os = "macos"),
                ..rune::Mods::default()
            },
        });
        rt.frame(0.3);
    }

    fn composer(rt: &Runtime<RiverApp>) -> (&str, usize, usize) {
        let c = &rt.state.chat.composer;
        (c.text.as_str(), c.anchor, c.focus)
    }

    const LINE: &str = "see @maya.chen http://x.org/a";

    fn typed_line() -> Runtime<RiverApp> {
        let mut rt = render(connected(), WINDOW_SIZE);
        type_text(&mut rt, LINE);
        rt
    }

    // ---- word motion (UAX #29) ---------------------------------------------------------------

    /// Words are Unicode's: `maya.chen` and `x.org` are one word each, `@`, `://` and `/` are
    /// not. Backwards is the same on every platform: to the word's start.
    #[test]
    fn word_left_and_word_backspace_stop_at_unicode_word_starts() {
        let mut rt = typed_line();
        let mut stops = Vec::new();
        for _ in 0..5 {
            word_key(&mut rt, Key::Left, false);
            stops.push(composer(&rt).2);
        }
        assert_eq!(stops, [28, 22, 15, 5, 0]);
        let mut rt = typed_line();
        let mut left = Vec::new();
        for _ in 0..3 {
            word_key(&mut rt, Key::Backspace, false);
            left.push(composer(&rt).0.to_owned());
        }
        assert_eq!(
            left,
            [
                "see @maya.chen http://x.org/",
                "see @maya.chen http://",
                "see @maya.chen "
            ]
        );
    }

    /// Forward word keys: off macOS (Windows and Linux alike, for now) the start of the next
    /// word, Rune's rule since 2026-11-12; on macOS the end of the word. Backwards is the start
    /// of the word everywhere. Ian wants end-of-word on Linux too (GTK, Qt, Chromium); that
    /// lands in a follow-up change, which will give Linux its own block here.
    #[cfg(target_os = "macos")]
    const FORWARD_STOPS: [usize; 5] = [3, 14, 19, 27, 29];
    #[cfg(not(target_os = "macos"))]
    const FORWARD_STOPS: [usize; 5] = [5, 15, 22, 28, 29];

    fn to_start(rt: &mut Runtime<RiverApp>) {
        for _ in 0..5 {
            word_key(rt, Key::Left, false);
        }
        assert_eq!(composer(rt).2, 0);
    }

    #[test]
    fn ctrl_right_moves_a_word_forward_by_the_platforms_rule() {
        let mut rt = typed_line();
        to_start(&mut rt);
        let mut stops = Vec::new();
        for _ in 0..5 {
            word_key(&mut rt, Key::Right, false);
            stops.push(composer(&rt).2);
        }
        assert_eq!(stops, FORWARD_STOPS);
    }

    #[test]
    fn ctrl_shift_right_selects_a_word_forward_by_the_platforms_rule() {
        let mut rt = typed_line();
        to_start(&mut rt);
        let mut ends = Vec::new();
        for _ in 0..5 {
            word_key(&mut rt, Key::Right, true);
            ends.push(composer(&rt).2);
        }
        assert_eq!(ends, FORWARD_STOPS);
        assert_eq!(composer(&rt).1, 0, "the anchor stays");
        // Ctrl+Shift+Left shrinks it back to the start of a word, on every platform.
        word_key(&mut rt, Key::Left, true);
        assert_eq!(composer(&rt).2, 28);
    }

    #[test]
    fn ctrl_delete_removes_forward_by_the_platforms_rule() {
        let mut rt = typed_line();
        to_start(&mut rt);
        word_key(&mut rt, Key::Delete, false);
        #[cfg(target_os = "macos")]
        assert_eq!(composer(&rt), (" @maya.chen http://x.org/a", 0, 0));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(composer(&rt), ("maya.chen http://x.org/a", 0, 0));
    }

    /// With a selection, Ctrl+Delete deletes the selection and nothing more.
    #[test]
    fn ctrl_delete_with_a_selection_deletes_only_the_selection() {
        let mut rt = typed_line();
        to_start(&mut rt);
        word_key(&mut rt, Key::Right, true);
        word_key(&mut rt, Key::Delete, false);
        #[cfg(target_os = "macos")]
        assert_eq!(composer(&rt), (" @maya.chen http://x.org/a", 0, 0));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(composer(&rt), ("maya.chen http://x.org/a", 0, 0));
    }

    /// A word key edits through the field's own change hook, so a warning about the text before
    /// it goes away.
    #[test]
    fn a_word_key_in_the_import_box_clears_the_warning_about_the_text_before() {
        const BAD: &str =
            "-----BEGIN RIVER IDENTITY-----\n3mJr7AoUXx2Wqd\n-----END RIVER IDENTITY-----";
        for (k, caret) in [(Key::Backspace, BAD.len()), (Key::Delete, 0)] {
            let mut app = connected();
            let rooms = super::super::example::data().rooms;
            app.chat.source = super::super::Source::Node(super::super::LinkStatus::Live);
            app.chat
                .show_node_rooms(rooms, None, super::super::LinkStatus::Live, 29_000_000);
            let mut rt = render(app, WINDOW_SIZE);
            rt.update(|s, _| {
                s.chat.open_import();
                s.chat.form.token.replace_text(BAD);
                s.chat.submit_import();
            });
            rt.frame(0.3);
            assert!(
                rt.state.chat.form.error.is_some(),
                "the bad token is refused"
            );
            press(&mut rt, "import-token");
            rt.update(|s, _| s.chat.form.token.set_caret(caret));
            word_key(&mut rt, k.clone(), false);
            assert_ne!(rt.state.chat.form.token.text, BAD, "{k:?} edited it");
            assert!(rt.state.chat.form.error.is_none(), "{k:?}");
        }
    }

    // ---- input method ------------------------------------------------------------------------

    fn composing(rt: &mut Runtime<RiverApp>) {
        rt.ime(rune::Ime::Enabled);
        rt.ime(rune::Ime::Preedit("かん".into(), Some((0, 6)), Vec::new()));
        rt.frame(0.3);
    }

    fn commit(rt: &mut Runtime<RiverApp>, text: &str) {
        rt.ime(rune::Ime::Preedit(String::new(), None, Vec::new()));
        rt.ime(rune::Ime::Commit(text.into()));
        rt.frame(0.3);
    }

    #[test]
    fn a_composition_hides_the_mention_popup_and_commit_brings_it_back() {
        let mut rt = render(connected(), WINDOW_SIZE);
        type_text(&mut rt, "hi @ma");
        assert!(rt.state.chat.mention_popup().is_some());
        composing(&mut rt);
        assert!(rt.state.chat.mention_popup().is_none());
        assert!(rt.find("mention-popup-content").is_none());
        commit(&mut rt, "y");
        assert_eq!(rt.state.chat.composer.text, "hi @may");
        assert_eq!(mention_names(&rt), ["Maya Chen"]);
        assert!(rt.find("mention-popup-content").is_some());
    }

    #[test]
    fn tab_and_enter_in_a_composition_complete_no_mention_and_send_nothing() {
        for k in [Key::Tab, Key::Enter] {
            let mut rt = render(connected(), WINDOW_SIZE);
            type_text(&mut rt, "hi @ma");
            composing(&mut rt);
            key(&mut rt, k.clone());
            let text = &rt.state.chat.composer.text;
            assert!(!text.contains("rv:"), "{k:?} completed a mention: {text:?}");
            assert!(!text.is_empty(), "{k:?} sent the draft: {text:?}");
            assert!(rt.state.chat.mention_popup().is_none(), "{k:?}");
        }
    }

    #[test]
    fn a_composition_hides_the_shortcode_popup_and_commit_brings_it_back() {
        let mut rt = render(connected(), WINDOW_SIZE);
        type_text(&mut rt, ":smi");
        assert!(rt.state.chat.shortcode_popup().is_some());
        composing(&mut rt);
        assert!(rt.state.chat.shortcode_popup().is_none());
        key(&mut rt, Key::Tab);
        assert!(
            rt.state.chat.composer.text.starts_with(":smi"),
            "Tab did not complete the emoji: {:?}",
            rt.state.chat.composer.text
        );
        let mut rt = render(connected(), WINDOW_SIZE);
        type_text(&mut rt, ":smi");
        composing(&mut rt);
        commit(&mut rt, "l");
        assert_eq!(rt.state.chat.composer.text, ":smil");
        assert!(rt.state.chat.shortcode_popup().is_some());
    }

    // ---- undo --------------------------------------------------------------------------------

    #[test]
    fn undo_restores_a_word_deleted_with_ctrl_backspace() {
        let mut rt = render(connected(), WINDOW_SIZE);
        type_text(&mut rt, "hello big world");
        word_key(&mut rt, Key::Backspace, false);
        assert_eq!(composer(&rt).0, "hello big ");
        command(&mut rt, 'z');
        assert_eq!(composer(&rt), ("hello big world", 15, 15));
    }

    /// Undo after completing a mention puts back what was typed, selected, with no popup left
    /// open; a second undo removes the typing.
    #[test]
    fn undo_after_a_mention_completes_leaves_no_popup_or_stuck_anchor() {
        let mut rt = render(connected(), WINDOW_SIZE);
        type_text(&mut rt, "hello world @ma");
        key(&mut rt, Key::Tab);
        assert_eq!(composer(&rt).0, "hello world @[Maya Chen](rv:12) ");
        command(&mut rt, 'z');
        assert_eq!(composer(&rt), ("hello world @ma", 12, 15));
        assert!(rt.state.chat.mention_popup().is_none());
        // Typing replaces the selection, and the popup works again from the new text.
        key(&mut rt, Key::Text("@pr".into()));
        assert_eq!(composer(&rt).0, "hello world @pr");
        assert_eq!(mention_names(&rt), ["Priya Raman"]);
        command(&mut rt, 'z');
        command(&mut rt, 'z');
        assert!(rt.state.chat.mention_popup().is_none());
    }

    #[test]
    fn undo_after_an_emoji_shortcode_completes_leaves_no_popup() {
        let mut rt = render(connected(), WINDOW_SIZE);
        type_text(&mut rt, "ok :smi");
        key(&mut rt, Key::Tab);
        assert_eq!(composer(&rt).0, "ok \u{1F604}");
        command(&mut rt, 'z');
        assert_eq!(composer(&rt), ("ok :smi", 3, 7));
        assert!(rt.state.chat.shortcode_popup().is_none());
        command(&mut rt, 'z');
        assert_eq!(composer(&rt), ("", 0, 0));
        assert!(rt.state.chat.shortcode_popup().is_none());
    }

    // ---- focus under and after dialogs -------------------------------------------------------

    fn open_dialog(rt: &mut Runtime<RiverApp>, which: &str) {
        match which {
            "room-details" => rt.update(|s, _| s.chat.open_details()),
            "shortcuts" => rt.update(|s, _| {
                s.chat.open_shortcuts();
            }),
            _ => rt.update(|s, _| s.chat.open_member("27")),
        }
        for i in 0..6 {
            rt.frame(1.0 + f64::from(i) / 60.0);
        }
    }

    fn dialog_open(rt: &Runtime<RiverApp>) -> bool {
        rt.find("room-details-dialog").is_some()
            || rt.find("shortcuts-dialog").is_some()
            || rt.state.chat.member_modal.is_some()
    }

    fn settle_frames(rt: &mut Runtime<RiverApp>) {
        for i in 0..6 {
            rt.frame(2.0 + f64::from(i) / 60.0);
        }
    }

    /// Where the keyboard goes when each dialog closes, by each way of closing it. The room
    /// details and member dialogs hand it back to the composer, which had it; the shortcuts
    /// page does not (nothing has it afterwards, as before the Rune bump). In every case Rune's
    /// focus and River's flag agree.
    #[test]
    fn closing_a_dialog_returns_the_keyboard_as_documented_and_rune_agrees() {
        // (dialog, expected landing: composer or nowhere)
        let dialogs = [
            ("room-details", true),
            ("member", true),
            ("shortcuts", false),
        ];
        for (which, to_composer) in dialogs {
            for how in ["escape", "x", "scrim", "button"] {
                if how == "button" && which != "shortcuts" {
                    continue; // the others close with the X (details' Cancel is the owner's)
                }
                let what = format!("{which} closed by {how}");
                let mut rt = render(connected(), WINDOW_SIZE);
                press(&mut rt, "composer-field");
                assert_composer_focus_agrees(&mut rt, "before");
                open_dialog(&mut rt, which);
                assert!(dialog_open(&rt), "{what}: opened");
                assert!(!rt.state.chat.composer.focused, "{what}: not under it");
                assert_composer_focus_agrees(&mut rt, &format!("{what}, open"));
                match how {
                    "escape" => key(&mut rt, Key::Escape),
                    "x" => press(&mut rt, "close"),
                    "scrim" => {
                        rt.pointer_down(2.0, 2.0);
                        rt.pointer_up(2.0, 2.0);
                        rt.frame(0.1);
                        rt.frame(0.2);
                    }
                    _ => press(&mut rt, "shortcuts-close"),
                }
                settle_frames(&mut rt);
                assert!(!dialog_open(&rt), "{what}: closed");
                assert_eq!(rt.state.chat.composer.focused, to_composer, "{what}");
                assert_composer_focus_agrees(&mut rt, &what);
                if !to_composer {
                    assert_eq!(rune_focus(&mut rt), None, "{what}: nothing has it");
                }
            }
        }
    }

    /// The real user path of a click on the panel toggle while a dialog is up: the click is
    /// outside the dialog, so it dismisses the dialog and does not toggle the panel, and the
    /// composer gets the keyboard back from Rune, which agrees with River's flag.
    #[test]
    fn pressing_the_members_toggle_under_a_dialog_only_dismisses_the_dialog() {
        let mut rt = render(connected(), WINDOW_SIZE);
        press(&mut rt, "composer-field");
        open_dialog(&mut rt, "room-details");
        let panel = rt.state.chat.members_open;
        press(&mut rt, "toggle-members");
        settle_frames(&mut rt);
        assert!(!dialog_open(&rt), "the press dismissed the dialog");
        assert_eq!(
            rt.state.chat.members_open, panel,
            "the panel did not toggle"
        );
        assert!(rt.state.chat.composer.focused);
        assert_composer_focus_agrees(&mut rt, "after the dismissal");
    }

    /// A direct-message thread is not a modal: River's own hand-back (`dm.restore_focus`) is the
    /// only one. Opened by a key (the quick switcher, a notification) while the composer has the
    /// keyboard, the thread's box takes it and Escape gives it back to the composer; Rune's
    /// focus agrees at each step. Opened by a click on its row the composer has already lost
    /// the keyboard to the click, so nothing is handed back and Rune puts the keyboard on the
    /// first control it can (the connection pill), which River's flag also reports as not the
    /// composer.
    #[test]
    fn a_dm_thread_takes_the_keyboard_and_gives_it_back_as_documented() {
        use super::super::dm_view_tests as dm;
        let thread_app = || {
            let mut rt = dm::app();
            dm::give(&mut rt, dm::conversation().snapshot(true));
            rt
        };
        // By key.
        let mut rt = thread_app();
        press(&mut rt, "composer-field");
        assert_composer_focus_agrees(&mut rt, "before");
        let room = rt.state.chat.room().expect("a room").key.clone();
        rt.update(|s, _| {
            assert!(s.chat.open_dm(&room, &dm::peer_key()));
        });
        rt.frame(0.3);
        assert!(rt.find("dm-thread").is_some());
        assert!(!rt.state.chat.composer.focused);
        assert_eq!(rune_focus(&mut rt).as_deref(), Some("dm-composer-field"));
        key(&mut rt, Key::Escape);
        settle_frames(&mut rt);
        assert!(rt.find("dm-thread").is_none());
        assert!(rt.state.chat.composer.focused);
        assert_composer_focus_agrees(&mut rt, "after closing the thread");
        // By a click on the row.
        let mut rt = thread_app();
        press(&mut rt, "composer-field");
        press(&mut rt, &dm::row_key());
        rt.frame(0.3);
        assert_eq!(rune_focus(&mut rt).as_deref(), Some("dm-composer-field"));
        key(&mut rt, Key::Escape);
        settle_frames(&mut rt);
        assert!(rt.find("dm-thread").is_none());
        assert!(!rt.state.chat.composer.focused);
        assert_eq!(rune_focus(&mut rt).as_deref(), Some("connection-pill"));
        assert_composer_focus_agrees(&mut rt, "after a click-opened thread closes");
    }

    // ---- Ctrl+Shift sets a field's direction ----------------------------------------------------

    #[cfg(not(target_os = "macos"))]
    fn chord(rt: &mut Runtime<RiverApp>, shift: PhysicalKeyCode) {
        let mods = |ctrl: bool, shift: bool| rune::Mods {
            ctrl,
            cmd: ctrl,
            shift,
            ..rune::Mods::default()
        };
        let mut phys = |key: PhysicalKeyCode, mods: rune::Mods, pressed: bool| {
            rt.physical_input(PhysicalInput::Key {
                key,
                mods,
                pressed,
                repeat: false,
            });
            rt.frame(0.3);
        };
        phys(PhysicalKeyCode::ControlLeft, mods(false, false), true);
        phys(shift, mods(true, false), true);
        phys(shift, mods(true, true), false);
        phys(PhysicalKeyCode::ControlLeft, mods(true, false), false);
    }

    /// Characterisation, not a wish: Rune (2026-11-12, Windows and Linux) makes Ctrl+Left Shift
    /// and Ctrl+Right Shift, pressed together and released with no key between, set the focused
    /// field's direction. That is also a common keyboard-layout switch chord, and River cannot
    /// opt out. The direction stays after a send. Tracked in freenet/river#955.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_ctrl_shift_chord_sets_the_composers_direction_and_it_survives_a_send() {
        let mut rt = render(connected(), WINDOW_SIZE);
        type_text(&mut rt, "abc");
        // The direction's type is Rune's internal one, so it is compared by its name.
        let dir = |rt: &Runtime<RiverApp>| format!("{:?}", rt.state.chat.composer.forced_dir());
        assert_eq!(dir(&rt), "None");
        chord(&mut rt, PhysicalKeyCode::ShiftRight);
        assert_eq!(
            dir(&rt),
            "Some(Rtl)",
            "Ctrl+Right Shift flips the composer to right to left"
        );
        assert_eq!(rt.state.chat.composer.text, "abc", "no text is typed");
        chord(&mut rt, PhysicalKeyCode::ShiftLeft);
        assert_eq!(dir(&rt), "Some(Ltr)");
        chord(&mut rt, PhysicalKeyCode::ShiftRight);
        key(&mut rt, Key::Enter);
        assert_eq!(rt.state.chat.composer.text, "", "sent");
        assert_eq!(
            dir(&rt),
            "Some(Rtl)",
            "the next draft is still right to left"
        );
    }
}
