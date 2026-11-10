use super::*;
use crate::rooms::outbox::InviteCode;
use crate::rooms::outbox::{Action, Outbox};
use crate::rooms::room::message_key;
use crate::rooms::work::WorkState;
use ed25519_dalek::SigningKey;
use freenet_stdlib::client_api::{DelegateRequest, HostResponse};
use freenet_stdlib::prelude::{
    ApplicationMessage, CodeHash, ContractCode, ContractKey, InboundDelegateMsg,
    OutboundDelegateMsg, Parameters, State, StateDelta, WrappedState,
};
use river_core::chat_delegate::{CasStoreResult, ChatDelegateKey, ChatDelegateRequestMsg};
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
    /// The private room of `room_tests::Fixture` ("Secret garden", secret v0), with its owner
    /// (seed 1) holding the secret too, so the owner can send in it.
    fn private(generation: u8) -> TestRoom {
        let f = crate::rooms::room_tests::Fixture::new();
        let mut state = f.private_state();
        crate::rooms::room_tests::add_secret(
            &f,
            &mut state,
            0,
            &crate::rooms::room_tests::SECRET,
            &f.owner,
        );
        let params = ChatRoomParametersV1 {
            owner: f.owner.verifying_key(),
        };
        let key = ContractKey::from_params_and_code(
            Parameters::from(cbor(&params)),
            ContractCode::from(vec![generation]),
        );
        TestRoom {
            owner: f.owner,
            state,
            key,
        }
    }

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
/// Runs before the fake chat delegate takes a CAS store, with the key: another writer (the web
/// UI) can store first.
type BeforeCas =
    Box<dyn FnMut(&mut BTreeMap<Vec<u8>, Vec<u8>>, &mut HashMap<Vec<u8>, u64>, &[u8]) + Send>;

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
    /// Every UPDATE received: the contract and its delta.
    updates: Vec<(ContractKey, ChatRoomStateV1Delta)>,
    /// How UPDATEs are answered.
    update_play: UpdatePlay,
    /// Each delegate key's generation (absent is 0), as the chat delegate keeps it.
    generations: HashMap<Vec<u8>, u64>,
    before_cas: Option<BeforeCas>,
    /// Every CAS store: the key and whether it was taken.
    cas: Vec<(Vec<u8>, bool)>,
    /// Every PUT received: the contract.
    puts: Vec<ContractKey>,
    /// Take the next CAS store of a room key, but answer nothing (then clear this).
    silence_first_room_store: bool,
    /// Answer the next listing with nothing (then clear this).
    silence_next_list: bool,
    /// The reads the last listing's reader still has to make.
    reads_due: usize,
    /// Versioned reads and CAS stores that arrived while a reader was still reading.
    overlaps: usize,
}

/// How the fake node answers an UPDATE.
#[derive(Clone, Debug, Default)]
enum UpdatePlay {
    /// `UpdateResponse`, as the node does when the contract took it.
    #[default]
    Accept,
    /// The node's update error with this cause.
    Refuse(&'static str),
    /// No answer at all.
    Ignore,
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
                match &read {
                    ChatDelegateRequestMsg::ListRequest => {
                        w.lists += 1;
                        // The reader gets every room key and the meta it is told of.
                        w.reads_due = w
                            .slots
                            .keys()
                            .filter(|k| {
                                parse_room_storage_key(k).is_some()
                                    || k.as_slice() == ROOMS_META_KEY
                            })
                            .count();
                    }
                    ChatDelegateRequestMsg::GetRequest { .. } => {
                        w.reads_due = w.reads_due.saturating_sub(1)
                    }
                    ChatDelegateRequestMsg::GetVersionedRequest { .. }
                    | ChatDelegateRequestMsg::CasStoreRequest { .. }
                        if w.reads_due > 0 =>
                    {
                        w.overlaps += 1
                    }
                    _ => {}
                }
                if w.delegate_missing {
                    out.push(encode(Err(missing_delegate())));
                    continue;
                }
                if matches!(read, ChatDelegateRequestMsg::ListRequest)
                    && std::mem::take(&mut w.silence_next_list)
                {
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
                    ChatDelegateRequestMsg::GetVersionedRequest { key } => {
                        ChatDelegateResponseMsg::GetVersionedResponse {
                            value: w.slots.get(key.as_bytes()).cloned(),
                            generation: w.generations.get(key.as_bytes()).copied().unwrap_or(0),
                            key,
                        }
                    }
                    // The chat delegate's compare-and-swap (`handle_cas_store_request`).
                    ChatDelegateRequestMsg::CasStoreRequest {
                        key,
                        value,
                        expected_generation,
                    } => {
                        if let Some(mut hook) = w.before_cas.take() {
                            let World {
                                slots, generations, ..
                            } = &mut *w;
                            hook(slots, generations, key.as_bytes());
                            w.before_cas = Some(hook);
                        }
                        let current = w.generations.get(key.as_bytes()).copied().unwrap_or(0);
                        let taken = current == expected_generation;
                        w.cas.push((key.as_bytes().to_vec(), taken));
                        if w.silence_first_room_store
                            && taken
                            && parse_room_storage_key(key.as_bytes()).is_some()
                        {
                            w.silence_first_room_store = false;
                            w.slots.insert(key.as_bytes().to_vec(), value);
                            w.generations.insert(key.as_bytes().to_vec(), current + 1);
                            continue;
                        }
                        let result = if taken {
                            w.slots.insert(key.as_bytes().to_vec(), value);
                            w.generations.insert(key.as_bytes().to_vec(), current + 1);
                            CasStoreResult::Stored {
                                generation: current + 1,
                            }
                        } else {
                            CasStoreResult::Conflict {
                                current_generation: current,
                                current_value: w.slots.get(key.as_bytes()).cloned(),
                            }
                        };
                        ChatDelegateResponseMsg::CasStoreResponse { key, result }
                    }
                    other => panic!("the app must only read and CAS-store, but sent {other:?}"),
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
        // A room contract UPDATE carrying a delta.
        ClientRequest::ContractOp(ContractRequest::Update {
            key,
            data: UpdateData::Delta(delta),
        }) => {
            let delta: ChatRoomStateV1Delta = ciborium::de::from_reader(delta.as_ref()).unwrap();
            w.updates.push((key, delta));
            match w.update_play.clone() {
                UpdatePlay::Accept => out.push(encode(Ok(HostResponse::ContractResponse(
                    ContractResponse::UpdateResponse {
                        key,
                        summary: freenet_stdlib::prelude::StateSummary::from(vec![]),
                    },
                )))),
                UpdatePlay::Refuse(cause) => out.push(encode(Err(ErrorKind::RequestError(
                    RequestError::ContractError(ContractError::Update {
                        key,
                        cause: cause.into(),
                    }),
                )
                .into()))),
                UpdatePlay::Ignore => {}
            }
        }
        // The seed PUT of a room just created: the network now holds it.
        ClientRequest::ContractOp(ContractRequest::Put {
            contract, state, ..
        }) => {
            let key = contract.key();
            w.puts.push(key);
            w.contracts
                .insert(*key.id(), (key, Contract::State(state.as_ref().to_vec())));
            out.push(encode(Ok(HostResponse::ContractResponse(
                ContractResponse::PutResponse { key },
            ))));
        }
        other => panic!("the app must only read, PUT and UPDATE with a delta, but sent {other:?}"),
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
        update_wait: Duration::from_secs(60),
        owed_limit: Duration::from_secs(180),
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
    commands: std::sync::mpsc::Sender<Command>,
}

const PATIENCE: Duration = Duration::from_secs(20);

impl Running {
    fn start(node: &FakeNode, dir: &Path, pace: Pace) -> Running {
        Running::start_with(node, dir, pace, Arc::default(), std::sync::mpsc::channel())
    }

    /// Start with an outbox and a command channel that may outlive this run, as the link's do.
    fn start_with(
        node: &FakeNode,
        dir: &Path,
        pace: Pace,
        outbox: Arc<Mutex<Outbox>>,
        (commands, inbox): (
            std::sync::mpsc::Sender<Command>,
            std::sync::mpsc::Receiver<Command>,
        ),
    ) -> Running {
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
                    &inbox,
                    &mut outbox.lock().unwrap(),
                )
            })
        };
        Running {
            updates,
            stop,
            handle: Some(handle),
            commands,
        }
    }

    /// Do something in the room `room`, as the chat screen would.
    fn act(&self, room: &TestRoom, action: Action) {
        self.commands
            .send(Command {
                room: bs58::encode(room.vk().to_bytes()).into_string(),
                action,
            })
            .unwrap();
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

// Sending ---------------------------------------------------------------------------------------

fn say(text: &str) -> Action {
    Action::Send {
        text: text.into(),
        reply_to: None,
    }
}

/// The message texted `text` in the newest room list: its delivery, or `None` if not shown.
fn delivery_of(session: &Running, text: &str) -> Option<Option<crate::chat::model::Pending>> {
    let (rooms, _, _) = session.last_rooms()?;
    rooms[0]
        .messages
        .iter()
        .find(|m| m.text == text)
        .map(|m| m.pending.clone())
}

fn update_notification(room: &TestRoom, messages: Vec<AuthorizedMessageV1>) -> Message {
    let delta = ChatRoomStateV1Delta {
        recent_messages: Some(messages),
        ..Default::default()
    };
    let note: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(
        ContractResponse::UpdateNotification {
            key: room.key,
            update: UpdateData::Delta(cbor(&delta).into()),
        },
    ));
    Message::Binary(bincode::serialize(&note).unwrap().into())
}

