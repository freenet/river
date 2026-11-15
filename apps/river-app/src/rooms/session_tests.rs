use super::*;
use crate::rooms::outbox::InviteCode;
use crate::rooms::outbox::{Action, Outbox};
use crate::rooms::room::message_key;
use crate::rooms::work::WorkState;
use ed25519_dalek::{SigningKey, VerifyingKey};
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

/// How the real handlers fail (`DelegateError::Other` in the delegate, an execution error to
/// the client).
fn delegate_other(why: &str) -> ClientError {
    ErrorKind::RequestError(RequestError::DelegateError(DelegateError::ExecutionError(
        why.to_owned().into(),
    )))
    .into()
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
    // Recorded, so the store is not registered again on every re-read; the list stays.
    assert!(rooms.issues().store_missing);
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
    /// The node has no chat delegate and refuses to register it.
    delegate_missing: bool,
    /// The node has no chat delegate until it is registered (a node River never ran on).
    unregistered: bool,
    /// The bearer token each WebSocket connection presented, by connection.
    conn_bearer: Vec<Option<String>>,
    /// What each connection was sent, in order, by connection: "register", "list", "get",
    /// "versioned", "cas", "contract".
    sent: Vec<(usize, &'static str)>,
    /// Take registrations but answer nothing (the delegate stays unregistered).
    silence_register: bool,
    /// Called for every listing, as it arrives.
    on_list: Option<Box<dyn FnMut() + Send>>,
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
    /// What the chat delegate holds under each room's `signing_key:` secret (by owner key
    /// bytes). Not always 32 bytes, as the real one may hold anything.
    signing_keys: HashMap<[u8; 32], Vec<u8>>,
    /// Every `StoreSigningKey` received: the room, the key, and whether the room's slot was
    /// stored when it arrived.
    key_stores: Vec<([u8; 32], [u8; 32], bool)>,
    /// Every `GetPublicKey` received.
    key_reads: Vec<[u8; 32]>,
    /// How the delegate answers the signing key requests.
    key_play: KeyPlay,
    /// The connection a key request was left unanswered on, until another connection.
    key_open_conn: Option<usize>,
    /// Task requests that arrived on a connection with a key request unanswered.
    overlap_with_key: usize,
    /// When each `StoreSigningKey` arrived.
    key_store_times: Vec<Instant>,
    /// The connection of every key request.
    key_conns: Vec<usize>,
    /// When each `GetPublicKey` arrived.
    key_read_times: Vec<Instant>,
    /// How long the node takes over each CAS store (slow task answers).
    cas_delay: Duration,
    /// Take the next CAS store of a room key, but answer nothing (then clear this).
    silence_first_room_store: bool,
    /// Answer the next listing with nothing (then clear this).
    silence_next_list: bool,
    /// The reads the last listing's reader still has to make.
    reads_due: usize,
    /// Versioned reads and CAS stores that arrived while a reader was still reading.
    overlaps: usize,
}

/// How the fake node answers signing key requests.
#[derive(Clone, Debug, Default)]
struct KeyPlay {
    /// Answer this many requests with an error `HostResponse` first, as the real handlers fail
    /// (`DelegateError::Other`), taking nothing.
    errors: usize,
    /// Answer `StoreSigningKey` with a `StoreSigningKeyResponse` error.
    refuse_store: bool,
    /// Answer nothing.
    silent: bool,
    /// Answer nothing to this many requests, then normally.
    silent_first: usize,
    /// Answer this many requests with `Missing`, as a node does for a delegate it lost.
    missing: usize,
}

impl KeyPlay {
    /// What the node does with a key request on connection `n`: `None` to answer it normally.
    /// An unanswered one marks the connection, so a task request beside it can be told.
    fn arrives(&mut self, open_conn: &mut Option<usize>, n: usize) -> Option<Option<ClientError>> {
        if self.silent || self.silent_first > 0 {
            self.silent_first = self.silent_first.saturating_sub(1);
            *open_conn = Some(n);
            return Some(None);
        }
        if self.missing > 0 {
            self.missing -= 1;
            return Some(Some(missing_delegate()));
        }
        if self.errors > 0 {
            self.errors -= 1;
            return Some(Some(delegate_other("failed")));
        }
        None
    }
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
                    let bearer = head.lines().find_map(|l| {
                        l.to_ascii_lowercase()
                            .starts_with("authorization: bearer ")
                            .then(|| l["authorization: bearer ".len()..].trim().to_owned())
                    });
                    if let Some(bearer) = &bearer {
                        w.bearers.push(bearer.clone());
                    }
                    w.conn_bearer.push(bearer);
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
                let replies = answer(world, n, &b);
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

fn answer(world: &Mutex<World>, n: usize, bytes: &[u8]) -> Vec<Vec<u8>> {
    let request: ClientRequest = bincode::deserialize(bytes).unwrap();
    let mut w = world.lock().unwrap();
    let mut out = Vec::new();
    let encode = |r: Result<HostResponse, ClientError>| bincode::serialize(&r).unwrap();
    if matches!(request, ClientRequest::ContractOp(_)) {
        w.sent.push((n, "contract"));
    }
    match request {
        // The node's answer to a registration: an empty response for the delegate's key.
        ClientRequest::DelegateOp(DelegateRequest::RegisterDelegate { delegate, .. }) => {
            w.sent.push((n, "register"));
            assert_eq!(delegate.key(), delegate::chat_delegate_key());
            if w.silence_register {
                // No answer, and nothing registered.
            } else if w.delegate_missing {
                out.push(encode(Err(ErrorKind::RequestError(
                    RequestError::DelegateError(DelegateError::RegisterError(
                        delegate.key().clone(),
                    )),
                )
                .into())));
            } else {
                w.unregistered = false;
                out.push(encode(Ok(HostResponse::DelegateResponse {
                    key: delegate.key().clone(),
                    values: vec![],
                })));
            }
        }
        ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { key, .. })
            if &key != delegate::chat_delegate_key() =>
        {
            panic!("the app only asks River's own chat delegate, but asked {key}");
        }
        ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { inbound, .. }) => {
            for msg in inbound {
                let InboundDelegateMsg::ApplicationMessage(m) = msg else {
                    continue;
                };
                let read: ChatDelegateRequestMsg =
                    ciborium::de::from_reader(m.payload.as_slice()).unwrap();
                w.sent.push((
                    n,
                    match &read {
                        ChatDelegateRequestMsg::ListRequest => "list",
                        ChatDelegateRequestMsg::GetRequest { .. } => "get",
                        ChatDelegateRequestMsg::GetVersionedRequest { .. } => "versioned",
                        ChatDelegateRequestMsg::CasStoreRequest { .. } => "cas",
                        _ => "other",
                    },
                ));
                match &read {
                    ChatDelegateRequestMsg::ListRequest => {
                        w.lists += 1;
                        if let Some(f) = &mut w.on_list {
                            f();
                        }
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
                if matches!(
                    read,
                    ChatDelegateRequestMsg::GetVersionedRequest { .. }
                        | ChatDelegateRequestMsg::CasStoreRequest { .. }
                        | ChatDelegateRequestMsg::ListRequest
                        | ChatDelegateRequestMsg::GetRequest { .. }
                ) && w.key_open_conn == Some(n)
                {
                    w.overlap_with_key += 1;
                }
                if w.delegate_missing || w.unregistered {
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
                        std::thread::sleep(w.cas_delay);
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
                    // The chat delegate's `handle_get_public_key`: a stored value that is not 32
                    // bytes reads as no key.
                    ChatDelegateRequestMsg::GetPublicKey { room_key } => {
                        w.key_reads.push(room_key);
                        w.key_read_times.push(Instant::now());
                        w.key_conns.push(n);
                        let World {
                            key_play,
                            key_open_conn,
                            ..
                        } = &mut *w;
                        match key_play.arrives(key_open_conn, n) {
                            Some(None) => continue,
                            Some(Some(e)) => {
                                out.push(encode(Err(e)));
                                continue;
                            }
                            None => {}
                        }
                        let public_key = w
                            .signing_keys
                            .get(&room_key)
                            .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                            .map(|sk| SigningKey::from_bytes(&sk).verifying_key().to_bytes());
                        ChatDelegateResponseMsg::GetPublicKeyResponse {
                            room_key,
                            public_key,
                        }
                    }
                    // `handle_store_signing_key`: stores over whatever is there.
                    ChatDelegateRequestMsg::StoreSigningKey {
                        room_key,
                        signing_key_bytes,
                    } => {
                        let slot_stored = VerifyingKey::from_bytes(&room_key)
                            .is_ok_and(|vk| w.slots.contains_key(&room_storage_key(&vk)));
                        w.key_stores
                            .push((room_key, signing_key_bytes, slot_stored));
                        w.key_store_times.push(Instant::now());
                        w.key_conns.push(n);
                        let World {
                            key_play,
                            key_open_conn,
                            ..
                        } = &mut *w;
                        match key_play.arrives(key_open_conn, n) {
                            Some(None) => continue,
                            Some(Some(e)) => {
                                out.push(encode(Err(e)));
                                continue;
                            }
                            None => {}
                        }
                        if w.key_play.refuse_store {
                            ChatDelegateResponseMsg::StoreSigningKeyResponse {
                                room_key,
                                result: Err("refused".into()),
                            }
                        } else {
                            w.signing_keys.insert(room_key, signing_key_bytes.to_vec());
                            ChatDelegateResponseMsg::StoreSigningKeyResponse {
                                room_key,
                                result: Ok(()),
                            }
                        }
                    }
                    other => panic!("the app sent a request it must not: {other:?}"),
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
        register_wait: Duration::from_secs(5),
        missing_lapse: Duration::ZERO,
        register_retry_first: Duration::from_secs(60),
        register_retry_max: Duration::from_secs(60),
        key_retry_first: Duration::from_millis(30),
        key_retry_max: Duration::from_millis(120),
        key_delay: Duration::from_millis(200),
        key_owed_cap: Duration::from_secs(60),
        grant: GrantPacing {
            pause: Duration::ZERO,
            margin: Duration::ZERO,
            outage: Duration::from_secs(60),
        },
        pointers: CheckPace {
            first_wait: Duration::from_secs(5),
            fetch_wait: Duration::from_secs(1),
            recheck: Duration::from_millis(100),
            retry_first: Duration::from_millis(20),
            first_retry: Duration::from_millis(10),
        },
    }
}

/// A watch with nothing to watch: never holds or stops a write. The session tests that are
/// not about a newer River use it, so they run as before.
fn no_watch() -> Watch {
    Watch::new(
        Vec::new(),
        Arc::new(crate::rooms::generation::NodePointers),
        None,
    )
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

    /// Start with `watch` deciding whether River has moved on.
    fn start_watched(node: &FakeNode, dir: &Path, pace: Pace, watch: Arc<Watch>) -> Running {
        Running::start_full(
            node,
            dir,
            pace,
            Arc::default(),
            std::sync::mpsc::channel(),
            watch,
        )
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
        Running::start_full(
            node,
            dir,
            pace,
            outbox,
            (commands, inbox),
            Arc::new(no_watch()),
        )
    }

    fn start_full(
        node: &FakeNode,
        dir: &Path,
        pace: Pace,
        outbox: Arc<Mutex<Outbox>>,
        (commands, inbox): (
            std::sync::mpsc::Sender<Command>,
            std::sync::mpsc::Receiver<Command>,
        ),
        watch: Arc<Watch>,
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
                    &watch,
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
            &no_watch(),
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

// Signing keys -----------------------------------------------------------------------------------

/// Create "Book club" in a session on `node` that already follows one other room.
fn create_and_wait(node: &FakeNode, label: &str) -> (Running, PathBuf) {
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, dir) = following(node, &[&a], label, quick());
    session.list(Action::CreateRoom {
        name: "Book club".into(),
        nickname: "Ada".into(),
        private: false,
    });
    session.wait_rooms("the new room is in the list", |r, _, _| {
        r.iter().any(|r| r.name == "Book club")
    });
    (session, dir)
}

/// The room "A" of `create_and_wait` (owner seed 1) holds its own key from the start; the sweep
/// stores it too, so a test about the created room looks at its key only.
fn key_stores_besides(w: &World, owner: &VerifyingKey) -> Vec<([u8; 32], [u8; 32], bool)> {
    w.key_stores
        .iter()
        .filter(|(room, _, _)| *room != owner.to_bytes())
        .cloned()
        .collect()
}

/// A created room's key reaches the delegate once, as the room's own `self_sk` under the owner's
/// key, after the room was stored. The pre-existing room's key is stored as well.
#[test]
fn a_created_rooms_signing_key_is_stored_once_under_the_owner_key() {
    let node = fake_node(vec![]);
    let (session, _dir) = create_and_wait(&node, "key-create");
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    session.wait_until("both keys are in the delegate", || {
        node.with(|w| w.signing_keys.len() == 2)
    });
    // Let any extra request show itself.
    std::thread::sleep(Duration::from_millis(300));
    node.with(|w| {
        let created = key_stores_besides(w, &a.vk());
        assert_eq!(created.len(), 1, "{created:?}");
        let (room, key, slot_stored) = created[0];
        assert!(slot_stored, "the room was stored before its key");
        // A created room's member is its owner: the key is the room's own `self_sk`.
        let stored = w
            .slots
            .iter()
            .filter_map(|(k, v)| parse_room_storage_key(k).map(|vk| (vk, v)))
            .find(|(vk, _)| vk.to_bytes() == room)
            .map(|(_, v)| match decode_room_slot(v).unwrap() {
                StoredRoomSlot::Present(r) => *r,
                StoredRoomSlot::Tombstone => panic!("stored"),
            })
            .unwrap();
        assert_eq!(stored.self_sk.as_ref().unwrap().to_bytes(), key);
        assert_eq!(w.signing_keys[&room], key.to_vec());
        // One store each for the two rooms, never more.
        assert_eq!(w.key_stores.len(), 2);
    });
}

/// A join's key is the invitee's own key, under the room owner's key.
#[test]
fn a_joined_rooms_signing_key_is_the_members_under_the_owner_key() {
    let node = fake_node(vec![]);
    let room = TestRoom::new(3, "Garden", 1, &["t1"]);
    let inviter = LiveRoom::new(room.stored());
    let (inv, link) = super::super::create::invitation(&inviter, "http://x/").unwrap();
    let real = super::super::create::contract_key(&room.vk());
    node.with(|w| {
        w.contracts
            .insert(*real.id(), (real, Contract::State(cbor(&room.state))));
    });
    let other = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&other], "key-join", quick());
    session.list(Action::Join {
        code: InviteCode(link),
        nickname: "Bea".into(),
    });
    session.wait_rooms("the joined room is in the list", |r, _, _| {
        r.iter().any(|r| r.name == "Garden")
    });
    session.wait_until("the joined room's key is in the delegate", || {
        node.with(|w| w.signing_keys.contains_key(&room.vk().to_bytes()))
    });
    std::thread::sleep(Duration::from_millis(300));
    node.with(|w| {
        let joined = key_stores_besides(w, &other.vk());
        assert_eq!(
            joined,
            [(
                room.vk().to_bytes(),
                inv.invitee_signing_key.to_bytes(),
                true
            )]
        );
        assert_ne!(inv.invitee_signing_key.verifying_key(), room.vk());
    });
}

/// A delegate that never answers about keys does not hold up a create: the sweep does not start
/// until its settle time after the read, which is far longer than the create takes here, so the
/// room is stored, shown and its task done with no key request yet made, on the one connection.
/// (A key step inside the task, or a sweep that starts at once, would be seen as a key request
/// before the room shows, or a create that waits out the deadline.)
#[test]
fn a_silent_delegate_does_not_delay_a_create() {
    let node = fake_node(vec![]);
    node.with(|w| w.key_play.silent = true);
    let pace = Pace {
        key_delay: Duration::from_secs(30),
        ..quick()
    };
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "key-silent", pace);
    let started = Instant::now();
    session.list(Action::CreateRoom {
        name: "Book club".into(),
        nickname: "Ada".into(),
        private: false,
    });
    session.wait_rooms("the new room is in the list", |r, _, _| {
        r.iter().any(|r| r.name == "Book club")
    });
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "took {:?}, the delegate deadline is 5 s",
        started.elapsed()
    );
    assert!(session.updates().iter().any(|u| matches!(
        u,
        RoomsUpdate::Rooms { work, .. }
            if work.iter().any(|w| matches!(w.state, WorkState::Done { .. }))
    )));
    node.with(|w| {
        assert!(
            w.key_reads.is_empty(),
            "a key request came before the room showed"
        );
        assert_eq!(w.connections.len(), 1);
        assert_eq!(w.puts.len(), 1);
    });
}

/// A create pressed while a key request is out and unanswered waits only for that one request
/// (the deadline, then a new connection), never sends beside it, and then finishes; both keys
/// are stored after.
#[test]
fn a_create_beside_a_key_request_waits_for_it_and_never_overlaps_it() {
    let node = fake_node(vec![]);
    node.with(|w| w.key_play.silent_first = 1);
    let pace = Pace {
        delegate_deadline: Duration::from_millis(400),
        ..quick()
    };
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "key-beside", pace);
    session.wait_until("the key request is out", || {
        node.with(|w| !w.key_reads.is_empty())
    });
    session.list(Action::CreateRoom {
        name: "Book club".into(),
        nickname: "Ada".into(),
        private: false,
    });
    session.wait_rooms("the new room is in the list", |r, _, _| {
        r.iter().any(|r| r.name == "Book club")
    });
    session.wait_until("both keys are stored", || {
        node.with(|w| w.signing_keys.len() == 2)
    });
    node.with(|w| {
        assert_eq!(
            w.overlap_with_key, 0,
            "a request went beside the key request"
        );
        assert_eq!(
            w.connections.len(),
            2,
            "one reconnect, for the unanswered request"
        );
        assert_eq!(w.overlaps, 0);
    });
}

/// A `Missing` answer to a key request is a failure of the sweep and goes the way a read's does:
/// the store is registered again on a new connection, and the keys follow.
#[test]
fn a_missing_answer_to_a_key_request_registers_the_store_again() {
    let node = fake_node(vec![]);
    node.with(|w| w.key_play.missing = 1);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "key-missing", quick());
    session.wait_until("the key is stored after registering again", || {
        node.with(|w| w.signing_keys.len() == 1)
    });
    node.with(|w| {
        assert_eq!(w.connections.len(), 2);
        assert!(w.sent.contains(&(1, "register")), "{:?}", w.sent);
        // The failed ask, then the check, then the check after the store.
        assert_eq!(w.key_reads.len(), 3);
    });
}

