//! The direct-message rail and thread view, driven as a person drives them. The messages are
//! composed by river-core's own `compose_direct_message` and read by `rooms::dm::threads`, so
//! what the screen shows went through the same verify-and-open path as a real room's.

use ed25519_dalek::{SigningKey, VerifyingKey};
use river_core::room_state::ChatRoomStateV1;
use river_core::room_state::direct_messages::{DirectMessagesV1, compose_direct_message};
use river_core::room_state::dm_body::{DirectMessageBody, InvitePayload, encode_body};
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
use rune::{Env, Key, KeyEvent, Runtime};

use super::dm_model::{DmBody, DmSnapshot, DmThread, Placeholder};
use super::dm_view::{DmUi, LOADING_SENT, NO_SEND, TRUNCATED};
use crate::rooms::dm::{self, Source};
use crate::rooms::dm_cache::OutboundCache;
use crate::{RiverApp, Setup, WINDOW_SIZE, view};

const OWNER: u8 = 1;
const ME: u8 = 2;
const PEER: u8 = 3;
const ROOM: &str = "town-square";
const T0: u64 = 1_700_000_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn vk(n: u8) -> VerifyingKey {
    sk(n).verifying_key()
}

fn id(n: u8) -> MemberId {
    MemberId::from(&vk(n))
}

/// A room with the owner, the user and a peer, and the messages between them.
struct Fixture {
    members: ChatRoomStateV1,
    dms: DirectMessagesV1,
    cache: OutboundCache,
}

impl Fixture {
    fn new() -> Self {
        let mut members = ChatRoomStateV1::default();
        for n in [ME, PEER] {
            members.members.members.push(AuthorizedMember::new(
                Member {
                    owner_member_id: id(OWNER),
                    invited_by: id(OWNER),
                    member_vk: vk(n),
                },
                &sk(OWNER),
            ));
        }
        Fixture {
            members,
            dms: DirectMessagesV1::default(),
            cache: OutboundCache::default(),
        }
    }

    fn compose(&mut self, from: u8, to: u8, ts: u64, body: &[u8]) {
        let m = compose_direct_message(&sk(from), &vk(to), &vk(OWNER), ts, ts, body).unwrap();
        if from == ME {
            // The user's own words are cached under the token, as the web UI caches them.
            let text = String::from_utf8_lossy(body).into_owned();
            self.cache
                .insert_for_test(&vk(OWNER), id(to), m.purge_token(), &text);
        }
        self.dms.messages.push(m);
    }

    /// What the window is sent for this room.
    fn snapshot(&self, hydrated: bool) -> DmSnapshot {
        let name_of = |m: MemberId| {
            if m == id(PEER) {
                "Priya".to_owned()
            } else {
                format!("Member {m}")
            }
        };
        let label = |_: &VerifyingKey| Some("Rust Help".to_owned());
        let me = sk(ME);
        let owner = vk(OWNER);
        let threads = dm::threads(&Source {
            room_key: ROOM,
            room_name: "Town Square",
            owner: &owner,
            members: &self.members.members,
            messages: &self.dms,
            self_sk: Some(&me),
            self_vk: Some(me.verifying_key()),
            cache: &self.cache,
            name_of: &name_of,
            room_label: &label,
        });
        DmSnapshot {
            threads: threads
                .threads
                .into_iter()
                .map(std::sync::Arc::new)
                .collect(),
            room_keys: vec![ROOM.to_owned()],
            truncated: Vec::new(),
            hydrated,
        }
    }
}

/// The rooms are listed and hold no messages yet: what arrives afterwards is news.
fn empty_rooms() -> DmSnapshot {
    DmSnapshot {
        room_keys: vec![ROOM.to_owned()],
        hydrated: true,
        ..Default::default()
    }
}

fn peer_uid() -> String {
    id(PEER).to_string()
}

fn app() -> Runtime<RiverApp> {
    let mut state = RiverApp::detached();
    state.setup = Setup::Connected {
        version: "0.2.155".into(),
    };
    let mut rt = Runtime::new(
        state,
        view,
        WINDOW_SIZE,
        Env {
            system_dark: true,
            scale: 1.0,
        },
    );
    rt.on_key = crate::spec().on_key;
    rt.poll = crate::spec().poll;
    rt.frame(0.0);
    rt.frame(1.0 / 60.0);
    rt
}

fn give(rt: &mut Runtime<RiverApp>, snapshot: DmSnapshot) {
    rt.update(move |s, _| s.chat.apply_dms(snapshot));
    // Two frames: what went away is gone from the tree after the second.
    rt.frame(0.1);
    rt.frame(0.6);
}

fn press(rt: &mut Runtime<RiverApp>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no element {key}"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.1);
    rt.frame(0.2);
}

fn esc(rt: &mut Runtime<RiverApp>) {
    rt.key(KeyEvent {
        key: Key::Escape,
        mods: Default::default(),
    });
    rt.frame(0.3);
}

fn texts(rt: &Runtime<RiverApp>) -> Vec<String> {
    rt.elements().into_iter().filter_map(|e| e.text).collect()
}

fn shows(rt: &Runtime<RiverApp>, needle: &str) -> bool {
    texts(rt).iter().any(|t| t.contains(needle))
}

