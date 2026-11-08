use super::*;
use ed25519_dalek::SigningKey;
use freenet_stdlib::client_api::{DelegateRequest, HostResponse};
use freenet_stdlib::prelude::{
    ApplicationMessage, CodeHash, ContractCode, ContractKey, InboundDelegateMsg,
    OutboundDelegateMsg, Parameters, State, StateDelta, WrappedState,
};
use river_core::chat_delegate::{ChatDelegateKey, ChatDelegateRequestMsg};
use river_core::room_state::ChatRoomParametersV1;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::member::MemberId;
use river_core::room_state::message::{AuthorizedMessageV1, MessageId, MessageV1, RoomMessageBody};
use river_core::room_state::privacy::RoomDisplayMetadata;
use std::collections::{BTreeMap, VecDeque};
use std::io::{ErrorKind as IoErrorKind, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use tungstenite::Message;
use tungstenite::protocol::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;

fn vk(seed: u8) -> VerifyingKey {
    SigningKey::from_bytes(&[seed; 32]).verifying_key()
}

fn cbor<T: serde::Serialize>(v: &T) -> Vec<u8> {
    let mut b = Vec::new();
    ciborium::ser::into_writer(v, &mut b).unwrap();
    b
}

// Rooms as the web UI stores them -------------------------------------------------------------

/// The web UI's `RoomData` field names (`ui/src/room_data.rs`), for writing test slots.
#[derive(serde::Serialize)]
struct SlotData {
    owner_vk: VerifyingKey,
    room_state: ChatRoomStateV1,
    self_sk: Option<SigningKey>,
    self_vk: Option<VerifyingKey>,
    contract_key: ContractKey,
    last_read_message_id: Option<MessageId>,
    self_nickname: Option<String>,
    invitation_secrets: BTreeMap<u32, [u8; 32]>,
}

#[derive(serde::Serialize)]
enum Slot {
    Present(Box<SlotData>),
    Tombstone,
}

/// A public room owned by `seed`'s key, under contract generation `generation`, holding
/// `texts` from its owner.
struct TestRoom {
    owner: SigningKey,
    state: ChatRoomStateV1,
    key: ContractKey,
}

impl TestRoom {
    fn new(seed: u8, name: &str, generation: u8, texts: &[&str]) -> TestRoom {
        let owner = SigningKey::from_bytes(&[seed; 32]);
        let owner_id = MemberId::from(&owner.verifying_key());
        let mut state = ChatRoomStateV1 {
            configuration: AuthorizedConfigurationV1::new(
                Configuration {
                    owner_member_id: owner_id,
                    display: RoomDisplayMetadata::public(name.into(), None),
                    ..Configuration::default()
                },
                &owner,
            ),
            ..Default::default()
        };
        let now = SystemTime::now();
        state.recent_messages.messages = texts
            .iter()
            .enumerate()
            .map(|(i, t)| message(&owner, now - Duration::from_secs(1000 - i as u64), t))
            .collect();
        let params = ChatRoomParametersV1 {
            owner: owner.verifying_key(),
        };
        let key = ContractKey::from_params_and_code(
            Parameters::from(cbor(&params)),
            ContractCode::from(vec![generation]),
        );
        TestRoom { owner, state, key }
    }

    fn vk(&self) -> VerifyingKey {
        self.owner.verifying_key()
    }

    fn id(&self) -> ContractInstanceId {
        *self.key.id()
    }

    fn slot(&self) -> Vec<u8> {
        cbor(&Slot::Present(Box::new(SlotData {
            owner_vk: self.vk(),
            room_state: self.state.clone(),
            self_sk: Some(self.owner.clone()),
            self_vk: None,
            contract_key: self.key,
            last_read_message_id: None,
            self_nickname: None,
            invitation_secrets: BTreeMap::new(),
        })))
    }

    fn stored(&self) -> StoredRoom {
        match decode_room_slot(&self.slot()).unwrap() {
            StoredRoomSlot::Present(r) => *r,
            StoredRoomSlot::Tombstone => unreachable!(),
        }
    }
}

fn message(owner: &SigningKey, time: SystemTime, text: &str) -> AuthorizedMessageV1 {
    AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: MemberId::from(&owner.verifying_key()),
            author: MemberId::from(&owner.verifying_key()),
            time,
            content: RoomMessageBody::public(text.into()),
        },
        owner,
    )
}

fn texts(room: &Room) -> Vec<String> {
    room.messages.iter().map(|m| m.text.clone()).collect()
}

// The pure pieces ----------------------------------------------------------------------------

#[test]
fn the_plan_reads_room_keys_and_meta_and_ignores_the_rest() {
    let a = room_storage_key(&vk(1));
    let b = room_storage_key(&vk(2));
    let keys: Vec<&[u8]> = vec![
        &a,
        b"rooms_meta",
        b"rooms_data",
        b"signing_key:xyz",
        b"room:not-base58!",
        &b,
        &a,
    ];
    assert_eq!(
        plan(keys.into_iter()),
        Plan {
            rooms: vec![vk(1), vk(2)],
            has_meta: true
        }
    );
    assert_eq!(
        plan(std::iter::empty()),
        Plan {
            rooms: vec![],
            has_meta: false
        }
    );
}

#[test]
fn failed_gets_name_their_room() {
    let id = ContractInstanceId::new([7; 32]);
    let key = ContractKey::from_id_and_code(id, CodeHash::new([1; 32]));
    let get: ClientError =
        ErrorKind::RequestError(RequestError::ContractError(ContractError::Get {
            key,
            cause: "no peers".into(),
        }))
        .into();
    assert_eq!(failed_contract(&get), Some(id));
    let missing: ClientError = ErrorKind::RequestError(RequestError::ContractError(
        ContractError::MissingContract { key: id },
    ))
    .into();
    assert_eq!(failed_contract(&missing), Some(id));
    assert_eq!(failed_contract(&ErrorKind::NodeUnavailable.into()), None);
}