#[test]
fn a_sent_message_shows_at_once_and_is_marked_sent_when_the_node_takes_it() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    // Held until the message has been seen on its way.
    node.with(|w| w.update_play = UpdatePlay::Ignore);
    let (session, _dir) = following(&node, &[&a], "send-ok", quick());
    session.act(&a, say("hello from the app"));
    session.wait_until("shown on its way", || {
        matches!(delivery_of(&session, "hello from the app"), Some(Some(p)) if p.failed.is_none())
    });
    let (key, delta) = node.with(|w| w.updates[0].clone());
    // The room's contract, a delta carrying one message signed by the room's key.
    assert_eq!(key, a.key);
    let messages = delta.recent_messages.unwrap();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].validate(&a.vk()).is_ok());
    assert!(delta.members.is_none() && delta.member_info.is_none());

    // The node answers: sent.
    let answer: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(
        ContractResponse::UpdateResponse {
            key: a.key,
            summary: freenet_stdlib::prelude::StateSummary::from(vec![]),
        },
    ));
    node.with(|w| {
        w.push
            .push(Message::Binary(bincode::serialize(&answer).unwrap().into()))
    });
    session.wait_until("marked sent", || {
        delivery_of(&session, "hello from the app") == Some(None)
    });
    assert_eq!(node.with(|w| w.updates.len()), 1);
}

#[test]
fn a_refused_update_is_shown_with_the_reason_and_retry_sends_it_again() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| w.update_play = UpdatePlay::Refuse("the contract said no"));
    let (session, _dir) = following(&node, &[&a], "send-refused", quick());
    session.act(&a, say("refused one"));
    session.wait_until("shown as not sent", || {
        matches!(
            delivery_of(&session, "refused one"),
            Some(Some(p)) if p.failed.as_deref().is_some_and(|f| f.contains("the contract said no"))
        )
    });
    let Some(Some(pending)) = delivery_of(&session, "refused one") else {
        unreachable!()
    };
    node.with(|w| w.update_play = UpdatePlay::Accept);
    session.act(
        &a,
        Action::Retry {
            entry: pending.entry,
        },
    );
    session.wait_until("sent on retry", || {
        delivery_of(&session, "refused one") == Some(None)
    });
    assert_eq!(node.with(|w| w.updates.len()), 2);
}

#[test]
fn an_unanswered_update_fails_and_counts_as_sent_once_the_network_brings_it() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| w.update_play = UpdatePlay::Ignore);
    let pace = Pace {
        update_wait: Duration::from_millis(200),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "send-unanswered", pace);
    session.act(&a, say("lost answer"));
    session.wait_until(
        "failed for want of an answer",
        || matches!(delivery_of(&session, "lost answer"), Some(Some(p)) if p.failed.is_some()),
    );
    // The node did take it, and the network brings it back.
    let (_, delta) = node.with(|w| w.updates[0].clone());
    let note = update_notification(&a, delta.recent_messages.unwrap());
    node.with(|w| w.push.push(note));
    session.wait_until("settled as sent", || {
        delivery_of(&session, "lost answer") == Some(None)
    });
}