/// The position of the first text containing `needle` in the open thread: after its header, so
/// the rail's previews of the same words do not count.
fn at(rt: &Runtime<RiverApp>, needle: &str) -> usize {
    let all = texts(rt);
    let from = all
        .iter()
        .position(|t| t.starts_with("in Town Square"))
        .expect("a thread is open");
    from + all[from..]
        .iter()
        .position(|t| t.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is not in the thread"))
}

/// The keys of the elements whose key starts with `prefix`.
fn keys_with(rt: &Runtime<RiverApp>, prefix: &str) -> Vec<String> {
    rt.elements()
        .into_iter()
        .filter_map(|e| e.key)
        .filter(|k| k.starts_with(prefix))
        .collect()
}

fn row_key() -> String {
    format!("dm-{ROOM}-{}", peer_uid())
}

/// A room with a conversation: the peer writes first and last, the user answers between.
fn conversation() -> Fixture {
    let mut f = Fixture::new();
    f.compose(PEER, ME, T0, b"first from the peer, with **bold**");
    f.compose(ME, PEER, T0 + 60, b"my answer");
    f.compose(PEER, ME, T0 + 3600, b"last from the peer");
    f
}

/// A room whose backlog (one old message from the peer) has been met, and the conversation that
/// arrives after it: the fixture, and the snapshot of the backlog alone.
fn conversation_after_seed() -> (Fixture, DmSnapshot) {
    let mut f = Fixture::new();
    f.compose(PEER, ME, T0 - 100, b"backlog");
    let seed = f.snapshot(true);
    f.compose(PEER, ME, T0, b"first from the peer, with **bold**");
    f.compose(ME, PEER, T0 + 60, b"my answer");
    f.compose(PEER, ME, T0 + 3600, b"last from the peer");
    (f, seed)
}

#[test]
fn the_rail_lists_the_thread_and_a_press_opens_it_in_the_main_pane() {
    let mut rt = app();
    // The rooms are shown first, and the messages arrive after, so they are unread.
    give(&mut rt, empty_rooms());
    assert!(rt.find("dm-heading").is_none(), "no threads, no section");
    let (f, seed) = conversation_after_seed();
    give(&mut rt, seed);
    give(&mut rt, f.snapshot(true));
    assert!(shows(&rt, "Direct messages"));
    let row = row_key();
    assert!(rt.find(&row).is_some(), "a rail row for the thread");
    assert!(rt.find("dm-unread").is_some(), "two messages are unread");
    assert!(rt.find("dm-thread").is_none());

    press(&mut rt, &row);

    assert!(rt.find("dm-thread").is_some(), "the thread is a pane");
    assert!(rt.find("conversation").is_none(), "in place of the room");
    assert!(rt.find("members").is_none());
    assert!(shows(&rt, "Priya"));
    assert!(shows(&rt, "in Town Square"));
    // Both sides, oldest first, the peer's text as markdown (the bold markers are gone).
    let (a, b, c) = (
        at(&rt, "first from the peer"),
        at(&rt, "my answer"),
        at(&rt, "last from the peer"),
    );
    assert!(a < b && b < c, "{a} {b} {c}");
    assert!(!shows(&rt, "**bold**"));
    assert!(shows(&rt, "You"));
    // Nothing to type into yet, and it says so.
    assert!(shows(&rt, NO_SEND));
    assert!(rt.find("composer").is_none());
}

#[test]
fn back_and_escape_return_to_the_room_and_a_dialog_takes_escape_first() {
    let mut rt = app();
    give(&mut rt, conversation().snapshot(true));
    press(&mut rt, &row_key());
    press(&mut rt, "dm-back");
    assert!(rt.find("dm-thread").is_none());
    assert!(rt.find("conversation").is_some());

    press(&mut rt, &row_key());
    esc(&mut rt);
    assert!(rt.find("dm-thread").is_none(), "Escape goes back");

    // With a dialog over the thread, Escape closes the dialog and leaves the thread.
    press(&mut rt, &row_key());
    rt.update(|s, _| s.chat.shortcuts_open = true);
    rt.frame(0.1);
    esc(&mut rt);
    assert!(!rt.state.chat.shortcuts_open);
    assert!(rt.find("dm-thread").is_some());
    esc(&mut rt);
    assert!(rt.find("dm-thread").is_none());

    // Choosing a room, even the one already open, shows its conversation.
    press(&mut rt, &row_key());
    let room = rt.state.chat.room().unwrap().key.clone();
    press(&mut rt, &format!("room-{room}"));
    assert!(rt.find("dm-thread").is_none());
    assert!(rt.find("conversation").is_some());
}

#[test]
fn the_message_button_opens_an_empty_thread_for_a_member_with_no_history() {
    let mut rt = app();
    let uid = rt
        .state
        .chat
        .room()
        .unwrap()
        .members
        .iter()
        .find(|m| m.nickname == "Jonas")
        .unwrap()
        .uid
        .clone();
    let id = rt
        .state
        .chat
        .room()
        .unwrap()
        .member_by_uid(&uid)
        .unwrap()
        .id;
    press(&mut rt, &format!("member-{id}"));
    assert!(rt.find("member-info-dialog").is_some());
    let button = rt
        .elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some("member-dm"))
        .expect("the Message button");
    assert_ne!(
        button.help.as_deref(),
        Some(super::view::NOT_YET),
        "it is not dimmed"
    );
    press(&mut rt, "member-dm");
    assert!(rt.find("member-info-dialog").is_none());
    assert!(rt.find("dm-thread").is_some());
    assert!(shows(&rt, "Jonas"));
    assert!(shows(&rt, "No messages yet"));
    assert!(shows(&rt, NO_SEND));
    // The user is not a person to message.
    let me = rt.state.chat.room().unwrap().me;
    assert!(!rt.state.chat.open_dm(ROOM, "nobody"));
    let own = rt.state.chat.room().unwrap().key.clone();
    let my_uid = rt
        .state
        .chat
        .room()
        .unwrap()
        .member(me)
        .unwrap()
        .uid
        .clone();
    assert!(!rt.state.chat.open_dm(&own, &my_uid));
}