/// A connection that drops with a key request out is followed by a new one on which the key is
/// asked about again and stored.
#[test]
fn a_dropped_connection_with_a_key_request_out_is_followed_by_a_store() {
    let node = fake_node(vec![]);
    node.with(|w| w.key_play.silent_first = 1);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "key-drop", quick());
    session.wait_until("the key request is out", || {
        node.with(|w| !w.key_reads.is_empty())
    });
    node.with(|w| w.drop_live = true);
    session.wait_until("the key is stored", || {
        node.with(|w| w.signing_keys.len() == 1)
    });
    assert!(node.with(|w| w.connections.len() >= 2));
}

/// The real handlers fail with an error `HostResponse`: the room is unaffected, nothing is
/// shown, and a later try stores the key, on the same connection.
#[test]
fn a_failed_key_request_is_tried_again_later_and_the_room_is_unaffected() {
    let node = fake_node(vec![]);
    node.with(|w| w.key_play.errors = 3);
    let (session, _dir) = create_and_wait(&node, "key-error");
    session.wait_until("every key is stored after the failures", || {
        node.with(|w| w.signing_keys.len() == 2)
    });
    node.with(|w| {
        assert_eq!(w.key_play.errors, 0, "each failure was taken");
        assert_eq!(w.puts.len(), 1);
        assert_eq!(w.key_stores.len(), 2);
        assert_eq!(w.connections.len(), 1);
    });
}