#[test]
fn what_was_out_when_the_connection_dropped_goes_again_unchanged() {
    let node = fake_node(vec![]);
    // A private room, so sealing it again would change it.
    let a = TestRoom::private(1);
    node.with(|w| w.update_play = UpdatePlay::Ignore);
    let (session, _dir) = following(&node, &[&a], "send-reconnect", quick());
    session.act(&a, say("across a drop"));
    session.wait_until("sent once", || node.with(|w| w.updates.len() == 1));
    // The room's secret rotates (shown by a message under the new one) before the drop.
    let f = crate::rooms::room_tests::Fixture::new();
    let v1 = [7u8; 32];
    let mut rotated = a.state.clone();
    crate::rooms::room_tests::add_secret(&f, &mut rotated, 1, &v1, &a.owner);
    let (ct, nonce) = river_core::ecies::encrypt_with_symmetric_key(
        &v1,
        &river_core::room_state::content::TextContentV1::new("under v1".into()).encode(),
    );
    rotated.recent_messages.messages.push(message_with(
        &a.owner,
        RoomMessageBody::private_text(ct, nonce, 1),
    ));
    let note: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(
        ContractResponse::UpdateNotification {
            key: a.key,
            update: UpdateData::State(cbor(&rotated).into()),
        },
    ));
    node.with(|w| {
        w.push
            .push(Message::Binary(bincode::serialize(&note).unwrap().into()))
    });
    session.wait_rooms("the rotation arrived", |r, _, _| {
        texts(&r[0]).contains(&"under v1".to_owned())
    });
    node.with(|w| {
        w.update_play = UpdatePlay::Accept;
        w.drop_live = true;
    });
    session.wait_until("sent again on the new connection", || {
        node.with(|w| w.updates.len() >= 2)
    });
    let (first, second) = node.with(|w| (w.updates[0].1.clone(), w.updates[1].1.clone()));
    assert_eq!(first.recent_messages, second.recent_messages);
    let sealed: Vec<Option<u32>> = second
        .recent_messages
        .unwrap()
        .iter()
        .map(|m| m.message.content.secret_version())
        .collect();
    assert_eq!(
        sealed,
        [Some(0)],
        "sent again as it was, not sealed again under v1"
    );
    session.wait_until("marked sent", || {
        delivery_of(&session, "across a drop") == Some(None)
    });
}

fn message_with(author: &SigningKey, content: RoomMessageBody) -> AuthorizedMessageV1 {
    AuthorizedMessageV1::new(
        MessageV1 {
            room_owner: MemberId::from(&author.verifying_key()),
            author: MemberId::from(&author.verifying_key()),
            time: SystemTime::now(),
            content,
        },
        author,
    )
}

#[test]
fn a_first_read_that_finds_no_rooms_keeps_what_an_earlier_run_left_unsent() {
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    for first_read in [Loaded::missing(), loaded(vec![])] {
        let mut outbox = Outbox::default();
        assert!(outbox.command(&LiveRoom::new(a.stored()), say("left unsent")));
        let mut rooms = Rooms {
            outbox,
            ..Rooms::default()
        };
        rooms.replace(first_read);
        assert!(!rooms.outbox.is_empty());
        // A read that finds the room keeps it too; one that finds rooms without it drops it.
        rooms.replace(loaded(vec![a.stored()]));
        assert!(!rooms.outbox.is_empty());
        let b = TestRoom::new(2, "B", 1, &["b1"]);
        rooms.replace(loaded(vec![b.stored()]));
        assert!(rooms.outbox.is_empty());
    }
}

#[test]
fn a_run_that_panics_gives_the_outbox_back() {
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let mut outbox = Outbox::default();
    assert!(outbox.command(&LiveRoom::new(a.stored()), say("kept")));
    outbox.waiting.push(Command {
        room: "x".into(),
        action: say("waiting"),
    });
    let (_tx, rx) = std::sync::mpsc::channel();
    let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(
            "127.0.0.1:1",
            None,
            &|_| panic!("a bug while reporting"),
            &quick(),
            // Asked to stop at once: if the panic were not where it is, the run would return.
            &|| true,
            &rx,
            &mut outbox,
        )
    }));
    assert!(ended.is_err());
    assert!(!outbox.is_empty());
    assert_eq!(outbox.waiting.len(), 1);
}

#[test]
fn a_command_waiting_for_the_rooms_survives_a_stop() {
    let node = fake_node(vec![Play::Drop; 1000]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| w.add_room(&a));
    let dir = temp_dir("send-waiting-stop");
    save_token(&dir, "kept-token");
    let outbox: Arc<Mutex<Outbox>> = Arc::default();
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Command {
        room: bs58::encode(a.vk().to_bytes()).into_string(),
        action: say("written before any rooms"),
    })
    .unwrap();
    let first = Running::start_with(&node, &dir, quick(), outbox.clone(), (tx, rx));
    // Every connection drops, so the rooms are never read; the command is taken while waiting.
    first.wait_until("taken into the outbox", || {
        node.with(|w| w.connections.len() >= 3)
    });
    drop(first);
    assert_eq!(outbox.lock().unwrap().waiting.len(), 1);
    node.with(|w| w.plays.clear());
    let second = Running::start_with(&node, &dir, quick(), outbox, std::sync::mpsc::channel());
    second.wait_until("sent once the next run reads the rooms", || {
        delivery_of(&second, "written before any rooms") == Some(None)
    });
}

#[test]
fn a_command_while_disconnected_shows_at_once_and_goes_out_on_reconnecting() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        backoff_first: Duration::from_secs(1),
        backoff_max: Duration::from_secs(1),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "send-offline", pace);
    // Every new connection is dropped until the message has been seen.
    node.with(|w| {
        w.plays.extend(std::iter::repeat_n(Play::Drop, 1000));
        w.drop_live = true;
    });
    session.wait_until("reconnecting", || {
        session
            .updates()
            .iter()
            .any(|u| matches!(u, RoomsUpdate::Status(LinkStatus::Reconnecting)))
    });
    session.act(&a, say("written offline"));
    session.wait_until("shown while disconnected", || {
        matches!(delivery_of(&session, "written offline"), Some(Some(_)))
    });
    assert!(node.with(|w| w.updates.is_empty()));
    node.with(|w| w.plays.clear());
    session.wait_until("sent and taken after reconnecting", || {
        delivery_of(&session, "written offline") == Some(None)
    });
}

fn accepted(room: &TestRoom) -> Message {
    let answer: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(
        ContractResponse::UpdateResponse {
            key: room.key,
            summary: freenet_stdlib::prelude::StateSummary::from(vec![]),
        },
    ));
    Message::Binary(bincode::serialize(&answer).unwrap().into())
}