#[test]
fn rooms_are_ordered_like_the_web_ui() {
    let (a, b, c) = (vk(1), vk(2), vk(3));
    let mut rest = vec![a, c];
    rest.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
    let mut expected = vec![b];
    expected.extend(rest);
    // An ordered key that is not present (a left room) is skipped.
    assert_eq!(order(&[vk(9), b], vec![a, b, c]), expected);
    let mut all = vec![a, b, c];
    all.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
    assert_eq!(order(&[], vec![c, b, a]), all);
}

fn missing_delegate() -> ClientError {
    ErrorKind::RequestError(RequestError::DelegateError(DelegateError::Missing(
        delegate::chat_delegate_key().clone(),
    )))
    .into()
}

#[test]
fn a_missing_delegate_in_any_wording_means_no_store_and_a_refused_list_stops() {
    assert_eq!(delegate_error(&missing_delegate()), Ok(()));
    for untyped in [
        "Delegate 7LS5 not found in store",
        "missing delegate 7LS5qk1qyBsTQfGFDtXjXWWbMTWVsSmNaeDSh4BiBf72",
    ] {
        let e: ClientError = ErrorKind::Unhandled {
            cause: untyped.to_owned().into(),
        }
        .into();
        assert_eq!(delegate_error(&e), Ok(()), "{untyped}");
    }
    // Not about the delegate being installed.
    let other_not_found: ClientError = ErrorKind::Unhandled {
        cause: "delegate request: contract not found".into(),
    }
    .into();
    assert!(delegate_error(&other_not_found).is_err());
    let refused: ClientError = ErrorKind::Unhandled {
        cause: format!("{LIST_REFUSED_MARKER}: origin").into(),
    }
    .into();
    let Err(SocketError::Failed(m)) = delegate_error(&refused) else {
        panic!("a refused list is an error");
    };
    assert!(m.starts_with(REFUSED_PREFIX), "{m}");
    let other: ClientError = ErrorKind::NodeUnavailable.into();
    assert!(matches!(
        delegate_error(&other),
        Err(SocketError::Failed(_))
    ));
}

fn get_answer(key: Vec<u8>, value: Option<Vec<u8>>) -> ChatDelegateResponseMsg {
    ChatDelegateResponseMsg::GetResponse {
        key: ChatDelegateKey::new(key),
        value,
    }
}

#[test]
fn the_loader_takes_answers_in_any_order_and_counts_what_it_cannot_read() {
    let (mut loader, first) = Loader::start();
    assert!(matches!(first.as_slice(), [Read::List]));
    // A Get answer before the list is not ours.
    assert!(matches!(
        loader.on_answer(get_answer(b"room:x".to_vec(), None)),
        Step::Wait
    ));
    let keys = vec![
        ChatDelegateKey::new(room_storage_key(&vk(1))),
        ChatDelegateKey::new(room_storage_key(&vk(2))),
        ChatDelegateKey::new(ROOMS_META_KEY.to_vec()),
    ];
    let Step::Send(reads) = loader.on_answer(ChatDelegateResponseMsg::ListResponse { keys }) else {
        panic!("gets follow the list");
    };
    assert_eq!(reads.len(), 3);
    // The meta answer first, then a stray, then the rooms: one gone, one unreadable.
    assert!(matches!(
        loader.on_answer(get_answer(ROOMS_META_KEY.to_vec(), None)),
        Step::Wait
    ));
    assert!(matches!(
        loader.on_answer(get_answer(b"other".to_vec(), Some(vec![1]))),
        Step::Wait
    ));
    assert!(matches!(
        loader.on_answer(get_answer(room_storage_key(&vk(1)), None)),
        Step::Wait
    ));
    let Step::Done(loaded) =
        loader.on_answer(get_answer(room_storage_key(&vk(2)), Some(vec![0xff, 0x00])))
    else {
        panic!("all answered");
    };
    assert!(loaded.slots.is_empty());
    assert_eq!((loaded.listed, loaded.unreadable), (2, 1));
    assert!(!loaded.store_missing);

    // An empty store is done at once.
    let (mut loader, _) = Loader::start();
    assert!(matches!(
        loader.on_answer(ChatDelegateResponseMsg::ListResponse { keys: vec![] }),
        Step::Done(_)
    ));
}

fn loaded(slots: Vec<StoredRoom>) -> Loaded {
    Loaded {
        listed: slots.len(),
        slots,
        meta: StoredRoomsMeta::default(),
        store_missing: false,
        unreadable: 0,
    }
}

fn shown<'a>(rooms: &'a Rooms, room: &TestRoom) -> &'a LiveRoom {
    rooms
        .list
        .iter()
        .find(|r| r.stored.owner_vk == room.vk())
        .expect("the room is shown")
}

#[test]
fn reading_again_adds_joined_rooms_drops_left_ones_and_follows_a_rekey() {
    let mut rooms = Rooms::default();
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let first = rooms.replace(loaded(vec![a.stored(), b.stored()]));
    assert_eq!(first.len(), 2, "both rooms need a subscription");

    // The network brings a message into A; then the same rooms are read again, with A's
    // read position moved. The network's message survives, and the position is taken.
    let mut newer = a.state.clone();
    newer
        .recent_messages
        .messages
        .push(message(&a.owner, SystemTime::now(), "from the network"));
    assert!(rooms.merge_bytes(&a.id(), &cbor(&newer)));
    let mut a_read = a.stored();
    a_read.last_read_message_id = a.state.recent_messages.messages.first().map(|m| m.id());
    assert!(rooms.replace(loaded(vec![a_read, b.stored()])).is_empty());
    let kept = shown(&rooms, &a);
    assert!(texts(&kept.view(&local_minutes)).contains(&"from the network".to_owned()));
    assert!(kept.stored.last_read_message_id.is_some());

    // B was left, C joined, and A moved to a new contract.
    let a2 = TestRoom::new(1, "A", 2, &["a1"]);
    let c = TestRoom::new(3, "C", 1, &["c1"]);
    let mut new = rooms.replace(loaded(vec![a2.stored(), c.stored()]));
    let mut expected = vec![a2.id(), c.id()];
    expected.sort_by_key(|id| id.as_bytes().to_vec());
    new.sort_by_key(|id| id.as_bytes().to_vec());
    assert_eq!(new, expected);
    assert!(rooms.list.iter().all(|r| r.stored.owner_vk != b.vk()));
    assert_eq!(shown(&rooms, &a2).contract_id(), a2.id());

    // A changed identity (rejoined with a new key) takes the stored slot.
    let mut rejoined = a2.stored();
    rejoined.self_sk = Some(SigningKey::from_bytes(&[77; 32]));
    assert!(rooms.replace(loaded(vec![rejoined, c.stored()])).is_empty());
    assert_eq!(
        shown(&rooms, &a2)
            .stored
            .self_sk
            .as_ref()
            .unwrap()
            .to_bytes(),
        [77; 32]
    );
}

