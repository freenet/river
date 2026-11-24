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
use super::dm_view::{DmUi, LOADING_SENT, TRUNCATED};
use crate::rooms::dm::{self, Source};
use crate::rooms::dm_cache::OutboundCache;
use crate::{RiverApp, Setup, WINDOW_SIZE, view};

pub(super) const OWNER: u8 = 1;
pub(super) const ME: u8 = 2;
pub(super) const PEER: u8 = 3;
pub(super) const ROOM: &str = "town-square";
pub(super) const T0: u64 = 1_700_000_000;

pub(super) fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

pub(super) fn vk(n: u8) -> VerifyingKey {
    sk(n).verifying_key()
}

pub(super) fn id(n: u8) -> MemberId {
    MemberId::from(&vk(n))
}

/// A room with the owner, the user and a peer, and the messages between them.
pub(super) struct Fixture {
    members: ChatRoomStateV1,
    dms: DirectMessagesV1,
    cache: OutboundCache,
}

impl Fixture {
    pub(super) fn new() -> Self {
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

    pub(super) fn compose(&mut self, from: u8, to: u8, ts: u64, body: &[u8]) {
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
    pub(super) fn snapshot(&self, hydrated: bool) -> DmSnapshot {
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
            purges: Vec::new(),
            archived: Vec::new(),
        }
    }
}

/// The rooms are listed and hold no messages yet: what arrives afterwards is news.
pub(super) fn empty_rooms() -> DmSnapshot {
    DmSnapshot {
        room_keys: vec![ROOM.to_owned()],
        hydrated: true,
        ..Default::default()
    }
}

pub(super) fn peer_key() -> String {
    dm::peer_key(id(PEER))
}

pub(super) fn app() -> Runtime<RiverApp> {
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

pub(super) fn give(rt: &mut Runtime<RiverApp>, snapshot: DmSnapshot) {
    rt.update(move |s, _| s.chat.apply_dms(snapshot));
    // Two frames: what went away is gone from the tree after the second.
    rt.frame(0.1);
    rt.frame(0.6);
}

pub(super) fn press(rt: &mut Runtime<RiverApp>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("no element {key}"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.1);
    rt.frame(0.2);
}

pub(super) fn esc(rt: &mut Runtime<RiverApp>) {
    rt.key(KeyEvent {
        key: Key::Escape,
        mods: Default::default(),
    });
    rt.frame(0.3);
}

pub(super) fn texts(rt: &Runtime<RiverApp>) -> Vec<String> {
    rt.elements().into_iter().filter_map(|e| e.text).collect()
}

pub(super) fn shows(rt: &Runtime<RiverApp>, needle: &str) -> bool {
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
pub(super) fn keys_with(rt: &Runtime<RiverApp>, prefix: &str) -> Vec<String> {
    rt.elements()
        .into_iter()
        .filter_map(|e| e.key)
        .filter(|k| k.starts_with(prefix))
        .collect()
}

pub(super) fn row_key() -> String {
    format!("dm-{ROOM}-{}", peer_key())
}

/// A room with a conversation: the peer writes first and last, the user answers between.
pub(super) fn conversation() -> Fixture {
    let mut f = Fixture::new();
    f.compose(PEER, ME, T0, b"first from the peer, with **bold**");
    f.compose(ME, PEER, T0 + 60, b"my answer");
    f.compose(PEER, ME, T0 + 3600, b"last from the peer");
    f
}

/// A room whose backlog (one old message from the peer) has been met, and the conversation that
/// arrives after it: the fixture, and the snapshot of the backlog alone.
pub(super) fn conversation_after_seed() -> (Fixture, DmSnapshot) {
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
    // A box of its own to write in, not the room's composer.
    assert!(rt.find("dm-composer-field").is_some());
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
    assert!(rt.find("dm-composer-field").is_some());
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
        .full_id
        .to_string();
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
    ui.open = Some((ROOM.to_owned(), peer_key()));
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
    assert!(chat.open_dm(&key, &peer_key()));
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
    assert_eq!(order, [dm::peer_key(id(OWNER)), peer_key()]);
    // Reading the newer one puts the other first, as it is the one still unread.
    rt.update(|s, _| {
        s.chat.open_dm(ROOM, &dm::peer_key(id(OWNER)));
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
    assert_eq!(order, [peer_key(), dm::peer_key(id(OWNER))]);
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
fn an_invitation_card_shows_the_peers_words_in_plain_text_and_offers_accept() {
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
    // It is offered for accepting (the example sends nothing: see `dm_invite_tests`), and the
    // build no longer says it is not available.
    assert_eq!(keys_with(&rt, "dm-invite-accept-").len(), 1);
    assert!(!shows(&rt, "not available yet in this build"));
}

#[test]
fn a_rail_preview_is_one_bounded_plain_line() {
    let long = format!("**bold** {}\u{7}\nsecond line", "word ".repeat(100));
    let thread = |body: DmBody, outgoing: bool| DmThread {
        room: ROOM.into(),
        room_name: "Town Square".into(),
        peer_key: "7".into(),
        peer: id(PEER),
        peer_name: "Priya".into(),
        peer_is_member: true,
        messages: vec![super::dm_model::DmMessage {
            key: "k".into(),
            outgoing,
            timestamp: T0,
            body,
            pending: None,
        }],
        last_ts: T0,
        last_inbound_ts: T0,
        last_inbound_signed: T0,
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
                room: "rust-help".into(),
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
    for file in [
        "chat/dm_view.rs",
        "chat/dm_compose.rs",
        "chat/dm_archive.rs",
    ] {
        let view = code(&read(file));
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
            assert!(!view.contains(banned), "{file} uses {banned}");
        }
    }
    // The composer never touches a delegate or signs: it only hands a command to the work.
    let compose = code(&read("chat/dm_compose.rs"));
    for banned in [
        "ClientRequest",
        "StoreRequest",
        "SigningKey",
        "compose_direct_message",
        "rooms::delegate",
        "rooms::socket",
    ] {
        assert!(!compose.contains(banned), "dm_compose.rs uses {banned}");
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
            "dm_archive",
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
    ui.open = Some((ROOM.to_owned(), peer_key()));
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
        room: "rust-help".into(),
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
        assert!(s.chat.open_dm(ROOM, &peer_key()));
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
    rt.update(|s, _| assert!(s.chat.open_dm(ROOM, &peer_key())));
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

// Sending ----------------------------------------------------------------------------------------

mod sending {
    use rune::{Key, KeyEvent, Mods, Runtime};

    use super::super::dm_model::{DmBody, DmMessage, DmSnapshot, DmThread};
    use super::super::model::Pending;
    use super::super::{LinkStatus, Mode, Source};
    use super::{T0, app, esc, give, id, press, shows, texts};
    use crate::RiverApp;
    use crate::rooms::outbox::Action;

    const ROOM: &str = "town-square";

    fn type_text(rt: &mut Runtime<RiverApp>, text: &str) {
        rt.key(KeyEvent {
            key: Key::Text(text.into()),
            mods: Mods::default(),
        });
        rt.frame(0.3);
    }

    fn enter(rt: &mut Runtime<RiverApp>, shift: bool) {
        rt.key(KeyEvent {
            key: Key::Enter,
            mods: Mods {
                shift,
                ..Mods::default()
            },
        });
        rt.frame(0.3);
    }

    /// The example room's members other than the user, by nickname.
    fn uid_of(rt: &Runtime<RiverApp>, name: &str) -> String {
        rt.state
            .chat
            .room()
            .unwrap()
            .members
            .iter()
            .find(|m| m.nickname == name)
            .unwrap()
            .full_id
            .to_string()
    }

    /// The app reading rooms from a node, with the thread with `name` open and its box focused.
    fn open_with(name: &str) -> (Runtime<RiverApp>, String) {
        let mut rt = app();
        rt.update(|s, _| s.chat.source = Source::Node(LinkStatus::Live));
        let uid = uid_of(&rt, name);
        let room = rt.state.chat.room().unwrap().key.clone();
        {
            let uid = uid.clone();
            rt.update(move |s, _| assert!(s.chat.open_dm(&room, &uid)));
        }
        rt.frame(0.1);
        rt.frame(0.2);
        (rt, uid)
    }

    fn sent(rt: &Runtime<RiverApp>) -> Vec<(String, Action)> {
        rt.state
            .chat
            .queued_commands()
            .iter()
            .map(|c| (c.room.clone(), c.action.clone()))
            .collect()
    }

    fn the_box(rt: &Runtime<RiverApp>, room: &str, peer: &str) -> String {
        rt.state
            .chat
            .dm
            .compose
            .draft(room, peer)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn the_box_sends_on_enter_keeps_its_text_until_the_answer_and_clears_on_success() {
        let (mut rt, uid) = open_with("Jonas");
        assert!(rt.find("dm-composer-field").is_some());
        assert!(rt.state.chat.dm.compose_edit_focused());
        press(&mut rt, "dm-composer-field");
        type_text(&mut rt, "  Hello Jonas  ");
        assert_eq!(the_box(&rt, ROOM, &uid), "  Hello Jonas  ");
        assert!(sent(&rt).is_empty());
        enter(&mut rt, false);

        // One command, with the trimmed text and the thread's room and peer. Nothing went to the
        // room's own send.
        let commands = sent(&rt);
        assert_eq!(commands.len(), 1);
        let (room, action) = commands[0].clone();
        assert_eq!(room, ROOM);
        let Action::SendDm { peer, text, ticket } = action else {
            panic!("a direct message, not {action:?}")
        };
        assert_eq!(
            (crate::rooms::dm::peer_key(peer).as_str(), text.as_str()),
            (uid.as_str(), "  Hello Jonas  ")
        );
        // The text stays in the box while the answer is awaited, and a second Enter sends
        // nothing more.
        assert_eq!(the_box(&rt, ROOM, &uid), "  Hello Jonas  ");
        assert!(rt.find("dm-sending").is_some());
        enter(&mut rt, false);
        assert_eq!(sent(&rt).len(), 1);
        // An answer to another send is not this one's.
        rt.update(|s, _| {
            s.chat.dm_sent(ROOM, "x", 999, Ok(()));
        });
        assert_eq!(the_box(&rt, ROOM, &uid), "  Hello Jonas  ");

        let peer = crate::rooms::dm::peer_key(peer);
        rt.update(move |s, _| {
            s.chat.dm_sent(ROOM, &peer, ticket, Ok(()));
        });
        rt.frame(0.2);
        assert_eq!(the_box(&rt, ROOM, &uid), "");
        assert!(rt.find("dm-sending").is_none());
        assert!(rt.state.chat.dm.compose.refusal_for(ROOM, &uid).is_none());
    }

    #[test]
    fn a_refused_send_keeps_the_text_and_says_why_and_it_can_be_sent_again() {
        let (mut rt, uid) = open_with("Jonas");
        press(&mut rt, "dm-composer-field");
        type_text(&mut rt, "keep this");
        enter(&mut rt, false);
        let Action::SendDm { peer, ticket, .. } = sent(&rt)[0].1.clone() else {
            panic!()
        };
        let why = "That person is not a member of this room now.".to_owned();
        let peer = crate::rooms::dm::peer_key(peer);
        {
            let (peer, why) = (peer.clone(), why.clone());
            rt.update(move |s, _| {
                s.chat.dm_sent(ROOM, &peer, ticket, Err(why));
            });
        }
        rt.frame(0.2);
        assert_eq!(the_box(&rt, ROOM, &uid), "keep this");
        assert!(shows(&rt, &why), "the reason is on screen");
        assert!(rt.find("dm-error").is_some());
        // Sending again clears the old reason and sends with a new ticket.
        enter(&mut rt, false);
        let commands = sent(&rt);
        assert_eq!(commands.len(), 2);
        let Action::SendDm { ticket: again, .. } = commands[1].1.clone() else {
            panic!()
        };
        assert_ne!(again, ticket);
        assert!(rt.find("dm-error").is_none());
        // The reason belongs to its thread: another thread does not show it.
        rt.update(move |s, _| {
            s.chat.dm_sent(ROOM, &peer, again, Err("again".into()));
        });
        rt.frame(0.2);
        assert!(rt.find("dm-error").is_some());
        let other = {
            let r = rt.state.chat.room().unwrap();
            r.members
                .iter()
                .find(|m| m.id != r.me && m.full_id.to_string() != uid)
                .unwrap()
                .full_id
                .to_string()
        };
        rt.update(move |s, _| assert!(s.chat.open_dm(ROOM, &other)));
        rt.frame(0.2);
        assert!(rt.find("dm-error").is_none());
    }

    #[test]
    fn shift_enter_adds_a_line_and_blank_text_sends_nothing() {
        let (mut rt, uid) = open_with("Jonas");
        press(&mut rt, "dm-composer-field");
        enter(&mut rt, false);
        type_text(&mut rt, "   ");
        enter(&mut rt, false);
        assert!(sent(&rt).is_empty(), "nothing to say");
        rt.update(|s, _| s.chat.dm.compose_edit().replace_text(""));
        type_text(&mut rt, "one");
        enter(&mut rt, true);
        type_text(&mut rt, "two");
        assert_eq!(the_box(&rt, ROOM, &uid), "one\ntwo");
        assert!(sent(&rt).is_empty());
        enter(&mut rt, false);
        let Action::SendDm { text, .. } = sent(&rt)[0].1.clone() else {
            panic!()
        };
        assert_eq!(text, "one\ntwo");
    }

    #[test]
    fn the_cap_is_counted_in_bytes_and_blocks_the_send_with_its_reason() {
        let (mut rt, uid) = open_with("Jonas");
        let cap = crate::rooms::dm_send::BODY_CAP;
        press(&mut rt, "dm-composer-field");
        // Under the cap but near it: a counter, no complaint.
        rt.update(move |s, _| s.chat.dm.compose_edit().replace_text("a".repeat(cap - 5)));
        rt.frame(0.2);
        assert!(shows(&rt, &format!("{}/{cap}", cap - 5)));
        assert!(!rt.state.chat.dm_over_limit());
        // A character of three bytes pushes it over: counted in bytes, not characters.
        rt.update(move |s, _| {
            s.chat
                .dm
                .compose_edit()
                .replace_text(format!("{}\u{20ac}\u{20ac}", "a".repeat(cap - 5)))
        });
        rt.frame(0.2);
        assert!(rt.state.chat.dm_over_limit());
        assert!(shows(&rt, "Message too long"));
        enter(&mut rt, false);
        assert!(sent(&rt).is_empty(), "too long is not sent");
        assert!(!the_box(&rt, ROOM, &uid).is_empty(), "and the text stays");
        // Exactly the cap is sent.
        rt.update(move |s, _| s.chat.dm.compose_edit().replace_text("a".repeat(cap)));
        enter(&mut rt, false);
        assert_eq!(sent(&rt).len(), 1);
    }

    #[test]
    fn a_room_that_does_not_let_you_send_gives_a_box_that_cannot_and_says_why() {
        for block in [
            "This computer doesn't hold your key for this room, so you can't send messages here.",
            crate::rooms::generation::UPDATE_REQUIRED_SEND,
        ] {
            let (mut rt, uid) = open_with("Jonas");
            let mut rooms = rt.state.chat.rooms().to_vec();
            for r in &mut rooms {
                r.send_block = Some(block.into());
            }
            rt.update(move |s, _| s.chat.set_rooms(rooms));
            rt.frame(0.2);
            assert!(rt.find("dm-thread").is_some(), "the thread is still there");
            assert!(shows(&rt, block));
            assert!(rt.find("dm-block").is_some());
            rt.update(|s, _| s.chat.dm.compose_edit().replace_text("hello"));
            assert!(!rt.state.chat.dm_send(), "nothing is sent");
            enter(&mut rt, false);
            assert!(sent(&rt).is_empty());
            assert_eq!(the_box(&rt, ROOM, &uid), "hello", "and the text stays");
        }
    }

    #[test]
    fn the_message_button_still_opens_the_thread_where_the_room_does_not_let_you_send() {
        let mut rt = app();
        rt.update(|s, _| s.chat.source = Source::Node(LinkStatus::Live));
        let uid = uid_of(&rt, "Jonas");
        let id = rt
            .state
            .chat
            .room()
            .unwrap()
            .member_by_peer_key(&uid)
            .unwrap()
            .id;
        let mut rooms = rt.state.chat.rooms().to_vec();
        for r in &mut rooms {
            r.send_block = Some("A newer version of River is out.".into());
        }
        rt.update(move |s, _| s.chat.set_rooms(rooms));
        rt.frame(0.2);
        press(&mut rt, &format!("member-{id}"));
        let button = rt
            .elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some("member-dm"))
            .expect("the Message button");
        assert!(button.opacity > 0.9, "not dimmed");
        press(&mut rt, "member-dm");
        // The thread opens to be read; the composer is what refuses.
        assert!(rt.find("dm-thread").is_some());
        assert!(rt.find("dm-block").is_some());
        rt.update(|s, _| s.chat.dm_write("kept"));
        assert!(!rt.state.chat.dm_send());
        assert!(sent(&rt).is_empty());
        assert_eq!(the_box(&rt, ROOM, &uid), "kept");
    }

    /// Two members with the same label (`uid`, the first 40 bits of the id) and different full
    /// ids, as a member who ground a key to imitate another would make. The test builds them by
    /// giving two members one label.
    fn with_twin_labels(rt: &mut Runtime<RiverApp>) -> (String, String) {
        let mut rooms = rt.state.chat.rooms().to_vec();
        let room = rooms.iter_mut().find(|r| r.key == ROOM).unwrap();
        let (a, b) = (
            room.members
                .iter()
                .position(|m| m.nickname == "Jonas")
                .unwrap(),
            room.members
                .iter()
                .position(|m| m.nickname == "Priya Raman")
                .unwrap(),
        );
        room.members[b].uid = room.members[a].uid.clone();
        let keys = (
            room.members[a].full_id.to_string(),
            room.members[b].full_id.to_string(),
        );
        assert_ne!(keys.0, keys.1);
        rt.update(move |s, _| s.chat.set_rooms(rooms));
        rt.frame(0.2);
        keys
    }

    #[test]
    fn members_with_one_label_have_their_own_threads_and_dialogs() {
        let mut rt = app();
        rt.update(|s, _| s.chat.source = Source::Node(LinkStatus::Live));
        let (a, b) = with_twin_labels(&mut rt);
        // Their threads are two, whatever their label: each opens with its own messages and
        // sends to its own full id.
        let thread = |key: &str, text: &str| DmThread {
            room: ROOM.into(),
            room_name: "Town Square".into(),
            peer_key: key.to_owned(),
            peer: crate::rooms::dm::peer_from_key(key).unwrap(),
            peer_name: "Same Name".into(),
            peer_is_member: true,
            messages: vec![DmMessage {
                key: format!("m-{key}"),
                outgoing: false,
                timestamp: T0,
                body: DmBody::Text(text.into()),
                pending: None,
            }],
            last_ts: T0,
            last_inbound_ts: T0,
            last_inbound_signed: T0,
        };
        give(
            &mut rt,
            DmSnapshot {
                threads: vec![
                    thread(&a, "for the first").into(),
                    thread(&b, "for the second").into(),
                ],
                room_keys: vec![ROOM.to_owned()],
                hydrated: true,
                ..Default::default()
            },
        );
        assert_eq!(rt.state.chat.dm.rail_order().len(), 2, "not merged");
        // Two rows with one name are told apart by a slice of the full id.
        assert!(
            shows(&rt, "Same Name ("),
            "the rail suffixes the shared name"
        );
        let rows = texts(&rt)
            .iter()
            .filter(|t| t.starts_with("Same Name ("))
            .count();
        assert_eq!(rows, 2);
        for (key, text, other) in [
            (&a, "for the first", "for the second"),
            (&b, "for the second", "for the first"),
        ] {
            let k = key.clone();
            rt.update(move |s, _| assert!(s.chat.open_dm(ROOM, &k)));
            rt.frame(0.2);
            assert!(shows(&rt, text));
            assert!(
                !rt.state
                    .chat
                    .dm_block()
                    .is_some_and(|w| w.contains("not a member"))
            );
            rt.update(|s, _| s.chat.dm_write("hello"));
            assert!(rt.state.chat.dm_send());
            let Action::SendDm { peer, .. } = sent(&rt).last().unwrap().1.clone() else {
                panic!()
            };
            assert_eq!(
                crate::rooms::dm::peer_key(peer),
                *key,
                "to the intended full id"
            );
            // (the pane lists the other thread's text only in the rail's preview)
            let _ = other;
            rt.update(|s, _| s.chat.close_dm());
            rt.frame(0.2);
            rt.frame(0.6);
        }
        // Each twin has their own dialog, found by full id, and its Message button goes to that
        // twin: the second one's dialog shows the second one, whatever the label is shared with.
        for (full, name) in [(&a, "Jonas"), (&b, "Priya Raman"), (&a, "Jonas")] {
            let id = rt
                .state
                .chat
                .room()
                .unwrap()
                .member_by_peer_key(full)
                .unwrap()
                .id;
            press(&mut rt, &format!("member-{id}"));
            assert_eq!(
                rt.state.chat.member_modal_full,
                Some(full.parse::<i64>().unwrap())
            );
            let dialog = rt.find("member-info-dialog").expect("the dialog");
            let _ = dialog;
            assert!(shows(&rt, name), "the dialog shows {name}");
            let button = rt
                .elements()
                .into_iter()
                .find(|e| e.key.as_deref() == Some("member-dm"))
                .expect("the Message button");
            assert!(button.opacity > 0.9, "enabled for {name}");
            // The label is shared, and the dialog says so; moderation is not offered for it.
            assert!(rt.find("member-dm-ambiguous").is_some());
            assert!(rt.find("member-ban").is_none());
            press(&mut rt, "member-dm");
            assert_eq!(
                rt.state.chat.dm.open_key(),
                Some((ROOM, full.as_str())),
                "the message goes to {name}"
            );
            rt.update(|s, _| s.chat.close_dm());
            rt.frame(0.2);
            rt.frame(0.6);
        }
    }

    #[test]
    fn each_thread_keeps_its_draft_and_the_rooms_own_composer_is_never_touched() {
        let mut rt = app();
        rt.update(|s, _| s.chat.source = Source::Node(LinkStatus::Live));
        // A reply started in the room, with text, before any thread is opened.
        let target = rt.state.chat.room().unwrap().messages[0].key.clone();
        {
            let target = target.clone();
            rt.update(move |s, _| {
                s.chat.start_reply(&target);
                s.chat.composer.replace_text("the room's reply");
            });
        }
        let (jonas, maya) = (uid_of(&rt, "Jonas"), uid_of(&rt, "Priya Raman"));
        let open = |rt: &mut Runtime<RiverApp>, uid: &str| {
            let uid = uid.to_owned();
            rt.update(move |s, _| assert!(s.chat.open_dm(ROOM, &uid)));
            rt.frame(0.2);
        };
        open(&mut rt, &jonas);
        press(&mut rt, "dm-composer-field");
        type_text(&mut rt, "to Jonas");
        open(&mut rt, &maya);
        assert_eq!(
            the_box(&rt, ROOM, &jonas),
            "to Jonas",
            "kept in the other thread"
        );
        assert_eq!(the_box(&rt, ROOM, &maya), "");
        type_text(&mut rt, "to Maya");
        open(&mut rt, &jonas);
        assert_eq!(the_box(&rt, ROOM, &jonas), "to Jonas");
        assert_eq!(the_box(&rt, ROOM, &maya), "to Maya");
        enter(&mut rt, false);
        // The direct message went as one, to Jonas; the room's reply is exactly as it was.
        let commands = sent(&rt);
        assert_eq!(commands.len(), 1);
        assert!(matches!(&commands[0].1, Action::SendDm { peer, text, .. }
            if crate::rooms::dm::peer_key(*peer) == jonas && text == "to Jonas"));
        assert_eq!(rt.state.chat.composer.text, "the room's reply");
        assert!(matches!(&rt.state.chat.mode, Mode::Reply(t) if *t == target));
        // Closing the thread keeps the drafts, and the room is as it was.
        press(&mut rt, "dm-back");
        assert_eq!(rt.state.chat.composer.text, "the room's reply");
        assert!(matches!(&rt.state.chat.mode, Mode::Reply(t) if *t == target));
        assert_eq!(the_box(&rt, ROOM, &maya), "to Maya");
        // Sending in the room sends the reply, not a direct message.
        press(&mut rt, "composer-field");
        assert!(rt.state.chat.send());
        let commands = sent(&rt);
        assert!(matches!(
            &commands[1].1,
            Action::Send { text, reply_to: Some(t) } if text == "the room's reply" && *t == target
        ));
        // A draft that holds nothing is not kept.
        open(&mut rt, &jonas);
        rt.update(|s, _| s.chat.dm.compose_edit().clear());
        press(&mut rt, "dm-back");
        assert!(rt.state.chat.dm.compose.draft(ROOM, &jonas).is_none());
    }

    fn pending_thread(rt: &mut Runtime<RiverApp>, uid: &str, p: Option<Pending>, text: &str) {
        let me_thread = DmThread {
            room: ROOM.into(),
            room_name: "Town Square".into(),
            peer_key: uid.to_owned(),
            peer: id(3),
            peer_name: "Jonas".into(),
            peer_is_member: true,
            messages: vec![DmMessage {
                key: "tok".into(),
                outgoing: true,
                timestamp: T0,
                body: DmBody::Text(text.into()),
                pending: p,
            }],
            last_ts: T0,
            last_inbound_ts: 0,
            last_inbound_signed: 0,
        };
        let snapshot = DmSnapshot {
            threads: vec![me_thread.into()],
            room_keys: vec![ROOM.to_owned()],
            hydrated: true,
            ..Default::default()
        };
        give(rt, snapshot);
    }

    #[test]
    fn a_message_on_its_way_says_so_and_a_failed_one_offers_retry_and_discard() {
        let (mut rt, uid) = open_with("Jonas");
        pending_thread(
            &mut rt,
            &uid,
            Some(Pending {
                entry: 4,
                failed: None,
                retry: false,
            }),
            "on its way",
        );
        assert!(shows(&rt, "on its way"));
        assert!(shows(&rt, "Sending\u{2026}"));
        assert!(rt.find("dm-retry-4").is_none());

        pending_thread(
            &mut rt,
            &uid,
            Some(Pending {
                entry: 4,
                failed: Some("Freenet refused it: no".into()),
                retry: true,
            }),
            "on its way",
        );
        assert!(shows(&rt, "Not sent. Freenet refused it: no"));
        press(&mut rt, "dm-retry-4");
        press(&mut rt, "dm-discard-4");
        let commands = sent(&rt);
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0], (ROOM.to_owned(), Action::Retry { entry: 4 }));
        assert_eq!(commands[1], (ROOM.to_owned(), Action::Discard { entry: 4 }));

        // One that cannot be retried offers only Discard, and says what became of it.
        pending_thread(
            &mut rt,
            &uid,
            Some(Pending {
                entry: 5,
                failed: Some("Freenet took it, but the room here doesn't show it.".into()),
                retry: false,
            }),
            "on its way",
        );
        assert!(rt.find("dm-retry-5").is_none());
        assert!(rt.find("dm-discard-5").is_some());
        // Once the room holds it, no state is shown.
        pending_thread(&mut rt, &uid, None, "on its way");
        assert!(!shows(&rt, "Sending"));
        assert!(rt.find("dm-not-sent").is_none());
    }

    #[test]
    fn an_answer_only_empties_a_box_that_still_holds_exactly_what_was_sent() {
        let (mut rt, uid) = open_with("Jonas");
        let answer = |rt: &mut Runtime<RiverApp>, ticket: u64, result: Result<(), String>| {
            let uid = uid.clone();
            rt.update(move |s, _| {
                s.chat.dm_sent(ROOM, &uid, ticket, result);
            });
        };
        let ticket = |rt: &Runtime<RiverApp>, n: usize| {
            let Action::SendDm { ticket, .. } = sent(rt)[n].1.clone() else {
                panic!()
            };
            ticket
        };
        // Untouched since Send: emptied.
        rt.update(|s, _| s.chat.dm_write("first"));
        assert!(rt.state.chat.dm_send());
        let t1 = ticket(&rt, 0);
        answer(&mut rt, t1, Ok(()));
        assert_eq!(the_box(&rt, ROOM, &uid), "");

        // Typed into since Send: left exactly as it is. The answer does not guess what is
        // already sent and what is new.
        rt.update(|s, _| s.chat.dm_write("second"));
        assert!(rt.state.chat.dm_send());
        let t2 = ticket(&rt, 1);
        rt.update(|s, _| s.chat.dm_write("second and then more"));
        answer(&mut rt, t2, Ok(()));
        assert_eq!(the_box(&rt, ROOM, &uid), "second and then more");

        // A late answer for an older send never mutates what was written after a newer one.
        rt.update(|s, _| s.chat.dm_write("ok"));
        assert!(rt.state.chat.dm_send());
        let t3 = ticket(&rt, 2);
        rt.update(|s, _| s.chat.dm_write("ok thanks"));
        answer(&mut rt, t3, Ok(()));
        assert_eq!(the_box(&rt, ROOM, &uid), "ok thanks");
    }

    #[test]
    fn the_box_is_read_only_while_a_send_waits_and_editable_again_when_it_is_answered_or_times_out()
    {
        use std::time::Instant;
        let (mut rt, uid) = open_with("Jonas");
        let t0 = Instant::now();
        let ticket = |rt: &Runtime<RiverApp>, n: usize| {
            let Action::SendDm { ticket, .. } = sent(rt)[n].1.clone() else {
                panic!()
            };
            ticket
        };
        let field_dim = |rt: &Runtime<RiverApp>| {
            rt.elements()
                .into_iter()
                .find(|e| e.key.as_deref() == Some("dm-composer-input"))
                .map(|e| e.opacity < 0.9)
                .unwrap()
        };
        press(&mut rt, "dm-composer-field");
        type_text(&mut rt, "first");
        assert!(!field_dim(&rt));
        assert!(rt.state.chat.dm_send_at(t0));
        rt.invalidate();
        rt.frame(0.2);
        rt.frame(0.8);
        // Pending: typing goes nowhere, the box looks it, and says so.
        assert!(field_dim(&rt));
        assert!(rt.find("dm-sending").is_some());
        type_text(&mut rt, " more");
        assert_eq!(the_box(&rt, ROOM, &uid), "first");
        // Ok: the box is cleared and editable.
        let t1 = ticket(&rt, 0);
        {
            let u = uid.clone();
            rt.update(move |s, _| {
                s.chat.dm_sent(ROOM, &u, t1, Ok(()));
            });
        }
        rt.invalidate();
        rt.frame(0.2);
        rt.frame(0.8);
        assert_eq!(the_box(&rt, ROOM, &uid), "");
        assert!(!field_dim(&rt));
        press(&mut rt, "dm-composer-field");
        type_text(&mut rt, "second");
        assert_eq!(the_box(&rt, ROOM, &uid), "second");

        // Err: the draft stays, and is editable, with the refusal shown.
        assert!(rt.state.chat.dm_send_at(t0));
        let t2 = ticket(&rt, 1);
        {
            let u = uid.clone();
            rt.update(move |s, _| {
                s.chat.dm_sent(ROOM, &u, t2, Err("refused".into()));
            });
        }
        rt.invalidate();
        rt.frame(0.2);
        rt.frame(0.8);
        assert_eq!(the_box(&rt, ROOM, &uid), "second");
        assert!(!field_dim(&rt));
        press(&mut rt, "dm-composer-field");
        type_text(&mut rt, " edit");
        assert_eq!(the_box(&rt, ROOM, &uid), "second edit");

        // Timeout: editable again, with the text kept.
        assert!(rt.state.chat.dm_send_at(t0));
        rt.invalidate();
        rt.frame(0.2);
        rt.frame(0.8);
        assert!(field_dim(&rt));
        rt.update(move |s, _| {
            s.chat.dm_tick(t0 + super::super::dm_compose::ANSWER_WAIT);
        });
        rt.invalidate();
        rt.frame(0.2);
        rt.frame(0.8);
        assert!(!field_dim(&rt));
        press(&mut rt, "dm-composer-field");
        type_text(&mut rt, "!");
        assert_eq!(the_box(&rt, ROOM, &uid), "second edit!");
    }

    #[test]
    fn a_late_refusal_of_an_older_send_does_not_show_over_a_newer_one_that_went_through() {
        let (mut rt, uid) = open_with("Jonas");
        let t0 = std::time::Instant::now();
        rt.update(|s, _| s.chat.dm_write("one"));
        assert!(rt.state.chat.dm_send_at(t0));
        let Action::SendDm { ticket: one, .. } = sent(&rt)[0].1.clone() else {
            panic!()
        };
        // It is not answered in time; the user changes the text and sends another.
        rt.update(|s, _| {
            s.chat.dm_tick(t0 + super::super::dm_compose::ANSWER_WAIT);
            s.chat.dm_write("two");
        });
        assert!(
            rt.state
                .chat
                .dm_send_at(t0 + super::super::dm_compose::ANSWER_WAIT)
        );
        let Action::SendDm { ticket: two, .. } = sent(&rt)[1].1.clone() else {
            panic!()
        };
        let u = uid.clone();
        rt.update(move |s, _| {
            s.chat.dm_sent(ROOM, &u, two, Ok(()));
        });
        let u = uid.clone();
        rt.update(move |s, _| {
            s.chat.dm_sent(ROOM, &u, one, Err("old news".into()));
        });
        assert!(rt.state.chat.dm.compose.refusal_for(ROOM, &uid).is_none());
        // Without a newer success, a refusal shows, and a newer refusal is not replaced by an
        // older one.
        rt.update(|s, _| s.chat.dm_write("three"));
        assert!(rt.state.chat.dm_send());
        rt.update(|s, _| s.chat.dm_write("four"));
        let Action::SendDm { ticket: three, .. } = sent(&rt)[2].1.clone() else {
            panic!()
        };
        let u = uid.clone();
        rt.update(move |s, _| {
            s.chat.dm_sent(ROOM, &u, three, Err("refused three".into()));
        });
        assert_eq!(
            rt.state.chat.dm.compose.refusal_for(ROOM, &uid),
            Some("refused three")
        );
    }

    #[test]
    fn a_send_is_per_thread_and_an_unanswered_one_says_so_keeps_the_text_and_can_be_sent_again() {
        use std::time::{Duration, Instant};
        let (mut rt, jonas) = open_with("Jonas");
        let priya = uid_of(&rt, "Priya Raman");
        let t0 = Instant::now();
        rt.update(|s, _| s.chat.dm_write("to Jonas"));
        assert!(rt.state.chat.dm_send_at(t0));
        let Action::SendDm { ticket: first, .. } = sent(&rt)[0].1.clone() else {
            panic!()
        };
        // Waiting in one thread does not hold another's Send.
        {
            let priya = priya.clone();
            rt.update(move |s, _| assert!(s.chat.open_dm(ROOM, &priya)));
        }
        rt.update(|s, _| s.chat.dm_write("to Priya"));
        assert!(rt.state.chat.dm_send_at(t0 + Duration::from_secs(1)));
        // Within the wait nothing is said; at the limit the first is marked, and only it (the
        // second was sent a second later).
        assert!(!rt.state.chat.dm_tick(t0 + Duration::from_secs(5)));
        assert!(
            rt.state
                .chat
                .dm_tick(t0 + super::super::dm_compose::ANSWER_WAIT)
        );
        assert!(
            !rt.state
                .chat
                .dm_tick(t0 + super::super::dm_compose::ANSWER_WAIT)
        );
        let jk = (ROOM.to_owned(), jonas.clone());
        let pk = (ROOM.to_owned(), priya.clone());
        assert!(rt.state.chat.dm.compose.is_silent_in(&jk));
        assert!(!rt.state.chat.dm.compose.is_silent_in(&pk));
        assert!(rt.state.chat.dm.compose.is_sending_in(&pk));
        // Back in the first thread: the words, the text kept, and Send works again.
        {
            let jonas = jonas.clone();
            rt.update(move |s, _| assert!(s.chat.open_dm(ROOM, &jonas)));
        }
        rt.frame(0.2);
        assert!(rt.find("dm-no-answer").is_some());
        assert!(shows(&rt, super::super::dm_compose::NO_ANSWER));
        assert!(rt.find("dm-sending").is_none());
        assert_eq!(the_box(&rt, ROOM, &jonas), "to Jonas");
        // The same text as the one that was not answered: composing it again would be a second,
        // different message, so Enter and the plain button do not, and the button says so.
        assert!(rt.state.chat.dm_needs_confirm());
        assert!(!rt.state.chat.dm_send_at(t0 + Duration::from_secs(30)));
        assert_eq!(sent(&rt).len(), 2, "Enter did not send it again");
        assert!(shows(&rt, "Send again anyway"));
        press(&mut rt, "dm-send");
        assert_eq!(sent(&rt).len(), 3, "the explicit press did");
        // A late answer to the first still takes its text out, if the box still holds it.
        let peer = jonas.clone();
        rt.update(move |s, _| {
            s.chat.dm_sent(ROOM, &peer, first, Ok(()));
        });
        assert_eq!(the_box(&rt, ROOM, &jonas), "");
        // A room that goes takes what waited for it.
        let keep: Vec<_> = rt
            .state
            .chat
            .rooms()
            .iter()
            .filter(|r| r.key != ROOM)
            .cloned()
            .collect();
        rt.update(move |s, _| s.chat.set_rooms(keep));
        assert!(!rt.state.chat.dm.compose.is_sending_in(&pk));
        assert!(!rt.state.chat.dm.compose.is_silent_in(&jk));
    }

    #[test]
    fn an_own_send_goes_to_the_end_of_its_thread_and_never_moves_another() {
        use super::super::dm_view::scroll_key;
        let (mut rt, uid) = open_with("Jonas");
        let many = |extra: Option<Pending>| {
            let mut messages: Vec<DmMessage> = (0..40)
                .map(|n| DmMessage {
                    key: format!("old-{n}"),
                    outgoing: n % 2 == 0,
                    timestamp: T0 + n,
                    body: DmBody::Text(format!("message number {n}")),
                    pending: None,
                })
                .collect();
            if let Some(p) = extra {
                messages.push(DmMessage {
                    key: "fresh".into(),
                    outgoing: true,
                    timestamp: T0 + 100,
                    body: DmBody::Text("just sent".into()),
                    pending: Some(p),
                });
            }
            DmSnapshot {
                threads: vec![
                    DmThread {
                        room: ROOM.into(),
                        room_name: "Town Square".into(),
                        peer_key: uid.clone(),
                        peer: crate::rooms::dm::peer_from_key(&uid).unwrap(),
                        peer_name: "Jonas".into(),
                        peer_is_member: true,
                        last_ts: T0 + 100,
                        last_inbound_ts: T0 + 99,
                        last_inbound_signed: T0 + 99,
                        messages,
                    }
                    .into(),
                ],
                room_keys: vec![ROOM.to_owned()],
                hydrated: true,
                ..Default::default()
            }
        };
        give(&mut rt, many(None));
        let key = scroll_key(ROOM, &uid);
        assert_eq!(rt.scroll_at_bottom(&key), Some(true), "opened at the end");
        let (x, y) = rt.find(&key).expect("the thread's scroll area");
        rt.wheel(x, y, 600.0);
        rt.frame(0.2);
        rt.frame(0.4);
        assert_eq!(rt.scroll_at_bottom(&key), Some(false), "scrolled up");

        // Send; the thread shows the row on its way; the node's answer is the queue.
        rt.update(|s, _| s.chat.dm_write("just sent"));
        assert!(rt.state.chat.dm_send());
        let Action::SendDm { peer, ticket, .. } = sent(&rt)[0].1.clone() else {
            panic!()
        };
        give(
            &mut rt,
            many(Some(Pending {
                entry: 1,
                failed: None,
                retry: false,
            })),
        );
        let answer = crate::rooms::RoomsUpdate::DmSend {
            room: ROOM.into(),
            peer: crate::rooms::dm::peer_key(peer),
            ticket,
            result: Ok(()),
        };
        rt.update(move |s, tx| s.apply_rooms(answer, tx));
        rt.frame(0.2);
        rt.frame(0.4);
        assert_eq!(rt.scroll_at_bottom(&key), Some(true), "at the end");
        assert!(
            rt.find("dm-row-fresh").is_some(),
            "the row on its way is there"
        );
        assert!(shows(&rt, "Sending\u{2026}"));

        // The thread the user left is not moved by its answer, and nothing else is.
        rt.update(|s, _| s.chat.dm_write("second"));
        assert!(rt.state.chat.dm_send());
        let Action::SendDm { peer, ticket, .. } = sent(&rt)[1].1.clone() else {
            panic!()
        };
        let priya = uid_of(&rt, "Priya Raman");
        rt.update(move |s, _| assert!(s.chat.open_dm(ROOM, &priya)));
        let moved = rt
            .state
            .chat
            .dm_sent(ROOM, &crate::rooms::dm::peer_key(peer), ticket, Ok(()));
        assert_eq!(
            moved, None,
            "no thread is scrolled for the one that is left"
        );
    }

    #[test]
    fn escape_leaves_the_thread_and_keeps_the_draft() {
        let (mut rt, uid) = open_with("Jonas");
        press(&mut rt, "dm-composer-field");
        type_text(&mut rt, "half a thought");
        esc(&mut rt);
        assert!(rt.find("dm-thread").is_none());
        assert_eq!(the_box(&rt, ROOM, &uid), "half a thought");
    }

    #[test]
    fn the_example_sends_nothing_but_empties_the_box() {
        let mut rt = app();
        let uid = uid_of(&rt, "Jonas");
        rt.update({
            let uid = uid.clone();
            move |s, _| assert!(s.chat.open_dm(ROOM, &uid))
        });
        rt.frame(0.2);
        rt.update(|s, _| s.chat.dm.compose_edit().replace_text("hello"));
        assert!(rt.state.chat.dm_send());
        assert_eq!(the_box(&rt, ROOM, &uid), "");
        assert!(sent(&rt).is_empty());
    }

    #[test]
    fn nothing_the_composer_holds_is_printed() {
        let (mut rt, _) = open_with("Jonas");
        rt.update(|s, _| s.chat.dm.compose_edit().replace_text("PLAINTEXT-SECRET"));
        assert!(rt.state.chat.dm_send());
        let shown = format!("{:?}{:#?}", rt.state.chat.dm, rt.state.chat.dm);
        assert!(!shown.contains("PLAINTEXT"), "{shown}");
        let commands = format!("{:?}", rt.state.chat.queued_commands());
        assert!(!commands.contains("PLAINTEXT"), "{commands}");
    }
}

/// Each composer scene draws what it is for: the thread, its composer, and the state it shows.
#[test]
fn every_composer_scene_draws_its_content() {
    use crate::{RiverApp, SCENES, scene_size, view};
    let drawn = |rt: &Runtime<RiverApp>, key: &str| rt.find(key).is_some();
    for (name, wants, texts_wanted, absent) in [
        (
            "chat-dm-composer",
            &["dm-composer-field", "dm-send"][..],
            &[][..],
            &["dm-error", "dm-byte-counter", "dm-not-sent"][..],
        ),
        (
            "chat-dm-typed",
            &["dm-composer-field"],
            &["Want to compare notes tomorrow?"],
            &["dm-error"],
        ),
        (
            "chat-dm-near-limit",
            &["dm-byte-counter"],
            &["/32512"],
            &["dm-error"],
        ),
        (
            "chat-dm-over-limit",
            &["dm-byte-counter"],
            &["Message too long"],
            &[],
        ),
        (
            "chat-dm-sending",
            &["dm-sending", "dm-sending-row"],
            &["Sending\u{2026}"],
            &["dm-retry-1"],
        ),
        (
            "chat-dm-failed",
            &["dm-not-sent", "dm-retry-1", "dm-discard-1"],
            &["Not sent. Freenet refused it"],
            &[],
        ),
        (
            "chat-dm-unconfirmed",
            &["dm-not-sent", "dm-discard-1"],
            &["Freenet took it"],
            &["dm-retry-1"],
        ),
        (
            "chat-dm-refused",
            &["dm-error", "dm-composer-field"],
            &["The room didn't keep this message"],
            &[],
        ),
        (
            "chat-dm-send-again",
            &["dm-no-answer", "dm-send"][..],
            &["Send again anyway"][..],
            &["dm-sending"][..],
        ),
        (
            "chat-dm-blocked",
            &["dm-block"],
            &["A newer version of River is out"],
            &[],
        ),
        (
            "chat-dm-narrow-typed",
            &["dm-composer-field"],
            &["Want to compare notes tomorrow?"],
            &[],
        ),
    ] {
        let scene = SCENES.iter().find(|(n, _)| *n == name).expect(name).1;
        let mut state = RiverApp::detached();
        scene(&mut state);
        let mut rt = Runtime::new(
            state,
            view,
            scene_size(name),
            Env {
                system_dark: true,
                scale: 1.0,
            },
        );
        rt.frame(0.0);
        rt.frame(0.5);
        assert!(drawn(&rt, "dm-thread"), "{name}: the thread");
        for key in wants {
            assert!(drawn(&rt, key), "{name}: {key} is drawn");
        }
        for key in absent {
            assert!(!drawn(&rt, key), "{name}: {key} is not drawn");
        }
        for text in texts_wanted {
            assert!(shows(&rt, text), "{name}: {text:?} is on screen");
        }
    }
}

// Deleting their messages ------------------------------------------------------------------------

mod purging {
    use rune::{Env, Key, KeyEvent, Mods, Runtime};

    use super::super::dm_model::{DmPurge, DmSnapshot};
    use super::super::dm_purge::{self, Block};
    use super::super::model::Pending;
    use super::super::{LinkStatus, Source};
    use super::{
        Fixture, ME, PEER, ROOM, T0, app, at, conversation, esc, give, id, peer_key, press, shows,
    };
    use crate::rooms::dm::token_from_hex;
    use crate::rooms::outbox::Action;
    use crate::{RiverApp, SCENES, scene_size, view};

    /// The app reading rooms from a node, with the conversation (two messages from the peer, one
    /// from the user) given and its thread open.
    fn open_thread() -> (Runtime<RiverApp>, Fixture) {
        let mut rt = app();
        rt.update(|s, _| s.chat.source = Source::Node(LinkStatus::Live));
        let f = conversation();
        give(&mut rt, f.snapshot(true));
        let key = peer_key();
        rt.update(move |s, _| assert!(s.chat.open_dm(ROOM, &key)));
        rt.frame(0.1);
        rt.frame(0.2);
        (rt, f)
    }

    fn sent(rt: &Runtime<RiverApp>) -> Vec<(String, Action)> {
        rt.state
            .chat
            .queued_commands()
            .iter()
            .map(|c| (c.room.clone(), c.action.clone()))
            .collect()
    }

    fn purges(rt: &Runtime<RiverApp>) -> Vec<(String, Action)> {
        sent(rt)
            .into_iter()
            .filter(|(_, a)| matches!(a, Action::PurgeDm { .. }))
            .collect()
    }

    fn dimmed(rt: &Runtime<RiverApp>, key: &str) -> bool {
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(key))
            .unwrap_or_else(|| panic!("no element {key}"))
            .opacity
            < 0.9
    }

    fn help(rt: &Runtime<RiverApp>, key: &str) -> Option<String> {
        rt.elements()
            .into_iter()
            .find(|e| e.key.as_deref() == Some(key))
            .and_then(|e| e.help)
    }

    fn inbound_keys(f: &Fixture) -> Vec<String> {
        f.snapshot(true).threads[0]
            .messages
            .iter()
            .filter(|m| !m.outgoing)
            .map(|m| m.key.clone())
            .collect()
    }

    #[test]
    fn the_header_offers_it_where_they_wrote_and_not_for_what_you_sent() {
        let (rt, f) = open_thread();
        assert!(rt.find("dm-purge").is_some());
        assert!(!dimmed(&rt, "dm-purge"));
        assert_eq!(rt.state.chat.dm_purge_block(), None);
        assert_eq!(inbound_keys(&f).len(), 2);

        // A thread of only what the user sent offers nothing to delete, and says why.
        let mut only_mine = Fixture::new();
        only_mine.compose(ME, PEER, T0, b"my words only");
        let mut rt = app();
        rt.update(|s, _| s.chat.source = Source::Node(LinkStatus::Live));
        give(&mut rt, only_mine.snapshot(true));
        let key = peer_key();
        rt.update(move |s, _| assert!(s.chat.open_dm(ROOM, &key)));
        rt.frame(0.2);
        assert_eq!(rt.state.chat.dm_purge_block(), Some(Block::Nothing));
        assert!(dimmed(&rt, "dm-purge"));
        assert!(
            help(&rt, "dm-purge")
                .unwrap()
                .contains("no messages from Priya to delete")
        );
        // Pressing it does nothing.
        press(&mut rt, "dm-purge");
        assert!(rt.find("dm-purge-confirm").is_none());
        assert!(!rt.state.chat.dm_purge_confirming());
        assert!(!rt.state.chat.dm_purge_ask());
    }

    #[test]
    fn it_asks_in_words_first_and_cancel_and_escape_delete_nothing() {
        let (mut rt, _f) = open_thread();
        press(&mut rt, "dm-purge");
        assert!(rt.find("dm-purge-confirm").is_some());
        assert_eq!(rt.state.chat.dm_purge_count(), Some(2));
        // It says what it deletes, for whom, and what it leaves.
        assert!(shows(&rt, "Delete their messages?"));
        assert!(shows(
            &rt,
            "This deletes the 2 messages Priya sent you in this room."
        ));
        assert!(shows(&rt, "removed from the room itself"));
        assert!(shows(&rt, "no longer sees them in the conversation either"));
        assert!(shows(&rt, "cannot be undone"));
        assert!(shows(&rt, "Messages you sent Priya are not deleted"));
        assert!(shows(&rt, "at most 1000 deleted messages"));
        assert!(shows(&rt, "Delete 2 messages"));

        press(&mut rt, "dm-purge-cancel");
        assert!(rt.find("dm-purge-confirm").is_none());
        assert!(purges(&rt).is_empty());
        // Escape closes the dialog first, not the thread.
        press(&mut rt, "dm-purge");
        assert!(rt.find("dm-purge-confirm").is_some());
        esc(&mut rt);
        assert!(rt.find("dm-purge-confirm").is_none());
        assert!(rt.find("dm-thread").is_some(), "the thread stays");
        assert!(purges(&rt).is_empty());
        // The room's own shortcuts do not open over it.
        press(&mut rt, "dm-purge");
        assert!(rt.state.chat.modal_dialog_open());
    }

    #[test]
    fn enter_and_space_cannot_confirm_a_deletion_from_the_moment_the_dialog_opens() {
        let (mut rt, _f) = open_thread();
        press(&mut rt, "dm-purge");
        assert!(rt.find("dm-purge-confirm").is_some());
        assert!(shows(&rt, "cannot be undone"));
        for key in [Key::Enter, Key::Text(" ".into())] {
            rt.key(KeyEvent {
                key,
                mods: Mods::default(),
            });
            rt.frame(0.3);
        }
        assert!(purges(&rt).is_empty(), "nothing was asked");
        assert!(rt.state.chat.dm_purge_confirming() || rt.find("dm-purge-confirm").is_none());
        // Whatever Enter did to the dialog, the dangerous button was never the one that took it.
        let focused: Vec<_> = rt
            .elements()
            .into_iter()
            .filter(|e| e.focused)
            .filter_map(|e| e.key)
            .collect();
        assert!(
            !focused.iter().any(|k| k == "dm-purge-confirm"),
            "{focused:?}"
        );
    }

    #[test]
    fn confirming_names_exactly_what_was_counted_and_nothing_that_came_after() {
        let (mut rt, f) = open_thread();
        let counted = inbound_keys(&f);
        press(&mut rt, "dm-purge");
        // Another message from the peer arrives while the dialog is open.
        let mut later = Fixture::new();
        later.compose(PEER, ME, T0, b"first from the peer, with **bold**");
        later.compose(ME, PEER, T0 + 60, b"my answer");
        later.compose(PEER, ME, T0 + 3600, b"last from the peer");
        later.compose(PEER, ME, T0 + 7200, b"arrived during the dialog");
        give(&mut rt, later.snapshot(true));
        assert!(rt.find("dm-purge-confirm").is_some());
        assert_eq!(
            rt.state.chat.dm_purge_count(),
            Some(2),
            "the count is what the user saw"
        );
        press(&mut rt, "dm-purge-confirm");

        let commands = purges(&rt);
        assert_eq!(commands.len(), 1);
        let (
            room,
            Action::PurgeDm {
                peer,
                tokens,
                ticket,
            },
        ) = commands[0].clone()
        else {
            unreachable!()
        };
        assert_eq!(room, ROOM);
        assert_eq!(crate::rooms::dm::peer_key(peer), peer_key());
        let mut want: Vec<_> = counted.iter().filter_map(|k| token_from_hex(k)).collect();
        want.sort();
        let mut got = tokens.clone();
        got.sort();
        assert_eq!(
            got, want,
            "the two counted, not the one that arrived, not the user's own"
        );
        assert_eq!(tokens.len(), 2);
        // The dialog is closed, and a second press of the action waits for the answer.
        assert!(rt.find("dm-purge-confirm").is_none());
        assert_eq!(rt.state.chat.dm_purge_block(), Some(Block::Under));
        assert!(dimmed(&rt, "dm-purge"));
        assert_eq!(rt.state.chat.dm_purge_waiting_ticket(), Some(ticket));
        assert!(!rt.state.chat.dm_purge_ask());
    }

    #[test]
    fn a_refusal_is_said_above_the_messages_and_the_next_ask_clears_it() {
        let (mut rt, _f) = open_thread();
        press(&mut rt, "dm-purge");
        press(&mut rt, "dm-purge-confirm");
        let ticket = rt.state.chat.dm_purge_waiting_ticket().unwrap();
        let why = dm_purge_cap();
        rt.update(move |s, _| s.chat.dm_purged(ROOM, &peer_key(), ticket, Err(why)));
        rt.frame(0.2);
        assert!(rt.find("dm-purge-error").is_some());
        assert!(shows(&rt, "Not deleted. The room records at most 1000"));
        // It is above the messages, in the notes.
        assert!(at(&rt, "Not deleted") < at(&rt, "last from the peer"));
        // The action is on again, and asking again clears the old refusal.
        assert_eq!(rt.state.chat.dm_purge_block(), None);
        press(&mut rt, "dm-purge");
        press(&mut rt, "dm-purge-confirm");
        assert!(rt.find("dm-purge-error").is_none());
        assert_eq!(purges(&rt).len(), 2);
    }

    fn dm_purge_cap() -> String {
        crate::rooms::dm_purge::Refusal::Cap {
            held: 995,
            adding: 12,
        }
        .text()
    }

    fn with_purge(rt: &mut Runtime<RiverApp>, f: &Fixture, p: Option<Pending>) {
        let mut snapshot: DmSnapshot = f.snapshot(true);
        if let Some(pending) = p {
            snapshot.purges.push(DmPurge {
                room: ROOM.into(),
                peer_key: peer_key(),
                count: 2,
                pending,
            });
        }
        give(rt, snapshot);
    }

    #[test]
    fn a_deletion_on_its_way_says_so_and_one_not_taken_offers_retry_and_discard() {
        let (mut rt, f) = open_thread();
        with_purge(
            &mut rt,
            &f,
            Some(Pending {
                entry: 7,
                failed: None,
                retry: false,
            }),
        );
        assert!(shows(&rt, "Deleting 2 messages\u{2026}"));
        assert!(rt.find("dm-purge-retry-7").is_none());
        assert_eq!(rt.state.chat.dm_purge_block(), Some(Block::Under));
        assert!(dimmed(&rt, "dm-purge"));

        with_purge(
            &mut rt,
            &f,
            Some(Pending {
                entry: 7,
                failed: Some("Freenet refused it: no".into()),
                retry: true,
            }),
        );
        assert!(shows(&rt, "Not deleted. Freenet refused it: no"));
        assert_eq!(rt.state.chat.dm_purge_block(), Some(Block::NotTaken));
        assert!(
            help(&rt, "dm-purge")
                .unwrap()
                .contains("Retry or discard it first")
        );
        press(&mut rt, "dm-purge-retry-7");
        press(&mut rt, "dm-purge-discard-7");
        let commands = sent(&rt);
        assert_eq!(commands[0], (ROOM.to_owned(), Action::Retry { entry: 7 }));
        assert_eq!(commands[1], (ROOM.to_owned(), Action::Discard { entry: 7 }));

        // One that cannot be tried again offers only Discard and says what became of it.
        with_purge(
            &mut rt,
            &f,
            Some(Pending {
                entry: 8,
                failed: Some("There are no messages from them left to delete.".into()),
                retry: false,
            }),
        );
        assert!(rt.find("dm-purge-retry-8").is_none());
        assert!(rt.find("dm-purge-discard-8").is_some());
        // Once the room holds it, the line goes.
        with_purge(&mut rt, &f, None);
        assert!(rt.find("dm-purge-not-taken").is_none());
        assert!(!shows(&rt, "Deleting 2 messages"));
    }

    #[test]
    fn it_is_off_with_its_reason_when_the_room_will_not_take_it() {
        // River has moved on.
        let (mut rt, _f) = open_thread();
        rt.update(|s, _| s.chat.issues.update_required = true);
        rt.frame(0.2);
        assert_eq!(rt.state.chat.dm_purge_block(), Some(Block::UpdateRequired));
        assert!(dimmed(&rt, "dm-purge"));
        assert!(shows(&rt, "Update this app to delete messages here"));
        assert!(rt.find("dm-purge-off").is_some());
        assert!(!rt.state.chat.dm_purge_ask());

        // No key, banned, not a member: the room's words for it.
        let (mut rt, _f) = open_thread();
        let why = crate::rooms::outbox::Blocked::NoKey.reason();
        let mut rooms = rt.state.chat.rooms().to_vec();
        for r in &mut rooms {
            r.send_block = Some(why.into());
        }
        rt.update(move |s, _| s.chat.set_rooms(rooms));
        rt.frame(0.2);
        assert_eq!(
            rt.state.chat.dm_purge_block(),
            Some(Block::Room(why.into()))
        );
        assert!(shows(&rt, why));
        assert!(dimmed(&rt, "dm-purge"));

        // A room not read from the network yet: the background work would refuse it.
        let (mut rt, f) = open_thread();
        let mut snapshot = f.snapshot(true);
        snapshot.room_keys.clear();
        give(&mut rt, snapshot);
        assert_eq!(rt.state.chat.dm_purge_block(), Some(Block::NotLoaded));
        assert!(shows(&rt, "Waiting for this room to load"));
    }

    #[test]
    fn a_dialog_open_while_the_room_goes_is_closed_with_it_and_a_silent_ask_can_be_asked_again() {
        let (mut rt, f) = open_thread();
        press(&mut rt, "dm-purge");
        assert!(rt.find("dm-purge-confirm").is_some());
        rt.update(|s, _| s.chat.set_rooms(Vec::new()));
        rt.frame(0.2);
        assert!(!rt.state.chat.dm_purge_confirming());

        // An ask nobody answered says so after the wait, and may be asked again.
        let (mut rt, _) = (open_thread().0, f);
        press(&mut rt, "dm-purge");
        let t0 = std::time::Instant::now();
        rt.update(move |s, _| assert!(s.chat.dm_purge_confirm_at(t0)));
        assert!(!rt.state.chat.dm_purge_ask());
        rt.update(move |s, _| {
            assert!(s.chat.dm_tick(t0 + dm_purge::ANSWER_WAIT));
        });
        rt.frame(0.2);
        assert!(shows(&rt, "River has not answered yet"));
        assert_eq!(rt.state.chat.dm_purge_block(), None);
        assert!(!dimmed(&rt, "dm-purge"));
        // A late answer still counts.
        let ticket = rt.state.chat.dm_purge_waiting_ticket().unwrap();
        rt.update(move |s, _| s.chat.dm_purged(ROOM, &peer_key(), ticket, Ok(())));
        rt.frame(0.2);
        rt.frame(0.8);
        assert!(!shows(&rt, "River has not answered yet"));
    }

    #[test]
    fn once_it_lands_the_thread_drops_the_rows_the_rail_follows_and_the_read_position_forgets_them()
    {
        let (mut rt, f) = open_thread();
        // The thread is read: both of the peer's messages are seen.
        rt.update(|s, _| {
            s.chat.dm.at_bottom = true;
            s.chat.dm.mark_read(true);
        });
        let counted = inbound_keys(&f);
        let seen = |rt: &Runtime<RiverApp>| {
            rt.state
                .chat
                .dm
                .seen
                .get(&(ROOM.to_owned(), peer_key()))
                .cloned()
                .unwrap_or_default()
        };
        assert!(counted.iter().all(|k| seen(&rt).contains(k)));

        press(&mut rt, "dm-purge");
        press(&mut rt, "dm-purge-confirm");
        let ticket = rt.state.chat.dm_purge_waiting_ticket().unwrap();
        rt.update(move |s, _| s.chat.dm_purged(ROOM, &peer_key(), ticket, Ok(())));
        // Queued, not landed: the messages are still in the thread, so they stay read (the
        // rail's badge does not count what is on its way out).
        assert!(counted.iter().all(|k| seen(&rt).contains(k)));
        assert_eq!(rt.state.chat.dm.unread_total(), 0);

        // The room holds the deletion: only what the user sent is left.
        let mut after = Fixture::new();
        after.compose(ME, PEER, T0 + 60, b"my answer");
        give(&mut rt, after.snapshot(true));
        // Gone from the thread: gone from the read position too.
        assert!(counted.iter().all(|k| !seen(&rt).contains(k)), "forgotten");
        assert!(!shows(&rt, "last from the peer"));
        assert!(!shows(&rt, "first from the peer"));
        assert!(shows(&rt, "my answer"), "what the user sent is not deleted");
        assert_eq!(rt.state.chat.dm.unread_total(), 0);
        // The action is off now: nothing of theirs is left.
        assert_eq!(rt.state.chat.dm_purge_block(), Some(Block::Nothing));
    }

    #[test]
    fn nothing_deleted_is_printed() {
        let (mut rt, _f) = open_thread();
        press(&mut rt, "dm-purge");
        press(&mut rt, "dm-purge-confirm");
        let shown = format!(
            "{:?}{:#?}{:?}",
            rt.state.chat.dm,
            rt.state.chat.dm,
            rt.state.chat.queued_commands()
        );
        let tokens: Vec<String> = inbound_keys(&conversation());
        for t in tokens {
            assert!(!shown.contains(&t), "a token was printed: {shown}");
        }
        assert!(shown.contains("tokens: 2"));
        let _ = id(ME);
    }

    /// Each purge scene draws what it is for.
    #[test]
    fn every_purge_scene_draws_its_content() {
        for (name, wants, texts_wanted, absent) in [
            (
                "chat-dm-purge",
                &["dm-purge", "dm-thread"][..],
                &["Delete their messages"][..],
                &["dm-purge-confirm", "dm-purge-under", "dm-purge-off"][..],
            ),
            (
                "chat-dm-purge-narrow",
                &["dm-purge"],
                &[],
                &["dm-purge-confirm"],
            ),
            (
                "chat-dm-purge-confirm",
                &["dm-purge-confirm", "dm-purge-cancel", "dm-purge-what"],
                &[
                    "Delete their messages?",
                    "Priya Raman sent you in this room",
                ],
                &[],
            ),
            (
                "chat-dm-purge-confirm-narrow",
                &["dm-purge-confirm", "dm-purge-cancel"],
                &["Delete their messages?"],
                &[],
            ),
            (
                "chat-dm-purge-under",
                &["dm-purge-under"],
                &["Deleting 2 messages"],
                &["dm-purge-retry-1"],
            ),
            (
                "chat-dm-purge-failed",
                &[
                    "dm-purge-not-taken",
                    "dm-purge-retry-1",
                    "dm-purge-discard-1",
                ],
                &["Not deleted. Freenet refused it"],
                &[],
            ),
            (
                "chat-dm-purge-refused",
                &["dm-purge-error"],
                &["Not deleted. The room records at most 1000"],
                &[],
            ),
            (
                "chat-dm-purge-off-nothing",
                &["dm-purge"],
                &[],
                &["dm-purge-confirm"],
            ),
            (
                "chat-dm-purge-off-held",
                &["dm-purge", "dm-purge-off"],
                &["Update this app to delete messages here"],
                &[],
            ),
            (
                "chat-dm-purge-off-key",
                &["dm-purge", "dm-purge-off"],
                &["This computer doesn't hold your key"],
                &[],
            ),
            (
                "chat-dm-purge-off-loading",
                &["dm-purge", "dm-purge-off"],
                &["Waiting for this room to load"],
                &[],
            ),
        ] {
            let scene = SCENES.iter().find(|(n, _)| *n == name).expect(name).1;
            let mut state = RiverApp::detached();
            scene(&mut state);
            let mut rt = Runtime::new(
                state,
                view,
                scene_size(name),
                Env {
                    system_dark: true,
                    scale: 1.0,
                },
            );
            rt.frame(0.0);
            rt.frame(0.5);
            assert!(rt.find("dm-thread").is_some(), "{name}: the thread");
            for key in wants {
                assert!(rt.find(key).is_some(), "{name}: {key} is drawn");
            }
            for key in absent {
                assert!(rt.find(key).is_none(), "{name}: {key} is not drawn");
            }
            for text in texts_wanted {
                assert!(shows(&rt, text), "{name}: {text:?} is on screen");
            }
        }
    }
}