#[test]
fn a_late_answer_settles_the_late_update_and_only_then_does_the_next_go() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| w.update_play = UpdatePlay::Ignore);
    let pace = Pace {
        update_wait: Duration::from_millis(100),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "send-late", pace);
    session.act(&a, say("first"));
    session.wait_until(
        "first shown unconfirmed",
        || matches!(delivery_of(&session, "first"), Some(Some(p)) if p.failed.is_some()),
    );
    session.act(&a, say("second"));
    session.wait_until(
        "second shown on its way",
        || matches!(delivery_of(&session, "second"), Some(Some(p)) if p.failed.is_none()),
    );
    // Second waits for first's answer; it has not gone out.
    assert_eq!(node.with(|w| w.updates.len()), 1);
    node.with(|w| w.push.push(accepted(&a)));
    session.wait_until("first sent by its own late answer", || {
        delivery_of(&session, "first") == Some(None)
    });
    session.wait_until("then second goes out", || {
        node.with(|w| w.updates.len() == 2)
    });
    assert!(
        matches!(delivery_of(&session, "second"), Some(Some(_))),
        "not yet answered"
    );
    node.with(|w| w.push.push(accepted(&a)));
    session.wait_until("second sent by its own answer", || {
        delivery_of(&session, "second") == Some(None)
    });
}

#[test]
fn two_rooms_each_have_an_update_out_at_once() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    node.with(|w| w.update_play = UpdatePlay::Ignore);
    let (session, _dir) = following(&node, &[&a, &b], "send-two-rooms", quick());
    session.act(&a, say("in A"));
    session.act(&b, say("in B"));
    session.wait_until("both out", || node.with(|w| w.updates.len() == 2));
    let keys: HashSet<ContractKey> = node.with(|w| w.updates.iter().map(|(k, _)| *k).collect());
    assert_eq!(keys, HashSet::from([a.key, b.key]));
    // B's answer settles B, not A.
    node.with(|w| w.push.push(accepted(&b)));
    session.wait_until("B settled", || {
        session.last_rooms().is_some_and(|(rooms, _, _)| {
            let find = |name: &str, text: &str| {
                rooms
                    .iter()
                    .find(|r| r.name == name)
                    .and_then(|r| r.messages.iter().find(|m| m.text == text))
                    .map(|m| m.pending.is_none())
            };
            find("B", "in B") == Some(true) && find("A", "in A") == Some(false)
        })
    });
}

#[test]
fn a_command_sent_before_the_rooms_are_read_waits_for_them() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| w.add_room(&a));
    let dir = temp_dir("send-early");
    save_token(&dir, "kept-token");
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Command {
        room: bs58::encode(a.vk().to_bytes()).into_string(),
        action: say("before anything"),
    })
    .unwrap();
    let session = Running::start_with(&node, &dir, quick(), Arc::default(), (tx, rx));
    session.wait_until("sent once the rooms are read", || {
        delivery_of(&session, "before anything") == Some(None)
    });
}

#[test]
fn edit_react_and_delete_reach_the_node_and_the_screen() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "send-actions", quick());
    let target = message_key(&a.state.recent_messages.messages[0].id());
    session.act(
        &a,
        Action::Edit {
            target: target.clone(),
            text: "a1 fixed".into(),
        },
    );
    session.act(
        &a,
        Action::React {
            target: target.clone(),
            emoji: "🎉".into(),
        },
    );
    session.wait_until("edited and reacted", || {
        session.last_rooms().is_some_and(|(rooms, _, _)| {
            rooms[0].messages.first().is_some_and(|m| {
                m.text == "a1 fixed" && m.edited && m.reactions.iter().any(|r| r.emoji == "🎉")
            })
        })
    });
    session.act(&a, Action::Delete { target });
    session.wait_until("deleted, all three sent and answered", || {
        node.with(|w| w.updates.len() == 3)
            && session.last_rooms().is_some_and(|(rooms, _, _)| {
                rooms[0].messages.is_empty() && rooms[0].notices.is_empty()
            })
    });
    let kinds: Vec<u32> = node.with(|w| {
        w.updates
            .iter()
            .flat_map(|(_, d)| d.recent_messages.clone().unwrap())
            .map(|m| match &m.message.content {
                RoomMessageBody::Public { data, .. } => {
                    river_core::room_state::content::ActionContentV1::decode(data)
                        .unwrap()
                        .action_type
                }
                RoomMessageBody::Private { .. } => panic!("a public room"),
            })
            .collect()
    });
    use river_core::room_state::content::{
        ACTION_TYPE_DELETE, ACTION_TYPE_EDIT, ACTION_TYPE_REACTION,
    };
    assert_eq!(
        kinds,
        [ACTION_TYPE_EDIT, ACTION_TYPE_REACTION, ACTION_TYPE_DELETE]
    );
}

#[test]
fn what_was_unsent_when_a_session_stopped_goes_out_in_the_next() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| {
        w.add_room(&a);
        w.update_play = UpdatePlay::Ignore;
    });
    let dir = temp_dir("send-restart");
    save_token(&dir, "kept-token");
    let outbox: Arc<Mutex<Outbox>> = Arc::default();
    let first = Running::start_with(
        &node,
        &dir,
        quick(),
        outbox.clone(),
        std::sync::mpsc::channel(),
    );
    first.wait_rooms("live", |r, _, s| r.len() == 1 && s == LinkStatus::Live);
    first.act(&a, say("survives a stop"));
    first.wait_until("sent once", || node.with(|w| w.updates.len() == 1));
    // Stopped (as on a state that needs the user), with no answer yet.
    drop(first);
    node.with(|w| w.update_play = UpdatePlay::Accept);
    let second = Running::start_with(&node, &dir, quick(), outbox, std::sync::mpsc::channel());
    second.wait_until("shown and sent in the next session", || {
        delivery_of(&second, "survives a stop") == Some(None)
    });
    let (one, two) = node.with(|w| (w.updates[0].1.clone(), w.updates[1].1.clone()));
    assert_eq!(
        one.recent_messages, two.recent_messages,
        "the same message again"
    );
}

// Round 3 --------------------------------------------------------------------------------------

#[test]
fn commands_waiting_through_a_read_that_finds_no_rooms_wait_for_one_that_does() {
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Command {
        room: bs58::encode(a.vk().to_bytes()).into_string(),
        action: say("carried over"),
    })
    .unwrap();
    let mut rooms = Rooms::default();
    rooms.replace(Loaded::missing());
    assert!(!rooms.take_commands(&rx));
    assert_eq!(rooms.outbox.waiting.len(), 1);
    let mut unreadable = loaded(vec![]);
    unreadable.listed = 1;
    unreadable.unreadable = 1;
    rooms.replace(unreadable);
    assert!(!rooms.take_commands(&rx));
    assert_eq!(rooms.outbox.waiting.len(), 1);
    rooms.replace(loaded(vec![a.stored()]));
    assert!(rooms.take_commands(&rx));
    assert!(rooms.outbox.waiting.is_empty());
    assert!(!rooms.outbox.is_empty());
}