#[test]
fn a_read_that_finds_no_store_or_no_rooms_keeps_the_list() {
    let mut rooms = Rooms::default();
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    rooms.replace(loaded(vec![a.stored()]));
    assert!(rooms.replace(Loaded::missing()).is_empty());
    assert_eq!(rooms.list.len(), 1);
    assert!(!rooms.issues().store_missing);
    assert!(rooms.replace(loaded(vec![])).is_empty());
    assert_eq!(rooms.list.len(), 1);
    // Before anything was shown, a missing store is said as such.
    let mut first = Rooms::default();
    first.replace(Loaded::missing());
    assert!(first.issues().store_missing);
}

#[test]
fn a_room_the_network_update_failed_for_is_named_until_one_works() {
    let mut rooms = Rooms::default();
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    rooms.replace(loaded(vec![a.stored()]));
    assert!(rooms.issues().not_updated.is_empty());
    assert!(rooms.merge_bytes(&a.id(), b"not cbor"));
    assert_eq!(rooms.issues().not_updated, ["A"]);
    // A second failure changes nothing on screen.
    assert!(!rooms.merge_bytes(&a.id(), b"still not cbor"));
    assert!(rooms.merge_bytes(&a.id(), &cbor(&a.state)));
    assert!(rooms.issues().not_updated.is_empty());
    // A room that is not shown is ignored.
    assert!(!rooms.merge_bytes(&ContractInstanceId::new([9; 32]), &cbor(&a.state)));
}

#[test]
fn every_kind_of_update_reaches_the_room_and_a_rejected_delta_changes_nothing() {
    let mut rooms = Rooms::default();
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    rooms.replace(loaded(vec![a.stored()]));
    let with = |text: &str| {
        let mut s = a.state.clone();
        s.recent_messages
            .messages
            .push(message(&a.owner, SystemTime::now(), text));
        s
    };
    let last_text = |rooms: &Rooms| {
        texts(&shown(rooms, &a).view(&local_minutes))
            .last()
            .cloned()
    };

    assert!(rooms.apply_update(&a.id(), &UpdateData::State(cbor(&with("by state")).into())));
    assert_eq!(last_text(&rooms).as_deref(), Some("by state"));
    let state = with("by state and delta");
    assert!(rooms.apply_update(
        &a.id(),
        &UpdateData::StateAndDelta {
            state: cbor(&state).into(),
            delta: StateDelta::from(vec![]),
        }
    ));
    assert_eq!(last_text(&rooms).as_deref(), Some("by state and delta"));
    let delta = ChatRoomStateV1Delta {
        recent_messages: Some(vec![message(&a.owner, SystemTime::now(), "by delta")]),
        ..Default::default()
    };
    assert!(rooms.apply_update(&a.id(), &UpdateData::Delta(cbor(&delta).into())));
    assert_eq!(last_text(&rooms).as_deref(), Some("by delta"));
    // An update about another contract is not this room's.
    assert!(!rooms.apply_update(
        &a.id(),
        &UpdateData::RelatedState {
            related_to: ContractInstanceId::new([5; 32]),
            state: State::from(vec![]),
        }
    ));

    // A delta the contract rejects: a valid message, then an upgrade pointer signed by the
    // wrong key, which `apply_delta` reaches after the messages. Nothing of it may stick.
    let before = shown(&rooms, &a).stored.room_state.clone();
    let wrong = SigningKey::from_bytes(&[99; 32]);
    let bad = ChatRoomStateV1Delta {
        recent_messages: Some(vec![message(&a.owner, SystemTime::now(), "half applied")]),
        upgrade: Some(river_core::room_state::upgrade::AuthorizedUpgradeV1::new(
            river_core::room_state::upgrade::UpgradeV1 {
                owner_member_id: MemberId::from(&a.vk()),
                version: 1,
                new_chatroom_address: blake3::Hash::from([0; 32]),
            },
            &wrong,
        )),
        ..Default::default()
    };
    assert!(rooms.apply_update(&a.id(), &UpdateData::Delta(cbor(&bad).into())));
    assert_eq!(rooms.issues().not_updated, ["A"]);
    assert_eq!(shown(&rooms, &a).stored.room_state, before);
}

// Against a fake node -------------------------------------------------------------------------