#[test]
fn read_positions_seed_once_and_only_later_messages_are_unread() {
    let mut ui = DmUi::default();
    let mut f = conversation();
    // The first snapshot of a room list: what is there is not news.
    ui.apply(f.snapshot(true));
    assert_eq!(ui.unread_total(), 0);
    // A message that arrives afterwards is unread, and a re-seed would have hidden it.
    f.compose(PEER, ME, T0 + 7200, b"newer");
    ui.apply(f.snapshot(true));
    assert_eq!(ui.unread_total(), 1);
    // A snapshot of no rooms seeds nothing, and does not use up the seed.
    let mut fresh = DmUi::default();
    fresh.apply(DmSnapshot::default());
    fresh.apply(conversation().snapshot(true));
    assert_eq!(fresh.unread_total(), 0, "seeded by the first with rooms");
}

#[test]
fn a_thread_is_read_only_when_open_in_front_and_at_its_newest_message() {
    let mut ui = DmUi::default();
    let (mut f, seed) = conversation_after_seed();
    ui.apply(seed);
    f.compose(PEER, ME, T0 + 7200, b"newer");
    ui.apply(f.snapshot(true));
    let unread = ui.unread_total();
    assert!(unread > 0);
    // Not open: nothing is read, in front or not.
    assert!(!ui.mark_read(true));
    ui.at_bottom = true;
    ui.open = Some((ROOM.to_owned(), peer_uid()));
    // Open but the window is away.
    assert!(!ui.mark_read(false));
    assert_eq!(ui.unread_total(), unread);
    // Open and in front but scrolled up: the newest message is not on screen.
    ui.at_bottom = false;
    assert!(!ui.mark_read(true));
    assert_eq!(ui.unread_total(), unread);
    // All three.
    ui.at_bottom = true;
    assert!(ui.mark_read(true));
    assert_eq!(ui.unread_total(), 0);
    // And a later message is unread again.
    f.compose(PEER, ME, T0 + 9000, b"later");
    ui.apply(f.snapshot(true));
    assert_eq!(ui.unread_total(), 1);
}

#[test]
fn the_window_marks_a_thread_read_when_it_is_in_front() {
    let mut rt = app();
    let (f, seed) = conversation_after_seed();
    give(&mut rt, seed);
    give(&mut rt, f.snapshot(true));
    assert_eq!(rt.state.chat.dm.unread_total(), 2);
    rt.update(|s, _| s.focus_changed(false));
    press(&mut rt, &row_key());
    rt.frame(0.5);
    assert_eq!(
        rt.state.chat.dm.unread_total(),
        2,
        "a window that is away reads nothing"
    );
    rt.update(|s, _| s.focus_changed(true));
    rt.frame(0.6);
    rt.frame(0.7);
    assert_eq!(rt.state.chat.dm.unread_total(), 0);
}

#[test]
fn a_thread_covering_the_room_does_not_mark_the_room_read() {
    let mut chat = super::Chat::example();
    let key = chat.room().unwrap().key.clone();
    let mut rooms = chat.rooms().to_vec();
    chat.show_node_rooms(rooms.clone(), None, super::LinkStatus::Live, 29_000_000);
    chat.apply_dms(conversation().snapshot(true));
    assert!(chat.open_dm(&key, &peer_uid()));
    // A message arrives in the covered room.
    let last = rooms
        .iter_mut()
        .find(|r| r.key == key)
        .and_then(|r| {
            let mut m = r.messages.last().cloned()?;
            m.id += 100;
            m.key = "new".into();
            m.author = r.members.iter().find(|x| x.id != r.me)?.id;
            m.time += 5;
            r.messages.push(m);
            Some(())
        })
        .is_some();
    assert!(last);
    chat.show_node_rooms(rooms.clone(), None, super::LinkStatus::Live, 29_000_001);
    assert_eq!(chat.room().unwrap().unread, 1, "not read behind the thread");
    assert!(chat.visible_room().is_none());
    chat.close_dm();
    assert_eq!(chat.room().unwrap().unread, 0, "read once it is in front");
    assert!(chat.visible_room().is_some());
}