/// A store the delegate refuses is asked again after waits that double: the gaps between the
/// stores are each at least the wait the schedule gives (30, 60, 120 ms here).
#[test]
fn a_refused_store_is_retried_with_a_doubling_wait() {
    let node = fake_node(vec![]);
    node.with(|w| w.key_play.refuse_store = true);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "key-refuse", quick());
    session.wait_until("four stores", || node.with(|w| w.key_stores.len() >= 4));
    node.with(|w| {
        let gaps: Vec<Duration> = w
            .key_store_times
            .windows(2)
            .map(|p| p[1].duration_since(p[0]))
            .collect();
        for (gap, wait) in gaps.iter().zip([30, 60, 120]) {
            assert!(
                *gap >= Duration::from_millis(wait),
                "gaps {gaps:?}, want at least {wait} ms"
            );
        }
        assert!(w.signing_keys.is_empty());
    });
}

/// The sweep waits for the connect-time subscribes: a room whose contract the node never
/// answers keeps one pending until the catch-up time, and no key request goes out before.
#[test]
fn the_sweep_waits_for_pending_subscribes() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    // The room is stored, but the node has no answer for its contract.
    node.with(|w| {
        w.slots.insert(room_storage_key(&a.vk()), a.slot());
    });
    let pace = Pace {
        catch_up: Duration::from_millis(1500),
        key_delay: Duration::ZERO,
        ..quick()
    };
    let dir = temp_dir("key-subs");
    save_token(&dir, "kept-token");
    let session = Running::start(&node, &dir, pace);
    session.wait_rooms("the room is shown", |r, _, _| r.len() == 1);
    std::thread::sleep(Duration::from_millis(600));
    assert!(
        node.with(|w| w.key_reads.is_empty()),
        "asked while a subscribe was pending"
    );
    session.wait_until("asked after the catch-up time", || {
        node.with(|w| !w.key_reads.is_empty())
    });
}