/// Rooms `list`, each with one update out.
fn owing(list: &[&TestRoom]) -> Rooms {
    let mut rooms = Rooms::default();
    rooms.replace(loaded(list.iter().map(|r| r.stored()).collect()));
    for room in &rooms.list {
        assert!(rooms.outbox.command(room, say("out")));
        let mut changed = false;
        assert!(
            rooms
                .outbox
                .next_update(room, Instant::now(), &mut changed)
                .is_some()
        );
    }
    rooms
}

#[test]
fn update_failures_that_name_no_update_go_to_the_one_room_that_owes() {
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let empty_ring: ClientError = ErrorKind::EmptyRing.into();
    let missing = |id| -> ClientError {
        ErrorKind::RequestError(RequestError::ContractError(
            ContractError::MissingContract { key: id },
        ))
        .into()
    };
    let none = Subscriptions::default();

    let one = owing(&[&a]);
    assert_eq!(
        one.failed_update_unnamed(&empty_ring, &none, false)
            .map(|(id, _)| id),
        Some(a.id())
    );
    assert_eq!(
        one.failed_update_unnamed(&missing(a.id()), &none, false)
            .map(|(id, _)| id),
        Some(a.id())
    );
    // A subscribe waiting for its answer could be the one that failed.
    let mut asking = Subscriptions::default();
    asking.pending.insert(b.id(), Instant::now());
    assert_eq!(one.failed_update_unnamed(&empty_ring, &asking, false), None);
    // A missing contract for a room that owes nothing is a subscribe's.
    assert_eq!(
        one.failed_update_unnamed(&missing(b.id()), &none, false),
        None
    );
    // A read of the stored rooms is under way: the failure may be one of its requests.
    let shutdown: ClientError = ErrorKind::Shutdown.into();
    assert_eq!(one.failed_update_unnamed(&shutdown, &none, true), None);
    assert_eq!(
        one.failed_update_unnamed(&shutdown, &none, false)
            .map(|(id, _)| id),
        Some(a.id())
    );
    // A subscribe for the owing room is waiting: its missing contract may be the subscribe's.
    let mut asking_a = Subscriptions::default();
    asking_a.pending.insert(a.id(), Instant::now());
    assert_eq!(
        one.failed_update_unnamed(&missing(a.id()), &asking_a, false),
        None
    );
    // A create or join has a request out: it may be the one that failed.
    let mut busy = owing(&[&a]);
    busy.outbox.work.create("Room", "Ada", false);
    assert!(
        busy.outbox
            .work
            .next_request(false, Instant::now())
            .is_some()
    );
    assert_eq!(busy.failed_update_unnamed(&shutdown, &none, false), None);
    // Two rooms owe: which one failed can't be told.
    let two = owing(&[&a, &b]);
    assert_eq!(two.failed_update_unnamed(&empty_ring, &none, false), None);
    assert_eq!(
        two.failed_update_unnamed(&missing(b.id()), &none, false)
            .map(|(id, _)| id),
        Some(b.id())
    );
}

#[test]
fn the_stored_rooms_are_not_read_while_a_create_or_join_works() {
    let pace = quick();
    let mut work = super::super::work::Work::default();
    assert!(!reload_due(&mut work, Duration::ZERO, &pace));
    assert!(reload_due(&mut work, pace.refresh, &pace));
    work.create("Room", "Ada", false);
    assert!(
        !reload_due(&mut work, pace.refresh * 2, &pace),
        "a task is working"
    );
}

#[test]
fn a_failed_subscribe_is_asked_again_only_for_a_room_shown() {
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let mut rooms = owing(&[&a]);
    let mut subs = Subscriptions::default();
    let gone = ContractInstanceId::new([9; 32]);
    assert!(!rooms.subscribe_failed(&mut subs, gone, &quick()));
    assert!(subs.retry.is_empty(), "a room not shown is not asked for");
    assert!(rooms.subscribe_failed(&mut subs, a.id(), &quick()));
    assert!(subs.retry.contains_key(&a.id()));
}

#[test]
fn a_rekeyed_room_owes_nothing_under_its_old_contract() {
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let mut rooms = owing(&[&a]);
    assert!(rooms.outbox.owes(&a.vk()));
    let rekeyed = TestRoom::new(1, "A", 2, &["a1"]);
    rooms.replace(loaded(vec![rekeyed.stored()]));
    assert!(!rooms.outbox.owes(&a.vk()));
    // What was in flight goes again, unchanged, under the new contract.
    let mut changed = false;
    let again = rooms
        .outbox
        .next_update(&rooms.list[0], Instant::now(), &mut changed)
        .expect("sent again without the user acting");
    assert_eq!(again.recent_messages.unwrap().len(), 1);

    // A room a complete read no longer lists owes nothing either.
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let mut rooms = owing(&[&a, &b]);
    rooms.replace(loaded(vec![a.stored()]));
    assert!(rooms.outbox.owes(&a.vk()));
    assert!(!rooms.outbox.owes(&b.vk()));
}

#[test]
fn an_update_failure_naming_nothing_reaches_the_screen() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| w.update_play = UpdatePlay::Ignore);
    let (session, _dir) = following(&node, &[&a], "send-keyless", quick());
    session.act(&a, say("into an empty ring"));
    session.wait_until("sent", || node.with(|w| w.updates.len() == 1));
    let error: Result<HostResponse, ClientError> = Err(ErrorKind::EmptyRing.into());
    node.with(|w| {
        w.push
            .push(Message::Binary(bincode::serialize(&error).unwrap().into()))
    });
    session.wait_until("shown as refused", || {
        matches!(
            delivery_of(&session, "into an empty ring"),
            Some(Some(p)) if p.failed.as_deref().is_some_and(|f| f.contains("no ring connections"))
        )
    });
}