#[test]
fn the_rail_orders_unread_first_then_newest_and_is_not_rebuilt_per_frame() {
    let mut rt = app();
    let mut f = Fixture::new();
    // Both conversations have a backlog that has been met.
    f.compose(PEER, ME, T0 - 100, b"backlog");
    f.compose(OWNER, ME, T0 - 100, b"backlog");
    give(&mut rt, f.snapshot(true));
    f.compose(PEER, ME, T0, b"old");
    // A second thread: with the owner, newer than the first.
    f.compose(OWNER, ME, T0 + 500, b"from the owner");
    let mut snap = f.snapshot(true);
    snap.threads.sort_by_key(|t| t.last_ts);
    give(&mut rt, snap.clone());
    // Both unread: the newest first.
    let order: Vec<String> = rt
        .state
        .chat
        .dm
        .rail_order()
        .into_iter()
        .map(|(_, p)| p.to_owned())
        .collect();
    assert_eq!(order, [id(OWNER).to_string(), peer_uid()]);
    // Reading the newer one puts the other first, as it is the one still unread.
    rt.update(|s, _| {
        s.chat.open_dm(ROOM, &id(OWNER).to_string());
        s.chat.dm.at_bottom = true;
        s.chat.dm_sync(true);
    });
    let order: Vec<String> = rt
        .state
        .chat
        .dm
        .rail_order()
        .into_iter()
        .map(|(_, p)| p.to_owned())
        .collect();
    assert_eq!(order, [peer_uid(), id(OWNER).to_string()]);
    // Frames build nothing: the rows are made when the data changes.
    let before = rt.state.chat.dm.rebuilds();
    for i in 0..10 {
        rt.frame(1.0 + f64::from(i) * 0.016);
    }
    assert_eq!(rt.state.chat.dm.rebuilds(), before);
}

#[test]
fn what_is_not_known_yet_is_said_as_a_state_of_the_cache_and_not_of_the_message() {
    // The user's message with no cached text.
    let mut f = Fixture::new();
    f.compose(PEER, ME, T0, b"hello");
    f.compose(ME, PEER, T0 + 60, b"mine");
    f.cache = OutboundCache::default();
    let mut rt = app();
    give(&mut rt, f.snapshot(false));
    press(&mut rt, &row_key());
    assert!(shows(&rt, "text is not available yet"), "{:?}", texts(&rt));
    assert!(shows(&rt, LOADING_SENT));
    assert!(!shows(&rt, "ciphertext only"));
    assert!(!shows(&rt, TRUNCATED));

    // Once the cache has been read and holds nothing for it, it is a message without text.
    let mut hydrated = f.snapshot(true);
    hydrated.truncated.push(ROOM.to_owned());
    give(&mut rt, hydrated);
    assert!(shows(&rt, "ciphertext only"));
    assert!(!shows(&rt, LOADING_SENT), "{:?}", texts(&rt));
    assert!(shows(&rt, TRUNCATED));
}

#[test]
fn an_invitation_is_a_read_only_card_in_plain_text() {
    let mut f = Fixture::new();
    let body = DirectMessageBody::Invite(Box::new(InvitePayload {
        room_owner_vk: vk(9),
        invitation_payload: vec![7; 40],
        personal_message: Some("join **us** [x](https://evil.example)".into()),
    }));
    f.compose(PEER, ME, T0, &encode_body(&body).unwrap());
    let mut rt = app();
    give(&mut rt, f.snapshot(true));
    press(&mut rt, &row_key());
    assert_eq!(keys_with(&rt, "dm-invite-note-").len(), 1);
    // The note is the peer's words, shown as written.
    assert!(shows(&rt, "join **us** [x](https://evil.example)"));
    // The room is named from this computer's rooms, or by a short form of its key.
    assert!(shows(&rt, "Rust Help"));
    // It cannot be accepted here.
    let accept = rt.elements().into_iter().any(|e| {
        e.key.as_deref().is_some_and(|k| k.contains("accept"))
            || e.text.as_deref().is_some_and(|t| t == "Accept")
    });
    assert!(!accept);
    assert!(shows(&rt, "not available yet in this build"));
}