/// The sweep waits for an owed update, but only for `key_owed_cap`: an update the node never
/// answers does not close the gate for good.
#[test]
fn the_sweep_waits_while_an_update_is_owed_but_only_up_to_a_cap() {
    let node = fake_node(vec![]);
    node.with(|w| w.update_play = UpdatePlay::Ignore);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        key_delay: Duration::from_millis(600),
        key_owed_cap: Duration::from_millis(1500),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "key-owed", pace);
    session.act(&a, say("hello"));
    let sent = Instant::now();
    session.wait_until("the update reached the node", || {
        node.with(|w| !w.updates.is_empty())
    });
    // Past the settle time, the update is still owed: nothing asked.
    std::thread::sleep(Duration::from_millis(1000));
    assert!(
        node.with(|w| w.key_reads.is_empty()),
        "asked while an update was owed"
    );
    // After the cap it is taken to be lost and the sweep goes on.
    session.wait_until("asked after the cap", || {
        node.with(|w| !w.key_reads.is_empty())
    });
    assert!(
        sent.elapsed() >= Duration::from_millis(1450),
        "{:?}",
        sent.elapsed()
    );
}

/// A key request goes only on a connection that presented the grant token.
#[test]
fn key_requests_go_only_on_connections_with_the_grant_token() {
    let node = fake_node(vec![]);
    let (session, _dir) = create_and_wait(&node, "key-grant");
    session.wait_until("the keys are stored", || {
        node.with(|w| w.signing_keys.len() == 2)
    });
    node.with(|w| {
        assert!(!w.key_conns.is_empty());
        for n in &w.key_conns {
            assert_eq!(w.conn_bearer[*n].as_deref(), Some("kept-token"));
        }
    });
}

/// A room that never answers its subscribe is asked again and again, and is pending most of the
/// time; that must not keep the sweep from storing the keys of the other rooms.
#[test]
fn a_room_that_never_answers_does_not_starve_the_sweep() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let dead = TestRoom::new(2, "Dead", 1, &["d1"]);
    node.with(|w| {
        w.add_room(&a);
        // Stored, but the node never answers for its contract (nor holds a key for it).
        w.slots.insert(room_storage_key(&dead.vk()), dead.slot());
    });
    let pace = Pace {
        catch_up: Duration::from_millis(300),
        // Asked again at once, so one is always pending.
        resubscribe_first: Duration::ZERO,
        resubscribe_max: Duration::ZERO,
        key_delay: Duration::from_millis(50),
        ..quick()
    };
    let dir = temp_dir("key-starve");
    save_token(&dir, "kept-token");
    let session = Running::start(&node, &dir, pace);
    session.wait_until(
        "both keys are stored although one room never answers",
        || {
            node.with(|w| {
                w.signing_keys.contains_key(&a.vk().to_bytes())
                    && w.signing_keys.contains_key(&dead.vk().to_bytes())
            })
        },
    );
    // It really was asked again, and is still asked.
    session.wait_until("the dead room is asked again", || {
        node.with(|w| w.subscribes_to(dead.id()) >= 3)
    });
}

/// After a create finishes, the read that shows the new room comes before any key request, even
/// when another room's key is due and the settle time has long passed. (Both guards matter: the
/// gate's "read pending" and the sweep coming after the read's start in the loop.)
#[test]
fn the_read_after_a_create_comes_before_a_key_request() {
    let node = fake_node(vec![]);
    // The first key request fails, so room B is unverified and held back for 700 ms.
    node.with(|w| {
        w.key_play.errors = 1;
        w.cas_delay = Duration::from_millis(400);
    });
    let b = TestRoom::new(1, "B", 1, &["b1"]);
    let pace = Pace {
        key_delay: Duration::from_millis(50),
        key_retry_first: Duration::from_millis(700),
        key_retry_max: Duration::from_millis(700),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&b], "key-order", pace);
    session.wait_until("B's first key request failed", || {
        node.with(|w| w.key_reads.len() == 1 && w.key_play.errors == 0)
    });
    // A create whose answers are slow: it is done after B's wait is over.
    session.list(Action::CreateRoom {
        name: "Book club".into(),
        nickname: "Ada".into(),
        private: false,
    });
    session.wait_rooms("the new room is in the list", |r, _, _| {
        r.iter().any(|r| r.name == "Book club")
    });
    node.with(|w| {
        let last_cas = w.sent.iter().rposition(|(_, k)| *k == "cas").unwrap();
        let after = &w.sent[last_cas + 1..];
        let list = after.iter().position(|(_, k)| *k == "list");
        let key = after.iter().position(|(_, k)| *k == "other");
        assert!(list.is_some(), "no read after the create: {after:?}");
        assert!(
            key.is_none_or(|k| list.unwrap() < k),
            "a key request came before the read: {after:?}"
        );
    });
}

/// After a connection drops with a key request out, the room is asked about again only after the
/// retry wait, on the new connection: the drop counted as a failure.
#[test]
fn a_dropped_connection_with_a_key_request_out_waits_before_the_next_ask() {
    let node = fake_node(vec![]);
    node.with(|w| w.key_play.silent_first = 1);
    let pace = Pace {
        key_delay: Duration::from_millis(50),
        key_retry_first: Duration::from_millis(600),
        key_retry_max: Duration::from_millis(600),
        ..quick()
    };
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "key-drop-gap", pace);
    session.wait_until("the key request is out", || {
        node.with(|w| !w.key_reads.is_empty())
    });
    node.with(|w| w.drop_live = true);
    session.wait_until("the key is stored", || {
        node.with(|w| w.signing_keys.len() == 1)
    });
    node.with(|w| {
        assert!(w.connections.len() >= 2);
        let gap = w.key_read_times[1].duration_since(w.key_read_times[0]);
        assert!(
            gap >= Duration::from_millis(600),
            "asked again after {gap:?}"
        );
        // The drop counted as the failure at once; it did not wait for the 5 s deadline.
        assert!(gap < Duration::from_secs(3), "asked again after {gap:?}");
    });
}

/// After a `Missing`, the next ask waits out the retry wait, though the store is registered
/// again at once.
#[test]
fn a_missing_answer_to_a_key_request_waits_before_the_next_ask() {
    let node = fake_node(vec![]);
    node.with(|w| w.key_play.missing = 1);
    let pace = Pace {
        key_delay: Duration::from_millis(50),
        key_retry_first: Duration::from_millis(600),
        key_retry_max: Duration::from_millis(600),
        ..quick()
    };
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "key-missing-gap", pace);
    session.wait_until("the key is stored", || {
        node.with(|w| w.signing_keys.len() == 1)
    });
    node.with(|w| {
        let gap = w.key_read_times[1].duration_since(w.key_read_times[0]);
        assert!(
            gap >= Duration::from_millis(600),
            "asked again after {gap:?}"
        );
    });
}