/// What the fake node does with one WebSocket connection.
#[derive(Clone, Debug)]
enum Play {
    /// The node's real sequence for a rejected token: the error, then the close frame.
    Reject(&'static str, u16),
    /// The same, then drop the connection at once without reading anything.
    RejectThenDrop(&'static str, u16),
    /// Refuse the upgrade with this HTTP status.
    Handshake(u16),
    /// Accept the upgrade, then drop the connection.
    Drop,
    /// Serve the world: the chat delegate's storage and the room contracts.
    Serve,
}

#[derive(Clone)]
enum Contract {
    State(Vec<u8>),
    NotFound,
}

type OnConnect = Box<dyn FnMut(usize) + Send>;

/// Everything the fake node knows and records. Tests change it while the session runs.
#[derive(Default)]
struct World {
    /// One per WebSocket connection, in order; `Serve` once they run out.
    plays: VecDeque<Play>,
    slots: BTreeMap<Vec<u8>, Vec<u8>>,
    delegate_missing: bool,
    contracts: HashMap<ContractInstanceId, (ContractKey, Contract)>,
    /// When each WebSocket connection was accepted.
    connections: Vec<Instant>,
    bearers: Vec<String>,
    /// "METHOD path" of each plain HTTP request.
    http: Vec<String>,
    lists: usize,
    subscribes: Vec<ContractInstanceId>,
    /// Sent on the newest connection as soon as it is served.
    push: Vec<Message>,
    /// Drop the newest connection.
    drop_live: bool,
    /// Stop reading on the newest connection, so pings go unanswered.
    stall_live: bool,
    /// How many grant requests to answer "node not ready" before taking one.
    grant_not_ready: usize,
    on_connect: Option<OnConnect>,
}

impl World {
    fn add_room(&mut self, room: &TestRoom) {
        self.slots.insert(room_storage_key(&room.vk()), room.slot());
        self.contracts
            .insert(room.id(), (room.key, Contract::State(cbor(&room.state))));
    }

    fn subscribes_to(&self, id: ContractInstanceId) -> usize {
        self.subscribes.iter().filter(|s| **s == id).count()
    }
}

#[derive(Clone)]
struct FakeNode {
    authority: String,
    world: Arc<Mutex<World>>,
}

impl FakeNode {
    fn with<R>(&self, f: impl FnOnce(&mut World) -> R) -> R {
        f(&mut self.world.lock().unwrap())
    }
}

/// The request head (through the blank line), read without consuming it.
fn peek_head(stream: &TcpStream) -> String {
    let mut buf = [0u8; 8192];
    loop {
        let n = stream.peek(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        if let Some(end) = text.find("\r\n\r\n") {
            return text[..end + 4].to_owned();
        }
        if n == buf.len() {
            return text;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Consume a whole HTTP request, body included. Answering before the request is read makes
/// Windows reset the connection, and the reset discards the answer.
fn consume_request(stream: &mut TcpStream, head: &str) {
    let body = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut sink = vec![0u8; head.len() + body];
    stream.read_exact(&mut sink).unwrap();
}

fn fake_node(plays: Vec<Play>) -> FakeNode {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    let world = Arc::new(Mutex::new(World {
        plays: plays.into(),
        ..Default::default()
    }));
    let node = FakeNode {
        authority: authority.clone(),
        world: world.clone(),
    };
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let head = peek_head(&stream);
            let lower = head.to_ascii_lowercase();
            if lower.contains("upgrade: websocket") {
                let (n, play) = {
                    let mut w = world.lock().unwrap();
                    if let Some(bearer) = head.lines().find_map(|l| {
                        l.to_ascii_lowercase()
                            .starts_with("authorization: bearer ")
                            .then(|| l["authorization: bearer ".len()..].trim().to_owned())
                    }) {
                        w.bearers.push(bearer);
                    }
                    w.connections.push(Instant::now());
                    let n = w.connections.len() - 1;
                    if let Some(f) = &mut w.on_connect {
                        f(n);
                    }
                    (n, w.plays.pop_front().unwrap_or(Play::Serve))
                };
                let world = world.clone();
                std::thread::spawn(move || serve_ws(stream, head, n, play, world));
            } else {
                let first = head.lines().next().unwrap_or("").to_owned();
                let mut parts = first.split(' ');
                let call = format!(
                    "{} {}",
                    parts.next().unwrap_or(""),
                    parts.next().unwrap_or("")
                );
                world.lock().unwrap().http.push(call.clone());
                let not_ready = call.starts_with("POST /v1/app-grants/request") && {
                    let mut w = world.lock().unwrap();
                    let n = w.grant_not_ready;
                    w.grant_not_ready = n.saturating_sub(1);
                    n > 0
                };
                let (status, body) = if not_ready {
                    (503, r#"{"error":"node not ready"}"#.to_owned())
                } else if call.starts_with("POST /v1/app-grants/request") {
                    (
                        200,
                        format!(
                            r#"{{"request_id":"req1","approve_url":"http://{authority}/v1/app-grants/approve/a1","expires_in_secs":600}}"#
                        ),
                    )
                } else if call.starts_with("GET /v1/app-grants/request/req1") {
                    (
                        200,
                        r#"{"status":"approved","token":"fresh-token"}"#.to_owned(),
                    )
                } else {
                    (404, "{}".to_owned())
                };
                consume_request(&mut stream, &head);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        }
    });
    node
}

fn rejection(prefix: &str, code: u16) -> [Message; 2] {
    let error: Result<HostResponse, ClientError> = Err(ErrorKind::Unhandled {
        cause: format!("{prefix}: explanation from the node").into(),
    }
    .into());
    [
        Message::Binary(bincode::serialize(&error).unwrap().into()),
        Message::Close(Some(CloseFrame {
            code: CloseCode::from(code),
            reason: prefix.into(),
        })),
    ]
}

fn serve_ws(mut stream: TcpStream, head: String, n: usize, play: Play, world: Arc<Mutex<World>>) {
    if let Play::Handshake(status) = play {
        consume_request(&mut stream, &head);
        let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Length: 0\r\n\r\n");
        return;
    }
    let Ok(mut ws) = tungstenite::accept(stream) else {
        return;
    };
    match play {
        Play::Reject(prefix, code) => {
            for m in rejection(prefix, code) {
                let _ = ws.send(m);
            }
            while ws.read().is_ok() {}
        }
        Play::RejectThenDrop(prefix, code) => {
            for m in rejection(prefix, code) {
                let _ = ws.send(m);
            }
            // Gone without reading what the client sends. The pause lets a client that writes
            // first get its request here before the close, so the close is a reset (an unread
            // request makes the kernel reset the connection), as it is against a real node.
            std::thread::sleep(Duration::from_millis(30));
        }
        Play::Drop => {}
        Play::Serve => serve(ws, n, &world),
        Play::Handshake(_) => unreachable!(),
    }
}

fn serve(mut ws: tungstenite::WebSocket<TcpStream>, n: usize, world: &Mutex<World>) {
    let _ = ws
        .get_mut()
        .set_read_timeout(Some(Duration::from_millis(5)));
    loop {
        {
            let mut w = world.lock().unwrap();
            if w.connections.len() - 1 != n {
                return;
            }
            if w.drop_live {
                w.drop_live = false;
                return;
            }
            for m in std::mem::take(&mut w.push) {
                if ws.send(m).is_err() {
                    return;
                }
            }
            if w.stall_live {
                drop(w);
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
        }
        match ws.read() {
            Ok(Message::Binary(b)) => {
                let replies = answer(world, &b);
                for r in replies {
                    if ws.send(Message::Binary(r.into())).is_err() {
                        return;
                    }
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), IoErrorKind::WouldBlock | IoErrorKind::TimedOut) => {}
            Err(_) => return,
        }
    }
}

fn answer(world: &Mutex<World>, bytes: &[u8]) -> Vec<Vec<u8>> {
    let request: ClientRequest = bincode::deserialize(bytes).unwrap();
    let mut w = world.lock().unwrap();
    let mut out = Vec::new();
    let encode = |r: Result<HostResponse, ClientError>| bincode::serialize(&r).unwrap();
    match request {
        ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { inbound, .. }) => {
            for msg in inbound {
                let InboundDelegateMsg::ApplicationMessage(m) = msg else {
                    continue;
                };
                let read: ChatDelegateRequestMsg =
                    ciborium::de::from_reader(m.payload.as_slice()).unwrap();
                if matches!(read, ChatDelegateRequestMsg::ListRequest) {
                    w.lists += 1;
                }
                if w.delegate_missing {
                    out.push(encode(Err(missing_delegate())));
                    continue;
                }
                let reply = match read {
                    ChatDelegateRequestMsg::ListRequest => ChatDelegateResponseMsg::ListResponse {
                        keys: w.slots.keys().cloned().map(ChatDelegateKey::new).collect(),
                    },
                    ChatDelegateRequestMsg::GetRequest { key } => {
                        ChatDelegateResponseMsg::GetResponse {
                            value: w.slots.get(key.as_bytes()).cloned(),
                            key,
                        }
                    }
                    other => panic!("the app must only read, but sent {other:?}"),
                };
                out.push(encode(Ok(HostResponse::DelegateResponse {
                    key: delegate::chat_delegate_key().clone(),
                    values: vec![OutboundDelegateMsg::ApplicationMessage(
                        ApplicationMessage::new(cbor(&reply)),
                    )],
                })));
            }
        }
        ClientRequest::ContractOp(ContractRequest::Get { key: id, .. }) => {
            w.subscribes.push(id);
            match w.contracts.get(&id).cloned() {
                Some((key, Contract::State(state))) => out.push(encode(Ok(
                    HostResponse::ContractResponse(ContractResponse::GetResponse {
                        key,
                        contract: None,
                        state: WrappedState::new(state),
                    }),
                ))),
                Some((_, Contract::NotFound)) => out.push(encode(Ok(
                    HostResponse::ContractResponse(ContractResponse::NotFound { instance_id: id }),
                ))),
                None => {}
            }
        }
        other => panic!("the app must only read, but sent {other:?}"),
    }
    out
}

fn quick() -> Pace {
    Pace {
        backoff_first: Duration::from_millis(10),
        backoff_max: Duration::from_millis(20),
        stable: Duration::from_secs(60),
        early_read: Duration::from_millis(50),
        early_read_retry: Duration::from_millis(100),
        tick: Duration::from_millis(10),
        idle_check: Duration::from_secs(60),
        pong_wait: Duration::from_secs(60),
        catch_up: Duration::from_millis(300),
        resubscribe_first: Duration::from_secs(60),
        resubscribe_max: Duration::from_secs(60),
        refresh: Duration::from_secs(3600),
        delegate_deadline: Duration::from_secs(5),
        grant: GrantPacing {
            pause: Duration::ZERO,
            margin: Duration::ZERO,
            outage: Duration::from_secs(60),
        },
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("river-session-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A session running against a fake node.
struct Running {
    updates: Arc<Mutex<Vec<RoomsUpdate>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Access>>,
}

const PATIENCE: Duration = Duration::from_secs(20);

impl Running {
    fn start(node: &FakeNode, dir: &Path, pace: Pace) -> Running {
        let updates = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let (updates, stop) = (updates.clone(), stop.clone());
            let authority = node.authority.clone();
            let dir = dir.to_owned();
            std::thread::spawn(move || {
                run(
                    &authority,
                    Some(&dir),
                    &|u| updates.lock().unwrap().push(u),
                    &pace,
                    &|| stop.load(Ordering::Relaxed),
                )
            })
        };
        Running {
            updates,
            stop,
            handle: Some(handle),
        }
    }

    fn updates(&self) -> Vec<RoomsUpdate> {
        self.updates.lock().unwrap().clone()
    }

    /// Wait until `done` holds, failing with `what` after a generous while.
    fn wait_until(&self, what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + PATIENCE;
        while !done() {
            assert!(Instant::now() < deadline, "never: {what}");
            assert!(
                !self.handle.as_ref().unwrap().is_finished(),
                "the session stopped before: {what}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Wait until the latest room list satisfies `done`.
    fn wait_rooms(&self, what: &str, done: impl Fn(&[Room], &Issues, LinkStatus) -> bool) {
        self.wait_until(what, || match self.last_rooms() {
            Some((rooms, issues, status)) => done(&rooms, &issues, status),
            None => false,
        });
    }

    fn last_rooms(&self) -> Option<(Vec<Room>, Issues, LinkStatus)> {
        self.updates().into_iter().rev().find_map(|u| match u {
            RoomsUpdate::Rooms {
                rooms,
                issues,
                status,
                ..
            } => Some((rooms, issues, status)),
            _ => None,
        })
    }

    /// Wait for the session to stop by itself, and say where.
    fn ended(mut self) -> Access {
        let handle = self.handle.take().unwrap();
        let deadline = Instant::now() + PATIENCE;
        while !handle.is_finished() {
            assert!(
                Instant::now() < deadline,
                "the session never stopped by itself"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.join().unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            // The session notices `stop` within a tick or a pause; don't hang a failing test
            // on one that doesn't.
            let deadline = Instant::now() + PATIENCE;
            while !h.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if h.is_finished() {
                let _ = h.join();
            } else if !std::thread::panicking() {
                panic!("the session did not stop when asked");
            }
        }
    }
}

fn save_token(dir: &Path, token: &str) {
    TokenStore::in_dir(dir)
        .save(&GrantToken::new(token.into()))
        .unwrap();
}

fn saved(dir: &Path) -> Option<GrantToken> {
    TokenStore::in_dir(dir).load()
}

fn names(rooms: &[Room]) -> Vec<String> {
    let mut n: Vec<String> = rooms.iter().map(|r| r.name.clone()).collect();
    n.sort();
    n
}

// Tokens ----------------------------------------------------------------------------------------

#[test]
fn a_saved_token_the_node_rejects_is_discarded_and_nothing_is_reopened() {
    let node = fake_node(vec![Play::Reject("AUTH_TOKEN_INVALID", 4401)]);
    let dir = temp_dir("4401-saved");
    save_token(&dir, "old-token");
    let session = Running::start(&node, &dir, quick());
    let updates = session.updates.clone();
    assert_eq!(session.ended(), Access::PermissionRemoved);
    assert_eq!(saved(&dir), None);
    // No new grant was asked for, so no approval page opened.
    node.with(|w| {
        assert!(w.http.is_empty());
        assert_eq!(w.bearers, ["old-token"]);
    });
    assert!(
        !updates
            .lock()
            .unwrap()
            .iter()
            .any(|u| matches!(u, RoomsUpdate::Access(Access::WaitingForApproval { .. })))
    );
}

/// The node sends the rejection and drops the connection before reading anything. Writing
/// first would draw a reset, which on Windows discards the rejection; reading first gets it.
///
/// Only Windows loses data to a reset, so on Linux and macOS this passes with or without the
/// read-first fix: it guards the fix only on the Windows CI job (`other-platforms`).
#[test]
fn a_rejection_the_node_drops_the_connection_after_is_still_read() {
    let node = fake_node(vec![Play::RejectThenDrop("AUTH_TOKEN_INVALID", 4401)]);
    let dir = temp_dir("4401-drop");
    save_token(&dir, "old-token");
    assert_eq!(
        Running::start(&node, &dir, quick()).ended(),
        Access::PermissionRemoved
    );
    assert_eq!(saved(&dir), None);
}

#[test]
fn a_token_the_node_rejects_as_soon_as_it_issued_it_stops() {
    let node = fake_node(vec![Play::Reject("AUTH_TOKEN_INVALID", 4401)]);
    let dir = temp_dir("4401-fresh");
    let session = Running::start(&node, &dir, quick());
    let updates = session.updates.clone();
    let end = session.ended();
    assert!(
        matches!(&end, Access::Refused(m) if m.contains("just gave")),
        "{end:?}"
    );
    assert_eq!(saved(&dir), None);
    node.with(|w| {
        assert_eq!(
            w.http,
            [
                "POST /v1/app-grants/request",
                "GET /v1/app-grants/request/req1"
            ]
        )
    });
    assert!(updates.lock().unwrap().iter().any(|u| matches!(
        u,
        RoomsUpdate::Access(Access::WaitingForApproval { approve_url })
            if approve_url.starts_with(&format!("http://{}/", node.authority))
    )));
}

/// A token the node accepted once is no longer "just issued": a later rejection means the
/// permission was removed.
#[test]
fn a_new_token_that_worked_and_is_then_revoked_is_a_removed_permission() {
    let node = fake_node(vec![Play::Serve]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| w.add_room(&a));
    let dir = temp_dir("fresh-then-revoked");
    let session = Running::start(&node, &dir, quick());
    session.wait_rooms("the rooms are live", |r, _, s| {
        r.len() == 1 && s == LinkStatus::Live
    });
    node.with(|w| w.push.extend(rejection("AUTH_TOKEN_INVALID", 4401)));
    assert_eq!(session.ended(), Access::PermissionRemoved);
    assert_eq!(saved(&dir), None);
}

/// Another window saved a newer grant while this one held the revoked token: the newer one
/// stays on disk and is used.
#[test]
fn a_newer_token_saved_meanwhile_is_kept_and_used() {
    let node = fake_node(vec![Play::Reject("AUTH_TOKEN_INVALID", 4401), Play::Serve]);
    let dir = temp_dir("newer-token");
    save_token(&dir, "old-token");
    let save_dir = dir.clone();
    node.with(|w| {
        w.on_connect = Some(Box::new(move |n| {
            if n == 0 {
                save_token(&save_dir, "new-token");
            }
        }))
    });
    let session = Running::start(&node, &dir, quick());
    session.wait_rooms("the rooms are read with the newer token", |_, _, _| true);
    node.with(|w| assert_eq!(w.bearers, ["old-token", "new-token"]));
    assert_eq!(saved(&dir), Some(GrantToken::new("new-token".into())));
    assert!(node.with(|w| w.http.is_empty()));
}

#[test]
fn a_connection_that_cannot_use_a_grant_keeps_the_token_and_says_why() {
    let node = fake_node(vec![Play::Reject("AUTH_TOKEN_NOT_USABLE_HERE", 4403)]);
    let dir = temp_dir("4403");
    save_token(&dir, "kept-token");
    let end = Running::start(&node, &dir, quick()).ended();
    assert!(
        matches!(&end, Access::Refused(m) if m.contains(&node.authority)),
        "{end:?}"
    );
    assert_eq!(saved(&dir), Some(GrantToken::new("kept-token".into())));
}

#[test]
fn a_node_that_cannot_check_the_token_yet_is_retried_with_the_same_token() {
    let node = fake_node(vec![
        Play::Reject("AUTH_TOKEN_UNAVAILABLE", 4503),
        Play::Serve,
    ]);
    node.with(|w| w.delegate_missing = true);
    let dir = temp_dir("4503");
    save_token(&dir, "kept-token");
    let session = Running::start(&node, &dir, quick());
    // The node has no chat delegate: no rooms, and the screen is told why.
    session.wait_rooms("an empty list, with the store missing", |r, issues, _| {
        r.is_empty() && issues.store_missing
    });
    assert_eq!(saved(&dir), Some(GrantToken::new("kept-token".into())));
    node.with(|w| assert_eq!(w.bearers, ["kept-token", "kept-token"]));
    assert!(
        session
            .updates()
            .iter()
            .any(|u| matches!(u, RoomsUpdate::Access(Access::Ready)))
    );
}

#[test]
fn a_refused_upgrade_stops_instead_of_retrying_forever() {
    let node = fake_node(vec![Play::Handshake(403)]);
    let dir = temp_dir("handshake");
    save_token(&dir, "kept-token");
    let end = Running::start(&node, &dir, quick()).ended();
    assert!(
        matches!(&end, Access::Refused(m) if m.contains("403")),
        "{end:?}"
    );
    assert_eq!(saved(&dir), Some(GrantToken::new("kept-token".into())));
}

// Following the rooms ---------------------------------------------------------------------------

fn following(node: &FakeNode, rooms: &[&TestRoom], dir: &str, pace: Pace) -> (Running, PathBuf) {
    node.with(|w| {
        for r in rooms {
            w.add_room(r);
        }
    });
    let dir = temp_dir(dir);
    save_token(&dir, "kept-token");
    let session = Running::start(node, &dir, pace);
    let n = rooms.len();
    session.wait_rooms("the rooms are live", move |r, _, s| {
        r.len() == n && s == LinkStatus::Live
    });
    (session, dir)
}

#[test]
fn every_room_is_subscribed_again_after_a_reconnect() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "resubscribe", quick());
    assert_eq!(node.with(|w| w.subscribes_to(a.id())), 1);
    node.with(|w| w.drop_live = true);
    session.wait_until("a second subscribe on a new connection", || {
        node.with(|w| w.connections.len() >= 2 && w.subscribes_to(a.id()) >= 2)
    });
    assert!(
        session
            .updates()
            .iter()
            .any(|u| matches!(u, RoomsUpdate::Status(LinkStatus::Reconnecting)))
    );
}

#[test]
fn an_update_notification_reaches_the_screen() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "notification", quick());
    let delta = ChatRoomStateV1Delta {
        recent_messages: Some(vec![message(&a.owner, SystemTime::now(), "live news")]),
        ..Default::default()
    };
    let note: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(
        ContractResponse::UpdateNotification {
            key: a.key,
            update: UpdateData::Delta(cbor(&delta).into()),
        },
    ));
    node.with(|w| {
        w.push
            .push(Message::Binary(bincode::serialize(&note).unwrap().into()))
    });
    session.wait_rooms("the new message shows", |r, _, _| {
        texts(&r[0]).contains(&"live news".to_owned())
    });
}

#[test]
fn reading_again_shows_joined_rooms_drops_left_ones_and_subscribes_a_rekey() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let pace = Pace {
        refresh: Duration::from_millis(50),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a, &b], "reload", pace);
    let a2 = TestRoom::new(1, "A", 2, &["a1"]);
    let c = TestRoom::new(3, "C", 1, &["c1"]);
    node.with(|w| {
        w.slots
            .insert(room_storage_key(&b.vk()), cbor(&Slot::Tombstone));
        w.add_room(&a2);
        w.add_room(&c);
    });
    session.wait_rooms("A and C, without B", |r, _, _| names(r) == ["A", "C"]);
    session.wait_until("the re-keyed A and the new C are subscribed", || {
        node.with(|w| w.subscribes_to(a2.id()) >= 1 && w.subscribes_to(c.id()) >= 1)
    });
}

#[test]
fn reading_again_with_no_store_or_no_rooms_keeps_the_list() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let pace = Pace {
        refresh: Duration::from_millis(20),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a, &b], "reload-missing", pace);
    let lists = node.with(|w| w.lists);
    node.with(|w| w.delegate_missing = true);
    session.wait_until("three reads with the store missing", || {
        node.with(|w| w.lists >= lists + 3)
    });
    node.with(|w| {
        w.delegate_missing = false;
        w.slots.clear();
    });
    let lists = node.with(|w| w.lists);
    session.wait_until("three reads with no rooms", || {
        node.with(|w| w.lists >= lists + 3)
    });
    let (rooms, issues, _) = session.last_rooms().unwrap();
    assert_eq!(names(&rooms), ["A", "B"]);
    assert!(!issues.store_missing);
}

#[test]
fn a_room_whose_subscribe_finds_nothing_is_named_and_asked_again() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    node.with(|w| {
        w.add_room(&a);
        w.add_room(&b);
        w.contracts.get_mut(&b.id()).unwrap().1 = Contract::NotFound;
    });
    let pace = Pace {
        resubscribe_first: Duration::from_millis(20),
        resubscribe_max: Duration::from_millis(40),
        ..quick()
    };
    let dir = temp_dir("notfound");
    save_token(&dir, "kept-token");
    let session = Running::start(&node, &dir, pace);
    session.wait_rooms("B is named as not followed", |r, issues, _| {
        r.len() == 2 && issues.not_following == ["B"]
    });
    session.wait_until("B is asked for again", || {
        node.with(|w| w.subscribes_to(b.id()) >= 3)
    });
    node.with(|w| w.contracts.get_mut(&b.id()).unwrap().1 = Contract::State(cbor(&b.state)));
    session.wait_rooms("B is followed", |_, issues, _| {
        issues.not_following.is_empty()
    });
}

#[test]
fn an_unanswered_ping_ends_the_connection() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        idle_check: Duration::from_millis(50),
        pong_wait: Duration::from_millis(100),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "pong", pace);
    // The node stops reading: no pong, and no error either. Only the pong deadline notices.
    // Count from the stall, so an unrelated earlier reconnect can't pass the test.
    let before = node.with(|w| {
        w.stall_live = true;
        w.connections.len()
    });
    let stalled = Instant::now();
    session.wait_until("a reconnect", || {
        node.with(|w| w.connections.len() > before)
    });
    // At least the pong deadline passed. (A ping sent just before the stall can expire as
    // early as `pong_wait` after it, so nothing longer can be asserted.)
    assert!(stalled.elapsed() >= Duration::from_millis(100));
    node.with(|w| w.stall_live = false);
}