#[test]
fn a_rail_preview_is_one_bounded_plain_line() {
    let long = format!("**bold** {}\u{7}\nsecond line", "word ".repeat(100));
    let thread = |body: DmBody, outgoing: bool| DmThread {
        room: ROOM.into(),
        room_name: "Town Square".into(),
        peer_uid: "7".into(),
        peer: id(PEER),
        peer_name: "Priya".into(),
        peer_is_member: true,
        messages: vec![super::dm_model::DmMessage {
            key: "k".into(),
            outgoing,
            timestamp: T0,
            body,
        }],
        last_ts: T0,
        last_inbound_ts: T0,
    };
    let mut ui = DmUi::default();
    ui.apply(DmSnapshot {
        threads: vec![thread(DmBody::Text(long), false).into()],
        room_keys: vec![ROOM.to_owned()],
        hydrated: true,
        ..Default::default()
    });
    let shown = {
        let mut rt = app();
        rt.update(move |s, _| s.chat.dm = ui);
        rt.frame(0.1);
        rt
    };
    let line = texts(&shown)
        .into_iter()
        .find(|t| t.contains("word word"))
        .expect("the preview");
    assert!(line.chars().count() <= 101, "{}", line.chars().count());
    assert!(!line.contains("**") && !line.chars().any(char::is_control));

    for (body, outgoing, expect) in [
        (
            DmBody::Invite {
                room_label: "Rust Help".into(),
                note: Some("secret note".into()),
            },
            false,
            "Invitation to Rust Help",
        ),
        (
            DmBody::SentInvite { note: None },
            true,
            "You sent an invitation",
        ),
        (DmBody::Text("mine".into()), true, "You: mine"),
        (
            DmBody::Placeholder(Placeholder::CannotDecrypt),
            false,
            "A message that could not be shown",
        ),
    ] {
        let mut ui = DmUi::default();
        ui.apply(DmSnapshot {
            threads: vec![thread(body, outgoing).into()],
            room_keys: vec![ROOM.to_owned()],
            hydrated: true,
            ..Default::default()
        });
        let mut rt = app();
        rt.update(move |s, _| s.chat.dm = ui);
        rt.frame(0.1);
        assert!(shows(&rt, expect), "{expect}: {:?}", texts(&rt));
        // Never the note of an invitation, which is not part of a preview.
        assert!(!shows(&rt, "secret note"));
    }
}

#[test]
fn the_direct_message_view_neither_logs_nor_feeds_titles_notifications_or_status() {
    fn read(path: &str) -> String {
        let root = env!("CARGO_MANIFEST_DIR");
        std::fs::read_to_string(format!("{root}/src/{path}"))
            .unwrap()
            .replace("\r\n", "\n")
    }
    // The code before its tests, comments dropped.
    fn code(s: &str) -> String {
        s.split("#[cfg(test)]")
            .next()
            .unwrap()
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }
    let view = code(&read("chat/dm_view.rs"));
    for banned in [
        "println!",
        "eprintln!",
        "dbg!",
        "tracing::",
        "log::",
        "{:?}",
        "{p:?}",
        "{m:?}",
    ] {
        assert!(!view.contains(banned), "dm_view.rs uses {banned}");
    }
    // What can leave the window with a thread's words in it. (`dm.is_open()` is a flag, not a
    // thread, and may be read.)
    for file in [
        "chat/search.rs",
        "chat/shortcuts.rs",
        "notify.rs",
        "notify_actions.rs",
        "chat/node_status.rs",
        "chat/overview.rs",
        "chat/switcher.rs",
    ] {
        // Without whitespace, so a line break inside a path does not hide it.
        let s: String = code(&read(file)).split_whitespace().collect();
        for banned in [
            "dm_view",
            "DmThread",
            "DmSnapshot",
            ".dm.snapshot",
            ".dm.unread",
            ".dm.open_key",
            ".dm.rail_order",
        ] {
            assert!(
                !s.contains(banned),
                "{file} reads a thread through {banned}"
            );
        }
    }
    // The title and the notifier's count do not take a thread's name or unread.
    let lib = code(&read("lib.rs"));
    assert!(!lib.contains("dm.unread") && !lib.contains("dm.snapshot"));
}

fn invite(note: &str) -> Vec<u8> {
    encode_body(&DirectMessageBody::Invite(Box::new(InvitePayload {
        room_owner_vk: vk(9),
        invitation_payload: vec![7; 40],
        personal_message: Some(note.into()),
    })))
    .unwrap()
}

#[test]
fn two_invitations_in_one_thread_have_their_own_keys() {
    let mut f = Fixture::new();
    f.compose(PEER, ME, T0, &invite("first note"));
    f.compose(PEER, ME, T0 + 400, &invite("second note"));
    let mut rt = app();
    give(&mut rt, f.snapshot(true));
    press(&mut rt, &row_key());
    let notes = keys_with(&rt, "dm-invite-note-");
    assert_eq!(notes.len(), 2);
    assert_ne!(notes[0], notes[1]);
    assert!(shows(&rt, "first note") && shows(&rt, "second note"));
    // The thread's own elements (the room list around it repeats some keys of its own).
    let all = keys_with(&rt, "dm-");
    let mut sorted = all.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), all.len(), "no key is used twice");
}

#[test]
fn a_hostile_timestamp_is_clamped_once_and_nothing_panics() {
    let mut f = Fixture::new();
    for (i, ts) in [u64::MAX, i64::MAX as u64 + 1, 0, i64::MAX as u64, T0]
        .into_iter()
        .enumerate()
    {
        f.compose(PEER, ME, ts, format!("peer {i}").as_bytes());
        f.compose(ME, PEER, ts, format!("mine {i}").as_bytes());
    }
    let snap = f.snapshot(true);
    for t in &snap.threads {
        assert!(t.last_ts <= dm::MAX_SHOWN_SECS && t.last_inbound_ts <= dm::MAX_SHOWN_SECS);
        assert!(t.messages.iter().all(|m| m.timestamp <= dm::MAX_SHOWN_SECS));
    }
    let mut rt = app();
    give(&mut rt, snap);
    press(&mut rt, &row_key());
    // Every frame of the thread, and of the rail, draws.
    for i in 0..5 {
        rt.frame(1.0 + f64::from(i));
    }
    assert!(shows(&rt, "peer 0") && shows(&rt, "mine 2"));
    // The conversions themselves never panic, for any number.
    for ts in [0, 1, T0, u64::MAX, i64::MAX as u64, i64::MAX as u64 + 1] {
        let _ = crate::rooms::room::local_minutes_of_secs(ts);
    }
}