/// Each thing the sweep waits for closes the gate on its own.
#[test]
fn the_sweep_gate_is_closed_by_each_thing_it_waits_for() {
    let open = SweepGate {
        task_working: false,
        read_out: false,
        read_pending_or_due: false,
        subscribes_pending: false,
        update_owed: false,
        complete_read: true,
        settled: true,
        writes: true,
    };
    assert!(open.open());
    assert!(
        !SweepGate {
            writes: false,
            ..open
        }
        .open()
    );
    assert!(
        !SweepGate {
            task_working: true,
            ..open
        }
        .open()
    );
    assert!(
        !SweepGate {
            read_out: true,
            ..open
        }
        .open()
    );
    assert!(
        !SweepGate {
            read_pending_or_due: true,
            ..open
        }
        .open()
    );
    assert!(
        !SweepGate {
            subscribes_pending: true,
            ..open
        }
        .open()
    );
    assert!(
        !SweepGate {
            update_owed: true,
            ..open
        }
        .open()
    );
    assert!(
        !SweepGate {
            complete_read: false,
            ..open
        }
        .open()
    );
    assert!(
        !SweepGate {
            settled: false,
            ..open
        }
        .open()
    );
}

/// A key the delegate already holds for a room, different from ours, is left alone, and the
/// delegate is not asked about that room again on every load.
#[test]
fn a_different_key_in_the_delegate_is_left_alone_and_not_asked_about_again() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| {
        w.signing_keys.insert(a.vk().to_bytes(), vec![99u8; 32]);
    });
    let pace = Pace {
        refresh: Duration::from_millis(400),
        key_delay: Duration::from_millis(50),
        ..quick()
    };
    node.with(|w| w.add_room(&a));
    let dir = temp_dir("key-mismatch");
    save_token(&dir, "kept-token");
    let session = Running::start(&node, &dir, pace);
    session.wait_rooms("the room is live", |r, _, s| {
        r.len() == 1 && s == LinkStatus::Live
    });
    let lists = node.with(|w| w.lists);
    session.wait_until("several more loads", || node.with(|w| w.lists >= lists + 3));
    node.with(|w| {
        assert!(w.key_stores.is_empty());
        assert_eq!(w.signing_keys[&a.vk().to_bytes()], vec![99u8; 32]);
        assert_eq!(w.key_reads.len(), 1, "asked once, not on every load");
    });
}

/// A room the app already had, whose key the delegate lacks (or holds as something that is not
/// a key), gets its key stored, with no create or join.
#[test]
fn a_room_from_before_gets_its_key_stored() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    node.with(|w| {
        // Not 32 bytes: the real delegate reads it as no key.
        w.signing_keys.insert(b.vk().to_bytes(), vec![1, 2, 3]);
    });
    let (session, _dir) = following(&node, &[&a, &b], "key-before", quick());
    session.wait_until("both keys are stored", || {
        node.with(|w| {
            w.signing_keys.get(&a.vk().to_bytes()) == Some(&a.owner.to_bytes().to_vec())
                && w.signing_keys.get(&b.vk().to_bytes()) == Some(&b.owner.to_bytes().to_vec())
        })
    });
    // A key that is there and ours is not stored again, on this or any later load.
    node.with(|w| assert_eq!(w.key_stores.len(), 2));
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

// Setting up River's storage --------------------------------------------------------------------

/// Every registration went out on a connection that presented the grant token: the node records
/// the first registration's app identity for good, and a connection without the token has none
/// (socket.rs).
fn assert_registers_only_with_the_grant(w: &World) {
    for (n, kind) in &w.sent {
        if *kind == "register" {
            assert!(
                w.conn_bearer[*n].as_deref().is_some_and(|t| !t.is_empty()),
                "connection {n} registered without the grant token"
            );
        }
    }
}

/// What connection `n` sent, in order.
fn sent_on(w: &World, n: usize) -> Vec<&'static str> {
    w.sent
        .iter()
        .filter(|(c, _)| *c == n)
        .map(|(_, k)| *k)
        .collect()
}

#[test]
fn a_node_river_never_ran_on_gets_the_store_registered_then_read() {
    let node = fake_node(vec![]);
    node.with(|w| w.unregistered = true);
    let dir = temp_dir("fresh-register");
    // No saved token: the grant is asked for, approved, then used.
    let session = Running::start(&node, &dir, quick());
    session.wait_rooms("an empty list with nothing missing", |r, issues, s| {
        r.is_empty() && !issues.store_missing && s == LinkStatus::Live
    });
    node.with(|w| {
        assert!(!w.unregistered);
        assert_registers_only_with_the_grant(w);
        assert_eq!(w.conn_bearer[0].as_deref(), Some("fresh-token"));
        // Listed, found missing, registered, listed again.
        assert_eq!(sent_on(w, 0)[..3], ["list", "register", "list"]);
    });
}

#[test]
fn a_node_that_has_the_store_is_not_sent_a_registration() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (_session, _dir) = following(&node, &[&a], "registered-already", quick());
    node.with(|w| {
        assert!(
            !w.sent.iter().any(|(_, k)| *k == "register"),
            "{:?}",
            w.sent
        );
    });
}

#[test]
fn a_room_created_on_a_fresh_node_is_stored_and_shown() {
    let node = fake_node(vec![]);
    node.with(|w| w.unregistered = true);
    let dir = temp_dir("fresh-create");
    let session = Running::start(&node, &dir, quick());
    // Sent before the rooms are first read: it waits for them, then goes into the new store.
    session.list(Action::CreateRoom {
        name: "Book club".into(),
        nickname: "Ada".into(),
        private: false,
    });
    session.wait_rooms("the room is created", |r, _, _| {
        r.iter().any(|r| r.name == "Book club")
    });
    node.with(|w| {
        assert_eq!(w.puts.len(), 1);
        assert_registers_only_with_the_grant(w);
        let first_cas = w.sent.iter().position(|(_, k)| *k == "cas").unwrap();
        let register = w.sent.iter().position(|(_, k)| *k == "register").unwrap();
        assert!(register < first_cas, "stored before the store was set up");
    });
}

#[test]
fn a_store_that_cannot_be_registered_is_said() {
    let node = fake_node(vec![]);
    node.with(|w| w.delegate_missing = true);
    let dir = temp_dir("register-refused");
    let session = Running::start(&node, &dir, quick());
    session.wait_rooms("the store is said to be missing", |r, issues, _| {
        r.is_empty() && issues.store_missing
    });
    node.with(|w| assert_eq!(sent_on(w, 0)[..3], ["list", "register", "list"]));
}

#[test]
fn an_unanswered_registration_is_waited_out_and_the_rooms_read_anyway() {
    let node = fake_node(vec![]);
    node.with(|w| {
        w.unregistered = true;
        w.silence_register = true;
    });
    let dir = temp_dir("register-silent");
    let pace = Pace {
        register_wait: Duration::from_millis(300),
        ..quick()
    };
    let started = Instant::now();
    let session = Running::start(&node, &dir, pace);
    session.wait_rooms("the store is said to be missing", |r, issues, _| {
        r.is_empty() && issues.store_missing
    });
    // It waited for the answer before reading again.
    assert!(started.elapsed() >= Duration::from_millis(300));
    node.with(|w| assert_eq!(sent_on(w, 0)[..3], ["list", "register", "list"]));
}