/// The backoff grows while connections fail at once, and starts over only after a connection
/// has stayed up for `stable`.
#[test]
fn the_backoff_starts_over_only_after_a_stable_connection() {
    // Each dropped connection heard nothing, so it is retried once at once, then waited out:
    // 16 drops wait 10, 20, ..., 1280 ms, and the next wait would be 2560 ms.
    let mut plays = vec![Play::Drop; 16];
    plays.push(Play::Serve);
    let node = fake_node(plays);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        backoff_first: Duration::from_millis(10),
        backoff_max: Duration::from_secs(10),
        stable: Duration::from_millis(100),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "backoff", pace);
    assert_eq!(node.with(|w| w.connections.len()), 17);
    let first = node.with(|w| w.connections[0]);
    let grown = node.with(|w| w.connections[16]) - first;
    assert!(grown >= Duration::from_millis(2000), "{grown:?}");
    std::thread::sleep(Duration::from_millis(300));
    let dropped = Instant::now();
    node.with(|w| w.drop_live = true);
    session.wait_until("a reconnect", || node.with(|w| w.connections.len() >= 18));
    let gap = node.with(|w| w.connections[17]) - dropped;
    assert!(
        gap < Duration::from_secs(1),
        "the backoff did not start over: {gap:?}"
    );
}