fn thread_of(f: &Fixture) -> std::sync::Arc<DmThread> {
    f.snapshot(true).threads[0].clone()
}

#[test]
fn what_is_unread_does_not_depend_on_what_the_sender_signed() {
    let mut ui = DmUi::default();
    let mut f = Fixture::new();
    f.compose(PEER, ME, T0, b"old");
    ui.apply(f.snapshot(true));
    assert_eq!(ui.unread_total(), 0);
    ui.open = Some((ROOM.to_owned(), peer_uid()));
    ui.at_bottom = true;
    // A forged far-future message is unread, and reading it does not hide later ones.
    f.compose(PEER, ME, u64::MAX, b"forged future");
    ui.apply(f.snapshot(true));
    assert_eq!(ui.unread_total(), 1);
    assert!(ui.mark_read(true));
    assert_eq!(ui.unread_total(), 0);
    f.compose(PEER, ME, T0 + 10, b"real, later in fact");
    ui.apply(f.snapshot(true));
    assert_eq!(
        ui.unread_total(),
        1,
        "a message stamped before the forged one"
    );
    // One stamped in the past, and one with the same second as a message already read.
    assert!(ui.mark_read(true));
    f.compose(PEER, ME, 5, b"stamped long ago");
    f.compose(PEER, ME, T0 + 10, b"same second");
    ui.apply(f.snapshot(true));
    assert_eq!(ui.unread_total(), 2);
    let _ = thread_of(&f);
}

#[test]
fn a_room_met_later_is_seeded_but_a_new_conversation_in_a_known_room_is_news() {
    let mut ui = DmUi::default();
    let mut f = Fixture::new();
    f.compose(PEER, ME, T0, b"backlog");
    // The first list has no room that has this thread's room.
    ui.apply(DmSnapshot {
        room_keys: vec!["other".into()],
        hydrated: true,
        ..Default::default()
    });
    // The room appears later with its backlog: not unread.
    let mut snap = f.snapshot(true);
    snap.room_keys.push("other".into());
    ui.apply(snap);
    assert_eq!(ui.unread_total(), 0);
    // A message after that is news, and so is a first message from someone new in the room.
    f.compose(PEER, ME, T0 + 5, b"news");
    f.compose(OWNER, ME, T0 + 6, b"from someone new");
    let mut snap = f.snapshot(true);
    snap.room_keys.push("other".into());
    ui.apply(snap);
    assert_eq!(ui.unread_total(), 2);
}

#[test]
fn nothing_is_forgotten_because_a_list_was_empty_partial_or_missing_a_thread() {
    let mut ui = DmUi::default();
    let f = conversation();
    ui.apply(f.snapshot(true));
    assert_eq!(ui.unread_total(), 0);
    // A fresh connection reports no rooms at first.
    ui.apply(DmSnapshot::default());
    ui.apply(f.snapshot(true));
    assert_eq!(ui.unread_total(), 0, "empty list, then the rooms again");
    // A partial list that lacks the room, and a room listed with its thread briefly missing.
    ui.apply(DmSnapshot {
        room_keys: vec!["other".into()],
        hydrated: true,
        ..Default::default()
    });
    ui.apply(DmSnapshot {
        room_keys: vec![ROOM.into()],
        hydrated: true,
        ..Default::default()
    });
    ui.apply(f.snapshot(true));
    assert_eq!(
        ui.unread_total(),
        0,
        "partial list, thread missing, then back"
    );
    // A message that arrived meanwhile is still news.
    let mut f2 = conversation();
    f2.dms = f.dms.clone();
    f2.compose(PEER, ME, T0 + 9999, b"arrived while the room was missing");
    ui.apply(f2.snapshot(true));
    assert_eq!(ui.unread_total(), 1);
}

fn preview_of(ui: &DmUi) -> String {
    ui.first_preview().to_owned()
}

#[test]
fn a_rail_preview_follows_the_cache_and_an_invitation_s_room_name() {
    // (a) Own newest message: text unavailable, then the cache fills (already hydrated).
    let mut f = Fixture::new();
    f.compose(PEER, ME, T0, b"hello");
    f.compose(ME, PEER, T0 + 5, b"my reply");
    let cache = std::mem::take(&mut f.cache);
    let mut ui = DmUi::default();
    ui.apply(f.snapshot(true));
    assert_eq!(preview_of(&ui), "You: sent, text not available");
    f.cache = cache;
    ui.apply(f.snapshot(true));
    assert_eq!(preview_of(&ui), "You: my reply");

    // (b) An invitation whose room is renamed.
    let mut g = Fixture::new();
    g.compose(PEER, ME, T0, &invite("note"));
    let mut ui = DmUi::default();
    ui.apply(g.snapshot(true));
    assert_eq!(preview_of(&ui), "Invitation to Rust Help");
    let mut snap = g.snapshot(true);
    let mut t = (*snap.threads[0]).clone();
    t.messages[0].body = DmBody::Invite {
        room_label: "Renamed".into(),
        note: None,
    };
    snap.threads[0] = t.into();
    ui.apply(snap);
    assert_eq!(preview_of(&ui), "Invitation to Renamed");
}

