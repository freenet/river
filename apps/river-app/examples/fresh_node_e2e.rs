// Test code stamps arbitrary times; the rule is for what production signs (freenet/river#971).
#![allow(clippy::disallowed_methods)]
//! End-to-end run of the native app against two Freenet nodes that River never ran on.
//!
//! Each node is fresh: no River web UI, no chat delegate. On each, the app's own background
//! work ([`Link`]) asks for a grant (approved here as the user would approve it on the page),
//! registers the chat delegate, and reads an empty room list. Then the app on node A creates a
//! room and makes an invitation, the app on node B joins from it, and each sends a message the
//! other receives; each sends the other a direct message, which the other opens and the sender
//! reads back as its own; each then changes its own nickname and the other sees it everywhere, and the
//! owner makes the joiner a deputy (the joiner's app shows the Deputy tag) and revokes it. Last,
//! each node's chat delegate is asked directly (on a connection of its own with the app's
//! grant) for the room's signing key, and must hold the key of that app's own
//! member: the app's background key sweep puts it there.
//!
//! Then a newer River. On node A, pointer records signed by a throwaway key are PUT, first
//! naming this build: an app trusting that key sends a message, which node B receives. Then the
//! chat delegate's record moves to a generation above the one this build carries. A third app on node
//! A, built to trust that throwaway key in place of River's, must notice it, show the room
//! read-only, and refuse to send, create or join: none of it may reach the node. The record is
//! PUT only to the local test node; River's own key and pointer addresses are never used. The
//! pointer contract WASM is `$POINTER_WASM` (the script fetches it), checked against
//! freenet-migrate's frozen pointer code hash.
//!
//! ```text
//! cargo run -p river-app --example fresh_node_e2e -- <node A host:port> <node B host:port> <dir>
//! ```
//!
//! `<dir>` holds each app's grant token. Run it through `apps/scripts/fresh-node-e2e.sh`,
//! which starts the two nodes. Never point it at a node with data you care about: it creates
//! a room there.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::HostResponse;
use river_app::chat::model::Room;
use river_app::rooms::delegate::{self, Read};
use river_app::rooms::generation::{
    Artifact, BuildRecord, NodePointers, Records, UPDATE_REQUIRED, UPDATE_REQUIRED_WORK, Watch,
    watched_pointers,
};
use river_app::rooms::modes::NotificationMode;
use river_app::rooms::socket::NodeSocket;
use river_app::rooms::token::{TokenStore, node_dir};
use river_app::rooms::work::WorkState;
use river_app::rooms::{Access, Action, Command, InviteCode, Issues, Link, RoomsUpdate};
use river_core::chat_delegate::{ChatDelegateResponseMsg, OutboundDmStore};
use river_core::stored_rooms::{
    ROOMS_META_KEY, StoredRoomSlot, StoredRoomsMeta, decode_room_slot, decode_rooms_meta,
    room_storage_key,
};

/// How long each step may take. Network operations between two loopback nodes take seconds;
/// this is generous so a slow CI runner does not fail it.
const STEP: Duration = Duration::from_secs(180);

/// One app: its link to its node and what it has reported.
struct App {
    name: &'static str,
    authority: String,
    /// Where the app keeps its grant token.
    dir: PathBuf,
    link: Link,
    updates: Receiver<RoomsUpdate>,
    rooms: Vec<Room>,
    issues: Issues,
    work: Vec<river_app::rooms::work::WorkView>,
    modes: std::collections::HashMap<String, NotificationMode>,
    invites: Vec<(String, Result<String, String>)>,
    /// Identity tokens reported, by room. The e2e holds them only as long as it needs them.
    exports: Vec<(String, Result<String, String>)>,
    /// The direct-message threads last reported, of every room.
    dms: Vec<std::sync::Arc<river_app::chat::dm_model::DmThread>>,
    /// Answers to direct messages asked to be sent, by ticket.
    dm_results: Vec<(u64, Result<(), String>)>,
    /// Answers to deletions of someone's messages asked for, by ticket.
    dm_purge_results: Vec<(u64, Result<(), String>)>,
    /// Answers to invitations asked to be sent or accepted in a direct message, by ticket.
    dm_invite_results: Vec<(u64, Result<(), String>)>,
    /// Deletions the rooms do not hold yet, as last reported.
    dm_purges: usize,
    /// The archive list as last reported.
    dm_archived: Vec<river_app::chat::dm_model::DmArchived>,
    /// The window's own direct-message state, fed every snapshot as the app feeds it, and the
    /// new messages it found (what the app turns into notifications), oldest first.
    dm_ui: river_app::chat::dm_view::DmUi,
    /// Rooms the snapshots listed as read from the network (met): what arrives in one afterwards
    /// is news, so a test waits for it before it sends.
    dm_met: std::collections::HashSet<String>,
    dm_news: Vec<river_app::chat::DmArrival>,
    access: Option<Access>,
}

impl App {
    fn start(name: &'static str, authority: &str, dir: &Path) -> App {
        App::start_with(name, authority, dir, None)
    }

    /// Start an app; with `watch`, it judges whether River moved on by that instead of
    /// River's own pointer records.
    fn start_with(name: &'static str, authority: &str, dir: &Path, watch: Option<Watch>) -> App {
        let (tx, rx) = channel();
        let report = move |u| {
            let _ = tx.send(u);
        };
        let link = match watch {
            Some(w) => Link::with_watch(authority, Some(dir.to_owned()), w, report),
            None => Link::with_authority(authority, Some(dir.to_owned()), report),
        };
        link.start();
        App {
            name,
            authority: authority.to_owned(),
            dir: dir.to_owned(),
            link,
            updates: rx,
            rooms: Vec::new(),
            issues: Issues::default(),
            work: Vec::new(),
            modes: Default::default(),
            invites: Vec::new(),
            exports: Vec::new(),
            dms: Vec::new(),
            dm_results: Vec::new(),
            dm_purge_results: Vec::new(),
            dm_invite_results: Vec::new(),
            dm_purges: 0,
            dm_archived: Vec::new(),
            dm_ui: Default::default(),
            dm_met: Default::default(),
            dm_news: Vec::new(),
            access: None,
        }
    }

    /// Take updates until `done` holds, approving a grant page when one is shown.
    fn until(&mut self, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + STEP;
        while !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "{}: never: {what} (last: {:?}; direct messages: {:?})",
                self.name,
                self.access,
                self.dms.iter().map(|t| &t.messages).collect::<Vec<_>>()
            );
            let Ok(update) = self.updates.recv_timeout(left.min(Duration::from_secs(1))) else {
                continue;
            };
            match update {
                RoomsUpdate::Access(a) => {
                    println!("{}: {a:?}", self.name);
                    if let Access::WaitingForApproval { approve_url } = &a {
                        approve(&self.authority, approve_url);
                    }
                    assert!(!a.is_stopped(), "{}: stopped: {a:?}", self.name);
                    self.access = Some(a);
                }
                RoomsUpdate::Rooms {
                    rooms,
                    issues,
                    work,
                    modes,
                    ..
                } => {
                    self.modes = modes;
                    self.rooms = rooms;
                    self.issues = issues;
                    self.work = work;
                }
                RoomsUpdate::Invite { room, link } => self.invites.push((room, link)),
                RoomsUpdate::Export { room, token } => self.exports.push((room, token)),
                RoomsUpdate::Dms(snapshot) => {
                    self.dm_met.extend(snapshot.room_keys.iter().cloned());
                    self.dm_ui.apply(snapshot.clone());
                    self.dm_news.extend(self.dm_ui.take_arrivals());
                    self.dm_purges = snapshot.purges.len();
                    self.dm_archived = snapshot.archived;
                    self.dms = snapshot.threads;
                }
                RoomsUpdate::DmPurge { ticket, result, .. } => {
                    self.dm_purge_results.push((ticket, result))
                }
                RoomsUpdate::DmSend { ticket, result, .. } => {
                    self.dm_results.push((ticket, result))
                }
                RoomsUpdate::DmInvite { ticket, result, .. } => {
                    self.dm_invite_results.push((ticket, result))
                }
                RoomsUpdate::Status(_) => {}
            }
        }
        println!("{}: {what}", self.name);
    }

    /// Take updates for `wait`, whatever they are.
    fn drain(&mut self, wait: Duration) {
        let until = Instant::now() + wait;
        let deadline = Instant::now() + wait + STEP;
        self.until("the wait is over", |_| Instant::now() >= until);
        assert!(Instant::now() < deadline);
    }

    fn send(&self, room: &str, action: Action) {
        self.link.send(Command {
            room: room.to_owned(),
            action,
        });
    }

    fn room(&self, name: &str) -> Option<&Room> {
        self.rooms.iter().find(|r| r.name == name)
    }

    /// The thread of the room keyed `room` with the member `peer` (`Member::uid`).
    fn thread(&self, room: &str, peer: &str) -> Option<&river_app::chat::dm_model::DmThread> {
        self.dms
            .iter()
            .find(|t| t.room == room && t.peer_key == peer)
            .map(|t| &**t)
    }

    /// Whether that thread holds a message with this text, as sent (`outgoing`) or received.
    /// A message of the user's own counts only when the room holds it and nothing is pending.
    fn dm_text(&self, room: &str, peer: &str, text: &str, outgoing: bool) -> bool {
        use river_app::chat::dm_model::DmBody;
        self.thread(room, peer).is_some_and(|t| {
            t.messages.iter().any(|m| {
                m.outgoing == outgoing
                    && m.pending.is_none()
                    && matches!(&m.body, DmBody::Text(body) if body == text)
            })
        })
    }

    fn has_text(&self, room: &str, text: &str) -> bool {
        self.room(room)
            .is_some_and(|r| r.messages.iter().any(|m| m.text == text))
    }
}

/// The `outbound_dms` value the chat delegate of the node at `authority` holds, asked on a
/// connection of its own with the app's grant, as the web UI would read it.
fn stored_store(authority: &str, dir: &Path) -> Result<OutboundDmStore, String> {
    let token = TokenStore::in_dir(&node_dir(dir, authority, false))
        .load()
        .ok_or("the app has no grant token")?;
    let mut socket = NodeSocket::connect(authority, &token, &Default::default())
        .map_err(|e| format!("{e:?}"))?;
    socket
        .set_read_timeout(Duration::from_secs(10))
        .map_err(|e| format!("{e:?}"))?;
    socket
        .send(&delegate::request(&Read::Get(
            river_app::rooms::dm_cache::KEY.to_vec(),
        )))
        .map_err(|e| format!("{e:?}"))?;
    loop {
        match socket.recv().map_err(|e| format!("{e:?}"))? {
            Ok(response @ HostResponse::DelegateResponse { .. }) => {
                if let Some(ChatDelegateResponseMsg::GetResponse { value, .. }) =
                    delegate::answers(&response).pop()
                {
                    let Some(bytes) = value else {
                        return Ok(OutboundDmStore::default());
                    };
                    return ciborium::de::from_reader(bytes.as_slice())
                        .map_err(|e| format!("the sent messages do not decode: {e}"));
                }
            }
            Ok(_) => {}
            Err(e) => return Err(format!("the node answered with an error: {e}")),
        }
    }
}

/// The sent direct messages the chat delegate of the node at `authority` holds under
/// `outbound_dms`: the texts, in the order stored, and how many archived threads the value holds.
fn stored_sent_texts(authority: &str, dir: &Path) -> Result<(Vec<String>, usize), String> {
    let store = stored_store(authority, dir)?;
    Ok((
        store.entries.into_iter().map(|e| e.plaintext).collect(),
        store.hidden_threads.len(),
    ))
}

/// The generation of the sent-message key in the delegate of the node at `authority`: it grows
/// with every store, whoever makes it, so it says whether anything wrote.
fn stored_sent_generation(authority: &str, dir: &Path) -> Result<u64, String> {
    let token = TokenStore::in_dir(&node_dir(dir, authority, false))
        .load()
        .ok_or("the app has no grant token")?;
    let mut socket = NodeSocket::connect(authority, &token, &Default::default())
        .map_err(|e| format!("{e:?}"))?;
    socket
        .set_read_timeout(Duration::from_secs(10))
        .map_err(|e| format!("{e:?}"))?;
    socket
        .send(&delegate::versioned(&delegate::Versioned::Read(
            river_app::rooms::dm_cache::KEY.to_vec(),
        )))
        .map_err(|e| format!("{e:?}"))?;
    loop {
        match socket.recv().map_err(|e| format!("{e:?}"))? {
            Ok(response @ HostResponse::DelegateResponse { .. }) => {
                if let Some(ChatDelegateResponseMsg::GetVersionedResponse { generation, .. }) =
                    delegate::answers(&response).pop()
                {
                    return Ok(generation);
                }
            }
            Ok(_) => {}
            Err(e) => return Err(format!("the node answered with an error: {e}")),
        }
    }
}

