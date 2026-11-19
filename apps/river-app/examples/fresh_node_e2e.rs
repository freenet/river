//! End-to-end run of the native app against two Freenet nodes that River never ran on.
//!
//! Each node is fresh: no River web UI, no chat delegate. On each, the app's own background
//! work ([`Link`]) asks for a grant (approved here as the user would approve it on the page),
//! registers the chat delegate, and reads an empty room list. Then the app on node A creates a
//! room and makes an invitation, the app on node B joins from it, and each sends a message the
//! other receives; each then changes its own nickname and the other sees it everywhere. Last,
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
use river_app::rooms::socket::NodeSocket;
use river_app::rooms::token::TokenStore;
use river_app::rooms::work::WorkState;
use river_app::rooms::{Access, Action, Command, InviteCode, Issues, Link, RoomsUpdate};
use river_core::chat_delegate::ChatDelegateResponseMsg;
use river_core::stored_rooms::{StoredRoomSlot, decode_room_slot, room_storage_key};

/// How long each step may take. Network operations between two loopback nodes take seconds;
/// this is generous so a slow CI runner does not fail it.
const STEP: Duration = Duration::from_secs(180);

/// One app: its link to its node and what it has reported.
struct App {
    name: &'static str,
    authority: String,
    link: Link,
    updates: Receiver<RoomsUpdate>,
    rooms: Vec<Room>,
    issues: Issues,
    work: Vec<river_app::rooms::work::WorkView>,
    invites: Vec<(String, Result<String, String>)>,
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
            link,
            updates: rx,
            rooms: Vec::new(),
            issues: Issues::default(),
            work: Vec::new(),
            invites: Vec::new(),
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
                "{}: never: {what} (last: {:?})",
                self.name,
                self.access
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
                    ..
                } => {
                    self.rooms = rooms;
                    self.issues = issues;
                    self.work = work;
                }
                RoomsUpdate::Invite { room, link } => self.invites.push((room, link)),
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

    fn has_text(&self, room: &str, text: &str) -> bool {
        self.room(room)
            .is_some_and(|r| r.messages.iter().any(|m| m.text == text))
    }
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
    let token = TokenStore::in_dir(dir)
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
    let token = TokenStore::in_dir(dir).load().expect("a grant token");
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

/// A fresh store: registered (not missing), with no rooms.
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
    rename_both_ways(&mut alice, &mut bob, ROOM, &key, "Bob", "Alice");
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
    // Each app's own key is in its node's delegate for the room, asked of the delegate itself:
    // the creator's is the owner's, the joiner's is the invitee's own, and they differ.
    let owner = expect_signing_key("A", &alice.authority, &dir_a, &key);
    let member = expect_signing_key("B", &bob.authority, &dir_b, &key);
    assert_ne!(owner, member, "the joiner's key is not the owner's");

    newer_river(a, &dir, &dir_a, ROOM, &key, &mut bob);
    // Nothing the outdated app was asked to write reached the room: the app on node B, which
    // follows the room, never receives it.
    bob.drain(Duration::from_secs(15));
    assert!(
        !bob.has_text(ROOM, "Sent from an outdated app"),
        "a message from the outdated app reached the room"
    );
    println!("E2E OK");
}
