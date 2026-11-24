// Test code stamps arbitrary times; the rule is for what production signs (freenet/river#971).
#![allow(clippy::disallowed_methods)]
//! Rehearsal of a re-key for the native app (freenet/river#889), run by
//! `apps/scripts/rekey-rehearsal.sh` against one isolated Freenet node.
//!
//! The same file is built in a checkout of each OLDER River given (whose chat delegate and room
//! contract are earlier generations) to seed the node, and in this checkout to load it. Each
//! older build seeds under its own tag `T`, so rooms from several generations can be told apart.
//!
//! - `seed <node> <dir> T`: create the rooms "T Alpha" (with the message "T before"), "T Beta" and
//!   "T Gamma", then leave "T Gamma". Everything goes to that build's delegate and room contract.
//! - `load <node> <dir> T1,T2,...`: this build must show every tag's Alpha and Beta, with Alpha's
//!   message, and no Gamma. Read back from the node: its current chat delegate holds each room
//!   and each Gamma's tombstone; each Alpha is sent into, and the message is read back from the
//!   room's CURRENT contract key.
//! - `seed-late <node> <dir> T`: an older build again, after this one ran: create "T Delta" (a late
//!   write from an earlier generation).
//! - `load-late <node> <dir> T1,T2,...`: this build must show "T Delta" too, for the last tag.
//!
//! Only API the older builds also have is used, so the file builds there too.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use ed25519_dalek::VerifyingKey;
use freenet_stdlib::client_api::{ClientRequest, ContractRequest, ContractResponse, HostResponse};
use river_app::chat::model::Room;
use river_app::rooms::delegate::{self, Read};
use river_app::rooms::socket::NodeSocket;
use river_app::rooms::token::{TokenStore, node_dir};
use river_app::rooms::{Access, Action, Command, Link, RoomsUpdate};
use river_core::chat_delegate::ChatDelegateResponseMsg;
use river_core::room_state::ChatRoomStateV1;
use river_core::stored_rooms::{StoredRoomSlot, decode_room_slot, room_storage_key};

const STEP: Duration = Duration::from_secs(240);

struct App {
    authority: String,
    link: Link,
    updates: Receiver<RoomsUpdate>,
    rooms: Vec<Room>,
    work: Vec<String>,
    /// Every line the room list showed for work, in order.
    work_seen: Vec<String>,
    /// The link reached the node and read the rooms.
    ready: bool,
}

impl App {
    fn start(authority: &str, dir: &Path) -> App {
        let (tx, rx) = channel();
        let link = Link::with_authority(authority, Some(dir.to_owned()), move |u| {
            let _ = tx.send(u);
        });
        link.start();
        App {
            authority: authority.to_owned(),
            link,
            updates: rx,
            rooms: Vec::new(),
            work: Vec::new(),
            work_seen: Vec::new(),
            ready: false,
        }
    }