/// Wait until the delegate on `authority` holds `text` as a sent direct message. Panics after
/// [`STEP`] with what it last saw.
fn wait_until_saved(name: &str, authority: &str, dir: &Path, text: &str) {
    let deadline = Instant::now() + STEP;
    loop {
        let seen = stored_sent_texts(authority, dir);
        if matches!(&seen, Ok((texts, _)) if texts.iter().any(|t| t == text)) {
            println!("{name}: the delegate holds the sent text");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{name}: the delegate never held the sent text (last: {:?})",
            seen.map(|(t, h)| (t.len(), h))
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Restart the app's link as a quit and start would: its session ends, and the next one starts
/// with nothing in memory (no sent text, no rooms) and reads everything again from the node.
fn restart(app: &mut App) {
    app.link.set_node(&app.authority, false);
    app.link.start();
    app.rooms.clear();
    app.dms.clear();
    app.dm_archived.clear();
    app.access = None;
    // Updates the old session had queued belong to it.
    while app.updates.try_recv().is_ok() {}
}

/// What node A sends node B, with a multi-byte character, a second line and spaces at both ends.
const FROM_A: &str = "  DM from A \u{1f44b}\nsecond line  ";
const FROM_B: &str = "DM from B, with a \u{20ac} sign";

/// Node B deletes node A's direct message in `room` (key `key`), after both have sent one
/// ([`direct_messages`]). Only what A sent B can be deleted: a deletion naming B's own message is
/// refused and queues nothing. Once the room holds the deletion, A's thread has lost its message
/// too (it is gone from the room for everyone), what B sent A is untouched, A's saved copy of
/// the text is removed from its chat delegate, and nothing comes back when A's app restarts.
fn delete_their_messages(alice: &mut App, bob: &mut App, room: &str, key: &str) {
    use river_app::rooms::dm::{peer_key, token_from_hex};
    let id_in = |app: &App, name: &str| {
        let full = app
            .room(room)
            .and_then(|r| r.members.iter().find(|m| m.nickname == name))
            .map(|m| m.full_id)
            .unwrap_or_else(|| panic!("{name} is not a member in {}'s view", app.name));
        river_core::room_state::member::MemberId(freenet_scaffold::util::FastHash(full))
    };
    let (bob_id, alice_id) = (id_in(alice, "Bob"), id_in(bob, "Alice"));
    let (bob_in_a, alice_in_b) = (peer_key(bob_id), peer_key(alice_id));
    let tokens_of = |app: &App, peer: &str, outgoing: bool| -> Vec<_> {
        app.thread(key, peer)
            .expect("a thread")
            .messages
            .iter()
            .filter(|m| m.outgoing == outgoing)
            .map(|m| token_from_hex(&m.key).expect("a message is keyed by its purge token"))
            .collect()
    };
    let (theirs, own) = (
        tokens_of(bob, &alice_in_b, false),
        tokens_of(bob, &alice_in_b, true),
    );
    assert_eq!((theirs.len(), own.len()), (1, 1));

    // What B sent cannot be deleted by B: refused, nothing queued, nothing changes.
    bob.send(
        key,
        Action::PurgeDm {
            peer: alice_id,
            tokens: own,
            ticket: 11,
        },
    );
    bob.until("B's deletion of its own message is refused", |app| {
        app.dm_purge_results.iter().any(|(t, r)| {
            *t == 11
                && r.as_ref()
                    .is_err_and(|e| e.contains("no messages from them"))
        })
    });
    assert_eq!(bob.dm_purges, 0);
    assert!(bob.dm_text(key, &alice_in_b, FROM_B, true));

    // What A sent B can: B asks, the room holds the deletion, and the message is gone from both
    // threads while each side's own other message stays.
    bob.send(
        key,
        Action::PurgeDm {
            peer: alice_id,
            tokens: theirs.clone(),
            ticket: 12,
        },
    );
    bob.until("B's deletion is queued", |app| {
        app.dm_purge_results
            .iter()
            .any(|(t, r)| *t == 12 && r.is_ok())
    });
    bob.until("node B no longer shows node A's message", |app| {
        !app.dm_text(key, &alice_in_b, FROM_A, false)
            && app.dm_text(key, &alice_in_b, FROM_B, true)
            && app.dm_purges == 0
    });
    alice.until("node A no longer shows its own deleted message", |app| {
        app.thread(key, &bob_in_a)
            .is_some_and(|t| t.messages.len() == 1 && app.dm_text(key, &bob_in_a, FROM_B, false))
    });
    println!("A and B: the deleted message is gone from both threads");

    // A's saved copy of the deleted text is removed from its chat delegate, once A's app has seen
    // the deletion; the text of the message it received is not A's to keep and was never there.
    let deadline = Instant::now() + STEP;
    loop {
        let seen = stored_sent_texts(&alice.authority, &alice.dir);
        if matches!(&seen, Ok((texts, _)) if texts.is_empty()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "A: the delegate still held the deleted text (last: {:?})",
            seen.map(|(t, h)| (t.len(), h))
        );
        alice.drain(Duration::from_millis(500));
    }
    println!("A: the deleted message's text is removed from the delegate");

    // Nothing of theirs is left to delete: A's original token names no message now.
    bob.send(
        key,
        Action::PurgeDm {
            peer: alice_id,
            tokens: theirs,
            ticket: 13,
        },
    );
    bob.until("a second deletion finds nothing to delete", |app| {
        app.dm_purge_results.iter().any(|(t, r)| {
            *t == 13
                && r.as_ref()
                    .is_err_and(|e| e.contains("no messages from them"))
        })
    });

    // A restarts: the deletion is in the room, so the message does not come back.
    restart(alice);
    alice.until(
        "A, restarted, reads the room without the deleted message",
        |app| {
            app.dm_text(key, &bob_in_a, FROM_B, false)
                && !app
                    .thread(key, &bob_in_a)
                    .is_some_and(|t| t.messages.iter().any(|m| m.outgoing))
        },
    );
    println!("A: after a restart the deleted message stays deleted");
}

/// Direct messages in `room` (key `key`), both ways: each app sends one to the other member, the
/// other opens it with its own key and shows the text, and the sender's own thread shows what it
/// sent (sealed to the other, so it comes from the sent-text kept in memory) with nothing pending.
/// The text has a multi-byte character, a second line and spaces at both ends, and is sent as written.
fn direct_messages(alice: &mut App, bob: &mut App, room: &str, key: &str) {
    // The recipient is named by River's full member id, as the screen does: never the label.
    let id_in = |app: &App, name: &str| {
        let full = app
            .room(room)
            .and_then(|r| r.members.iter().find(|m| m.nickname == name))
            .map(|m| m.full_id)
            .unwrap_or_else(|| panic!("{name} is not a member in {}'s view", app.name));
        river_core::room_state::member::MemberId(freenet_scaffold::util::FastHash(full))
    };
    let (bob_id, alice_id) = (id_in(alice, "Bob"), id_in(bob, "Alice"));
    let (bob_in_a, alice_in_b) = (
        river_app::rooms::dm::peer_key(bob_id),
        river_app::rooms::dm::peer_key(alice_id),
    );
    // Sent as written, with its spaces: the web UI sends the draft as typed.
    let (from_a, from_b) = (FROM_A, FROM_B);
    // A refused send says why, leaves nothing queued and sends nothing.
    alice.send(
        key,
        Action::SendDm {
            peer: bob_id,
            text: "   ".into(),
            ticket: 1,
        },
    );
    alice.until("an empty direct message is refused", |app| {
        app.dm_results
            .iter()
            .any(|(t, r)| *t == 1 && r.as_ref().is_err_and(|e| e.contains("Write something")))
    });
    // B must have read the room from the network first: what is in a room when it is first read
    // is seeded as seen, so a message sent before that is not news.
    bob.until("node B has read the room from the network", |app| {
        app.dm_met.contains(key)
    });
    alice.until("node A has read the room from the network", |app| {
        app.dm_met.contains(key)
    });
    alice.send(
        key,
        Action::SendDm {
            peer: bob_id,
            text: from_a.into(),
            ticket: 2,
        },
    );
    alice.until("node A's direct message is queued", |app| {
        app.dm_results.iter().any(|(t, r)| *t == 2 && r.is_ok())
    });
    alice.until("node A shows its own direct message, sent", |app| {
        app.dm_text(key, &bob_in_a, from_a, true)
    });
    bob.until("node B opens node A's direct message", |app| {
        app.dm_text(key, &alice_in_b, from_a, false)
    });
    // What the app would notify about: B got one new message, in that thread, and A (which only
    // sent so far) got none. A's own sends are never news.
    assert_eq!(bob.dm_news.len(), 1, "B: {:?}", bob.dm_news);
    assert_eq!(bob.dm_news[0].count, 1);
    assert_eq!(bob.dm_news[0].peer_key, alice_in_b);
    assert!(bob.dm_news[0].room == key && !bob.dm_news[0].preview.is_empty());
    assert!(alice.dm_news.is_empty(), "A: {:?}", alice.dm_news);
    // A second later: two messages in one second are ordered by purge token, so the order sent
    // is only the order shown when their times differ.
    std::thread::sleep(Duration::from_millis(1100));
    bob.send(
        key,
        Action::SendDm {
            peer: alice_id,
            text: from_b.into(),
            ticket: 3,
        },
    );
    bob.until("node B's direct message is queued", |app| {
        app.dm_results.iter().any(|(t, r)| *t == 3 && r.is_ok())
    });
    bob.until("node B shows its own direct message, sent", |app| {
        app.dm_text(key, &alice_in_b, from_b, true)
    });
    alice.until("node A opens node B's direct message", |app| {
        app.dm_text(key, &bob_in_a, from_b, false)
    });
    assert_eq!(alice.dm_news.len(), 1, "A: {:?}", alice.dm_news);
    assert_eq!(alice.dm_news[0].peer_key, bob_in_a);
    assert_eq!(
        bob.dm_news.len(),
        1,
        "B's own send is not news: {:?}",
        bob.dm_news
    );
    // Both threads hold both messages, A's first (it was sent and settled before B's), and the
    // sender's view of what it sent is the text and not a placeholder.
    for (app, peer) in [(&*alice, &bob_in_a), (&*bob, &alice_in_b)] {
        use river_app::chat::dm_model::DmBody;
        let t = app.thread(key, peer).expect("a thread");
        assert_eq!(t.messages.len(), 2, "{}: {:?}", app.name, t);
        assert!(t.messages.iter().all(|m| m.pending.is_none()));
        let bodies: Vec<&str> = t
            .messages
            .iter()
            .map(|m| match &m.body {
                DmBody::Text(body) => body.as_str(),
                other => panic!("{}: not text: {other:?}", app.name),
            })
            .collect();
        assert_eq!(bodies, [from_a, from_b], "{}: in the order sent", app.name);
    }
    // The room's own conversation is untouched by direct messages.
    assert!(!alice.has_text(room, from_a) && !bob.has_text(room, from_b));

    // The sent text is in each node's chat delegate, where the web UI reads it. Then A's app
    // restarts: its memory is gone, and its own sent message reads as text from the delegate.
    wait_until_saved("A", &alice.authority, &alice.dir, from_a);
    wait_until_saved("B", &bob.authority, &bob.dir, from_b);
    let (texts, hidden) = stored_sent_texts(&alice.authority, &alice.dir).expect("A's store");
    assert_eq!((texts, hidden), (vec![from_a.to_owned()], 0));
    let generation = stored_sent_generation(&alice.authority, &alice.dir).expect("A's generation");
    assert!(generation > 0, "the key was stored");
    restart(alice);
    alice.until("A, restarted, reads its own sent message back", |app| {
        app.dm_text(key, &bob_in_a, from_a, true) && app.dm_text(key, &bob_in_a, from_b, false)
    });
    println!("A: after a restart, its own sent direct message reads as its text");
    // Reading it back wrote nothing: a few seconds of the app running (many of its ticks and a
    // reread of its rooms) leave the key at the generation it had before the restart.
    alice.drain(Duration::from_secs(4));
    let (texts, _) = stored_sent_texts(&alice.authority, &alice.dir).expect("A's store");
    assert_eq!(texts, vec![from_a.to_owned()]);
    assert_eq!(
        stored_sent_generation(&alice.authority, &alice.dir).expect("A's generation"),
        generation,
        "a store was made after the restart"
    );
}

/// Invitations in direct messages: A makes a second room (a public one, then a private one), sends
/// B an invitation to it in a direct message in `room` (key `key`), and B accepts it from the
/// message. B's thread shows a card for a room it does not have, with A's note; A's own thread
/// shows the sent card; the sent text A's delegate holds is the web UI's `[Invitation]` summary
/// and nothing of the invitation; B joins through the card (a public room, then a private one,
/// where B can read it with the secret the invitation carried); accepting again is refused as
/// already a member; and an accept naming a message that is not there is refused.
fn invitations_in_direct_messages(alice: &mut App, bob: &mut App, room: &str, key: &str) {
    use river_app::chat::dm_model::DmBody;
    use river_app::rooms::dm::{peer_key, token_from_hex};
    let id_in = |app: &App, name: &str| {
        let full = app
            .room(room)
            .and_then(|r| r.members.iter().find(|m| m.nickname == name))
            .map(|m| m.full_id)
            .unwrap_or_else(|| panic!("{name} is not a member in {}'s view", app.name));
        river_core::room_state::member::MemberId(freenet_scaffold::util::FastHash(full))
    };
    let (bob_id, alice_id) = (id_in(alice, "Bob"), id_in(bob, "Alice"));
    let (bob_in_a, alice_in_b) = (peer_key(bob_id), peer_key(alice_id));

    for (n, (name, private)) in [("Invited room", false), ("Invited private room", true)]
        .into_iter()
        .enumerate()
    {
        let mut tickets = 20 + 10 * n as u64;
        let mut ticket = || {
            tickets += 1;
            tickets
        };
        let (send_ticket, missing_ticket, accept_ticket, again_ticket) =
            (ticket(), ticket(), ticket(), ticket());
        let note = format!("come along to {name}");
        alice.send(
            "",
            Action::CreateRoom {
                name: name.into(),
                nickname: "Alice".into(),
                private,
            },
        );
        alice.until(&format!("{name} is created and shown"), |app| {
            app.room(name).is_some_and(|r| r.send_block.is_none())
        });
        let target = alice.room(name).unwrap().key.clone();

        alice.send(
            key,
            Action::SendInviteDm {
                peer: bob_id,
                target: target.clone(),
                note: Some(note.clone()),
                ticket: send_ticket,
            },
        );
        alice.until("the invitation is queued", |app| {
            app.dm_invite_results
                .iter()
                .any(|(t, r)| *t == send_ticket && r.is_ok())
        });
        // A's own card: sent, with the note, nothing pending.
        alice.until("A shows the invitation it sent", |app| {
            app.thread(key, &bob_in_a).is_some_and(|t| {
                t.messages.iter().any(|m| {
                    m.outgoing
                        && m.pending.is_none()
                        && matches!(&m.body, DmBody::SentInvite { note: Some(n) } if *n == note)
                })
            })
        });
        // B's card: a room B does not have, A's note, and the key B would join (named by the
        // message): the label is a short form of the key.
        let card = |app: &App| {
            app.thread(key, &alice_in_b).and_then(|t| {
                t.messages.iter().find_map(|m| match &m.body {
                    DmBody::Invite {
                        room,
                        room_label,
                        note: Some(n),
                    } if !m.outgoing && *n == note => {
                        Some((token_from_hex(&m.key), room.clone(), room_label.clone()))
                    }
                    _ => None,
                })
            })
        };
        bob.until("B opens A's invitation", |app| card(app).is_some());
        let (token, named, label) = card(bob).unwrap();
        assert_eq!(named, target, "the card names the room it is for");
        assert!(label.starts_with("Room "), "{label}");
        assert!(bob.room(name).is_none(), "B is not in the room yet");
        let token = token.expect("a message is keyed by its purge token");

        // What A's delegate holds for it is the summary and nothing else.
        let summary = format!("[Invitation] {note}");
        wait_until_saved("A", &alice.authority, &alice.dir, &summary);
        let (texts, _) = stored_sent_texts(&alice.authority, &alice.dir).expect("A's store");
        let mine: Vec<_> = texts.iter().filter(|t| t.contains(&note)).collect();
        assert!(
            !mine.is_empty() && mine.iter().all(|t| **t == summary),
            "the entries naming the note are exactly the summary: {texts:?}"
        );

        // An accept naming a message that is not there is refused, and joins nothing.
        let none = river_core::room_state::direct_messages::PurgeToken([7; 16]);
        bob.send(
            key,
            Action::AcceptInviteDm {
                peer: alice_id,
                token: none,
                ticket: missing_ticket,
            },
        );
        bob.until(
            "an accept of a message that is not there is refused",
            |app| {
                app.dm_invite_results.iter().any(|(t, r)| {
                    *t == missing_ticket && r.as_ref().is_err_and(|e| e.contains("no longer"))
                })
            },
        );
        assert!(bob.room(name).is_none());

        // B accepts: the room appears, readable (a private room opens with the invitation's
        // secrets), and A sees B in it.
        bob.send(
            key,
            Action::AcceptInviteDm {
                peer: alice_id,
                token,
                ticket: accept_ticket,
            },
        );
        bob.until("the accept is started", |app| {
            app.dm_invite_results
                .iter()
                .any(|(t, r)| *t == accept_ticket && r.is_ok())
        });
        bob.until(&format!("B joined {name} from the card"), |app| {
            if let Some(t) = app
                .work
                .iter()
                .find(|t| matches!(t.state, WorkState::Failed { .. }))
            {
                panic!("joining failed: {t:?}");
            }
            app.room(name).is_some_and(|r| r.send_block.is_none())
        });
        alice.until(&format!("A sees B join {name}"), |app| {
            app.room(name).is_some_and(|r| r.members.len() >= 2)
        });
        // The room works both ways: B is a member with an entry of its own, and what A posts is
        // read in plaintext by B (in the private room, with the secrets the invitation carried).
        let hello = format!("Welcome to {name}");
        alice.send(
            &target,
            Action::Send {
                text: hello.clone(),
                reply_to: None,
            },
        );
        bob.until(&format!("B reads A's message in {name}"), |app| {
            app.has_text(name, &hello)
                && app
                    .room(name)
                    .is_some_and(|r| r.members.iter().any(|m| m.id == r.me))
        });
        // Accepting again: B is a member now.
        bob.send(
            key,
            Action::AcceptInviteDm {
                peer: alice_id,
                token,
                ticket: again_ticket,
            },
        );
        bob.until("a second accept is refused", |app| {
            app.dm_invite_results.iter().any(|(t, r)| {
                *t == again_ticket && r.as_ref().is_err_and(|e| e.contains("already a member"))
            })
        });
        println!("B: accepted the invitation to {name} from the direct message");

        bob.until("B's join work is done", |app| app.work.is_empty());
        // Both leave the room again, so the steps after this start from the rooms they had.
        let (b_authority, b_dir) = (bob.authority.clone(), bob.dir.clone());
        leave_room(bob, &b_authority, &b_dir, name, &target, Some(room));
        let (a_authority, a_dir) = (alice.authority.clone(), alice.dir.clone());
        leave_room(alice, &a_authority, &a_dir, name, &target, Some(room));
    }
}

/// Archive, as the screen does: A archives its thread with B, the delegate holds the entry (the
/// web UI's own shape, keyed by B's full member id) with the sent texts kept, the thread stays
/// archived across a restart, a new message from B brings it back and the entry is dropped from
/// the delegate (the gate), and sending into an archived thread unarchives it. Runs after
/// [`delete_their_messages`]: A's thread holds B's one message.
fn archive_thread(alice: &mut App, bob: &mut App, room: &str, key: &str) {
    use river_app::rooms::dm::peer_key;
    use river_app::rooms::dm_archive::is_hidden;
    let id_in = |app: &App, name: &str| {
        let full = app
            .room(room)
            .and_then(|r| r.members.iter().find(|m| m.nickname == name))
            .map(|m| m.full_id)
            .unwrap_or_else(|| panic!("{name} is not a member in {}'s view", app.name));
        river_core::room_state::member::MemberId(freenet_scaffold::util::FastHash(full))
    };
    let (bob_id, alice_id) = (id_in(alice, "Bob"), id_in(bob, "Alice"));
    let (bob_in_a, alice_in_b) = (peer_key(bob_id), peer_key(alice_id));
    let held = |app: &App| {
        app.dm_archived
            .iter()
            .find(|a| a.room == key && a.peer_key == bob_in_a)
            .map(|a| a.hidden_at_ts)
    };
    let stored = |app: &App| {
        stored_store(&app.authority, &app.dir)
            .expect("the store")
            .hidden_threads
    };
    let texts_before = stored_sent_texts(&alice.authority, &alice.dir)
        .expect("A's store")
        .0;
    let newest = alice
        .thread(key, &bob_in_a)
        .expect("a thread")
        .last_inbound_signed;
    assert!(newest > 0, "B wrote to A");
    assert_eq!(held(alice), None);

    // A archives the thread: it is archived on screen, and the delegate holds the entry in the
    // web UI's shape: A's room and B's full id, at B's newest message.
    alice.send(
        key,
        Action::ArchiveDm {
            peer: bob_id,
            row_inbound_ts: newest,
        },
    );
    alice.until("A's archive list holds B's thread", |app| {
        held(app) == Some(newest)
    });
    let deadline = Instant::now() + STEP;
    loop {
        let list = stored(alice);
        if list.len() == 1 {
            let h = &list[0];
            assert_eq!(h.peer, bob_id, "keyed by the full member id");
            assert_eq!(h.hidden_at_ts, newest);
            assert_eq!(
                bs58::encode(h.room_owner_vk).into_string(),
                key,
                "keyed by the room's owner"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the delegate never held the entry"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    assert_eq!(
        stored_sent_texts(&alice.authority, &alice.dir)
            .expect("A's store")
            .0,
        texts_before,
        "the sent texts are kept"
    );
    println!("A: the archive is in the delegate, with the sent texts kept");

    // A restarts: the archive is read back, and the thread is off A's rail (the peer has not
    // written past the cutoff).
    restart(alice);
    alice.until("A, restarted, reads the archive back", |app| {
        held(app) == Some(newest) && app.thread(key, &bob_in_a).is_some()
    });
    assert!(is_hidden(
        held(alice),
        alice.thread(key, &bob_in_a).unwrap().last_inbound_signed
    ));
    println!("A: after a restart the thread is still archived");

    // B writes again, strictly later: the thread is back on A's rail, and A's app drops the
    // entry from the delegate.
    // Times are whole seconds: wait until the clock is past the cutoff, so B's message is stamped
    // strictly later (the gap is checked on the message below, not assumed).
    let clock_deadline = Instant::now() + STEP;
    while std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_secs()
        <= newest
    {
        assert!(
            Instant::now() < clock_deadline,
            "the clock never passed the cutoff {newest}: B's message could not be stamped later"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let again = "DM from B after the archive";
    bob.send(
        key,
        Action::SendDm {
            peer: alice_id,
            text: again.into(),
            ticket: 90,
        },
    );
    bob.until("B's second direct message is queued", |app| {
        app.dm_results.iter().any(|(t, r)| *t == 90 && r.is_ok())
    });
    alice.until("A receives B's new message", |app| {
        app.dm_text(key, &bob_in_a, again, false)
    });
    let latest = alice.thread(key, &bob_in_a).unwrap().last_inbound_signed;
    assert!(latest > newest, "B's message is stamped past the cutoff");
    assert!(
        !is_hidden(Some(newest), latest),
        "a newer message from B brings the thread back"
    );
    let deadline = Instant::now() + STEP;
    while !stored(alice).is_empty() {
        assert!(Instant::now() < deadline, "the entry was never dropped");
        alice.drain(Duration::from_millis(500));
    }
    alice.until("A's archive list no longer holds it", |app| {
        held(app).is_none()
    });
    println!("A: a newer message from B brought the thread back and dropped the entry");

    // Archive again, then send into the thread: sending unarchives it, on the same write path.
    let newest = alice.thread(key, &bob_in_a).unwrap().last_inbound_signed;
    alice.send(
        key,
        Action::ArchiveDm {
            peer: bob_id,
            row_inbound_ts: newest,
        },
    );
    alice.until("A archives it again", |app| held(app) == Some(newest));
    let reply = "A replies into an archived thread";
    alice.send(
        key,
        Action::SendDm {
            peer: bob_id,
            text: reply.into(),
            ticket: 91,
        },
    );
    alice.until("A's reply is queued", |app| {
        app.dm_results.iter().any(|(t, r)| *t == 91 && r.is_ok())
    });
    alice.until("sending unarchived the thread", |app| held(app).is_none());
    let deadline = Instant::now() + STEP;
    while !stored(alice).is_empty() {
        assert!(Instant::now() < deadline, "the entry was never removed");
        alice.drain(Duration::from_millis(500));
    }
    wait_until_saved("A", &alice.authority, &alice.dir, reply);
    bob.until("B receives A's reply", |app| {
        app.dm_text(key, &alice_in_b, reply, false)
    });
    println!("A: sending into an archived thread unarchived it");
}

/// Allow the grant on its approval page, as the user would: read the page's nonce, wait out
/// the node's delay before Allow takes effect, then submit Allow from the node's own origin.
fn approve(authority: &str, url: &str) {
    let agent = ureq::agent();
    let page = agent
        .get(url)
        .call()
        .expect("the approval page")
        .into_string()
        .expect("the page's text");
    let nonce = page
        .split("data-nonce=\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .expect("a nonce on the approval page")
        .to_owned();
    // The node refuses an Allow sent sooner than this after the page was shown.
    std::thread::sleep(Duration::from_millis(1700));
    let status = agent
        .post(url)
        .set("Origin", &format!("http://{authority}"))
        .set("Content-Type", "application/json")
        .send_string(&format!("{{\"nonce\":\"{nonce}\",\"allow\":true}}"))
        .map(|r| r.status())
        .unwrap_or_else(|e| panic!("allowing the grant failed: {e}"));
    println!("approved the grant on {authority} (HTTP {status})");
}

/// Ask node `authority` directly, on a connection of its own with the app's grant (read from
/// `dir`, where the app keeps it), for the room's stored identity and the signing key River's
/// chat delegate holds for it. `Ok((identity, held))`: the public half of `self_sk` as the
/// stored room has it, and the public key the delegate answers with, if any.
fn delegate_key(
    authority: &str,
    dir: &Path,
    owner: &VerifyingKey,
) -> Result<(VerifyingKey, Option<[u8; 32]>), String> {
    let token = TokenStore::in_dir(&node_dir(dir, authority, false))
        .load()
        .ok_or("the app has no grant token")?;
    let mut socket = NodeSocket::connect(authority, &token, &Default::default())
        .map_err(|e| format!("{e:?}"))?;
    socket
        .set_read_timeout(Duration::from_secs(10))
        .map_err(|e| format!("{e:?}"))?;
    let mut ask = |request| -> Result<ChatDelegateResponseMsg, String> {
        socket.send(&request).map_err(|e| format!("{e:?}"))?;
        loop {
            match socket.recv().map_err(|e| format!("{e:?}"))? {
                Ok(response @ HostResponse::DelegateResponse { .. }) => {
                    if let Some(answer) = delegate::answers(&response).pop() {
                        return Ok(answer);
                    }
                }
                Ok(_) => {}
                Err(e) => return Err(format!("the node answered with an error: {e}")),
            }
        }
    };
    let slot = match ask(delegate::request(&Read::Get(room_storage_key(owner))))? {
        ChatDelegateResponseMsg::GetResponse { value: Some(v), .. } => v,
        other => return Err(format!("no stored room: {other:?}")),
    };
    let StoredRoomSlot::Present(room) = decode_room_slot(&slot)? else {
        return Err("the room is a tombstone".into());
    };
    let identity = room
        .self_verifying_key()
        .ok_or("the stored room has no identity")?;
    match ask(delegate::public_key(owner))? {
        ChatDelegateResponseMsg::GetPublicKeyResponse { public_key, .. } => {
            Ok((identity, public_key))
        }
        other => Err(format!("unexpected answer: {other:?}")),
    }
}

/// The room list's view preferences (`rooms_meta`) as the delegate on `authority` holds them.
fn stored_meta(authority: &str, dir: &Path) -> Result<StoredRoomsMeta, String> {
    let token = TokenStore::in_dir(&node_dir(dir, authority, false))
        .load()
        .ok_or("the app has no grant token")?;
    let mut socket = NodeSocket::connect(authority, &token, &Default::default())
        .map_err(|e| format!("{e:?}"))?;
    socket
        .set_read_timeout(Duration::from_secs(10))
        .map_err(|e| format!("{e:?}"))?;
    socket
        .send(&delegate::request(&Read::Get(ROOMS_META_KEY.to_vec())))
        .map_err(|e| format!("{e:?}"))?;
    loop {
        match socket.recv().map_err(|e| format!("{e:?}"))? {
            Ok(response @ HostResponse::DelegateResponse { .. }) => {
                if let Some(ChatDelegateResponseMsg::GetResponse { value, .. }) =
                    delegate::answers(&response).pop()
                {
                    return decode_rooms_meta(&value.ok_or("no rooms_meta")?);
                }
            }
            Ok(_) => {}
            Err(e) => return Err(format!("the node answered with an error: {e}")),
        }
    }
}

/// The bytes node `authority` holds in the chat delegate's storage for the room owned by the
/// key `room_key` (the stored slot, as the web UI wrote it), asked on a connection of its own.
fn stored_slot(authority: &str, dir: &Path, room_key: &str) -> Vec<u8> {
    let bytes: [u8; 32] = bs58::decode(room_key)
        .into_vec()
        .expect("a base58 room key")
        .try_into()
        .expect("32 bytes");
    let owner = VerifyingKey::from_bytes(&bytes).expect("the room key is the owner's key");
    let token = TokenStore::in_dir(&node_dir(dir, authority, false))
        .load()
        .expect("the app's grant token");
    let mut socket = NodeSocket::connect(authority, &token, &Default::default()).expect("connect");
    socket
        .set_read_timeout(Duration::from_secs(10))
        .expect("a read timeout");
    socket
        .send(&delegate::request(&Read::Get(room_storage_key(&owner))))
        .expect("send");
    loop {
        match socket.recv().expect("recv") {
            Ok(response @ HostResponse::DelegateResponse { .. }) => {
                if let Some(ChatDelegateResponseMsg::GetResponse { value: Some(v), .. }) =
                    delegate::answers(&response).pop()
                {
                    return v;
                }
                panic!("no stored room");
            }
            Ok(_) => {}
            Err(e) => panic!("the node answered with an error: {e}"),
        }
    }
}

/// Wait until the delegate on `authority` holds, for the room, the public key of the member's
/// own stored identity, then say so. Panics after [`STEP`] with what it last saw.
fn expect_signing_key(name: &str, authority: &str, dir: &Path, room_key: &str) -> VerifyingKey {
    let bytes: [u8; 32] = bs58::decode(room_key)
        .into_vec()
        .expect("a base58 room key")
        .try_into()
        .expect("32 bytes");
    let owner = VerifyingKey::from_bytes(&bytes).expect("the room key is the owner's key");
    let deadline = Instant::now() + STEP;
    loop {
        let last = match delegate_key(authority, dir, &owner) {
            Ok((identity, Some(held))) if held == identity.to_bytes() => {
                println!("{name}: the delegate holds the member's own signing key");
                return identity;
            }
            Ok((identity, held)) => {
                format!(
                    "identity {:?}, delegate holds {held:?}",
                    identity.to_bytes()
                )
            }
            Err(e) => e,
        };
        assert!(
            Instant::now() < deadline,
            "{name}: the delegate never held the room's signing key (last: {last})"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// PUT a pointer record for `app_id` signed by `sk` at `version`, naming `code_hash`, to the
/// test node at `authority`. Returns once the node answers the PUT.
fn put_test_pointer(
    authority: &str,
    dir: &Path,
    pointer_wasm: &[u8],
    sk: &ed25519_dalek::SigningKey,
    app_id: &str,
    version: u32,
    code_hash: [u8; 32],
) {
    use ed25519_dalek::Signer;
    use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse};
    use freenet_stdlib::prelude::{
        ContractCode, ContractContainer, ContractWasmAPIVersion, Parameters, RelatedContracts,
        WrappedContract, WrappedState,
    };
    let params = freenet_migrate::pointer_params(&sk.verifying_key(), app_id.as_bytes()).unwrap();
    let msg = freenet_migrate::pointer_signing_message(&params, version, &code_hash);
    let record = freenet_migrate::PointerRecord {
        version,
        code_hash,
        signature: sk.sign(&msg).to_bytes(),
    }
    .encode()
    .to_vec();
    let contract = WrappedContract::new(
        std::sync::Arc::new(ContractCode::from(pointer_wasm.to_vec())),
        Parameters::from(params),
    );
    let expected =
        freenet_migrate::pointer_contract_id(&sk.verifying_key(), app_id.as_bytes()).unwrap();
    assert_eq!(
        *contract.key().id(),
        expected,
        "the pointer WASM does not give the address freenet-migrate derives"
    );
    let token = TokenStore::in_dir(&node_dir(dir, authority, false))
        .load()
        .expect("a grant token");
    let mut socket = NodeSocket::connect(authority, &token, &Default::default()).unwrap();
    socket.set_read_timeout(STEP).unwrap();
    socket
        .send(&ClientRequest::ContractOp(ContractRequest::Put {
            contract: ContractContainer::Wasm(ContractWasmAPIVersion::V1(contract)),
            state: WrappedState::new(record),
            related_contracts: RelatedContracts::default(),
            subscribe: false,
            blocking_subscribe: false,
        }))
        .unwrap();
    loop {
        match socket.recv().expect("an answer to the pointer PUT") {
            Ok(HostResponse::ContractResponse(ContractResponse::PutResponse { key })) => {
                assert_eq!(*key.id(), expected);
                println!("put a test pointer for {app_id} at v{version}: {expected}");
                return;
            }
            Ok(_) => {}
            Err(e) => panic!("the pointer PUT failed: {e}"),
        }
    }
}

/// On node A, a newer River named by a test pointer: a third app that trusts the test key
/// shows the room, says an update is needed, and nothing it is asked to write reaches the
/// node.
fn newer_river(
    authority: &str,
    dir: &Path,
    dir_a: &Path,
    room: &str,
    room_key: &str,
    bob: &mut App,
) {
    let path = std::env::var("POINTER_WASM").expect(
        "POINTER_WASM names the pointer contract WASM (apps/scripts/fresh-node-e2e.sh fetches it)",
    );
    let wasm = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    assert_eq!(
        bs58::encode(blake3::hash(&wasm).as_bytes()).into_string(),
        freenet_migrate::POINTER_CODE_HASH_B58,
        "{path} is not the frozen pointer contract"
    );
    let sk = ed25519_dalek::SigningKey::from_bytes(&rand::random::<[u8; 32]>());
    // This build at version 1 of the test sequence, for the WASM it really carries.
    let records = Records {
        author: format!(
            "river:v1:vk:{}",
            bs58::encode(sk.verifying_key().as_bytes()).into_string()
        ),
        records: Artifact::ALL
            .iter()
            .map(|a| BuildRecord {
                app_id: a.app_id().to_owned(),
                version: 1,
                code_hash: *blake3::hash(a.embedded_wasm()).as_bytes(),
            })
            .collect(),
    };
    let watched = watched_pointers(&records, Artifact::embedded_wasm);
    assert_eq!(watched.len(), 2);
    // First both pointers name this build. An app trusting the test key is current, and what
    // it sends reaches node B: the check below that the outdated app's message does not can
    // fail.
    put_test_pointer(
        authority,
        dir_a,
        &wasm,
        &sk,
        "river.room-contract",
        1,
        records.records[0].code_hash,
    );
    put_test_pointer(
        authority,
        dir_a,
        &wasm,
        &sk,
        "river.chat-delegate",
        1,
        records.records[1].code_hash,
    );
    let dir_d = dir.join("app-d");
    std::fs::create_dir_all(&dir_d).unwrap();
    let current = Watch::new(
        watched.clone(),
        std::sync::Arc::new(NodePointers),
        Some(&dir_d),
    );
    let mut dave = App::start_with("D", authority, &dir_d, Some(current));
    dave.until("the room shows, and can be written to", |app| {
        !app.issues.update_required && app.room(room).is_some_and(|r| r.send_block.is_none())
    });
    let sent = "Sent from a current app trusting the test key";
    dave.send(
        room_key,
        Action::Send {
            text: sent.into(),
            reply_to: None,
        },
    );
    bob.until("node B receives the current app's message", |app| {
        app.has_text(room, sent)
    });

    // Then the chat delegate's pointer moves on.
    put_test_pointer(
        authority,
        dir_a,
        &wasm,
        &sk,
        "river.chat-delegate",
        2,
        [0x42; 32],
    );

    let dir_c = dir.join("app-c");
    std::fs::create_dir_all(&dir_c).unwrap();
    let watch = Watch::new(watched, std::sync::Arc::new(NodePointers), Some(&dir_c));
    let mut carol = App::start_with("C", authority, &dir_c, Some(watch));
    carol.until(
        "the room shows, read-only, and an update is asked for",
        |app| {
            app.issues.update_required
                && app.room(room).is_some_and(|r| {
                    r.send_block
                        .as_deref()
                        .is_some_and(|b| b.contains("Update"))
                })
        },
    );
    let text = "Sent from an outdated app";
    carol.send(
        room_key,
        Action::Send {
            text: text.into(),
            reply_to: None,
        },
    );
    carol.send(
        "",
        Action::CreateRoom {
            name: "Outdated room".into(),
            nickname: "Carol".into(),
            private: false,
        },
    );
    carol.until("both are refused, saying why", |app| {
        let sent = app.room(room).is_some_and(|r| {
            r.messages.iter().any(|m| {
                m.text == text
                    && m.pending
                        .as_ref()
                        .is_some_and(|p| p.failed.as_deref() == Some(UPDATE_REQUIRED))
            })
        });
        let created = app.work.iter().any(
            |w| matches!(&w.state, WorkState::Failed { why, .. } if why == UPDATE_REQUIRED_WORK),
        );
        sent && created
    });
    // The record is kept, so a later start knows without the network.
    assert!(dir_c.join("newer-river.river.chat-delegate").exists());
    println!("C: an app River has moved past writes nothing");
}

/// `bob` (a member) and then `alice` (the owner) change their own nickname in `room`, and each
/// other's app shows the new name everywhere the room names a member, with nothing left
/// waiting on the changer's own screen. The names are `<name>` plus a suffix, so reruns in the
/// same room do not match an earlier run's.
fn rename_both_ways(
    alice: &mut App,
    bob: &mut App,
    room: &str,
    key: &str,
    bob_was: &str,
    alice_was: &str,
) {
    let renamed = |app: &App, who: &str, name: &str| {
        app.room(room).is_some_and(|r| {
            r.members.iter().any(|m| m.nickname == name)
                && !r.members.iter().any(|m| m.nickname == who)
                && r.messages.iter().all(|m| r.nickname(m.author) != who)
        })
    };
    let settled = |app: &App| app.room(room).is_some_and(|r| r.own_nickname.is_none());
    let bob_now = format!("{bob_was} Two");
    let alice_now = format!("{alice_was} Prime");
    bob.send(
        key,
        Action::SetNickname {
            nickname: format!("  {bob_now}  "),
        },
    );
    alice.until("the other app sees Bob's new nickname everywhere", |app| {
        renamed(app, bob_was, &bob_now)
    });
    bob.until("Bob's app shows its new nickname, settled", |app| {
        renamed(app, bob_was, &bob_now) && settled(app)
    });
    alice.send(
        key,
        Action::SetNickname {
            nickname: alice_now.clone(),
        },
    );
    bob.until("the other app sees the owner's new nickname", |app| {
        renamed(app, alice_was, &alice_now)
    });
    alice.until("the owner's app shows its new nickname, settled", |app| {
        renamed(app, alice_was, &alice_now) && settled(app)
    });
}

/// The owner on node A makes Bob (renamed by [`rename_both_ways`]) a deputy: a new signed record
/// of the owner's with Bob in its `deputies` and the owner's nickname carried over. The app on
/// node B, Bob's, shows him with the Deputy role, appointed by the owner. Revoking removes it
/// on both, and the owner's nickname survives both changes.
fn deputize_and_revoke(alice: &mut App, bob: &mut App, room: &str, key: &str) {
    use river_app::chat::model::{Appointer, Role};
    const BOB: &str = "Bob Two";
    const ALICE: &str = "Alice Prime";
    let uid = alice
        .room(room)
        .and_then(|r| r.members.iter().find(|m| m.nickname == BOB))
        .map(|m| m.uid.clone())
        .expect("Bob is a member on node A");
    let is_deputy = |app: &App| {
        app.room(room).is_some_and(|r| {
            r.members.iter().any(|m| {
                m.nickname == BOB && m.role == Role::Deputy && m.deputized_by == [Appointer::Owner]
            })
        })
    };
    let not_deputy = |app: &App| {
        app.room(room).is_some_and(|r| {
            r.members
                .iter()
                .any(|m| m.nickname == BOB && m.role == Role::Member && !m.my_deputy)
        })
    };
    let settled = |app: &App| app.room(room).is_some_and(|r| r.deputy_changes.is_empty());
    let owner_named = |app: &App| {
        app.room(room)
            .is_some_and(|r| r.members.iter().any(|m| m.nickname == ALICE))
    };
    alice.send(
        key,
        Action::SetDeputy {
            target: uid.clone(),
            grant: true,
        },
    );
    alice.until("node A shows Bob as its owner's deputy, settled", |app| {
        is_deputy(app)
            && settled(app)
            && app
                .room(room)
                .is_some_and(|r| r.members.iter().any(|m| m.nickname == BOB && m.my_deputy))
    });
    bob.until("node B's app shows Bob with the Deputy tag", |app| {
        is_deputy(app)
    });
    assert!(
        owner_named(alice) && owner_named(bob),
        "the owner's nickname was kept"
    );
    alice.send(
        key,
        Action::SetDeputy {
            target: uid,
            grant: false,
        },
    );
    alice.until("node A shows Bob as a plain member again, settled", |app| {
        not_deputy(app) && settled(app)
    });
    bob.until("node B's app shows Bob without the Deputy tag", |app| {
        not_deputy(app)
    });
    assert!(
        owner_named(alice) && owner_named(bob),
        "the owner's nickname was kept"
    );
}

/// A member the example plays itself, with no app: she joins by an invitation and reads a room
/// from a node with her own key. Used for a member who stays when another is banned.
struct Carol {
    authority: String,
    dir: PathBuf,
    owner: VerifyingKey,
    sk: ed25519_dalek::SigningKey,
    /// Every room secret she has held, by version: the invitation's, and each one a blob in the
    /// room was sealed to her for. A ban removes her blobs from the room, so what she can read
    /// afterwards is decided by this set and not by what the room still holds for her.
    known: std::sync::Mutex<std::collections::HashMap<u32, [u8; 32]>>,
}

impl Carol {
    fn socket(authority: &str, dir: &Path) -> NodeSocket {
        let token = TokenStore::in_dir(&node_dir(dir, authority, false))
            .load()
            .expect("a grant token");
        let mut socket =
            NodeSocket::connect(authority, &token, &Default::default()).expect("a connection");
        socket
            .set_read_timeout(Duration::from_secs(10))
            .expect("a timeout");
        socket
    }

    /// The room's state as `authority` holds it.
    fn state(&self) -> river_core::room_state::ChatRoomStateV1 {
        use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse};
        let key = river_app::rooms::create::contract_key(&self.owner);
        let mut socket = Carol::socket(&self.authority, &self.dir);
        socket
            .send(&ClientRequest::ContractOp(ContractRequest::Get {
                key: *key.id(),
                return_contract_code: false,
                subscribe: false,
                blocking_subscribe: false,
            }))
            .expect("a GET");
        loop {
            match socket.recv().expect("an answer") {
                Ok(HostResponse::ContractResponse(ContractResponse::GetResponse {
                    state, ..
                })) => {
                    return ciborium::de::from_reader(state.as_ref()).expect("a room state");
                }
                Ok(_) => {}
                Err(e) => panic!("the node refused the GET: {e}"),
            }
        }
    }

    /// Join `link`'s room as "Carol" and wait for the node to take it.
    fn join(authority: &str, dir: &Path, link: &str) -> Carol {
        use freenet_stdlib::client_api::{ClientRequest, ContractRequest};
        use freenet_stdlib::prelude::{StateDelta, UpdateData};
        let code = river_core::invitation::invitation_text_from_paste(link);
        let invitation =
            river_core::invitation::Invitation::from_encoded_string(&code).expect("an invitation");
        let carol = Carol {
            authority: authority.to_owned(),
            dir: dir.to_owned(),
            owner: invitation.room,
            sk: invitation.invitee_signing_key.clone(),
            known: std::sync::Mutex::new(invitation.room_secrets.iter().copied().collect()),
        };
        let key = river_app::rooms::create::contract_key(&carol.owner);
        let plan = river_app::rooms::create::join(
            &invitation,
            "Carol",
            &carol.state(),
            key,
            std::time::SystemTime::now(),
        )
        .unwrap_or_else(|e| panic!("Carol cannot join: {}", e.why));
        let delta = plan.delta.expect("Carol is new to the room");
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&delta, &mut bytes).expect("a delta");
        let mut socket = Carol::socket(authority, dir);
        socket
            .send(&ClientRequest::ContractOp(ContractRequest::Update {
                key,
                data: UpdateData::Delta(StateDelta::from(bytes)),
            }))
            .expect("an UPDATE");
        loop {
            match socket.recv().expect("an answer") {
                Ok(HostResponse::ContractResponse(
                    freenet_stdlib::client_api::ContractResponse::UpdateResponse { .. },
                )) => break,
                Ok(_) => {}
                Err(e) => panic!("the node refused Carol's join: {e}"),
            }
        }
        println!("D: Carol joined");
        carol
    }

    /// An invitation Carol writes for a new member, as the app writes one for its user.
    fn invitation(&self) -> river_core::invitation::Invitation {
        use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
        let invitee = ed25519_dalek::SigningKey::from_bytes(&rand::random::<[u8; 32]>());
        let member = Member {
            owner_member_id: MemberId::from(&self.owner),
            invited_by: MemberId::from(&self.sk.verifying_key()),
            member_vk: invitee.verifying_key(),
        };
        river_core::invitation::Invitation {
            room: self.owner,
            invitee: AuthorizedMember::new(member, &self.sk),
            invitee_signing_key: invitee,
            room_secrets: Vec::new(),
        }
    }

    /// Wait (bounded) until node B holds a message the owner wrote under secret version
    /// `version`, so a later "cannot read" is about the key and not about that message not
    /// having arrived.
    fn wait_for_sealed(&self, version: u32) {
        use river_core::room_state::member::MemberId;
        use river_core::room_state::message::RoomMessageBody;
        let owner = MemberId::from(&self.owner);
        let deadline = Instant::now() + STEP;
        loop {
            let held = self.state().recent_messages.messages.iter().any(|m| {
                m.message.author == owner
                    && matches!(
                        &m.message.content,
                        RoomMessageBody::Private { secret_version, .. } if *secret_version == version
                    )
            });
            if held {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "node B never held the owner's message under secret version {version} (it did not \
                 arrive, or it was sealed under a different version)"
            );
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Wait (bounded) until Carol reads `text`: the positive control for [`Carol::reads`].
    fn wait_reads(&self, text: &str) {
        let deadline = Instant::now() + STEP;
        while !self.reads(text) {
            assert!(Instant::now() < deadline, "Carol never read {text:?}");
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// The newest secret version she holds a key for.
    fn newest_known_version(&self) -> Option<u32> {
        self.known.lock().unwrap().keys().max().copied()
    }

    /// Whether Carol, with her own key and what node B holds, can read the owner's message
    /// `text` in the room.
    fn reads(&self, text: &str) -> bool {
        use river_core::ecies::{
            decrypt_with_symmetric_key, try_decrypt_secret_from_member_blob_raw,
        };
        use river_core::room_state::content::TextContentV1;
        use river_core::room_state::message::RoomMessageBody;
        let state = self.state();
        let me = river_core::room_state::member::MemberId::from(&self.sk.verifying_key());
        let wanted = TextContentV1::new(text.to_owned()).encode();
        let mut known = self.known.lock().unwrap();
        for b in &state.secrets.encrypted_secrets {
            if b.secret.member_id != me {
                continue;
            }
            if let Ok(s) = try_decrypt_secret_from_member_blob_raw(
                &b.secret.ciphertext,
                &b.secret.nonce,
                &b.secret.sender_ephemeral_public_key,
                &self.sk,
            ) {
                known.insert(b.secret.secret_version, s);
            }
        }
        // Every key she holds is tried on every private message, whatever version it names: a
        // message that opens under any of them is read.
        state
            .recent_messages
            .messages
            .iter()
            .any(|m| match &m.message.content {
                RoomMessageBody::Private {
                    ciphertext, nonce, ..
                } => known.values().any(|s| {
                    decrypt_with_symmetric_key(s, ciphertext, nonce)
                        .is_ok_and(|plain| plain == wanted)
                }),
                _ => false,
            })
    }
}

/// The owner on node A bans Bob. Node A drops Bob and everyone he invited; the ban is one update
/// (in a private room it carries the rotated secret). Node B's app is then blocked from the
/// room as banned and, in a private room, never reads what the owner writes afterwards: the
/// room moved to secret version 1, which Bob was not given.
fn ban_and_lock_out(
    alice: &mut App,
    bob: &mut App,
    carol: Option<&Carol>,
    room: &str,
    key: &str,
    private: bool,
) {
    let is_bob = |n: &str| n == "Bob Two" || n == "Bob";
    let uid = alice
        .room(room)
        .and_then(|r| r.members.iter().find(|m| is_bob(&m.nickname)))
        .map(|m| m.uid.clone())
        .expect("Bob is a member on node A");
    let version = |app: &App| app.room(room).map(|r| r.details.secret_version);
    let before = version(alice).unwrap();
    alice.send(key, Action::Ban { target: uid });
    alice.until(
        "node A no longer lists Bob, and nothing is left unsent",
        |app| {
            app.room(room).is_some_and(|r| {
                !r.members.iter().any(|m| is_bob(&m.nickname))
                    && r.notices.is_empty()
                    && r.details.usage.bans == 1
            })
        },
    );
    if private {
        alice.until("node A's room moved to the next secret version", |app| {
            version(app) == Some(before + 1)
        });
    }
    bob.until("node B's app is blocked as banned", |app| {
        app.room(room)
            .and_then(|r| r.send_block.clone())
            .is_some_and(|b| b.contains("banned"))
    });
    // Bob's app has the post-ban state (the new secret version's record) before the owner writes,
    // so what it holds afterwards is only what came after.
    if private {
        bob.until("Bob's app has the post-ban state", |app| {
            version(app) == Some(before + 1)
        });
    }
    let bob_messages_before = bob.room(room).map_or(0, |r| r.messages.len());
    // What the owner writes after the ban. In a private room Carol, a member who stays, reads it
    // through the node (the rotation gave her the new secret) and can write too; Bob, whose node
    // has the same post-ban state, has no readable copy of it.
    let after = format!("After the ban in {room}");
    alice.send(
        key,
        Action::Send {
            text: after.clone(),
            reply_to: None,
        },
    );
    alice.until("the owner sees their own message", |app| {
        app.has_text(room, &after)
    });
    if let Some(carol) = carol {
        // Carol, a member who stays, reads it from node B with the secret the rotation gave her.
        let deadline = Instant::now() + STEP;
        while !carol.reads(&after) {
            assert!(
                Instant::now() < deadline,
                "Carol never read the message sealed under the new secret"
            );
            std::thread::sleep(Duration::from_secs(2));
        }
        println!("D: Carol reads the message sealed under the new secret");
    }
    if private {
        // Not reading it is Bob lacking the secret, not Bob not having received it: wait until
        // Bob's app holds one more message than it did before the owner wrote (it shows as one
        // it cannot read), and only then look for the text.
        bob.until("Bob's app holds the new message too", |app| {
            app.room(room)
                .is_some_and(|r| r.messages.len() > bob_messages_before)
        });
        assert!(
            !bob.has_text(room, &after),
            "the banned member read a message sealed after the ban"
        );
    }
}

/// A room's notification mode is stored in the chat delegate beside the room list (the web UI's
/// `rooms_meta`): set in one app, it is read back by an app started afterwards, as after the app
/// is restarted, and picking it did not lose the room or the open room the create stored.
fn notification_modes(alice: &mut App, authority: &str, dir: &Path, room: &str, key: &str) {
    assert_eq!(alice.modes.get(key), None, "a new room notifies for all");
    alice.send(
        key,
        Action::SetNotificationMode {
            mode: NotificationMode::MentionsAndReplies,
        },
    );
    alice.until("the mode is shown", |app| {
        app.modes.get(key) == Some(&NotificationMode::MentionsAndReplies)
    });
    // Stored, not just remembered: the app's work is done once its task has left the list.
    alice.until("the mode is stored", |app| app.work.is_empty());

    let mut again = App::start("A (restarted)", authority, dir);
    again.until("the rooms and the stored mode are read back", |app| {
        app.access == Some(Access::Ready)
            && app.room(room).is_some()
            && app.modes.get(key) == Some(&NotificationMode::MentionsAndReplies)
    });
    // And back to the default, read back the same way.
    again.send(
        key,
        Action::SetNotificationMode {
            mode: NotificationMode::All,
        },
    );
    again.until("the default is stored", |app| {
        app.modes.get(key) == Some(&NotificationMode::All) && app.work.is_empty()
    });
    let mut last = App::start("A (restarted again)", authority, dir);
    last.until("the default is read back", |app| {
        app.access == Some(Access::Ready)
            && app.room(room).is_some()
            && app.modes.get(key) == Some(&NotificationMode::All)
    });
    println!("notification modes round-trip through the node");
}

/// Leaving a room: the app drops it at once, the chat delegate's slot for it holds the web UI's
/// tombstone (read from the delegate itself, as the web UI would read it), and an app started
/// afterwards, as after restarting, does not bring it back while the room `keep` is still
/// there.
fn leave_room(
    app: &mut App,
    authority: &str,
    dir: &Path,
    name: &str,
    key: &str,
    keep: Option<&str>,
) {
    let bytes: [u8; 32] = bs58::decode(key)
        .into_vec()
        .expect("a base58 room key")
        .try_into()
        .expect("32 bytes");
    let owner = VerifyingKey::from_bytes(&bytes).expect("the room key is the owner's key");
    app.send(key, Action::LeaveRoom);
    app.until("the room is gone and the leave is stored", |a| {
        a.room(name).is_none() && a.work.is_empty()
    });
    if let Some(keep) = keep {
        assert!(app.room(keep).is_some(), "leaving one room kept the other");
    }
    let deadline = Instant::now() + STEP;
    loop {
        match delegate_key(authority, dir, &owner) {
            Err(e) if e.contains("tombstone") => break,
            other => assert!(
                Instant::now() < deadline,
                "the delegate never held a tombstone for the room (last: {other:?})"
            ),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // The room list the web UI reads no longer names the room: not in its order, not open.
    let meta = stored_meta(authority, dir).expect("rooms_meta is stored");
    assert!(
        !meta.room_order.contains(&owner),
        "rooms_meta still orders the room"
    );
    assert_ne!(
        meta.current_room_key,
        Some(owner),
        "rooms_meta still opens the room"
    );
    let mut again = App::start("restarted after leaving", authority, dir);
    // Ready is after the stored rooms were read; with another room, that room shows too.
    match keep {
        Some(keep) => again.until("the rooms are read back without the room left", |a| {
            a.access == Some(Access::Ready) && a.room(keep).is_some()
        }),
        None => again.until("the empty room list is read back", ready_and_empty),
    }
    assert!(
        again.room(name).is_none(),
        "the room came back after a restart"
    );
    println!("left “{name}”: a tombstone in the delegate, and it stays gone after a restart");
}

/// The owner on node A changes the room's name, description and a limit; the app on node B,
/// which follows the room, shows them. B's member cannot change the room, and the owner puts the
/// name back so the steps after this still find the room by it.
fn room_details(alice: &mut App, bob: &mut App, name: &str, key: &str) {
    use river_app::rooms::outbox::ConfigEdit;
    const RENAMED: &str = "Renamed by its owner";
    const ABOUT: &str = "What **this** room is for";
    alice.send(
        key,
        Action::SetRoomDetails {
            edit: ConfigEdit {
                name: Some(RENAMED.into()),
                description: Some(ABOUT.into()),
                max_user_bans: Some(7),
                ..ConfigEdit::default()
            },
        },
    );
    alice.until(
        "node A shows the owner's change, taken by the node",
        |app| {
            app.room(RENAMED).is_some_and(|r| {
                r.description == ABOUT
                    && r.details.limits.max_user_bans == 7
                    && r.details.pending.is_none()
            })
        },
    );
    bob.until("node B shows the new name, description and limit", |app| {
        app.room(RENAMED)
            .is_some_and(|r| r.description == ABOUT && r.details.limits.max_user_bans == 7)
    });
    assert!(bob.room(name).is_none(), "the old name is gone on B");
    let on_b = bob.room(RENAMED).unwrap();
    assert!(
        matches!(on_b.details.editable, river_app::chat::model::Editable::No),
        "B's member only views the details"
    );
    // An app-side guard: the member's app refuses before signing anything, so this never
    // reaches the node (the contract's own refusal of a configuration the owner did not sign is
    // covered by the room contract's tests).
    bob.send(
        key,
        Action::SetRoomDetails {
            edit: ConfigEdit {
                name: Some("Hijacked".into()),
                ..ConfigEdit::default()
            },
        },
    );
    bob.until("node B's member is told only the owner can", |app| {
        app.room(RENAMED).is_some_and(|r| {
            r.details
                .pending
                .as_ref()
                .is_some_and(|p| p.failed.as_deref().is_some_and(|f| f.contains("owner")))
        })
    });
    let entry = bob
        .room(RENAMED)
        .unwrap()
        .details
        .pending
        .as_ref()
        .unwrap()
        .entry;
    bob.send(key, Action::Discard { entry });
    bob.until("the refused change is discarded", |app| {
        app.room(RENAMED)
            .is_some_and(|r| r.details.pending.is_none())
    });
    // Back to the original name and limit.
    alice.send(
        key,
        Action::SetRoomDetails {
            edit: ConfigEdit {
                name: Some(name.into()),
                description: Some(String::new()),
                max_user_bans: Some(10),
                ..ConfigEdit::default()
            },
        },
    );
    bob.until("node B shows the name put back", |app| {
        app.room(name)
            .is_some_and(|r| r.description.is_empty() && r.details.limits.max_user_bans == 10)
    });
    alice.until("node A has nothing more on its way", |app| {
        app.room(name).is_some_and(|r| r.details.pending.is_none())
    });
}

/// Moving an identity between the two nodes: export on A, import on B.
///
/// First a room node B's store does not hold: A's app creates it and exports its identity (the
/// owner's), B's app imports it (adding the room, read from the network), shows it, and sends in
/// it as that same member, which A's app sees as its own member's message. Then a room B's app
/// already holds (as Bob): importing without saying replace is refused and changes nothing;
/// replacing swaps the identity, so B's app now writes as A's member. A token that does not
/// check is refused before anything is sent.
fn moving_an_identity(
    alice: &mut App,
    bob: &mut App,
    existing: &str,
    existing_key: &str,
    bob_authority: &str,
    bob_dir: &Path,
) {
    const MOVED: &str = "Moving room";
    alice.send(
        "",
        Action::CreateRoom {
            name: MOVED.into(),
            nickname: "Alice".into(),
            private: false,
        },
    );
    alice.until("node A creates the room to move", |app| {
        app.room(MOVED).is_some()
    });
    let moved_key = alice.room(MOVED).unwrap().key.clone();
    let alice_me = alice.room(MOVED).unwrap().me;
    alice.exports.clear();
    alice.send(&moved_key, Action::ExportId);
    alice.until("node A makes the identity token", |app| {
        !app.exports.is_empty()
    });
    let token = match alice.exports.pop().unwrap() {
        (room, Ok(t)) if room == moved_key => t,
        other => panic!("no token: {:?}", other.1.map(|_| "<token>")),
    };
    assert!(token.starts_with("-----BEGIN RIVER IDENTITY-----"));
    assert!(
        bob.room(MOVED).is_none(),
        "node B does not hold the room yet"
    );

    // A token that does not check is refused before anything is sent.
    bob.send(
        "",
        Action::ImportId {
            token: river_app::rooms::IdentityToken(
                "-----BEGIN RIVER IDENTITY-----\nabc\n-----END RIVER IDENTITY-----".into(),
            ),
            replace: false,
        },
    );
    bob.until("node B refuses a bad token", |app| {
        app.work
            .iter()
            .any(|t| matches!(&t.state, WorkState::Failed { why, retry: false } if why.starts_with("Invalid token")))
    });
    let bad = bob
        .work
        .iter()
        .find(|t| matches!(t.state, WorkState::Failed { .. }))
        .unwrap()
        .id;
    bob.send("", Action::DismissWork { id: bad });
    bob.until("the refusal is dismissed", |app| app.work.is_empty());

    bob.send(
        "",
        Action::ImportId {
            token: river_app::rooms::IdentityToken(token),
            replace: false,
        },
    );
    bob.until("node B imports the room and shows it", |app| {
        if let Some(t) = app
            .work
            .iter()
            .find(|t| matches!(t.state, WorkState::Failed { .. }))
        {
            panic!("importing failed: {t:?}");
        }
        app.room(MOVED).is_some_and(|r| r.send_block.is_none())
    });
    assert_eq!(bob.room(MOVED).unwrap().me, alice_me, "B is A's member now");
    let text = "Sent from node B as the moved identity";
    bob.send(
        &moved_key,
        Action::Send {
            text: text.into(),
            reply_to: None,
        },
    );
    alice.until("node A sees B's message as its own member's", |app| {
        app.room(MOVED).is_some_and(|r| {
            r.messages
                .iter()
                .any(|m| m.text == text && m.author == alice_me)
        })
    });

    // A room B already holds: not replaced unless the user says so.
    alice.exports.clear();
    alice.send(existing_key, Action::ExportId);
    alice.until(
        "node A makes the owner's token for the shared room",
        |app| !app.exports.is_empty(),
    );
    let owners = match alice.exports.pop().unwrap() {
        (room, Ok(t)) if room == existing_key => t,
        other => panic!("no token: {:?}", other.1.map(|_| "<token>")),
    };
    let alice_existing_me = alice.room(existing).unwrap().me;
    let bobs_before = bob.room(existing).unwrap().me;
    // Bob's own token for the shared room, to put his identity back at the end.
    bob.exports.clear();
    bob.send(existing_key, Action::ExportId);
    bob.until("node B makes its own member's token", |app| {
        !app.exports.is_empty()
    });
    let bobs_token = match bob.exports.pop().unwrap() {
        (room, Ok(t)) if room == existing_key => t,
        other => panic!("no token: {:?}", other.1.map(|_| "<token>")),
    };
    let slot_before = stored_slot(bob_authority, bob_dir, existing_key);
    bob.send(
        "",
        Action::ImportId {
            token: river_app::rooms::IdentityToken(owners.clone()),
            replace: false,
        },
    );
    bob.until("node B refuses to replace without being told to", |app| {
        app.work.iter().any(|t| {
            matches!(&t.state, WorkState::Failed { why, retry: false } if why.contains("already have"))
        })
    });
    assert_eq!(
        bob.room(existing).unwrap().me,
        bobs_before,
        "nothing changed"
    );
    assert_eq!(
        stored_slot(bob_authority, bob_dir, existing_key),
        slot_before,
        "the refused import left the stored slot exactly as it was"
    );
    let refused = bob
        .work
        .iter()
        .find(|t| matches!(t.state, WorkState::Failed { .. }))
        .unwrap()
        .id;
    bob.send("", Action::DismissWork { id: refused });
    bob.until("the refusal is dismissed", |app| app.work.is_empty());

    bob.send(
        "",
        Action::ImportId {
            token: river_app::rooms::IdentityToken(owners),
            replace: true,
        },
    );
    bob.until(
        "node B's app is the owner's member in the shared room",
        |app| {
            app.room(existing)
                .is_some_and(|r| r.me == alice_existing_me)
        },
    );
    let text = "Sent from node B after replacing its identity";
    bob.send(
        existing_key,
        Action::Send {
            text: text.into(),
            reply_to: None,
        },
    );
    alice.until(
        "node A sees the replaced identity's message as its own member's",
        |app| {
            app.room(existing).is_some_and(|r| {
                r.messages
                    .iter()
                    .any(|m| m.text == text && m.author == r.me)
            })
        },
    );

    // And back: Bob's own token, with his nickname record in it, replaces the owner's. Node A's
    // app sees his message under his name again.
    bob.send(
        "",
        Action::ImportId {
            token: river_app::rooms::IdentityToken(bobs_token),
            replace: true,
        },
    );
    bob.until("node B's app is Bob again", |app| {
        app.room(existing).is_some_and(|r| r.me == bobs_before)
    });
    let text = "Sent from node B as Bob again";
    bob.send(
        existing_key,
        Action::Send {
            text: text.into(),
            reply_to: None,
        },
    );
    alice.until("node A shows Bob's message under his nickname", |app| {
        app.room(existing).is_some_and(|r| {
            r.messages.iter().any(|m| {
                m.text == text
                    && r.members
                        .iter()
                        .any(|mem| mem.id == m.author && mem.nickname == "Bob Two")
            })
        })
    });

    // A room that was imported is left like any other, and stays left: this also leaves node
    // B with only the rooms the steps after this expect.
    leave_room(
        bob,
        bob_authority,
        bob_dir,
        MOVED,
        &moved_key,
        Some(existing),
    );
}

/// An invitation whose inviter has since been banned: the app that accepts it must say so, in
/// good time, and must neither join nor leave anything on the node. Carol (played by the example,
/// see [`Carol`]) joins a public room, writes an invitation of her own, and the owner bans her;
/// Bob's app, which holds no such room, then tries to join with it.
fn invitation_from_a_banned_inviter(alice: &mut App, bob: &mut App, b: &str, dir_b: &Path) {
    const ROOM: &str = "Fresh node pruned-inviter room";
    alice.send(
        "",
        Action::CreateRoom {
            name: ROOM.into(),
            nickname: "Alice".into(),
            private: false,
        },
    );
    alice.until("the pruned-inviter room is created", |app| {
        app.room(ROOM).is_some()
    });
    let key = alice.room(ROOM).unwrap().key.clone();
    alice.invites.clear();
    alice.send(&key, Action::Invite);
    alice.until("an invitation to the pruned-inviter room is made", |app| {
        !app.invites.is_empty()
    });
    let link = match alice.invites.pop().unwrap() {
        (room, Ok(link)) if room == key => link,
        other => panic!("no invitation: {:?}", other.1.map(|_| "<link>")),
    };
    let carol = Carol::join(b, dir_b, &link);
    // The positive control: an invitation Carol writes, in the same way, while she is a member
    // is one the room takes. (Run through the code the app joins with, on node B's copy of the
    // room; nothing is sent.)
    let contract_key = river_app::rooms::create::contract_key(&carol.owner);
    let control = river_app::rooms::create::join(
        &carol.invitation(),
        "Dave",
        &carol.state(),
        contract_key,
        std::time::SystemTime::now(),
    )
    .unwrap_or_else(|e| panic!("an invitation from a member is refused: {}", e.why));
    assert!(control.adds_member(), "the control invitation adds nobody");
    let invitation = carol.invitation();
    let invitee = invitation.invitee.member.member_vk;
    alice.until("node A sees Carol in the pruned-inviter room", |app| {
        app.room(ROOM)
            .is_some_and(|r| r.members.iter().any(|m| m.nickname == "Carol"))
    });
    let carol_uid = alice
        .room(ROOM)
        .and_then(|r| r.members.iter().find(|m| m.nickname == "Carol"))
        .map(|m| m.uid.clone())
        .expect("Carol is a member on node A");
    alice.send(&key, Action::Ban { target: carol_uid });
    alice.until("node A dropped Carol", |app| {
        app.room(ROOM).is_some_and(|r| {
            !r.members.iter().any(|m| m.nickname == "Carol") && r.notices.is_empty()
        })
    });
    // The node B copy of the room has the ban too, so the app reads what the owner left.
    let deadline = Instant::now() + STEP;
    while carol
        .state()
        .members
        .members
        .iter()
        .any(|m| m.member.member_vk == carol.sk.verifying_key())
    {
        assert!(
            Instant::now() < deadline,
            "node B never learned that Carol was banned"
        );
        std::thread::sleep(Duration::from_secs(1));
    }

    // The same invitation, built the same way, is now refused by the join code itself.
    assert!(
        river_app::rooms::create::join(
            &invitation,
            "Dave",
            &carol.state(),
            contract_key,
            std::time::SystemTime::now(),
        )
        .err()
        .is_some_and(|e| !e.retry
            && e.why.contains("banned")
            && e.why.contains("no longer be valid")),
        "an invitation from a banned member is still taken, or is refused without saying why"
    );
    let code = invitation.to_encoded_string().expect("an invite code");
    let started = Instant::now();
    bob.send(
        "",
        Action::Join {
            code: InviteCode(code),
            nickname: "Bob".into(),
        },
    );
    let failed = |app: &App| {
        app.work.iter().find_map(|t| match &t.state {
            WorkState::Failed { why, retry } if t.room.as_deref() == Some(key.as_str()) => {
                Some((why.clone(), *retry))
            }
            _ => None,
        })
    };
    bob.until("the app says the join failed", |app| {
        assert!(
            app.room(ROOM).is_none(),
            "the app joined a room through an invitation from a banned member"
        );
        failed(app).is_some()
    });
    let (why, retry) = failed(bob).expect("a failure was shown");
    println!(
        "D: the join with a banned inviter's invitation failed after {:?}: {why:?} (Try again: {retry})",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "the failure took too long to show"
    );
    // The reason names the cause, and Try again is not offered: asking again cannot help.
    assert!(
        why.contains("banned") && why.contains("no longer be valid"),
        "the failure does not say why: {why:?}"
    );
    assert!(
        !retry,
        "Try again is offered for an invitation that cannot work"
    );
    // Nothing reached the room: the invitee is not a member on the node.
    assert!(
        !carol
            .state()
            .members
            .members
            .iter()
            .any(|m| m.member.member_vk == invitee),
        "the invitee reached the room through a banned inviter"
    );
    // Dismiss the failed task, as the user would, so the app has nothing left over.
    let id = bob
        .work
        .iter()
        .find(|t| matches!(t.state, WorkState::Failed { .. }))
        .expect("the failed join is still listed")
        .id;
    bob.send("", Action::DismissWork { id });
    bob.until("the failed join is dismissed", |app| app.work.is_empty());
}

const ROTATION_ROOM: &str = "Fresh node rotation room";

/// A private room where the member who stays is a real app and the one removed is not: Carol
/// (played by the example, see [`Carol`]) is banned, the owner's ban rotates the secret, and Bob's
/// app, which stayed, reads what the owner and he write under the new version, while Carol, whose
/// node holds those messages too, cannot open them with any key she has ever held. Then the
/// owner's own Rotate does the same once more. (`ban_and_lock_out` covers the other way round: the app banned, a bystander
/// staying.)
fn rotation_keeps_a_member_and_drops_another(
    alice: &mut App,
    bob: &mut App,
    b: &str,
    dir_b: &Path,
) -> String {
    const ROOM: &str = ROTATION_ROOM;
    alice.send(
        "",
        Action::CreateRoom {
            name: ROOM.into(),
            nickname: "Alice".into(),
            private: true,
        },
    );
    alice.until("the rotation room is created", |app| {
        app.room(ROOM).is_some()
    });
    let key = alice.room(ROOM).unwrap().key.clone();
    let invite = |alice: &mut App| {
        alice.invites.clear();
        alice.send(&key, Action::Invite);
        alice.until("an invitation to the rotation room is made", |app| {
            !app.invites.is_empty()
        });
        match alice.invites.pop().unwrap() {
            (room, Ok(link)) if room == key => link,
            other => panic!("no invitation: {:?}", other.1.map(|_| "<link>")),
        }
    };
    let link = invite(alice);
    bob.send(
        "",
        Action::Join {
            code: InviteCode(link),
            nickname: "Bob".into(),
        },
    );
    bob.until("the rotation room is joined and readable", |app| {
        app.room(ROOM).is_some_and(|r| r.send_block.is_none())
    });
    let clink = invite(alice);
    let carol = Carol::join(b, dir_b, &clink);
    alice.until("node A sees Bob and Carol in the rotation room", |app| {
        app.room(ROOM).is_some_and(|r| {
            r.members.iter().any(|m| m.nickname == "Bob")
                && r.members.iter().any(|m| m.nickname == "Carol")
        })
    });

    // Before anything is removed, the member who stays reads.
    let before_text = "Written before Carol is removed";
    alice.send(
        &key,
        Action::Send {
            text: before_text.into(),
            reply_to: None,
        },
    );
    bob.until("Bob reads the message written before the removal", |app| {
        app.has_text(ROOM, before_text)
    });
    // The positive control: with the keys she held, Carol does read what was written while she
    // was a member, so "cannot read" below is a finding and not a broken reader.
    carol.wait_reads(before_text);

    // The owner bans Carol, which rotates the secret in the same update.
    let version = |app: &App| app.room(ROOM).map(|r| r.details.secret_version);
    let v0 = version(alice).unwrap();
    let carol_uid = alice
        .room(ROOM)
        .and_then(|r| r.members.iter().find(|m| m.nickname == "Carol"))
        .map(|m| m.uid.clone())
        .expect("Carol is a member on node A");
    alice.send(&key, Action::Ban { target: carol_uid });
    alice.until("node A dropped Carol and moved to the next secret", |app| {
        app.room(ROOM).is_some_and(|r| {
            !r.members.iter().any(|m| m.nickname == "Carol")
                && r.notices.is_empty()
                && r.details.secret_version == v0 + 1
        })
    });
    bob.until("Bob's app holds the post-ban secret version", |app| {
        version(app) == Some(v0 + 1)
    });
    bob.until("Bob is still a member and can write", |app| {
        app.room(ROOM).is_some_and(|r| {
            r.send_block.is_none() && r.members.iter().any(|m| m.nickname == "Bob")
        })
    });
    let after_ban = "Written after Carol is banned";
    alice.send(
        &key,
        Action::Send {
            text: after_ban.into(),
            reply_to: None,
        },
    );
    bob.until(
        "Bob reads the owner's message under the new secret",
        |app| app.has_text(ROOM, after_ban),
    );
    carol.wait_for_sealed(v0 + 1);
    assert!(
        !carol.reads(after_ban),
        "the banned member read a message sealed after the ban"
    );
    assert!(
        carol.newest_known_version().is_some_and(|v| v <= v0),
        "Carol was given a secret after the ban: {:?}",
        carol.newest_known_version()
    );
    // The member who stayed writes under the new secret and the owner reads it.
    let from_bob = "Bob writes after Carol is banned";
    bob.send(
        &key,
        Action::Send {
            text: from_bob.into(),
            reply_to: None,
        },
    );
    alice.until(
        "the owner reads Bob's message under the new secret",
        |app| app.has_text(ROOM, from_bob),
    );

    // The owner's Rotate moves the room on again; Bob keeps reading, Carol still cannot.
    alice.send(&key, Action::RotateSecret);
    alice.until("node A moved to the rotated secret", |app| {
        app.room(ROOM).is_some_and(|r| {
            r.details.secret_version == v0 + 2
                && r.details.rotation.is_none()
                && r.notices.is_empty()
        })
    });
    bob.until("Bob's app holds the rotated secret version", |app| {
        version(app) == Some(v0 + 2)
    });
    let after_rotate = "Written after the owner rotates";
    alice.send(
        &key,
        Action::Send {
            text: after_rotate.into(),
            reply_to: None,
        },
    );
    bob.until("Bob reads the message under the rotated secret", |app| {
        app.has_text(ROOM, after_rotate)
    });
    carol.wait_for_sealed(v0 + 2);
    assert!(
        !carol.reads(after_rotate),
        "the banned member read a message sealed after the rotation"
    );
    assert!(
        carol.newest_known_version().is_some_and(|v| v <= v0),
        "Carol was given a secret after the rotation: {:?}",
        carol.newest_known_version()
    );
    println!("D: rotation keeps Bob reading and keeps Carol out");
    key
}

/// A fresh store: registered (not missing), with no rooms.
fn ready_and_empty(app: &App) -> bool {
    app.access == Some(Access::Ready) && app.rooms.is_empty() && !app.issues.store_missing
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [a, b, dir] = args.as_slice() else {
        eprintln!("usage: fresh_node_e2e <node A host:port> <node B host:port> <dir>");
        std::process::exit(2);
    };
    let dir = PathBuf::from(dir);
    let (dir_a, dir_b) = (dir.join("app-a"), dir.join("app-b"));
    for d in [&dir_a, &dir_b] {
        std::fs::create_dir_all(d).unwrap();
    }

    let mut alice = App::start("A", a, &dir_a);
    alice.until("registered, with no rooms", ready_and_empty);

    const ROOM: &str = "Fresh node room";
    alice.send(
        "",
        Action::CreateRoom {
            name: ROOM.into(),
            nickname: "Alice".into(),
            private: false,
        },
    );
    alice.until("the room is created and shown", |app| {
        if let Some(t) = app
            .work
            .iter()
            .find(|t| matches!(t.state, WorkState::Failed { .. }))
        {
            panic!("creating failed: {t:?}");
        }
        app.room(ROOM).is_some()
    });
    let key = alice.room(ROOM).unwrap().key.clone();
    alice.send(&key, Action::Invite);
    alice.until("an invitation is made", |app| !app.invites.is_empty());
    let link = match alice.invites.pop().unwrap() {
        (room, Ok(link)) if room == key => link,
        other => panic!("no invitation: {:?}", other.1.map(|_| "<link>")),
    };

    let mut bob = App::start("B", b, &dir_b);
    bob.until("registered, with no rooms", ready_and_empty);
    bob.send(
        "",
        Action::Join {
            code: InviteCode(link),
            nickname: "Bob".into(),
        },
    );
    bob.until("the room is joined and shown", |app| {
        if let Some(t) = app
            .work
            .iter()
            .find(|t| matches!(t.state, WorkState::Failed { .. }))
        {
            panic!("joining failed: {t:?}");
        }
        app.room(ROOM).is_some_and(|r| r.send_block.is_none())
    });

    let from_a = "Hello from the app on node A";
    alice.send(
        &key,
        Action::Send {
            text: from_a.into(),
            reply_to: None,
        },
    );
    bob.until("node B receives node A's message", |app| {
        app.has_text(ROOM, from_a)
    });
    let from_b = "Hello back from the app on node B";
    bob.send(
        &key,
        Action::Send {
            text: from_b.into(),
            reply_to: None,
        },
    );
    alice.until("node A receives node B's message", |app| {
        app.has_text(ROOM, from_b)
    });
    alice.until("node A sees Bob as a member", |app| {
        app.room(ROOM)
            .is_some_and(|r| r.members.iter().any(|m| m.nickname == "Bob"))
    });
    // Changing your own nickname: a signed update the other node's app sees, for the joiner (a
    // member's record, signed by their own key) and for the creator (the owner's, which the
    // room checks against the owner's key).
    direct_messages(&mut alice, &mut bob, ROOM, &key);
    delete_their_messages(&mut alice, &mut bob, ROOM, &key);
    invitations_in_direct_messages(&mut alice, &mut bob, ROOM, &key);
    archive_thread(&mut alice, &mut bob, ROOM, &key);
    rename_both_ways(&mut alice, &mut bob, ROOM, &key, "Bob", "Alice");
    deputize_and_revoke(&mut alice, &mut bob, ROOM, &key);
    // A nickname over the room's limit (50 bytes) is refused by the app and said, and is
    // never signed: what the app shows is checked, not how long a wait stayed quiet.
    bob.send(
        &key,
        Action::SetNickname {
            nickname: "n".repeat(51),
        },
    );
    bob.until("node B says the 51-byte nickname was refused", |app| {
        app.room(ROOM).is_some_and(|r| {
            r.notices
                .iter()
                .any(|n| n.text.contains("this room allows at most 50"))
        })
    });
    let refused = bob.room(ROOM).unwrap().notices[0].entry;
    bob.send(&key, Action::Discard { entry: refused });
    bob.until("the refusal is dismissed", |app| {
        app.room(ROOM).is_some_and(|r| r.notices.is_empty())
    });

    // The same in a private room, where the nickname is sealed under the room's secret: the
    // other app must open it with the secret it was invited with.
    const PRIVATE: &str = "Fresh node private room";
    alice.send(
        "",
        Action::CreateRoom {
            name: PRIVATE.into(),
            nickname: "Alice".into(),
            private: true,
        },
    );
    alice.until("the private room is created and shown", |app| {
        app.room(PRIVATE).is_some()
    });
    let pkey = alice.room(PRIVATE).unwrap().key.clone();
    alice.invites.clear();
    alice.send(&pkey, Action::Invite);
    alice.until("an invitation to the private room is made", |app| {
        !app.invites.is_empty()
    });
    let plink = match alice.invites.pop().unwrap() {
        (room, Ok(link)) if room == pkey => link,
        other => panic!("no invitation: {:?}", other.1.map(|_| "<link>")),
    };
    bob.send(
        "",
        Action::Join {
            code: InviteCode(plink),
            nickname: "Bob".into(),
        },
    );
    bob.until("the private room is joined and readable", |app| {
        app.room(PRIVATE).is_some_and(|r| r.send_block.is_none())
    });
    alice.until("node A sees Bob in the private room", |app| {
        app.room(PRIVATE)
            .is_some_and(|r| r.members.iter().any(|m| m.nickname == "Bob"))
    });
    rename_both_ways(&mut alice, &mut bob, PRIVATE, &pkey, "Bob", "Alice");
    deputize_and_revoke(&mut alice, &mut bob, PRIVATE, &pkey);
    // Carol, a third member who stays when Bob is banned. There is no third node to run an app
    // on (an app's rooms live in its node's delegate, so a second app on node B would be Bob),
    // so the example plays her itself: it joins with an invitation and reads the room from
    // node B with her own key.
    alice.invites.clear();
    alice.send(&pkey, Action::Invite);
    alice.until("an invitation for Carol is made", |app| {
        !app.invites.is_empty()
    });
    let clink = match alice.invites.pop().unwrap() {
        (room, Ok(link)) if room == pkey => link,
        other => panic!("no invitation: {:?}", other.1.map(|_| "<link>")),
    };
    let carol = Carol::join(b, &dir_b, &clink);
    alice.until("node A sees Carol in the private room", |app| {
        app.room(PRIVATE)
            .is_some_and(|r| r.members.iter().any(|m| m.nickname == "Carol"))
    });
    // A ban of Bob in the private room: the owner's app bans and rotates the secret in one
    // update. (Last in this room: Bob is not a member of it afterwards.)
    bob.send(
        &pkey,
        Action::Send {
            text: "Bob speaks before the ban".into(),
            reply_to: None,
        },
    );
    alice.until("the owner reads Bob's message", |app| {
        app.has_text(PRIVATE, "Bob speaks before the ban")
    });
    ban_and_lock_out(&mut alice, &mut bob, Some(&carol), PRIVATE, &pkey, true);
    // The owner's Rotate: the next secret version, with nothing left unsent.
    let before = alice.room(PRIVATE).unwrap().details.secret_version;
    alice.send(&pkey, Action::RotateSecret);
    alice.until("node A's private room moved to the rotated secret", |app| {
        app.room(PRIVATE).is_some_and(|r| {
            r.details.secret_version == before + 1
                && r.details.rotation.is_none()
                && r.notices.is_empty()
        })
    });
    // The same in a public room of its own.
    const BAN_ROOM: &str = "Fresh node ban room";
    alice.send(
        "",
        Action::CreateRoom {
            name: BAN_ROOM.into(),
            nickname: "Alice".into(),
            private: false,
        },
    );
    alice.until("the ban room is created", |app| {
        app.room(BAN_ROOM).is_some()
    });
    let bkey = alice.room(BAN_ROOM).unwrap().key.clone();
    alice.invites.clear();
    alice.send(&bkey, Action::Invite);
    alice.until("an invitation to the ban room is made", |app| {
        !app.invites.is_empty()
    });
    let blink = match alice.invites.pop().unwrap() {
        (room, Ok(link)) if room == bkey => link,
        other => panic!("no invitation: {:?}", other.1.map(|_| "<link>")),
    };
    bob.send(
        "",
        Action::Join {
            code: InviteCode(blink),
            nickname: "Bob".into(),
        },
    );
    bob.until("the ban room is joined", |app| {
        app.room(BAN_ROOM).is_some_and(|r| r.send_block.is_none())
    });
    bob.send(
        &bkey,
        Action::Send {
            text: "Bob speaks in the ban room".into(),
            reply_to: None,
        },
    );
    alice.until("the owner reads Bob in the ban room", |app| {
        app.has_text(BAN_ROOM, "Bob speaks in the ban room")
    });
    ban_and_lock_out(&mut alice, &mut bob, None, BAN_ROOM, &bkey, false);
    invitation_from_a_banned_inviter(&mut alice, &mut bob, b, &dir_b);
    let rkey = rotation_keeps_a_member_and_drops_another(&mut alice, &mut bob, b, &dir_b);
    // Each app's own key is in its node's delegate for the room, asked of the delegate itself:
    // the creator's is the owner's, the joiner's is the invitee's own, and they differ.
    let owner = expect_signing_key("A", &alice.authority, &dir_a, &key);
    let member = expect_signing_key("B", &bob.authority, &dir_b, &key);
    assert_ne!(owner, member, "the joiner's key is not the owner's");

    room_details(&mut alice, &mut bob, ROOM, &key);
    notification_modes(&mut alice, a, &dir_a, ROOM, &key);
    moving_an_identity(&mut alice, &mut bob, ROOM, &key, b, &dir_b);

    // A member leaves, then the owner leaves: each room is gone for good in its own app.
    leave_room(&mut bob, b, &dir_b, PRIVATE, &pkey, Some(ROOM));
    leave_room(&mut alice, a, &dir_a, PRIVATE, &pkey, Some(ROOM));
    // The banned member still holds the ban room; leaving it is allowed, so the last-room step
    // below still ends with an empty list.
    leave_room(&mut bob, b, &dir_b, BAN_ROOM, &bkey, Some(ROOM));
    leave_room(&mut bob, b, &dir_b, ROTATION_ROOM, &rkey, Some(ROOM));

    newer_river(a, &dir, &dir_a, ROOM, &key, &mut bob);
    // Nothing the outdated app was asked to write reached the room: the app on node B, which
    // follows the room, never receives it.
    bob.drain(Duration::from_secs(15));
    assert!(
        !bob.has_text(ROOM, "Sent from an outdated app"),
        "a message from the outdated app reached the room"
    );
    // Last, a member leaves the only room they have left: an empty list, and the delegate's
    // `rooms_meta` no longer names it.
    leave_room(&mut bob, b, &dir_b, ROOM, &key, None);
    println!("E2E OK");
}