/// A node that answers grant requests "not ready" while it starts: River keeps asking,
/// with backoff, and goes on once the node takes the request.
#[test]
fn a_node_not_ready_for_grants_is_asked_again_until_it_is() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| {
        w.add_room(&a);
        // More than one request's own retries, so the session's backoff retry is needed.
        w.grant_not_ready = 8;
    });
    let dir = temp_dir("grant-not-ready");
    let session = Running::start(&node, &dir, quick());
    session.wait_rooms("the rooms are live", |r, _, s| {
        r.len() == 1 && s == LinkStatus::Live
    });
    node.with(|w| {
        assert_eq!(w.grant_not_ready, 0);
        assert_eq!(w.bearers, ["fresh-token"]);
    });
    assert_eq!(saved(&dir), Some(GrantToken::new("fresh-token".into())));
}

/// An ask that goes unanswered after the list is live is still timed out, named and
/// asked again; and a room that leaves the list is no longer asked for.
#[test]
fn late_asks_time_out_and_left_rooms_are_not_asked_for() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let pace = Pace {
        catch_up: Duration::from_millis(100),
        resubscribe_first: Duration::from_millis(20),
        resubscribe_max: Duration::from_millis(20),
        refresh: Duration::from_millis(30),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a, &b], "late-asks", pace);
    // B starts finding nothing, then stops answering at all: its asks time out.
    node.with(|w| w.contracts.get_mut(&b.id()).unwrap().1 = Contract::NotFound);
    let notification: Result<HostResponse, ClientError> =
        Ok(HostResponse::ContractResponse(ContractResponse::NotFound {
            instance_id: b.id(),
        }));
    node.with(|w| {
        w.push.push(Message::Binary(
            bincode::serialize(&notification).unwrap().into(),
        ))
    });
    session.wait_rooms("B is named", |_, issues, _| issues.not_following == ["B"]);
    node.with(|w| {
        w.contracts.remove(&b.id());
    });
    let asked = node.with(|w| w.subscribes_to(b.id()));
    session.wait_until("B is asked again after unanswered asks time out", || {
        node.with(|w| w.subscribes_to(b.id()) >= asked + 2)
    });
    // B is left: it drops off the list and is no longer asked for.
    node.with(|w| {
        w.slots
            .insert(room_storage_key(&b.vk()), cbor(&Slot::Tombstone))
    });
    session.wait_rooms("B is gone", |r, issues, _| {
        names(r) == ["A"] && issues.not_following.is_empty()
    });
    std::thread::sleep(Duration::from_millis(100));
    let asked = node.with(|w| w.subscribes_to(b.id()));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(node.with(|w| w.subscribes_to(b.id())), asked);
}