#[test]
fn a_store_that_goes_missing_is_registered_first_on_a_new_connection() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        refresh: Duration::from_millis(50),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "store-gone", pace);
    let before = node.with(|w| w.connections.len());
    // The node loses the delegate (its data was reset); the stored rooms come back with it.
    node.with(|w| w.unregistered = true);
    session.wait_until("a new connection registered", || {
        node.with(|w| {
            w.connections.len() > before && !w.unregistered && sent_on(w, before).len() >= 2
        })
    });
    node.with(|w| {
        // The new connection registers before anything else, with the grant.
        assert_eq!(sent_on(w, before)[..2], ["register", "list"]);
        assert_registers_only_with_the_grant(w);
    });
    session.wait_rooms("the room is shown, nothing missing", |r, issues, _| {
        names(r) == ["A"] && !issues.store_missing
    });
}

/// A tripwire, not a proof: `RegisterDelegate` is built in one place in this app's production
/// code, the grant socket. It catches a second builder written the usual way; a request
/// smuggled in some other form would pass it. What enforces the invariant is
/// `NodeSocket::send` refusing the request and `connect` requiring a grant token.
#[test]
fn only_the_grant_socket_registers_the_delegate() {
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
    let mut found = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(src)];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.ends_with(".rs") || name.ends_with("_tests.rs") || name == "tests.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // Production code only: stop at the file's test module.
            let code = text.split("#[cfg(test)]\nmod tests").next().unwrap();
            if code.contains("RegisterDelegate {") {
                found.push(name);
            }
        }
    }
    assert_eq!(found, ["socket.rs"]);
}

/// The read after registering waits out the backoff the `Missing` answer armed on the node, so
/// the node does not refuse it as rate limited.
#[test]
fn the_read_after_registering_waits_out_the_missing_backoff() {
    let node = fake_node(vec![]);
    node.with(|w| w.unregistered = true);
    let dir = temp_dir("missing-lapse");
    let pace = Pace {
        missing_lapse: Duration::from_millis(400),
        ..quick()
    };
    let lists = Arc::new(Mutex::new(Vec::<Instant>::new()));
    {
        let lists = lists.clone();
        node.with(|w| {
            w.on_list = Some(Box::new(move || lists.lock().unwrap().push(Instant::now())))
        });
    }
    let session = Running::start(&node, &dir, pace);
    session.wait_rooms("live", |r, issues, s| {
        r.is_empty() && !issues.store_missing && s == LinkStatus::Live
    });
    let lists = lists.lock().unwrap();
    assert!(lists.len() >= 2);
    assert!(
        lists[1] - lists[0] >= Duration::from_millis(400),
        "read again {:?} after the Missing",
        lists[1] - lists[0]
    );
}

fn registrations(w: &World) -> usize {
    w.sent.iter().filter(|(_, k)| *k == "register").count()
}

/// A node that refuses the registration, or never answers it, is not asked again on every
/// re-read of the stored rooms; the app's own retries wait `register_retry_first` and longer (each one re-sends the whole WASM, and an
/// unanswered one waits out `register_wait` each time).
#[test]
fn a_refused_or_unanswered_registration_is_not_repeated_on_every_reread() {
    for silent in [false, true] {
        let node = fake_node(vec![]);
        node.with(|w| {
            w.unregistered = true;
            w.delegate_missing = !silent;
            w.silence_register = silent;
        });
        let dir = temp_dir(if silent {
            "register-silent-loop"
        } else {
            "register-refused-loop"
        });
        let pace = Pace {
            refresh: Duration::from_millis(30),
            register_wait: Duration::from_millis(100),
            ..quick()
        };
        let session = Running::start(&node, &dir, pace);
        session.wait_rooms("the store is said to be missing", |r, issues, _| {
            r.is_empty() && issues.store_missing
        });
        // Several refresh intervals' worth of re-reads.
        let lists = node.with(|w| w.lists);
        session.wait_until("eight more reads", || node.with(|w| w.lists >= lists + 8));
        node.with(|w| {
            assert_eq!(registrations(w), 1, "silent={silent}: {:?}", w.sent);
        });
        let (_, issues, _) = session.last_rooms().unwrap();
        assert!(issues.store_missing);
    }
}

/// A create or join the user asks for while the store is missing is a reason to try setting it
/// up again: it reconnects, and the new connection registers first. The user decides how often.
#[test]
fn a_create_while_the_store_is_missing_tries_to_set_it_up_again() {
    let node = fake_node(vec![]);
    node.with(|w| {
        w.unregistered = true;
        w.delegate_missing = true;
    });
    let dir = temp_dir("missing-then-create");
    let session = Running::start(&node, &dir, quick());
    session.wait_rooms("the store is said to be missing", |r, issues, _| {
        r.is_empty() && issues.store_missing
    });
    // The node now takes registrations.
    node.with(|w| w.delegate_missing = false);
    session.list(Action::CreateRoom {
        name: "Book club".into(),
        nickname: "Ada".into(),
        private: false,
    });
    session.wait_until("the create failed and the store was set up", || {
        node.with(|w| registrations(w) == 2 && !w.unregistered)
    });
    let id = session.last_work()[0].id;
    session.list(Action::RetryWork { id });
    session.wait_rooms("the room is created", |r, issues, _| {
        r.iter().any(|r| r.name == "Book club") && !issues.store_missing
    });
}

/// A store that was there, goes missing, and then cannot be registered again: one attempt on
/// the next connection, not one per re-read. The rooms stay shown.
#[test]
fn a_store_that_goes_missing_and_cannot_be_set_up_again_is_tried_once() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        refresh: Duration::from_millis(30),
        register_wait: Duration::from_millis(100),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "gone-refused", pace);
    node.with(|w| w.delegate_missing = true);
    session.wait_until("one registration was tried", || {
        node.with(|w| registrations(w) == 1)
    });
    let lists = node.with(|w| w.lists);
    session.wait_until("eight more reads", || node.with(|w| w.lists >= lists + 8));
    node.with(|w| assert_eq!(registrations(w), 1, "{:?}", w.sent));
    let (rooms, _, _) = session.last_rooms().unwrap();
    assert_eq!(names(&rooms), ["A"]);
}

#[test]
fn only_the_chat_delegate_missing_counts_as_the_store_missing() {
    let other = freenet_stdlib::prelude::Delegate::from((
        &freenet_stdlib::prelude::DelegateCode::from(vec![1, 2, 3]),
        &Parameters::from(Vec::<u8>::new()),
    ))
    .key()
    .clone();
    let theirs: ClientError = ErrorKind::RequestError(RequestError::DelegateError(
        DelegateError::Missing(other.clone()),
    ))
    .into();
    assert!(delegate_error(&theirs).is_err());
    assert!(!chat_delegate_missing(&theirs));
    assert!(names_another_delegate(&theirs));
    assert!(!names_another_delegate(&missing_delegate()));
    let their_register: ClientError = ErrorKind::RequestError(RequestError::DelegateError(
        DelegateError::RegisterError(other),
    ))
    .into();
    assert!(names_another_delegate(&their_register));
}