#[test]
fn the_rail_badges_add_up_to_the_unread_total_even_for_the_open_thread() {
    let mut rt = app();
    let (f, seed) = conversation_after_seed();
    give(&mut rt, seed);
    give(&mut rt, f.snapshot(true));
    let badge = |rt: &Runtime<RiverApp>| -> usize {
        keys_with(rt, "dm-unread").len() * rt.state.chat.dm.unread_total().min(1)
    };
    assert_eq!(rt.state.chat.dm.unread_total(), 2);
    assert_eq!(badge(&rt), 1);
    // Open but the window is away: still unread, and still badged.
    rt.update(|s, _| s.focus_changed(false));
    press(&mut rt, &row_key());
    assert_eq!(rt.state.chat.dm.unread_total(), 2);
    assert_eq!(badge(&rt), 1, "the open thread's badge matches its count");
    assert!(shows(&rt, "2"));
}

#[test]
fn escape_with_a_link_menu_open_in_a_thread_closes_only_the_menu() {
    let mut rt = app();
    give(&mut rt, conversation().snapshot(true));
    press(&mut rt, &row_key());
    rt.update(|s, _| {
        s.chat.open_link_menu(
            "https://freenet.org/",
            rune::PointerPress { x: 400.0, y: 300.0 },
        );
    });
    rt.frame(0.1);
    assert!(rt.state.chat.link_menu.is_some());
    esc(&mut rt);
    assert!(rt.state.chat.link_menu.is_none());
    assert!(rt.find("dm-thread").is_some(), "the thread stays");
    esc(&mut rt);
    assert!(rt.find("dm-thread").is_none());
}

#[test]
fn opening_a_thread_clears_what_belonged_to_the_room_s_screen() {
    let mut rt = app();
    give(&mut rt, conversation().snapshot(true));
    rt.update(|s, _| {
        s.chat.open_notify_dialog();
        s.chat.open_details();
        s.chat.deputy_confirm = Some(("x".into(), true));
    });
    rt.update(|s, _| {
        assert!(s.chat.open_dm(ROOM, &peer_uid()));
        assert!(s.chat.deputy_confirm.is_none() && s.chat.details.is_none());
    });
    rt.frame(0.1);
    assert!(rt.find("dm-thread").is_some());
}

#[test]
fn names_and_previews_drop_hidden_and_reordering_characters() {
    let mut f = Fixture::new();
    f.compose(PEER, ME, T0, "hi\u{202E}evil\u{200B}x".as_bytes());
    let mut snap = f.snapshot(true);
    let mut t = (*snap.threads[0]).clone();
    t.peer_name = "Pri\u{202E}ya\u{200B}".into();
    snap.threads[0] = t.into();
    let mut rt = app();
    give(&mut rt, snap);
    assert!(
        texts(&rt)
            .iter()
            .all(|t| !t.contains('\u{202E}') && !t.contains('\u{200B}'))
    );
    press(&mut rt, &row_key());
    assert!(shows(&rt, "Priya"));
}

#[test]
fn opening_a_thread_closes_the_member_box_and_the_message_search_without_focusing_a_hidden_composer()
 {
    for search_kind in ["members", "messages"] {
        let mut rt = app();
        give(&mut rt, conversation().snapshot(true));
        rt.update(|s, _| s.chat.composer.focused = true);
        rt.frame(0.1);
        rt.update(|s, _| {
            if search_kind == "members" {
                s.chat.toggle_member_search();
            } else {
                assert!(s.chat.search_chord());
            }
        });
        rt.frame(0.1);
        let open = |rt: &Runtime<RiverApp>| {
            rt.state.chat.member_search.open || rt.state.chat.search.is_some()
        };
        assert!(open(&rt), "{search_kind} search is open");
        press(&mut rt, &row_key());
        assert!(rt.find("dm-thread").is_some());
        let c = &rt.state.chat;
        assert!(!open(&rt), "{search_kind}: closed by the thread");
        assert!(
            !c.member_search.query.focused,
            "{search_kind}: no stale flag"
        );
        assert!(
            !c.composer.focused,
            "{search_kind}: not into a hidden composer"
        );
        // Frames and hooks never hand the keyboard to the hidden composer either.
        rt.update(|s, _| {
            s.chat.close_member_search_if_hidden();
            s.chat.close_search_under_dialog();
            s.chat.focus_composer();
        });
        for i in 0..3 {
            rt.frame(1.0 + f64::from(i));
        }
        assert!(!rt.state.chat.composer.focused, "{search_kind}");
        assert!(!rt.state.chat.member_search.query.focused);
    }
}

/// A snapshot of `f` whose room's state has not loaded: listed with a stub, so not in
/// `room_keys`.
fn unloaded(f: &Fixture) -> DmSnapshot {
    DmSnapshot {
        room_keys: Vec::new(),
        ..f.snapshot(true)
    }
}