/// A notification shows the room is followed, even if its ask was counted as failed.
#[test]
fn a_notification_marks_the_room_followed() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| {
        w.add_room(&a);
        w.contracts.get_mut(&a.id()).unwrap().1 = Contract::NotFound;
    });
    let dir = temp_dir("notification-follows");
    save_token(&dir, "kept-token");
    let session = Running::start(&node, &dir, quick());
    session.wait_rooms("A is named", |_, issues, _| issues.not_following == ["A"]);
    let delta = ChatRoomStateV1Delta {
        recent_messages: Some(vec![message(&a.owner, SystemTime::now(), "hello")]),
        ..Default::default()
    };
    let note: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(
        ContractResponse::UpdateNotification {
            key: a.key,
            update: UpdateData::Delta(cbor(&delta).into()),
        },
    ));
    node.with(|w| {
        w.push
            .push(Message::Binary(bincode::serialize(&note).unwrap().into()))
    });
    session.wait_rooms("A is followed", |_, issues, _| {
        issues.not_following.is_empty()
    });
}

/// A shown room whose stored copy can't be merged on a reload is named as not updated.
#[test]
fn a_stored_copy_that_cannot_be_merged_is_named() {
    let mut rooms = Rooms::default();
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    rooms.replace(loaded(vec![a.stored()]));
    let mut bad = a.stored();
    // An upgrade pointer signed by someone other than the owner: the merge refuses it.
    bad.room_state.upgrade = river_core::room_state::upgrade::OptionalUpgradeV1(Some(
        river_core::room_state::upgrade::AuthorizedUpgradeV1::new(
            river_core::room_state::upgrade::UpgradeV1 {
                owner_member_id: MemberId::from(&a.vk()),
                version: 1,
                new_chatroom_address: blake3::Hash::from([0; 32]),
            },
            &SigningKey::from_bytes(&[99; 32]),
        ),
    ));
    assert!(rooms.replace(loaded(vec![bad])).is_empty());
    assert_eq!(rooms.issues().not_updated, ["A"]);
}