/// A node that refused the registration at first and takes it later: the app sets the store
/// up by itself on its retry backoff, without the user doing anything, and the registrations
/// stay far fewer than the re-reads.
#[test]
fn a_store_refused_at_first_is_set_up_later_by_itself_on_a_backoff() {
    let node = fake_node(vec![]);
    node.with(|w| {
        w.unregistered = true;
        w.delegate_missing = true;
    });
    let dir = temp_dir("register-recovers");
    let pace = Pace {
        refresh: Duration::from_millis(20),
        register_wait: Duration::from_millis(100),
        register_retry_first: Duration::from_millis(150),
        register_retry_max: Duration::from_millis(600),
        ..quick()
    };
    let session = Running::start(&node, &dir, pace);
    session.wait_rooms("the store is said to be missing", |r, issues, _| {
        r.is_empty() && issues.store_missing
    });
    // Many re-reads while the node still refuses.
    let lists = node.with(|w| w.lists);
    session.wait_until("forty more reads", || node.with(|w| w.lists >= lists + 40));
    let (refused, reads) = node.with(|w| (registrations(w), w.lists - lists));
    assert!(refused >= 2, "the app retried by itself: {refused}");
    assert!(
        refused * 4 <= reads,
        "{refused} registrations over {reads} re-reads"
    );
    // The node is healthy now: the next retry sets the store up, with no command sent.
    node.with(|w| w.delegate_missing = false);
    session.wait_rooms("the store is found", |r, issues, s| {
        r.is_empty() && !issues.store_missing && s == LinkStatus::Live
    });
    node.with(|w| {
        assert!(!w.unregistered);
        assert_registers_only_with_the_grant(w);
    });
    // Found: no more registrations, however many re-reads follow.
    let after = node.with(|w| registrations(w));
    let lists = node.with(|w| w.lists);
    session.wait_until("twenty more reads", || node.with(|w| w.lists >= lists + 20));
    assert_eq!(node.with(|w| registrations(w)), after);
}

// A newer River ------------------------------------------------------------------------------

use crate::rooms::generation::tests::{FakePointers, delegate_state, room_state, test_watched};
use crate::rooms::generation::{CheckPace, Fetch, UPDATE_REQUIRED_WORK};

/// A watch over the test pointers, answered by `source`.
fn watched(source: &Arc<FakePointers>, delegate_version: Option<u32>) -> Arc<Watch> {
    let w = test_watched();
    answer_pointers(source, delegate_version);
    Arc::new(Watch::new(w, source.clone(), None))
}

/// The pointers answer: the room contract current, the chat delegate at `delegate_version`
/// (`None`: nothing reachable at all).
fn answer_pointers(source: &FakePointers, delegate_version: Option<u32>) {
    let w = test_watched();
    *source.answer.lock().unwrap() = match delegate_version {
        Some(v) => vec![
            (w[0].pointer_id, Fetch::State(room_state(1))),
            (w[1].pointer_id, Fetch::State(delegate_state(v))),
        ],
        None => Vec::new(),
    };
}

fn following_watched(
    node: &FakeNode,
    rooms: &[&TestRoom],
    dir: &str,
    outbox: Outbox,
    watch: Arc<Watch>,
) -> (Running, PathBuf) {
    node.with(|w| {
        for r in rooms {
            w.add_room(r);
        }
    });
    let dir = temp_dir(dir);
    save_token(&dir, "kept-token");
    let session = Running::start_full(
        node,
        &dir,
        quick(),
        Arc::new(Mutex::new(outbox)),
        std::sync::mpsc::channel(),
        watch,
    );
    let n = rooms.len();
    session.wait_rooms("the rooms are live", move |r, _, s| {
        r.len() == n && s == LinkStatus::Live
    });
    (session, dir)
}

/// What the node took that writes: UPDATEs, PUTs, CAS stores, signing key stores and
/// registrations.
fn writes_taken(node: &FakeNode) -> (usize, usize, usize, usize, usize) {
    node.with(|w| {
        (
            w.updates.len(),
            w.puts.len(),
            w.cas.len(),
            w.key_stores.len(),
            w.sent.iter().filter(|(_, k)| *k == "register").count(),
        )
    })
}

/// Once a verified pointer names a newer chat delegate, nothing is written: not what was
/// waiting from an earlier run, not a new message, a create, a join, nor the signing key the
/// sweep would store. Each fails with why, the composer says why, and the rooms still show and
/// are still subscribed.
#[test]
fn a_newer_river_stops_every_write_and_keeps_the_rooms_showing() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    // Left unsent by an earlier run.
    let mut outbox = Outbox::default();
    assert!(outbox.command(&LiveRoom::new(a.stored()), say("from before")));
    // An invitation to another room, to try to join.
    let room = TestRoom::new(3, "Garden", 1, &["t1"]);
    let (_, link) =
        super::super::create::invitation(&LiveRoom::new(room.stored()), "http://x/").unwrap();
    let source = Arc::new(FakePointers::default());
    let watch = watched(&source, Some(2));
    let (session, _dir) = following_watched(&node, &[&a], "stale-all", outbox, watch.clone());
    session.wait_until("River is known to have moved on", || watch.is_stale());
    session.wait_rooms("the update is asked for", |r, issues, _| {
        issues.update_required && r[0].send_block.as_deref() == Some(UPDATE_REQUIRED_SEND)
    });
    session.wait_until("what was waiting fails, saying why", || {
        matches!(delivery_of(&session, "from before"),
            Some(Some(p)) if p.failed.as_deref() == Some(UPDATE_REQUIRED))
    });

    session.act(&a, say("written now"));
    session.list(Action::CreateRoom {
        name: "New".into(),
        nickname: "Me".into(),
        private: false,
    });
    session.list(Action::Join {
        code: InviteCode(link),
        nickname: "Me".into(),
    });
    session.wait_until("each fails, saying why", || {
        let sent = matches!(delivery_of(&session, "written now"),
            Some(Some(p)) if p.failed.as_deref() == Some(UPDATE_REQUIRED));
        let work = session.last_work();
        sent && work.len() == 2
            && work.iter().all(|w| {
                matches!(&w.state, WorkState::Failed { why, retry: false } if why == UPDATE_REQUIRED_WORK)
            })
    });
    // Past the sweep's delay, which would otherwise store the room's signing key.
    std::thread::sleep(quick().key_delay * 3);
    assert_eq!(writes_taken(&node), (0, 0, 0, 0, 0));
    // Still reading: the room is subscribed.
    assert_eq!(node.with(|w| w.subscribes_to(a.id())), 1);
}

/// The control for the test above: with the same rooms and a current pointer, the same
/// writes all go out.
#[test]
fn a_current_river_writes_as_usual() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let mut outbox = Outbox::default();
    assert!(outbox.command(&LiveRoom::new(a.stored()), say("from before")));
    let source = Arc::new(FakePointers::default());
    let watch = watched(&source, Some(1));
    let (session, _dir) = following_watched(&node, &[&a], "current-all", outbox, watch.clone());
    session.wait_until("sent, and the key stored", || {
        let (updates, _, _, key_stores, _) = writes_taken(&node);
        updates == 1 && key_stores == 1
    });
    assert!(!watch.is_stale());
    let (_, issues, _) = session.last_rooms().unwrap();
    assert!(!issues.update_required);
}