    fn until(&mut self, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + STEP;
        while !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "never: {what} (rooms: {:?}; work: {:?})",
                self.names(),
                self.work
            );
            let Ok(update) = self.updates.recv_timeout(left.min(Duration::from_secs(1))) else {
                continue;
            };
            match update {
                RoomsUpdate::Access(a) => {
                    println!("access: {a:?}");
                    if let Access::WaitingForApproval { approve_url } = &a {
                        approve(&self.authority, approve_url);
                    }
                    assert!(!a.is_stopped(), "stopped: {a:?}");
                    self.ready |= matches!(a, Access::Ready);
                }
                RoomsUpdate::Rooms { rooms, work, .. } => {
                    self.rooms = rooms;
                    self.work = work
                        .iter()
                        .map(|w| format!("{} [{:?}]", w.what, w.state))
                        .collect();
                    for line in &self.work {
                        if self.work_seen.last() != Some(line) {
                            println!("work: {line}");
                            self.work_seen.push(line.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        println!("ok: {what}");
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

    fn names(&self) -> Vec<String> {
        let mut n: Vec<String> = self.rooms.iter().map(|r| r.name.clone()).collect();
        n.sort();
        n
    }

    fn has_text(&self, room: &str, text: &str) -> bool {
        self.room(room)
            .is_some_and(|r| r.messages.iter().any(|m| m.text == text))
    }

    /// Whether `text` was sent in `room` and the node took it.
    fn sent(&self, room: &str, text: &str) -> bool {
        self.room(room).is_some_and(|r| {
            r.messages
                .iter()
                .any(|m| m.text == text && m.pending.is_none())
        })
    }

    fn create(&mut self, name: &str) {
        self.send(
            "",
            Action::CreateRoom {
                name: name.into(),
                nickname: "Ada".into(),
                private: false,
            },
        );
        self.until(&format!("{name} is created"), |app| {
            app.room(name).is_some()
        });
    }

    fn say(&mut self, name: &str, text: &str) {
        let key = self.room(name).unwrap().key.clone();
        self.send(
            &key,
            Action::Send {
                text: text.into(),
                reply_to: None,
            },
        );
        self.until(&format!("{text:?} is sent in {name}"), |app| {
            app.sent(name, text)
        });
    }
}

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
    agent
        .post(url)
        .set("Origin", &format!("http://{authority}"))
        .set("Content-Type", "application/json")
        .send_string(&format!("{{\"nonce\":\"{nonce}\",\"allow\":true}}"))
        .unwrap_or_else(|e| panic!("allowing the grant failed: {e}"));
    println!("approved the grant");
}

/// A connection of its own to the node, with the app's grant.
fn socket(authority: &str, dir: &Path) -> NodeSocket {
    let token = TokenStore::in_dir(&node_dir(dir, authority, false))
        .load()
        .expect("the app's grant token");
    let mut socket = NodeSocket::connect(authority, &token, &Default::default()).expect("connect");
    socket
        .set_read_timeout(Duration::from_secs(30))
        .expect("a read timeout");
    socket
}

fn owner_of(room: &Room) -> VerifyingKey {
    let bytes: [u8; 32] = bs58::decode(&room.key)
        .into_vec()
        .expect("a base58 room key")
        .try_into()
        .expect("32 bytes");
    VerifyingKey::from_bytes(&bytes).expect("the room key is the owner's key")
}

/// What this build's chat delegate holds for `owner`'s room.
fn stored(authority: &str, dir: &Path, owner: &VerifyingKey) -> Option<StoredRoomSlot> {
    let mut socket = socket(authority, dir);
    socket
        .send(&delegate::request(&Read::Get(room_storage_key(owner))))
        .expect("send");
    loop {
        if let Ok(response @ HostResponse::DelegateResponse { .. }) = socket.recv().expect("recv")
            && let Some(ChatDelegateResponseMsg::GetResponse { value, .. }) =
                delegate::answers(&response).pop()
        {
            return value.map(|v| decode_room_slot(&v).expect("a slot the app reads"));
        }
    }
}

/// The messages under `owner`'s room's CURRENT contract key (this build's room contract).
fn current_texts(authority: &str, dir: &Path, owner: &VerifyingKey) -> Vec<String> {
    let mut socket = socket(authority, dir);
    let key = river_app::rooms::create::contract_key(owner);
    socket
        .send(&ClientRequest::ContractOp(ContractRequest::Get {
            key: *key.id(),
            return_contract_code: false,
            subscribe: false,
            blocking_subscribe: false,
        }))
        .expect("send");
    loop {
        match socket.recv().expect("recv") {
            Ok(HostResponse::ContractResponse(ContractResponse::GetResponse { state, .. })) => {
                let state: ChatRoomStateV1 =
                    ciborium::de::from_reader(state.as_ref()).expect("a room state");
                return state
                    .recent_messages
                    .messages
                    .iter()
                    .filter_map(|m| m.message.content.as_public_string())
                    .collect();
            }
            Ok(HostResponse::ContractResponse(ContractResponse::NotFound { .. })) => {
                return Vec::new();
            }
            Ok(_) => {}
            Err(e) => panic!("reading the current key failed: {e}"),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, mode, authority, dir, tags] = args.as_slice() else {
        panic!("usage: rekey_rehearsal seed|load|seed-late|load-late <node> <dir> <tag[,tag...]>");
    };
    let tags: Vec<&str> = tags.split(',').collect();
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut app = App::start(authority, &dir);
    app.until("connected", |app| app.ready);
    let name = |t: &str, r: &str| format!("{t} {r}");
    match mode.as_str() {
        "seed" => {
            let t = tags[0];
            app.create(&name(t, "Alpha"));
            app.say(&name(t, "Alpha"), &format!("{t} before"));
            app.create(&name(t, "Beta"));
            app.create(&name(t, "Gamma"));
            let gamma = app.room(&name(t, "Gamma")).unwrap().key.clone();
            app.send(&gamma, Action::LeaveRoom);
            app.until("Gamma is left and every write is done", |app| {
                app.room(&name(t, "Gamma")).is_none() && app.work.is_empty()
            });
            // The leave's tombstone is in this build's store before the run ends.
            let owner = {
                let bytes: [u8; 32] = bs58::decode(&gamma).into_vec().unwrap().try_into().unwrap();
                VerifyingKey::from_bytes(&bytes).unwrap()
            };
            let deadline = Instant::now() + STEP;
            while !matches!(
                stored(authority, &dir, &owner),
                Some(StoredRoomSlot::Tombstone)
            ) {
                assert!(
                    Instant::now() < deadline,
                    "Gamma's tombstone was never stored"
                );
                std::thread::sleep(Duration::from_millis(200));
            }
            // For `load`, which must find the tombstone carried.
            std::fs::write(dir.join(format!("gamma-{t}")), &gamma).unwrap();
        }
        "load" => {
            let mut want: Vec<String> = tags
                .iter()
                .flat_map(|t| [name(t, "Alpha"), name(t, "Beta")])
                .collect();
            want.sort();
            app.until(
                "every Alpha and Beta is carried over, and no Gamma",
                |app| {
                    app.names() == want
                        && tags
                            .iter()
                            .all(|t| app.has_text(&name(t, "Alpha"), &format!("{t} before")))
                        && app.work.is_empty()
                },
            );
            assert!(
                app.work_seen
                    .iter()
                    .any(|l| l.contains("from an earlier version of River")),
                "the list said nothing about carrying rooms over: {:?}",
                app.work_seen
            );
            for t in &tags {
                // Gamma's tombstone was carried, so the room stays left.
                let gamma = std::fs::read_to_string(dir.join(format!("gamma-{t}"))).unwrap();
                let gamma: [u8; 32] = bs58::decode(gamma.trim())
                    .into_vec()
                    .unwrap()
                    .try_into()
                    .unwrap();
                let gamma = VerifyingKey::from_bytes(&gamma).unwrap();
                assert!(
                    matches!(
                        stored(authority, &dir, &gamma),
                        Some(StoredRoomSlot::Tombstone)
                    ),
                    "{t} Gamma's tombstone is not in the current chat delegate"
                );
                let alpha = app.room(&name(t, "Alpha")).unwrap().clone();
                let beta = app.room(&name(t, "Beta")).unwrap().clone();
                for room in [&alpha, &beta] {
                    assert!(
                        matches!(
                            stored(authority, &dir, &owner_of(room)),
                            Some(StoredRoomSlot::Present(_))
                        ),
                        "{} is not in the current chat delegate",
                        room.name
                    );
                }
                let text = format!("{t} after");
                app.say(&alpha.name, &text);
                let owner = owner_of(&alpha);
                let deadline = Instant::now() + STEP;
                loop {
                    let texts = current_texts(authority, &dir, &owner);
                    if texts.contains(&text) && texts.contains(&format!("{t} before")) {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "{}'s current contract key never held both messages: {texts:?}",
                        alpha.name
                    );
                    std::thread::sleep(Duration::from_millis(500));
                }
                println!("ok: {} is under its current contract key", alpha.name);
            }
        }
        "seed-late" => {
            let t = tags[0];
            app.create(&name(t, "Delta"));
            app.say(&name(t, "Delta"), &format!("{t} late"));
        }
        "load-late" => {
            let t = tags[tags.len() - 1];
            app.until(
                "Delta, written late by an older build, is carried over",
                |app| {
                    app.room(&name(t, "Delta")).is_some()
                        && app.has_text(&name(t, "Delta"), &format!("{t} late"))
                },
            );
            let delta = owner_of(app.room(&name(t, "Delta")).unwrap());
            assert!(matches!(
                stored(authority, &dir, &delta),
                Some(StoredRoomSlot::Present(_))
            ));
        }
        other => panic!("unknown mode {other}"),
    }
    println!("rehearsal step {mode} passed");
}
