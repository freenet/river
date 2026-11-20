//! End-to-end run of the native app against two Freenet nodes that River never ran on.
//!
//! Each node is fresh: no River web UI, no chat delegate. On each, the app's own background
//! work ([`Link`]) asks for a grant (approved here as the user would approve it on the page),
//! registers the chat delegate, and reads an empty room list. Then the app on node A creates a
//! room and makes an invitation, the app on node B joins from it, and each sends a message the
//! other receives; each then changes its own nickname and the other sees it everywhere, and the
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
use river_app::rooms::token::TokenStore;
use river_app::rooms::work::WorkState;
use river_app::rooms::{Access, Action, Command, InviteCode, Issues, Link, RoomsUpdate};
use river_core::chat_delegate::ChatDelegateResponseMsg;
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
    link: Link,
    updates: Receiver<RoomsUpdate>,
    rooms: Vec<Room>,
    issues: Issues,
    work: Vec<river_app::rooms::work::WorkView>,
    modes: std::collections::HashMap<String, NotificationMode>,
    invites: Vec<(String, Result<String, String>)>,
    /// Identity tokens reported, by room. The e2e holds them only as long as it needs them.
    exports: Vec<(String, Result<String, String>)>,
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
            modes: Default::default(),
            invites: Vec::new(),
            exports: Vec::new(),
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

/// The room list's view preferences (`rooms_meta`) as the delegate on `authority` holds them.
fn stored_meta(authority: &str, dir: &Path) -> Result<StoredRoomsMeta, String> {
    let token = TokenStore::in_dir(dir)
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
    let token = TokenStore::in_dir(dir)
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
}

impl Carol {
    fn socket(authority: &str, dir: &Path) -> NodeSocket {
        let token = TokenStore::in_dir(dir).load().expect("a grant token");
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
        let mut secrets = std::collections::HashMap::new();
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
                secrets.insert(b.secret.secret_version, s);
            }
        }
        state
            .recent_messages
            .messages
            .iter()
            .any(|m| match &m.message.content {
                RoomMessageBody::Private {
                    ciphertext,
                    nonce,
                    secret_version,
                    ..
                } => secrets
                    .get(secret_version)
                    .and_then(|s| decrypt_with_symmetric_key(s, ciphertext, nonce).ok())
                    .is_some_and(|plain| plain == wanted),
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