#[test]
fn the_first_message_in_a_room_with_none_at_startup_is_unread() {
    let mut ui = DmUi::default();
    // A stored room whose state is loaded and holds no direct messages.
    ui.apply(empty_rooms());
    let mut f = Fixture::new();
    f.compose(PEER, ME, T0, b"the first ever");
    ui.apply(f.snapshot(true));
    assert_eq!(ui.unread_total(), 1);
}

#[test]
fn a_room_listed_before_its_state_loads_is_seeded_when_its_state_and_history_arrive() {
    let mut ui = DmUi::default();
    let mut f = Fixture::new();
    f.compose(PEER, ME, T0, b"history");
    // Listed with a stub that already shows a message: not met, and not unread.
    ui.apply(unloaded(&f));
    assert_eq!(ui.unread_total(), 0);
    // The state arrives with the backlog: seeded.
    ui.apply(f.snapshot(true));
    assert_eq!(ui.unread_total(), 0, "the history is seeded");
    f.compose(PEER, ME, T0 + 5, b"news");
    ui.apply(f.snapshot(true));
    assert_eq!(ui.unread_total(), 1);
    // Also when the stub has nothing and the backlog comes with the state.
    let mut ui = DmUi::default();
    let mut g = Fixture::new();
    ui.apply(unloaded(&g));
    g.compose(PEER, ME, T0, b"backlog with the state");
    ui.apply(g.snapshot(true));
    assert_eq!(ui.unread_total(), 0);
}

#[test]
fn the_seen_sets_are_cut_to_present_threads_and_an_empty_snapshot_keeps_them() {
    let mut ui = DmUi::default();
    let mut f = Fixture::new();
    f.compose(PEER, ME, T0, b"a");
    f.compose(PEER, ME, T0 + 1, b"b");
    ui.apply(f.snapshot(true));
    let held = |ui: &DmUi| ui.seen.values().map(|s| s.len()).sum::<usize>();
    assert_eq!(held(&ui), 2);
    ui.apply(DmSnapshot::default());
    ui.apply(empty_rooms());
    assert_eq!(held(&ui), 2, "an empty or partial snapshot forgets nothing");
    // The thread is present with one message fewer: only that one is kept.
    let mut snap = f.snapshot(true);
    let mut t = (*snap.threads[0]).clone();
    t.messages.truncate(1);
    snap.threads[0] = t.into();
    ui.apply(snap);
    assert_eq!(held(&ui), 1);
}

/// Open the thread as a shortcut does (a click on the rail would itself take the keyboard from
/// the composer before the thread opened).
fn open(rt: &mut Runtime<RiverApp>) {
    rt.update(|s, _| assert!(s.chat.open_dm(ROOM, &peer_uid())));
    rt.frame(0.1);
}

#[test]
fn the_composer_gets_the_keyboard_back_when_a_thread_closes() {
    // Back.
    let mut rt = app();
    give(&mut rt, conversation().snapshot(true));
    rt.update(|s, _| s.chat.composer.focused = true);
    rt.frame(0.1);
    open(&mut rt);
    assert!(!rt.state.chat.composer.focused);
    press(&mut rt, "dm-back");
    assert!(rt.state.chat.composer.focused, "Back restores it");

    // Not focused before: not focused after.
    rt.update(|s, _| s.chat.composer.focused = false);
    open(&mut rt);
    press(&mut rt, "dm-back");
    assert!(!rt.state.chat.composer.focused);

    // Ctrl+F from a thread, then Escape: the composer, as before the thread.
    rt.update(|s, _| s.chat.composer.focused = true);
    rt.frame(0.1);
    open(&mut rt);
    rt.update(|s, _| assert!(s.chat.search_chord()));
    rt.frame(0.1);
    assert!(
        rt.find("dm-thread").is_none(),
        "the chord went back to the room"
    );
    assert!(rt.state.chat.search.is_some());
    esc(&mut rt);
    assert!(rt.state.chat.search.is_none());
    assert!(rt.state.chat.composer.focused, "Escape gives it back");

    // A member search that had been open when the thread opened also restores it.
    rt.update(|s, _| {
        s.chat.composer.focused = true;
        s.chat.toggle_member_search();
    });
    open(&mut rt);
    press(&mut rt, "dm-back");
    assert!(rt.state.chat.composer.focused);

    // A thread closed while a dialog has the keyboard gives nothing back.
    rt.update(|s, _| s.chat.composer.focused = true);
    open(&mut rt);
    rt.update(|s, _| {
        s.chat.shortcuts_open = true;
        s.chat.close_dm();
    });
    assert!(!rt.state.chat.composer.focused, "a dialog has the keyboard");
    // And one that another room closes does not either.
    rt.update(|s, _| {
        s.chat.shortcuts_open = false;
        s.chat.composer.focused = true;
    });
    open(&mut rt);
    let room = rt.state.chat.room().unwrap().key.clone();
    rt.update(move |s, _| s.chat.select(&room));
    assert!(!rt.state.chat.composer.focused);
    // An edit cannot start behind the thread.
    open(&mut rt);
    rt.update(|s, _| s.chat.start_edit("anything"));
    assert!(!rt.state.chat.composer.focused);
}