#[test]
fn retry_of_an_unanswered_update_reconnects_once_and_sends_it_again_unchanged() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| w.update_play = UpdatePlay::Ignore);
    // No forced reconnect in this test's lifetime: only the Retry can reconnect.
    let pace = Pace {
        update_wait: Duration::from_millis(100),
        owed_limit: Duration::from_secs(3600),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "send-retry-late", pace);
    session.act(&a, say("say it again"));
    session.wait_until(
        "unconfirmed",
        || matches!(delivery_of(&session, "say it again"), Some(Some(p)) if p.failed.is_some()),
    );
    assert_eq!(node.with(|w| w.connections.len()), 1);
    node.with(|w| w.update_play = UpdatePlay::Accept);
    let Some(Some(pending)) = delivery_of(&session, "say it again") else {
        unreachable!()
    };
    session.act(
        &a,
        Action::Retry {
            entry: pending.entry,
        },
    );
    session.wait_until("sent on a new connection, and taken", || {
        delivery_of(&session, "say it again") == Some(None)
    });
    let (connections, updates) = node.with(|w| (w.connections.len(), w.updates.clone()));
    assert_eq!(connections, 2, "one reconnect");
    assert_eq!(updates.len(), 2, "one resend");
    assert_eq!(updates[0].1.recent_messages, updates[1].1.recent_messages);
}

#[test]
fn an_answer_owed_too_long_reconnects_and_resends_then_gives_up() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| w.update_play = UpdatePlay::Ignore);
    let pace = Pace {
        update_wait: Duration::from_millis(50),
        owed_limit: Duration::from_millis(150),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "send-owed-long", pace);
    session.act(&a, say("unheard"));
    session.wait_until("given up on, after the bounded resends", || {
        matches!(
            delivery_of(&session, "unheard"),
            Some(Some(p)) if p.failed.as_deref() == Some(super::super::outbox::NEVER_ANSWERED)
        )
    });
    let updates = node.with(|w| w.updates.clone());
    assert_eq!(
        updates.len() as u32,
        super::super::outbox::MAX_FORCED_RECONNECTS
    );
    assert!(
        updates
            .windows(2)
            .all(|w| w[0].1.recent_messages == w[1].1.recent_messages)
    );
    assert!(
        node.with(|w| w.connections.len()) as u32 >= super::super::outbox::MAX_FORCED_RECONNECTS
    );
}

// Creating, joining and inviting ---------------------------------------------------------------

impl Running {
    /// Something about the room list, as the chat screen sends it (its `room` is empty).
    fn list(&self, action: Action) {
        self.commands
            .send(Command {
                room: String::new(),
                action,
            })
            .unwrap();
    }

    fn last_work(&self) -> Vec<WorkView> {
        self.updates()
            .into_iter()
            .rev()
            .find_map(|u| match u {
                RoomsUpdate::Rooms { work, .. } => Some(work),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn invites(&self) -> Vec<(String, Result<String, String>)> {
        self.updates()
            .into_iter()
            .filter_map(|u| match u {
                RoomsUpdate::Invite { room, link } => Some((room, link)),
                _ => None,
            })
            .collect()
    }
}

/// A `rooms_meta` as a web UI wrote it: a field this app does not know, and room `open` open.
fn web_ui_meta(open: VerifyingKey, marker: u32) -> Vec<u8> {
    use ciborium::Value;
    let mut out = Vec::new();
    ciborium::ser::into_writer(
        &Value::Map(vec![
            (
                Value::Text("current_room_key".into()),
                Value::serialized(&open).unwrap(),
            ),
            (Value::Text("web_ui_marker".into()), Value::from(marker)),
        ]),
        &mut out,
    )
    .unwrap();
    out
}

fn meta_marker(bytes: &[u8]) -> Option<u32> {
    let value: ciborium::Value = ciborium::de::from_reader(bytes).ok()?;
    value.as_map()?.iter().find_map(|(k, v)| {
        (k.as_text() == Some("web_ui_marker"))
            .then(|| v.as_integer().and_then(|i| u32::try_from(i).ok()))?
    })
}

/// The stored rooms other than `known`, decoded.
fn stored_besides(w: &World, known: &[VerifyingKey]) -> Vec<StoredRoom> {
    w.slots
        .iter()
        .filter_map(|(k, v)| {
            let vk = parse_room_storage_key(k)?;
            if known.contains(&vk) {
                return None;
            }
            match decode_room_slot(v).ok()? {
                StoredRoomSlot::Present(r) => Some(*r),
                StoredRoomSlot::Tombstone => None,
            }
        })
        .collect()
}

#[test]
fn a_room_created_here_is_stored_published_and_opened() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| {
        w.slots
            .insert(ROOMS_META_KEY.to_vec(), web_ui_meta(a.vk(), 7));
    });
    let (session, _dir) = following(&node, &[&a], "create", quick());
    session.list(Action::CreateRoom {
        name: "Book club".into(),
        nickname: "Ada".into(),
        private: true,
    });
    session.wait_rooms("the new room is in the list", |r, _, _| {
        r.iter().any(|r| r.name == "Book club")
    });
    let created = node.with(|w| {
        let created = stored_besides(w, &[a.vk()]);
        assert_eq!(created.len(), 1);
        let room = created[0].clone();
        // Published once, under the new room's own contract, which is the web UI's key for it.
        assert_eq!(w.puts, [room.contract_key]);
        assert_eq!(
            room.contract_key,
            super::super::create::contract_key(&room.owner_vk)
        );
        assert_eq!(room.self_verifying_key(), Some(room.owner_vk));
        // The web UI opens it next, and nothing else it stored in the meta was lost.
        let meta = w.slots.get(ROOMS_META_KEY).unwrap();
        assert_eq!(
            decode_rooms_meta(meta).unwrap().current_room_key,
            Some(room.owner_vk)
        );
        assert_eq!(meta_marker(meta), Some(7));
        // Every write was a CAS store that was taken: one for the room, one for the meta.
        assert_eq!(w.cas.iter().filter(|(_, ok)| *ok).count(), 2);
        assert_eq!(w.overlaps, 0);
        room
    });
    // Shown as the owner, private, and opened by the screen once (the task said it is done).
    let (rooms, _, _) = session.last_rooms().unwrap();
    let shown = rooms.iter().find(|r| r.name == "Book club").unwrap();
    assert!(shown.private);
    assert_eq!(
        shown.key,
        bs58::encode(created.owner_vk.to_bytes()).into_string()
    );
    assert!(session.updates().iter().any(|u| matches!(
        u,
        RoomsUpdate::Rooms { work, .. }
            if work.iter().any(|w| w.state == WorkState::Done { room: shown.key.clone() })
    )));
    session.wait_until("the finished task is dropped", || {
        session.last_work().is_empty()
    });
}

/// The web UI stores the meta between this app's read and its store: the store is refused, and
/// this app reads again and stores what the web UI stored with only the open room changed.
#[test]
fn a_meta_the_web_ui_changed_meanwhile_is_read_again_not_overwritten() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let open = a.vk();
    node.with(|w| {
        w.slots
            .insert(ROOMS_META_KEY.to_vec(), web_ui_meta(open, 1));
        let mut once = true;
        w.before_cas = Some(Box::new(move |slots, generations, key| {
            if key == ROOMS_META_KEY && std::mem::take(&mut once) {
                slots.insert(ROOMS_META_KEY.to_vec(), web_ui_meta(open, 2));
                *generations.entry(key.to_vec()).or_default() += 1;
            }
        }));
    });
    let (session, _dir) = following(&node, &[&a], "create-conflict", quick());
    session.list(Action::CreateRoom {
        name: "Book club".into(),
        nickname: "Ada".into(),
        private: false,
    });
    session.wait_rooms("the new room is in the list", |r, _, _| {
        r.iter().any(|r| r.name == "Book club")
    });
    node.with(|w| {
        let meta_stores: Vec<bool> = w
            .cas
            .iter()
            .filter(|(k, _)| k.as_slice() == ROOMS_META_KEY)
            .map(|(_, ok)| *ok)
            .collect();
        assert_eq!(meta_stores, [false, true], "refused once, then taken");
        let meta = w.slots.get(ROOMS_META_KEY).unwrap();
        assert_eq!(meta_marker(meta), Some(2), "the web UI's write is kept");
        let created = stored_besides(w, &[a.vk()]);
        assert_eq!(
            decode_rooms_meta(meta).unwrap().current_room_key,
            Some(created[0].owner_vk)
        );
    });
}