/// A pointer that cannot be reached at all never stops the app writing.
#[test]
fn an_unreachable_pointer_does_not_stop_writes() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let source = Arc::new(FakePointers::default());
    let watch = watched(&source, None);
    let (session, _dir) = following_watched(
        &node,
        &[&a],
        "unreachable",
        Outbox::default(),
        watch.clone(),
    );
    session.act(&a, say("still sent"));
    session.wait_until("sent", || writes_taken(&node).0 == 1);
    assert!(!watch.is_stale());
}

/// On a node River never ran on, a build River has moved past does not register its chat
/// delegate: that would set up storage the web UI no longer reads.
#[test]
fn an_outdated_build_does_not_register_on_a_fresh_node() {
    let node = fake_node(vec![]);
    node.with(|w| w.unregistered = true);
    let dir = temp_dir("stale-fresh");
    let source = Arc::new(FakePointers::default());
    let session = Running::start_watched(&node, &dir, quick(), watched(&source, Some(2)));
    session.wait_rooms("no store, and the update asked for", |r, issues, _| {
        r.is_empty() && issues.store_missing && issues.update_required
    });
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(writes_taken(&node), (0, 0, 0, 0, 0));
    assert!(node.with(|w| w.unregistered));
}

/// A re-key published while the app runs is noticed by the periodic check, and writes stop
/// from then on.
#[test]
fn a_re_key_while_running_stops_later_writes() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let source = Arc::new(FakePointers::default());
    let watch = watched(&source, Some(1));
    let (session, _dir) = following_watched(
        &node,
        &[&a],
        "rekey-running",
        Outbox::default(),
        watch.clone(),
    );
    session.act(&a, say("before"));
    session.wait_until("sent", || writes_taken(&node).0 == 1);
    answer_pointers(&source, Some(3));
    session.wait_rooms("the update is asked for", |_, issues, _| {
        issues.update_required
    });
    session.act(&a, say("after"));
    session.wait_until("refused, saying why", || {
        matches!(delivery_of(&session, "after"),
            Some(Some(p)) if p.failed.as_deref() == Some(UPDATE_REQUIRED))
    });
    assert_eq!(writes_taken(&node).0, 1);
}

/// Rooms are live with a message left from an earlier run waiting, under `watch`, and the
/// app does not wait for the rooms to go live first (the first check may still be out).
fn start_with_waiting(
    node: &FakeNode,
    a: &TestRoom,
    dir: &str,
    pace: Pace,
    watch: Arc<Watch>,
) -> Running {
    node.with(|w| w.add_room(a));
    let mut outbox = Outbox::default();
    assert!(outbox.command(&LiveRoom::new(a.stored()), say("from before")));
    let dir = temp_dir(dir);
    save_token(&dir, "kept-token");
    Running::start_full(
        node,
        &dir,
        pace,
        Arc::new(Mutex::new(outbox)),
        std::sync::mpsc::channel(),
        watch,
    )
}

/// The first answer is slow and names a newer delegate: nothing waiting goes out meanwhile.
#[test]
fn a_slow_first_answer_holds_writes_until_it_comes() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let source = Arc::new(FakePointers::default());
    *source.delay.lock().unwrap() = Duration::from_millis(500);
    let watch = watched(&source, Some(2));
    let session = start_with_waiting(&node, &a, "slow-newer", quick(), watch.clone());
    session.wait_until("refused, saying why", || {
        matches!(delivery_of(&session, "from before"),
            Some(Some(p)) if p.failed.as_deref() == Some(UPDATE_REQUIRED))
    });
    assert_eq!(writes_taken(&node), (0, 0, 0, 0, 0));
}

/// A node that never answers the pointers holds writes only for `first_wait`.
#[test]
fn a_hung_pointer_fetch_holds_writes_only_for_the_first_wait() {
    struct Hangs;
    impl crate::rooms::generation::PointerSource for Hangs {
        fn fetch(
            &self,
            _: &str,
            _: &GrantToken,
            _: &[ContractInstanceId],
            _: Duration,
        ) -> Vec<(ContractInstanceId, Fetch)> {
            std::thread::sleep(Duration::from_secs(60));
            Vec::new()
        }
    }
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        pointers: CheckPace {
            first_wait: Duration::from_millis(600),
            ..quick().pointers
        },
        ..quick()
    };
    let watch = Arc::new(Watch::new(test_watched(), Arc::new(Hangs), None));
    let started = Instant::now();
    let session = start_with_waiting(&node, &a, "hung", pace, watch);
    session.wait_until("sent after the wait", || writes_taken(&node).0 == 1);
    assert!(started.elapsed() >= Duration::from_millis(600));
}

/// A node that has just started answers "not found" for the pointers at once. That must not
/// let the message waiting from an earlier run out before the newer record arrives.
#[test]
fn a_fast_not_found_does_not_let_writes_out_before_the_newer_record() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let source = Arc::new(FakePointers::default());
    let absent: Vec<_> = test_watched()
        .iter()
        .map(|w| (w.pointer_id, Fetch::Absent))
        .collect();
    source
        .script
        .lock()
        .unwrap()
        .extend([absent.clone(), absent.clone(), absent]);
    let watch = watched(&source, Some(2));
    let session = start_with_waiting(&node, &a, "absent-first", quick(), watch);
    session.wait_until("refused, saying why", || {
        matches!(delivery_of(&session, "from before"),
            Some(Some(p)) if p.failed.as_deref() == Some(UPDATE_REQUIRED))
    });
    assert_eq!(writes_taken(&node), (0, 0, 0, 0, 0));
}

/// On a node River never ran on, the session does not block on the first check: it shows that
/// it is setting up River's storage, then registers on a new connection once the check is
/// settled, before anything else on it.
#[test]
fn a_fresh_node_is_registered_once_the_first_check_settles() {
    let node = fake_node(vec![]);
    node.with(|w| w.unregistered = true);
    let dir = temp_dir("deferred-register");
    let source = Arc::new(FakePointers::default());
    *source.delay.lock().unwrap() = Duration::from_millis(400);
    let watch = watched(&source, Some(1));
    let session = Running::start_watched(&node, &dir, quick(), watch);
    session.wait_rooms(
        "setting up, shown while the check is out",
        |r, issues, _| r.is_empty() && issues.setting_up,
    );
    assert!(
        !node.with(|w| w.sent.iter().any(|(_, k)| *k == "register")),
        "registered before the check settled"
    );
    session.wait_rooms("registered and read", |r, issues, _| {
        r.is_empty() && !issues.store_missing && !issues.setting_up
    });
    node.with(|w| {
        assert!(!w.unregistered);
        assert_registers_only_with_the_grant(w);
        let n = w
            .sent
            .iter()
            .find(|(_, k)| *k == "register")
            .map(|(n, _)| *n)
            .unwrap();
        assert!(n >= 1, "registered on a later connection");
        assert_eq!(
            sent_on(w, n)[0],
            "register",
            "registered before anything else"
        );
    });
}