/// The web UI stores a room under the same key first (it cannot happen for a fresh key, but a
/// join can meet it): never overwritten, and the user is told.
#[test]
fn a_room_the_web_ui_stored_first_is_never_overwritten() {
    let node = fake_node(vec![]);
    let room = TestRoom::new(3, "Theirs", 1, &["t1"]);
    // An invitation into `room`, which the web UI then joins with a different key first.
    let inviter = LiveRoom::new(room.stored());
    let (_, link) = super::super::create::invitation(&inviter, "http://x/").unwrap();
    let real = super::super::create::contract_key(&room.vk());
    let slot = room_storage_key(&room.vk());
    let web_ui_copy = room.slot();
    node.with(|w| {
        w.contracts
            .insert(*real.id(), (real, Contract::State(cbor(&room.state))));
        let copy = web_ui_copy.clone();
        let key = slot.clone();
        w.before_cas = Some(Box::new(move |slots, generations, k| {
            if k == key.as_slice() && !slots.contains_key(&key) {
                slots.insert(key.clone(), copy.clone());
                *generations.entry(key.clone()).or_default() += 1;
            }
        }));
    });
    let other = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&other], "join-raced", quick());
    session.list(Action::Join {
        code: InviteCode(link),
        nickname: "Bea".into(),
    });
    session.wait_until("the join is refused", || {
        session.last_work().iter().any(|w| {
            matches!(&w.state, WorkState::Failed { why, retry: false } if why.contains("already"))
        })
    });
    node.with(|w| {
        assert_eq!(
            w.slots.get(&slot),
            Some(&web_ui_copy),
            "the web UI's copy stands"
        );
        assert!(w.updates.is_empty(), "no join was sent");
        assert!(w.puts.is_empty());
    });
}

#[test]
fn joining_from_an_invitation_stores_the_room_sends_the_join_and_shows_it() {
    let node = fake_node(vec![]);
    let room = TestRoom::new(3, "Garden", 1, &["t1"]);
    let inviter = LiveRoom::new(room.stored());
    let (inv, link) = super::super::create::invitation(&inviter, "http://x/").unwrap();
    let real = super::super::create::contract_key(&room.vk());
    node.with(|w| {
        w.contracts
            .insert(*real.id(), (real, Contract::State(cbor(&room.state))));
        // The user left this room once: its key holds a tombstone.
        w.slots
            .insert(room_storage_key(&room.vk()), cbor(&Slot::Tombstone));
    });
    let other = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&other], "join", quick());
    // As pasted: the whole link.
    session.list(Action::Join {
        code: InviteCode(format!("  {link}\n")),
        nickname: "Bea".into(),
    });
    session.wait_rooms("the joined room is in the list", |r, _, _| {
        r.iter().any(|r| r.name == "Garden")
    });
    let me = inv.invitee_signing_key.verifying_key();
    node.with(|w| {
        let StoredRoomSlot::Present(stored) =
            decode_room_slot(&w.slots[&room_storage_key(&room.vk())]).unwrap()
        else {
            panic!("the tombstone was replaced by the room");
        };
        assert_eq!(stored.self_verifying_key(), Some(me));
        assert_eq!(stored.contract_key, real);
        assert_eq!(stored.self_nickname.as_deref(), Some("Bea"));
        // One UPDATE to the room, adding the invitee; no PUT.
        assert!(w.puts.is_empty());
        assert_eq!(w.updates.len(), 1);
        let (key, delta) = &w.updates[0];
        assert_eq!(*key, real);
        assert_eq!(
            delta.members.as_ref().unwrap().added()[0].member.member_vk,
            me
        );
        assert_eq!(w.overlaps, 0);
    });
    let (rooms, _, _) = session.last_rooms().unwrap();
    let garden = rooms.iter().find(|r| r.name == "Garden").unwrap();
    assert!(
        garden
            .members
            .iter()
            .any(|m| m.id == garden.me && m.nickname == "Bea")
    );
    assert!(garden.send_block.is_none());
}

#[test]
fn a_failed_create_is_said_and_try_again_finishes_it() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "create-fails", quick());
    // The store refuses the first write.
    node.with(|w| w.delegate_missing = true);
    session.list(Action::CreateRoom {
        name: "Book club".into(),
        nickname: "Ada".into(),
        private: false,
    });
    session.wait_until("the failure is shown", || {
        session
            .last_work()
            .iter()
            .any(|w| matches!(&w.state, WorkState::Failed { retry: true, .. }))
    });
    let id = session.last_work()[0].id;
    assert!(
        node.with(|w| w.puts.is_empty()),
        "nothing published before it is stored"
    );
    node.with(|w| w.delegate_missing = false);
    session.list(Action::RetryWork { id });
    session.wait_rooms("the room is created", |r, _, _| {
        r.iter().any(|r| r.name == "Book club")
    });
    assert_eq!(node.with(|w| w.puts.len()), 1);
}

/// A store whose answer never comes: the connection is dropped (so the answer can't arrive
/// late and be taken for another), and on the next one the room is found already stored and
/// the create goes on without storing it twice.
#[test]
fn an_unanswered_store_reconnects_and_the_create_goes_on() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        delegate_deadline: Duration::from_secs(2),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "create-silent", pace);
    node.with(|w| w.silence_first_room_store = true);
    session.list(Action::CreateRoom {
        name: "Quiet".into(),
        nickname: "Ada".into(),
        private: false,
    });
    // The room can show from the store before the create has finished (a new connection
    // reads the stored rooms first), so wait for the create itself.
    session.wait_until("the create finishes", || {
        node.with(|w| !w.puts.is_empty()) && session.last_work().is_empty()
    });
    node.with(|w| {
        assert!(
            !w.silence_first_room_store,
            "the first store went unanswered"
        );
        assert!(w.connections.len() >= 2, "it cost a connection");
        assert_eq!(w.puts.len(), 1);
        let slot_stores = w
            .cas
            .iter()
            .filter(|(k, ok)| parse_room_storage_key(k).is_some() && *ok)
            .count();
        assert_eq!(slot_stores, 1, "stored once");
    });
}

#[test]
fn an_invitation_is_made_for_the_room_and_its_link_reported() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "invite", quick());
    session.act(&a, Action::Invite);
    session.wait_until("the invitation is reported", || {
        !session.invites().is_empty()
    });
    let (room, link) = session.invites().remove(0);
    assert_eq!(room, bs58::encode(a.vk().to_bytes()).into_string());
    let link = link.unwrap();
    let prefix = format!(
        "http://{}/v1/contract/web/{}/?invitation=",
        node.authority,
        super::super::grant::RIVER_WEB_APP
    );
    let code = link.strip_prefix(&prefix).expect("the web app's link");
    let inv = river_core::invitation::Invitation::from_encoded_string(code).unwrap();
    assert_eq!(inv.room, a.vk());
    inv.invitee.verify_signature(&a.vk()).unwrap();
    // Nothing was written or sent for it.
    node.with(|w| assert!(w.cas.is_empty() && w.updates.is_empty() && w.puts.is_empty()));
}

/// The stored rooms are due to be read again all the time here: creates still finish (a read
/// due never starves them), and their writes never go out while a read is under way.
#[test]
fn creating_while_the_stored_rooms_are_read_often_never_overlaps_a_read() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        refresh: Duration::from_millis(1),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "create-busy", pace);
    for name in ["One", "Two"] {
        session.list(Action::CreateRoom {
            name: name.into(),
            nickname: "Ada".into(),
            private: false,
        });
    }
    session.wait_rooms("both rooms are created", |r, _, _| r.len() == 3);
    node.with(|w| {
        // The first read, and at least one more after the creates. Two creates can finish
        // into one read (the second is still working when the first asks for it), so no more
        // than that is certain.
        assert!(w.lists >= 2, "the rooms were read again: {}", w.lists);
        assert_eq!(w.overlaps, 0);
        assert_eq!(w.puts.len(), 2);
    });
}

/// A join whose UPDATE goes unanswered reconnects, and the next connection shows the room it
/// stored. While the join is still owed an answer, a message written in that room waits: an
/// answer names only the room, so the join's and the message's could not be told apart.
#[test]
fn a_room_being_joined_sends_nothing_else_until_the_join_is_done() {
    let node = fake_node(vec![]);
    let garden = TestRoom::new(3, "Garden", 1, &["t1"]);
    let inviter = LiveRoom::new(garden.stored());
    let (_, link) = super::super::create::invitation(&inviter, "http://x/").unwrap();
    let real = super::super::create::contract_key(&garden.vk());
    node.with(|w| {
        w.contracts
            .insert(*real.id(), (real, Contract::State(cbor(&garden.state))));
        w.update_play = UpdatePlay::Ignore;
    });
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        update_wait: Duration::from_secs(2),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "join-holds", pace);
    session.list(Action::Join {
        code: InviteCode(link),
        nickname: "Bea".into(),
    });
    session.wait_rooms("the stored room shows after a reconnect", |r, _, _| {
        r.iter().any(|r| r.name == "Garden")
    });
    session.act(&garden, say("hi"));
    let is_join = |d: &ChatRoomStateV1Delta| {
        d.recent_messages.as_ref().is_some_and(|m| {
            m.iter()
                .all(|m| m.message.content == RoomMessageBody::join_event())
        })
    };
    session.wait_until("the message goes out once the join gives up", || {
        node.with(|w| w.updates.iter().any(|(k, d)| *k == real && !is_join(d)))
    });
    let kinds: Vec<bool> = node.with(|w| {
        w.updates
            .iter()
            .filter(|(k, _)| *k == real)
            .map(|(_, d)| is_join(d))
            .collect()
    });
    let first_other = kinds.iter().position(|join| !join).unwrap();
    assert!(first_other >= 2, "the join was sent again first: {kinds:?}");
    assert!(
        kinds[first_other..].iter().all(|join| !join),
        "no join after the message: {kinds:?}"
    );
    assert!(
        session
            .last_work()
            .iter()
            .any(|w| matches!(w.state, WorkState::Failed { .. }))
    );
}

/// A read of the stored rooms that never finishes drops the connection: its requests are still
/// out, and an answer or error coming late could be taken for a create's or join's.
#[test]
fn a_read_of_the_stored_rooms_that_takes_too_long_reconnects() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        refresh: Duration::from_millis(50),
        delegate_deadline: Duration::from_secs(2),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "reload-slow", pace);
    node.with(|w| w.silence_next_list = true);
    session.wait_until("a new connection", || {
        node.with(|w| w.connections.len() >= 2)
    });
    session.wait_rooms("the rooms are still shown", |r, _, _| r.len() == 1);
}

/// An invitation asked for in a room that has gone (left in the browser) is said to have failed,
/// so the screen does not wait for it forever; and the report that carries a link never prints it.
#[test]
fn an_invitation_for_a_room_gone_is_said_to_have_failed() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let gone = TestRoom::new(2, "B", 1, &["b1"]);
    let (session, _dir) = following(&node, &[&a], "invite-gone", quick());
    session.act(&gone, Action::Invite);
    session.wait_until("the invitation is said to have failed", || {
        session.invites().iter().any(|(room, link)| {
            *room == bs58::encode(gone.vk().to_bytes()).into_string() && link.is_err()
        })
    });
    let shown = format!(
        "{:?}",
        RoomsUpdate::Invite {
            room: "r".into(),
            link: Ok("http://x/?invitation=SECRETCODE".into()),
        }
    );
    assert!(!shown.contains("SECRETCODE"), "{shown}");
}
