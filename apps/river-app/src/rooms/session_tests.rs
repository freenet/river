use super::*;
use crate::rooms::identity::IdentityToken;
use crate::rooms::outbox::InviteCode;
use crate::rooms::outbox::{Action, Outbox};
use crate::rooms::room::message_key;
use crate::rooms::work::WorkState;
use ed25519_dalek::{SigningKey, VerifyingKey};
use freenet_stdlib::client_api::{DelegateRequest, HostResponse};
use freenet_stdlib::prelude::{
    ApplicationMessage, CodeHash, ContractCode, ContractKey, DelegateKey, InboundDelegateMsg,
    OutboundDelegateMsg, Parameters, State, StateDelta, WrappedState,
};
use river_core::chat_delegate::{CasStoreResult, ChatDelegateKey, ChatDelegateRequestMsg};
use river_core::room_state::ChatRoomParametersV1;
use river_core::room_state::configuration::{AuthorizedConfigurationV1, Configuration};
use river_core::room_state::identity::IdentityExport;
use river_core::room_state::member::{AuthorizedMember, Member, MemberId};
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
            has_meta: true,
            has_dms: false
        }
    );
    assert_eq!(
        plan(std::iter::empty()),
        Plan {
            rooms: vec![],
            has_meta: false,
            has_dms: false
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
        listed_rooms: slots.iter().map(|s| s.owner_vk).collect(),
        slots,
        meta: StoredRoomsMeta::default(),
        modes: Default::default(),
        store_missing: false,
        unreadable: 0,
        dms: Default::default(),
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
fn every_kind_of_update_reaches_the_room_and_a_rejected_delta_keeps_what_came_before() {
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
    // wrong key, which `apply_delta` reaches after the messages. As in the web UI the message,
    // applied before the failing step, stays (freenet/river#971; `LiveRoom::merge_network_state`
    // has why); the forged pointer does not, and the room is named as not updated.
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
    assert_eq!(last_text(&rooms).as_deref(), Some("half applied"));
    assert!(shown(&rooms, &a).stored.room_state.upgrade.0.is_none());
    // Failing again while already named, with a new message before the failing step: the
    // message is kept and must show, though the room's name on the issues list does not change.
    let again = ChatRoomStateV1Delta {
        recent_messages: Some(vec![message(&a.owner, SystemTime::now(), "kept again")]),
        ..bad.clone()
    };
    assert!(
        rooms.apply_update(&a.id(), &UpdateData::Delta(cbor(&again).into())),
        "what the failed merge kept is reported"
    );
    assert_eq!(last_text(&rooms).as_deref(), Some("kept again"));
    // The same failure with nothing new changes nothing.
    assert!(!rooms.apply_update(&a.id(), &UpdateData::Delta(cbor(&again).into())));
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
    /// When each registration reached the node.
    register_times: Vec<Instant>,
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
    /// Every contract GET, subscribing or not.
    subscribes: Vec<ContractInstanceId>,
    /// Every GET that does not subscribe (a read again of a room), with when it arrived.
    reads: Vec<(ContractInstanceId, Instant)>,
    /// How GETs that do not subscribe are answered.
    read_play: ReadPlay,
    /// Answer no subscribing GET for these rooms.
    ignore_subscribes: HashSet<ContractInstanceId>,
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
    /// Answer every CAS store of this delegate key with a refusal (`CasStoreResult::Failed`).
    fail_cas: Option<Vec<u8>>,
    /// Answer the next listing with nothing (then clear this).
    silence_next_list: bool,
    /// The reads the last listing's reader still has to make.
    reads_due: usize,
    /// Versioned reads and CAS stores that arrived while a reader was still reading.
    overlaps: usize,
    /// Earlier chat delegates the node holds, with what each stores. Any other is `Missing`.
    legacy: HashMap<DelegateKey, BTreeMap<Vec<u8>, Vec<u8>>>,
    /// Every request to an earlier chat delegate: the connection and the delegate.
    legacy_asks: Vec<(usize, DelegateKey)>,
    /// Take requests to this earlier delegate but answer nothing.
    silence_legacy: Option<DelegateKey>,
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

/// How the fake node answers a GET that does not subscribe (a read again of a room).
#[derive(Clone, Debug, Default)]
enum ReadPlay {
    /// With the network's copy, as for a subscribe.
    #[default]
    Answer,
    /// No answer at all.
    Silence,
    /// `NotFound`.
    NotFound,
    /// The node's GET error, naming the contract.
    ErrNamed,
    /// An error that names nothing (`OperationError`), as the node reports a request that
    /// failed to start.
    ErrNameless,
    /// With this other contract's state, under the asked contract's key (a bad node).
    Swap(ContractInstanceId),
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

    fn reads_of(&self, id: ContractInstanceId) -> usize {
        self.reads.iter().filter(|(r, _)| *r == id).count()
    }

    /// The network's copy of `room`, as it holds it now.
    fn network_state(&self, room: &TestRoom) -> ChatRoomStateV1 {
        match &self.contracts[&room.id()].1 {
            Contract::State(bytes) => ciborium::de::from_reader(bytes.as_slice()).unwrap(),
            Contract::NotFound => panic!("the network has no copy"),
        }
    }

    /// Another message from `room`'s owner reaches the network's copy, and the node notifies
    /// the newest connection as the real node does: with the whole new state.
    fn notify_full(&mut self, room: &TestRoom, text: &str) {
        let mut state = self.network_state(room);
        state
            .recent_messages
            .messages
            .push(message(&room.owner, SystemTime::now(), text));
        let bytes = cbor(&state);
        self.contracts
            .insert(room.id(), (room.key, Contract::State(bytes.clone())));
        let note: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(
            ContractResponse::UpdateNotification {
                key: room.key,
                update: UpdateData::State(bytes.into()),
            },
        ));
        self.push
            .push(Message::Binary(bincode::serialize(&note).unwrap().into()));
    }

    /// Push `error` to the newest connection, as if answering a request still out.
    fn push_error(&mut self, error: ClientError) {
        let e: Result<HostResponse, ClientError> = Err(error);
        self.push
            .push(Message::Binary(bincode::serialize(&e).unwrap().into()));
    }

    /// The network's copy of `room` now also holds `text` from its owner, which no
    /// notification announces.
    fn network_says(&mut self, room: &TestRoom, text: &str) {
        let mut state = room.state.clone();
        state
            .recent_messages
            .messages
            .push(message(&room.owner, SystemTime::now(), text));
        self.contracts
            .insert(room.id(), (room.key, Contract::State(cbor(&state))));
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

/// The fake node's side of one connection. Its socket has NO receive timeout: Windows documents
/// that a receive that times out leaves the connection in an indeterminate state (the same
/// reason the app's own socket has none), so this one is non-blocking and sleeps between reads.
/// Any read error other than "nothing yet" and a normal close is printed with its raw OS code,
/// so a connection the node drops on its own account never looks like the app's doing.
fn serve(mut ws: tungstenite::WebSocket<TcpStream>, n: usize, world: &Mutex<World>) {
    if let Err(e) = ws.get_mut().set_nonblocking(true) {
        eprintln!("fake node: cannot make the socket non-blocking: {e}");
        return;
    }
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
                if !sent_or_queued(ws.send(m)) {
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
                    if !sent_or_queued(ws.send(Message::Binary(r.into()))) {
                        return;
                    }
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if e.kind() == IoErrorKind::WouldBlock => {
                // Nothing yet: push anything left queued, then sleep a little.
                if !sent_or_queued(ws.flush()) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return;
            }
            Err(e) => {
                let code = match &e {
                    tungstenite::Error::Io(io) => io.raw_os_error(),
                    _ => None,
                };
                eprintln!("fake node: connection {n} read ended: {e} (os error {code:?})");
                return;
            }
        }
    }
}

/// Whether a write on the fake node's non-blocking socket went out or is queued to go out on
/// the next flush (`WouldBlock`); a real failure is printed with its raw OS code.
fn sent_or_queued(result: Result<(), tungstenite::Error>) -> bool {
    match result {
        Ok(()) => true,
        Err(tungstenite::Error::Io(e)) if e.kind() == IoErrorKind::WouldBlock => true,
        Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => false,
        Err(e) => {
            let code = match &e {
                tungstenite::Error::Io(io) => io.raw_os_error(),
                _ => None,
            };
            eprintln!("fake node: a write ended: {e} (os error {code:?})");
            false
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
            w.register_times.push(Instant::now());
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
        // An earlier chat delegate (carrying rooms over): answered from `legacy` when the node
        // holds it, `Missing` otherwise, as a node answers for a delegate never registered.
        ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { key, inbound, .. })
            if &key != delegate::chat_delegate_key() =>
        {
            assert!(
                super::super::carry::predecessors().iter().any(|p| p.key == key),
                "the app asked a delegate that is neither River's nor an earlier River's: {key}"
            );
            w.legacy_asks.push((n, key.clone()));
            if w.silence_legacy.as_ref() == Some(&key) {
                return out;
            }
            for msg in inbound {
                let InboundDelegateMsg::ApplicationMessage(m) = msg else {
                    continue;
                };
                let read: ChatDelegateRequestMsg =
                    ciborium::de::from_reader(m.payload.as_slice()).unwrap();
                let Some(store) = w.legacy.get(&key) else {
                    out.push(encode(Err(ErrorKind::RequestError(
                        RequestError::DelegateError(DelegateError::Missing(key.clone())),
                    )
                    .into())));
                    continue;
                };
                let reply = match read {
                    ChatDelegateRequestMsg::ListRequest => ChatDelegateResponseMsg::ListResponse {
                        keys: store.keys().cloned().map(ChatDelegateKey::new).collect(),
                    },
                    ChatDelegateRequestMsg::GetRequest { key: k } => {
                        ChatDelegateResponseMsg::GetResponse {
                            value: store.get(k.as_bytes()).cloned(),
                            key: k,
                        }
                    }
                    other => panic!("the app only reads an earlier delegate, but sent {other:?}"),
                };
                out.push(encode(Ok(HostResponse::DelegateResponse {
                    key: key.clone(),
                    values: vec![OutboundDelegateMsg::ApplicationMessage(
                        ApplicationMessage::new(cbor(&reply)),
                    )],
                })));
            }
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
                        let failing = w.fail_cas.as_deref() == Some(key.as_bytes());
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
                        let result = if failing {
                            CasStoreResult::Failed("disk".into())
                        } else if taken {
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
        ClientRequest::ContractOp(ContractRequest::Get {
            key: id, subscribe, ..
        }) => {
            w.subscribes.push(id);
            if subscribe && w.ignore_subscribes.contains(&id) {
                return out;
            }
            let mut from = id;
            if !subscribe {
                w.reads.push((id, Instant::now()));
                let named = |id| -> ClientError {
                    ErrorKind::RequestError(RequestError::ContractError(ContractError::Get {
                        key: w.contracts[&id].0,
                        cause: "no route".into(),
                    }))
                    .into()
                };
                match w.read_play.clone() {
                    ReadPlay::Answer => {}
                    ReadPlay::Silence => return out,
                    ReadPlay::NotFound => {
                        out.push(encode(Ok(HostResponse::ContractResponse(
                            ContractResponse::NotFound { instance_id: id },
                        ))));
                        return out;
                    }
                    ReadPlay::ErrNamed => {
                        out.push(encode(Err(named(id))));
                        return out;
                    }
                    ReadPlay::ErrNameless => {
                        out.push(encode(Err(ErrorKind::OperationError {
                            cause: "the request could not start".into(),
                        }
                        .into())));
                        return out;
                    }
                    ReadPlay::Swap(other) => from = other,
                }
            }
            let asked_key = w.contracts.get(&id).map(|(k, _)| *k);
            match w
                .contracts
                .get(&from)
                .cloned()
                .map(|(k, c)| (asked_key.unwrap_or(k), c))
            {
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
        resync_idle: Duration::from_secs(3600),
        resync_focus: Duration::from_secs(3600),
        resync_spread: Duration::ZERO,
        resync_wait: Duration::from_secs(3600),
        resync_late: Duration::from_secs(3600),
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

    /// [`wait_until`](Self::wait_until) for a test that depends on the one connection it
    /// started with: an answer pushed to the node reaches that connection only, and is not
    /// resent on a later one. If the session reconnects meanwhile (a dropped socket, a read
    /// that failed on a loaded machine), the wait fails saying so at once, instead of timing
    /// out as "never: ..." with the cause hidden.
    fn wait_on_one_connection(&self, node: &FakeNode, what: &str, done: impl Fn() -> bool) {
        self.wait_until(what, || {
            let connections = node.with(|w| w.connections.len());
            assert!(
                connections <= 1,
                "the session reconnected ({connections} connections) before: {what}; this test \
                 needs its first connection, so a lost connection is an environment fault \
                 (the log above says why it was lost)"
            );
            done()
        });
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

// Reading rooms again on a live connection (freenet/river#971) --------------------------------

impl Running {
    /// Tell the session the window came to the front.
    fn focus(&self) {
        self.commands
            .send(Command {
                room: String::new(),
                action: Action::Resync,
            })
            .unwrap();
    }
}

fn shows(session: &Running, text: &str) -> bool {
    session
        .last_rooms()
        .is_some_and(|(r, _, _)| r.iter().any(|room| texts(room).iter().any(|t| t == text)))
}

/// The node took a message for the open room and its notification never came. The room is read
/// again once the node has said nothing about it for `resync_idle`, and the message shows,
/// on the same connection.
#[test]
fn the_open_room_is_read_again_when_the_node_goes_quiet_on_it_and_what_it_missed_shows() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        resync_idle: Duration::from_millis(300),
        resync_wait: Duration::from_secs(5),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-quiet", pace);
    session.act(&a, Action::Viewing);
    node.with(|w| w.network_says(&a, "missed"));
    session.wait_on_one_connection(&node, "the missed message shows", || {
        shows(&session, "missed")
    });
    assert!(
        node.with(|w| w.reads_of(a.id())) >= 1,
        "it came from a read again"
    );
}

/// A notification that cannot be applied is followed by a read of the room, and the read's
/// state shows. Before, the room was only named as not updated and stayed behind until the
/// next notification happened to carry what it missed.
#[test]
fn a_notification_that_cannot_be_applied_is_followed_by_a_read_whose_state_shows() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        resubscribe_first: Duration::from_millis(100),
        resubscribe_max: Duration::from_millis(400),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-heal", pace);
    node.with(|w| {
        w.network_says(&a, "carried by the read");
        let note: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(
            ContractResponse::UpdateNotification {
                key: a.key,
                update: UpdateData::Delta(b"not cbor".to_vec().into()),
            },
        ));
        w.push
            .push(Message::Binary(bincode::serialize(&note).unwrap().into()));
    });
    session.wait_on_one_connection(&node, "the read's state shows", || {
        shows(&session, "carried by the read")
            && session
                .last_rooms()
                .is_some_and(|(_, issues, _)| issues.not_updated.is_empty())
    });
    assert!(node.with(|w| w.reads_of(a.id())) >= 1);
}

/// With a quiet but healthy node, the open room is read again at most once per `resync_idle`,
/// a room not open is not read again at all, and the connection stays up.
#[test]
fn a_quiet_node_is_read_again_at_the_idle_pace_only_for_the_open_room_and_kept() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let idle = Duration::from_millis(200);
    let pace = Pace {
        resync_idle: idle,
        resync_wait: Duration::from_secs(5),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a, &b], "resync-pace", pace);
    let start = Instant::now();
    session.act(&a, Action::Viewing);
    session.wait_on_one_connection(&node, "the open room is read again, three times", || {
        node.with(|w| w.reads_of(a.id()) >= 3)
    });
    let elapsed = start.elapsed();
    let (reads_a, reads_b, connections) =
        node.with(|w| (w.reads_of(a.id()), w.reads_of(b.id()), w.connections.len()));
    let most = (elapsed.as_millis() / idle.as_millis()) as usize + 1;
    assert!(
        reads_a <= most,
        "{reads_a} reads in {elapsed:?}, at most {most}"
    );
    assert_eq!(
        reads_b, 0,
        "a room not open is not read again while the node is healthy"
    );
    assert_eq!(connections, 1, "reading again never reconnects");
}

/// Reads the node never answers: one at a time per room, given up after `resync_wait` (and then
/// asked again), never a reconnect.
#[test]
fn reads_the_node_never_answers_go_one_at_a_time_and_never_reconnect() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let wait = Duration::from_millis(400);
    let pace = Pace {
        resync_idle: Duration::from_millis(50),
        resync_wait: wait,
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-silent", pace);
    node.with(|w| w.read_play = ReadPlay::Silence);
    let start = Instant::now();
    session.act(&a, Action::Viewing);
    session.wait_on_one_connection(
        &node,
        "an unanswered read is given up and asked again",
        || node.with(|w| w.reads_of(a.id()) >= 3),
    );
    let elapsed = start.elapsed();
    let (reads, connections) = node.with(|w| (w.reads_of(a.id()), w.connections.len()));
    let most = (elapsed.as_millis() / wait.as_millis()) as usize + 1;
    assert!(
        reads <= most,
        "{reads} reads in {elapsed:?}, at most {most}"
    );
    assert_eq!(connections, 1);
}

/// A room that owes the answer to an update is not read again: an error naming nothing could
/// then be either's.
#[test]
fn a_room_owing_an_update_answer_is_not_read_again() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        resync_idle: Duration::from_millis(50),
        resync_wait: Duration::from_millis(100),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-owed", pace);
    node.with(|w| w.update_play = UpdatePlay::Ignore);
    session.act(&a, say("unanswered"));
    session.wait_until("the update reached the node", || {
        node.with(|w| !w.updates.is_empty())
    });
    session.act(&a, Action::Viewing);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(node.with(|w| w.reads_of(a.id())), 0);
    assert_eq!(node.with(|w| w.connections.len()), 1);
}

/// Coming to the front reads again every room the node has said nothing about for
/// `resync_focus`, once; coming to the front again at once reads nothing more.
#[test]
fn coming_to_the_front_reads_again_each_room_not_heard_about_lately_once() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let pace = Pace {
        resync_focus: Duration::from_millis(300),
        resync_wait: Duration::from_secs(5),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a, &b], "resync-focus", pace);
    node.with(|w| {
        w.network_says(&a, "news in A");
        w.network_says(&b, "news in B");
    });
    // Well past `resync_focus`, so both rooms are quiet enough however slow the machine.
    std::thread::sleep(Duration::from_secs(1));
    session.focus();
    session.wait_on_one_connection(&node, "both rooms are read again and show it", || {
        shows(&session, "news in A") && shows(&session, "news in B")
    });
    assert_eq!(
        node.with(|w| (w.reads_of(a.id()), w.reads_of(b.id()))),
        (1, 1)
    );
    session.focus();
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        node.with(|w| (w.reads_of(a.id()), w.reads_of(b.id()))),
        (1, 1),
        "rooms just heard from are not read again"
    );
}

/// Whether the room `room` (that room, not any) shows `text`.
fn shows_in(session: &Running, room: &TestRoom, text: &str) -> bool {
    let key = bs58::encode(room.vk().to_bytes()).into_string();
    session.last_rooms().is_some_and(|(r, _, _)| {
        r.iter()
            .any(|shown| shown.key == key && texts(shown).iter().any(|t| t == text))
    })
}

/// Wait until a read of `room` has reached the node.
fn wait_for_a_read(session: &Running, node: &FakeNode, room: &TestRoom) {
    session.wait_on_one_connection(node, "a read is out", || {
        node.with(|w| w.reads_of(room.id()) >= 1)
    });
}

/// Notifications for the open room keep it from being read again: the node is plainly telling
/// River about it. The notifications carry the whole state, as the real node's do.
#[test]
fn notifications_for_the_open_room_keep_it_from_being_read_again() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let idle = Duration::from_millis(1500);
    let pace = Pace {
        resync_idle: idle,
        resync_wait: Duration::from_secs(5),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-notified", pace);
    session.act(&a, Action::Viewing);
    let start = Instant::now();
    let mut n = 0;
    // A notification about every 100 ms, against a 1.5 s idle time, for over three idle times:
    // only a stall of more than a second between two of them lets a read in.
    while start.elapsed() < idle * 3 {
        n += 1;
        let text = format!("note {n}");
        node.with(|w| w.notify_full(&a, &text));
        session.wait_on_one_connection(&node, "the notification shows", || {
            shows_in(&session, &a, &text)
        });
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(n >= 15, "only {n} notifications");
    assert_eq!(node.with(|w| w.reads_of(a.id())), 0);
}

/// A message the node took arrives in a whole-state notification, as the real node sends it,
/// and shows in that room only.
#[test]
fn a_whole_state_notification_shows_what_it_carries_in_its_room_only() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let (session, _dir) = following(&node, &[&a, &b], "resync-full-note", quick());
    node.with(|w| w.notify_full(&a, "for A"));
    session.wait_on_one_connection(&node, "it shows in A", || shows_in(&session, &a, "for A"));
    assert!(!shows_in(&session, &b, "for A"));
    assert!(session.last_rooms().unwrap().1.not_updated.is_empty());
}

/// A room whose subscribe the node has not answered is not read again: the read's answer could
/// not be told from the subscribe's.
#[test]
fn a_room_whose_subscribe_is_unanswered_is_not_read_again() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    node.with(|w| {
        w.ignore_subscribes.insert(a.id());
    });
    let pace = Pace {
        resync_idle: Duration::from_millis(50),
        resync_wait: Duration::from_secs(5),
        // Asked once and left waiting for the whole test.
        catch_up: Duration::from_secs(30),
        ..quick()
    };
    node.with(|w| {
        w.add_room(&a);
        w.add_room(&b);
    });
    let dir = temp_dir("resync-sub-pending");
    save_token(&dir, "kept-token");
    let session = Running::start(&node, &dir, pace);
    session.wait_rooms("both rooms show", |r, _, _| r.len() == 2);
    session.act(&a, Action::Viewing);
    // B, with its subscribe answered, is read once it is open: the rule is live.
    std::thread::sleep(Duration::from_millis(500));
    session.act(&b, Action::Viewing);
    wait_for_a_read(&session, &node, &b);
    assert_eq!(node.with(|w| w.reads_of(a.id())), 0);
}

/// A read that fails in any way the node fails one (nothing found, its GET error, an error
/// naming nothing) changes nothing on screen, does not touch the subscription, never
/// reconnects, and the next read comes by the usual rule, not after `resync_wait`.
#[test]
fn a_read_that_fails_changes_nothing_and_the_next_comes_at_the_usual_pace() {
    for (play, name) in [
        (ReadPlay::NotFound, "not-found"),
        (ReadPlay::ErrNamed, "named"),
        (ReadPlay::ErrNameless, "nameless"),
    ] {
        let node = fake_node(vec![]);
        let a = TestRoom::new(1, "A", 1, &["a1"]);
        let pace = Pace {
            resync_idle: Duration::from_millis(100),
            // Far longer than the test: a read is freed by its failure, not by giving up.
            resync_wait: Duration::from_secs(3600),
            ..quick()
        };
        let (session, _dir) = following(&node, &[&a], &format!("resync-fail-{name}"), pace);
        node.with(|w| w.read_play = play.clone());
        session.act(&a, Action::Viewing);
        session.wait_on_one_connection(&node, &format!("{name}: three reads"), || {
            node.with(|w| w.reads_of(a.id()) >= 3)
        });
        let (subscribes, connections) = node.with(|w| {
            (
                w.subscribes_to(a.id()) - w.reads_of(a.id()),
                w.connections.len(),
            )
        });
        assert_eq!(subscribes, 1, "{name}: subscribed again");
        assert_eq!(connections, 1, "{name}");
        let (_, issues, status) = session.last_rooms().unwrap();
        assert!(issues.not_updated.is_empty(), "{name}: {issues:?}");
        assert_eq!(status, LinkStatus::Live, "{name}");
    }
}

/// A bad node answers a read of B with A's state: neither room takes the other's messages.
#[test]
fn an_answer_with_another_rooms_state_leaks_into_neither() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let pace = Pace {
        resync_idle: Duration::from_millis(50),
        resync_wait: Duration::from_secs(5),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a, &b], "resync-swap", pace);
    node.with(|w| {
        w.network_says(&a, "only A");
        w.read_play = ReadPlay::Swap(a.id());
    });
    session.act(&b, Action::Viewing);
    session.wait_on_one_connection(&node, "B is read twice", || {
        node.with(|w| w.reads_of(b.id()) >= 2)
    });
    assert!(!shows_in(&session, &b, "only A"));
    assert!(!shows_in(&session, &a, "only A"), "A was not read");
    assert!(shows_in(&session, &b, "b1") && shows_in(&session, &a, "a1"));
}

/// A merge that fails the same way every time (the network's copy holds an upgrade pointer
/// signed by someone else, which every merge refuses) is read again with a growing wait, and
/// then no more on this connection: reading it again cannot help.
#[test]
fn a_merge_that_always_fails_the_same_way_is_read_again_a_few_times_then_left() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let mut bad = a.state.clone();
    bad.upgrade = river_core::room_state::upgrade::OptionalUpgradeV1(Some(
        river_core::room_state::upgrade::AuthorizedUpgradeV1::new(
            river_core::room_state::upgrade::UpgradeV1 {
                owner_member_id: MemberId::from(&a.vk()),
                version: 1,
                new_chatroom_address: blake3::Hash::from([0; 32]),
            },
            &SigningKey::from_bytes(&[99; 32]),
        ),
    ));
    // The first heal wait is 0.8-1.2 s and the second, doubled, 1.6-2.4 s. A delay can only
    // lengthen a gap, so the second gap is at least 1.6 s; without the doubling it would be at
    // most 1.2 s plus any delay.
    let first = Duration::from_secs(1);
    let pace = Pace {
        resubscribe_first: first,
        resubscribe_max: first * 8,
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-hopeless", pace);
    let noted = Instant::now();
    node.with(|w| {
        w.contracts
            .insert(a.id(), (a.key, Contract::State(cbor(&bad))));
        w.notify_full(&a, "kept though the merge fails");
    });
    let give_up = super::super::resync::HEAL_GIVE_UP as usize;
    // The notification's merge is the first failure; each read's is the next.
    session.wait_on_one_connection(&node, "a read after each failure but the last", || {
        node.with(|w| w.reads_of(a.id()) >= give_up - 1)
    });
    // The next heal would come 3.2-4.8 s after the last read: wait past that.
    std::thread::sleep(first * 5);
    let times: Vec<Instant> = node.with(|w| w.reads.iter().map(|(_, t)| *t).collect());
    assert_eq!(
        times.len(),
        give_up - 1,
        "reads of a room that can never merge"
    );
    let [one, two] = times[..] else {
        panic!("{times:?}")
    };
    assert!(
        two - one >= first.mul_f64(1.5),
        "the wait doubles: {:?} then {:?}",
        one - noted,
        two - one
    );
    assert_eq!(node.with(|w| w.connections.len()), 1);
    assert_eq!(session.last_rooms().unwrap().1.not_updated, ["A"]);
}

/// The stored rooms are not read again while a room read is out: an error naming nothing could
/// then be either's, and taken for the stored rooms' it aborts their read.
#[test]
fn the_stored_rooms_are_not_read_again_while_a_room_read_is_out() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        resync_idle: Duration::from_millis(50),
        resync_wait: Duration::from_secs(3600),
        refresh: Duration::from_millis(300),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-vs-reload", pace);
    node.with(|w| w.read_play = ReadPlay::Silence);
    session.act(&a, Action::Viewing);
    wait_for_a_read(&session, &node, &a);
    let lists = node.with(|w| w.lists);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        node.with(|w| w.lists),
        lists,
        "a stored-rooms read went out beside it"
    );
}

/// The signing key sweep sends nothing while a room read is out, for the same reason.
#[test]
fn the_signing_key_sweep_waits_while_a_room_read_is_out() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        resync_idle: Duration::from_millis(50),
        resync_wait: Duration::from_secs(3600),
        key_delay: Duration::from_millis(1000),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-vs-sweep", pace);
    node.with(|w| w.read_play = ReadPlay::Silence);
    session.act(&a, Action::Viewing);
    wait_for_a_read(&session, &node, &a);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(node.with(|w| w.key_reads.is_empty() && w.key_stores.is_empty()));
}

/// A create sends nothing while a room read is out, for the same reason.
#[test]
fn a_create_waits_while_a_room_read_is_out() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        resync_idle: Duration::from_millis(50),
        resync_wait: Duration::from_secs(3600),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-vs-create", pace);
    node.with(|w| w.read_play = ReadPlay::Silence);
    session.act(&a, Action::Viewing);
    wait_for_a_read(&session, &node, &a);
    let before = node.with(|w| w.sent.len());
    session.list(Action::CreateRoom {
        name: "Book club".into(),
        nickname: "Ada".into(),
        private: false,
    });
    std::thread::sleep(Duration::from_secs(1));
    let after: Vec<&'static str> =
        node.with(|w| w.sent[before..].iter().map(|(_, k)| *k).collect());
    assert!(
        after.iter().all(|k| *k == "contract") && node.with(|w| w.puts.is_empty()),
        "the create sent {after:?} beside the read"
    );
}

/// A room read whose node answers `MissingContract` while the room also owes an update's
/// answer: the error may be the read's, so the message is not marked as failed for it.
#[test]
fn a_missing_contract_while_a_read_is_out_does_not_fail_a_message() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        resync_idle: Duration::from_millis(50),
        resync_wait: Duration::from_secs(3600),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-vs-update", pace);
    node.with(|w| {
        w.read_play = ReadPlay::Silence;
        w.update_play = UpdatePlay::Ignore;
    });
    session.act(&a, Action::Viewing);
    wait_for_a_read(&session, &node, &a);
    session.act(&a, say("still sending"));
    session.wait_until("the update reached the node", || {
        node.with(|w| !w.updates.is_empty())
    });
    node.with(|w| {
        w.push_error(
            ErrorKind::RequestError(RequestError::ContractError(
                ContractError::MissingContract { key: a.id() },
            ))
            .into(),
        )
    });
    // The read took it: once the update is answered too, nothing holds the room, and it is
    // read again at the idle pace (with `resync_wait` an hour, only the error freed the read).
    node.with(|w| {
        let answer: Result<HostResponse, ClientError> = Ok(HostResponse::ContractResponse(
            ContractResponse::UpdateResponse {
                key: a.key,
                summary: freenet_stdlib::prelude::StateSummary::from(vec![]),
            },
        ));
        w.push
            .push(Message::Binary(bincode::serialize(&answer).unwrap().into()));
    });
    session.wait_on_one_connection(&node, "the read is freed and asked again", || {
        node.with(|w| w.reads_of(a.id()) >= 2)
    });
    let failed = session.last_rooms().is_some_and(|(r, _, _)| {
        r.iter().flat_map(|r| r.messages.iter()).any(|m| {
            m.text == "still sending" && m.pending.as_ref().is_some_and(|p| p.failed.is_some())
        })
    });
    assert!(!failed, "the read's error was taken for the update's");
}

/// The open room is still read again after a reconnect: the new connection is told which room
/// the window shows.
#[test]
fn the_open_room_is_still_read_again_after_a_reconnect() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        resync_idle: Duration::from_millis(200),
        resync_wait: Duration::from_secs(5),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-reconnect", pace);
    session.act(&a, Action::Viewing);
    wait_for_a_read(&session, &node, &a);
    node.with(|w| w.drop_live = true);
    session.wait_until("a new connection", || {
        node.with(|w| w.connections.len() == 2)
    });
    let since = Instant::now();
    session.wait_until("a read on the new connection", || {
        node.with(|w| w.reads.iter().any(|(id, at)| *id == a.id() && *at > since))
    });
}

/// The node gives up on one attempt of a GET after 60 s (freenet-core `OPERATION_TTL`, the
/// retry driver's per-attempt timeout) and makes up to 4 attempts (`MAX_RETRIES` = 3 more), then
/// reports the failure with an error naming nothing: up to about 4 minutes after the read.
const NODE_ATTEMPT_TTL: Duration = Duration::from_secs(60);
const NODE_GET_ATTEMPTS: u32 = 4;

/// A read blocks other requests for `resync_wait`, past one attempt's limit, and is then kept
/// as late, its errors claimed by nobody, until past the node's limit for all attempts.
#[test]
fn a_read_is_kept_late_past_the_nodes_limit_for_every_attempt() {
    let pace = Pace::default();
    assert!(
        pace.resync_wait >= NODE_ATTEMPT_TTL * 2,
        "{:?}",
        pace.resync_wait
    );
    assert!(pace.resync_wait < pace.owed_limit);
    assert!(
        pace.resync_late > NODE_ATTEMPT_TTL * NODE_GET_ATTEMPTS,
        "{:?}",
        pace.resync_late
    );
}

/// A read given up on, whose error naming nothing comes long after (the node retried it), while
/// a read of the stored rooms started meanwhile is out: the error is claimed by nobody, so it
/// does not abort that read.
#[test]
fn a_very_late_read_error_does_not_abort_a_stored_rooms_read_started_after() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        resync_idle: Duration::from_millis(50),
        resync_wait: Duration::from_millis(300),
        resync_late: Duration::from_secs(60),
        refresh: Duration::from_millis(200),
        delegate_deadline: Duration::from_secs(60),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-very-late", pace);
    node.with(|w| w.read_play = ReadPlay::Silence);
    session.act(&a, Action::Viewing);
    wait_for_a_read(&session, &node, &a);
    // The read is given up on; a read of the stored rooms starts, and the node holds it.
    let lists = node.with(|w| {
        w.silence_next_list = true;
        w.lists
    });
    session.wait_on_one_connection(&node, "a stored-rooms read is out", || {
        node.with(|w| w.lists > lists && !w.silence_next_list)
    });
    let lists = node.with(|w| w.lists);
    node.with(|w| {
        w.push_error(
            ErrorKind::OperationError {
                cause: "GET failed".into(),
            }
            .into(),
        )
    });
    // Taken for the stored rooms' read, it would end it, and the next would start within
    // `refresh`.
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        node.with(|w| w.lists),
        lists,
        "the stored-rooms read was aborted"
    );
}

/// Each `Pace` setting reaches the rules it is for.
#[test]
fn the_read_pacing_comes_from_the_matching_settings() {
    let pace = Pace {
        resync_idle: Duration::from_secs(1),
        resync_focus: Duration::from_secs(2),
        resync_spread: Duration::from_secs(3),
        resubscribe_first: Duration::from_secs(4),
        resubscribe_max: Duration::from_secs(5),
        ..quick()
    };
    let r = pace.resync();
    assert_eq!(
        (r.idle, r.focus, r.spread, r.heal_first, r.heal_max),
        (
            pace.resync_idle,
            pace.resync_focus,
            pace.resync_spread,
            pace.resubscribe_first,
            pace.resubscribe_max
        )
    );
}

/// A read given up on frees the room, but the next waits a whole `resync_idle` from then, so
/// on a node that answers no read the requests that may not go beside one get their turn.
#[test]
fn after_a_read_is_given_up_the_next_waits_a_whole_idle_time() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let wait = Duration::from_millis(400);
    let idle = Duration::from_millis(600);
    let pace = Pace {
        resync_idle: idle,
        resync_wait: wait,
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-gap", pace);
    node.with(|w| w.read_play = ReadPlay::Silence);
    session.act(&a, Action::Viewing);
    session.wait_on_one_connection(&node, "two reads", || {
        node.with(|w| w.reads_of(a.id()) >= 2)
    });
    let gap = node.with(|w| w.reads[1].1 - w.reads[0].1);
    // The node stamps arrival, not sending: 20 ms of slack for that. Without the restart the
    // gap would be about `wait` alone.
    assert!(gap >= wait + idle - Duration::from_millis(20), "{gap:?}");
}

/// An error naming nothing that comes long after the read was asked, while a message is owed
/// and the stored rooms are due to be read again: the read still counts as out, so neither the
/// message nor a read of the stored rooms takes it.
#[test]
fn a_late_read_error_is_not_blamed_on_what_came_after_the_read() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        resync_idle: Duration::from_millis(50),
        resync_wait: Duration::from_secs(10),
        refresh: Duration::from_millis(200),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-late-error", pace);
    // `resync_wait` (10 s) is far past the 1.5 s this test takes, so the read is still out
    // when its error comes, however slow the machine.
    node.with(|w| {
        w.read_play = ReadPlay::Silence;
        w.update_play = UpdatePlay::Ignore;
    });
    session.act(&a, Action::Viewing);
    wait_for_a_read(&session, &node, &a);
    let lists = node.with(|w| w.lists);
    session.act(&a, say("sent meanwhile"));
    session.wait_until("the update reached the node", || {
        node.with(|w| !w.updates.is_empty())
    });
    std::thread::sleep(Duration::from_secs(1));
    node.with(|w| {
        w.push_error(
            ErrorKind::OperationError {
                cause: "GET failed".into(),
            }
            .into(),
        )
    });
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(node.with(|w| w.lists), lists, "a stored-rooms read started");
    let failed = session.last_rooms().is_some_and(|(r, _, _)| {
        r.iter().flat_map(|r| r.messages.iter()).any(|m| {
            m.text == "sent meanwhile" && m.pending.as_ref().is_some_and(|p| p.failed.is_some())
        })
    });
    assert!(!failed, "the read's error was taken for the message's");
}

/// A GET the node gave up on after its retries comes back as `NotFound`, not as an error. One
/// for a read already given up on is that read's: the subscription is not in question, so the
/// room is not shown as not following.
#[test]
fn a_late_reads_not_found_leaves_the_subscription_alone() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        resync_focus: Duration::from_millis(50),
        resync_wait: Duration::from_millis(200),
        resync_late: Duration::from_secs(60),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "resync-late-not-found", pace);
    node.with(|w| w.read_play = ReadPlay::Silence);
    // One read, from coming to the front; no room is open, so no other read follows.
    std::thread::sleep(Duration::from_millis(200));
    session.focus();
    wait_for_a_read(&session, &node, &a);
    // Well past `resync_wait`: the read has been given up on and is late.
    std::thread::sleep(Duration::from_secs(1));
    node.with(|w| {
        let answer: Result<HostResponse, ClientError> =
            Ok(HostResponse::ContractResponse(ContractResponse::NotFound {
                instance_id: a.id(),
            }));
        w.push
            .push(Message::Binary(bincode::serialize(&answer).unwrap().into()));
    });
    std::thread::sleep(Duration::from_millis(500));
    let (_, issues, _) = session.last_rooms().unwrap();
    assert!(issues.not_following.is_empty(), "{issues:?}");
    assert_eq!(node.with(|w| w.connections.len()), 1);
}

/// No read starts while a read of the stored rooms is out.
#[test]
fn no_room_read_starts_beside_a_read_of_the_stored_rooms() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        resync_idle: Duration::from_millis(50),
        refresh: Duration::from_millis(100),
        delegate_deadline: Duration::from_secs(30),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "reload-vs-resync", pace);
    let lists = node.with(|w| {
        w.silence_next_list = true;
        w.lists
    });
    session.wait_on_one_connection(&node, "a stored-rooms read is out", || {
        node.with(|w| w.lists > lists && !w.silence_next_list)
    });
    session.act(&a, Action::Viewing);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(node.with(|w| w.reads_of(a.id())), 0);
}

/// No read starts while a signing key request is out.
#[test]
fn no_room_read_starts_beside_a_signing_key_request() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    node.with(|w| w.key_play.silent = true);
    let pace = Pace {
        resync_idle: Duration::from_millis(50),
        key_delay: Duration::ZERO,
        delegate_deadline: Duration::from_secs(30),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "keys-vs-resync", pace);
    session.wait_on_one_connection(&node, "a key request is out", || {
        node.with(|w| !w.key_reads.is_empty() || !w.key_stores.is_empty())
    });
    session.act(&a, Action::Viewing);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(node.with(|w| w.reads_of(a.id())), 0);
}

/// No read starts while a create has a request out.
#[test]
fn no_room_read_starts_beside_a_creates_request() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let pace = Pace {
        resync_idle: Duration::from_millis(50),
        delegate_deadline: Duration::from_secs(30),
        ..quick()
    };
    let (session, _dir) = following(&node, &[&a], "create-vs-resync", pace);
    node.with(|w| w.silence_first_room_store = true);
    session.list(Action::CreateRoom {
        name: "Book club".into(),
        nickname: "Ada".into(),
        private: false,
    });
    session.wait_on_one_connection(&node, "the create's store is out", || {
        node.with(|w| !w.silence_first_room_store)
    });
    session.act(&a, Action::Viewing);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(node.with(|w| w.reads_of(a.id())), 0);
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
    session.wait_on_one_connection(&node, "both out", || node.with(|w| w.updates.len() == 2));
    let keys: HashSet<ContractKey> = node.with(|w| w.updates.iter().map(|(k, _)| *k).collect());
    assert_eq!(keys, HashSet::from([a.key, b.key]));
    // B's answer settles B, not A.
    node.with(|w| w.push.push(accepted(&b)));
    session.wait_on_one_connection(&node, "B settled", || {
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

/// An error naming nothing is a room read's only when that read is the one thing out that
/// could fail that way.
#[test]
fn an_error_naming_nothing_is_a_reads_only_when_nothing_else_could_have_failed() {
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let pace = quick().resync();
    let t0 = Instant::now();
    let later = t0 + Duration::from_secs(7200);
    let reading = |ids: &[ContractInstanceId]| {
        let mut r = super::super::resync::Resyncs::new(t0);
        r.focus(ids, t0, &pace);
        assert_eq!(r.due(None, ids, &|_| false, later, &pace).len(), ids.len());
        r
    };
    let nameless: ClientError = ErrorKind::OperationError {
        cause: "GET failed".into(),
    }
    .into();
    let named: ClientError =
        ErrorKind::RequestError(RequestError::ContractError(ContractError::Get {
            key: a.key,
            cause: "no route".into(),
        }))
        .into();
    let none = Subscriptions::default();
    let mut quiet = Rooms::default();
    quiet.replace(loaded(vec![a.stored(), b.stored()]));
    let one = reading(&[a.id()]);

    assert_eq!(
        quiet.nameless_read(&nameless, &none, &one, false),
        Some(a.id())
    );
    assert_eq!(
        quiet.nameless_read(&named, &none, &one, false),
        None,
        "it names something"
    );
    assert_eq!(
        quiet.nameless_read(&nameless, &none, &reading(&[a.id(), b.id()]), false),
        None,
        "two reads out"
    );
    let mut asking = Subscriptions::default();
    asking.pending.insert(b.id(), Instant::now());
    assert_eq!(
        quiet.nameless_read(&nameless, &asking, &one, false),
        None,
        "a subscribe is waiting"
    );
    assert_eq!(
        quiet.nameless_read(&nameless, &none, &one, true),
        None,
        "the stored rooms are being read"
    );
    let owed = owing(&[&b]);
    assert_eq!(
        owed.nameless_read(&nameless, &none, &one, false),
        None,
        "an update's answer is owed"
    );
    let mut stale = owing(&[&b]);
    stale.outbox.purge_room(&b.vk(), b.id());
    assert!(stale.outbox.has_stale());
    assert_eq!(
        stale.nameless_read(&nameless, &none, &one, false),
        None,
        "a room left still owes an answer"
    );
    let mut busy = Rooms::default();
    busy.replace(loaded(vec![a.stored()]));
    busy.outbox.work.create("Room", "Ada", false);
    assert!(
        busy.outbox
            .work
            .next_request(false, Instant::now())
            .is_some()
    );
    assert_eq!(
        busy.nameless_read(&nameless, &none, &one, false),
        None,
        "a create has a request out"
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

/// A `rooms_meta` as the web UI wrote it with a muted room and a room order, plus a field this
/// app does not know.
fn web_ui_meta_with_modes(muted: VerifyingKey, order: &[VerifyingKey]) -> Vec<u8> {
    use ciborium::Value;
    let mut out = Vec::new();
    ciborium::ser::into_writer(
        &Value::Map(vec![
            (
                Value::Text("notification_modes".into()),
                Value::Map(vec![(
                    Value::serialized(&muted).unwrap(),
                    Value::serialized(&NotificationMode::Muted).unwrap(),
                )]),
            ),
            (
                Value::Text("room_order".into()),
                Value::serialized(&order.to_vec()).unwrap(),
            ),
            (Value::Text("web_ui_marker".into()), Value::from(11)),
        ]),
        &mut out,
    )
    .unwrap();
    out
}

impl Running {
    fn last_modes(&self) -> HashMap<String, NotificationMode> {
        self.updates()
            .into_iter()
            .rev()
            .find_map(|u| match u {
                RoomsUpdate::Rooms { modes, .. } => Some(modes),
                _ => None,
            })
            .unwrap_or_default()
    }
}

/// The two apps share the setting: the mode the web UI stored is shown, a pick here is stored in
/// the same `rooms_meta` without losing the web UI's order or unknown fields, and a new run
/// reads the pick back.
#[test]
fn a_room_mode_is_read_from_the_web_ui_stored_here_and_read_back_on_the_next_run() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 2, &["b1"]);
    node.with(|w| {
        w.slots.insert(
            ROOMS_META_KEY.to_vec(),
            web_ui_meta_with_modes(a.vk(), &[b.vk(), a.vk()]),
        );
    });
    let (session, dir) = following(&node, &[&a, &b], "modes-roundtrip", quick());
    let key = |r: &TestRoom| bs58::encode(r.vk().to_bytes()).into_string();
    // Muted in the browser: muted here.
    assert_eq!(
        session.last_modes().get(&key(&a)),
        Some(&NotificationMode::Muted)
    );
    assert_eq!(session.last_modes().get(&key(&b)), None);

    session.act(
        &b,
        Action::SetNotificationMode {
            mode: NotificationMode::MentionsAndReplies,
        },
    );
    session.wait_until("the pick is stored", || {
        node.with(|w| {
            crate::rooms::modes::decode_modes(w.slots.get(ROOMS_META_KEY).unwrap())
                .contains_key(&b.vk())
        })
    });
    node.with(|w| {
        let meta = w.slots.get(ROOMS_META_KEY).unwrap();
        let modes = crate::rooms::modes::decode_modes(meta);
        assert_eq!(modes[&b.vk()], NotificationMode::MentionsAndReplies);
        // The web UI's own entry, order and unknown field are as it left them.
        assert_eq!(modes[&a.vk()], NotificationMode::Muted);
        assert_eq!(
            decode_rooms_meta(meta).unwrap().room_order,
            vec![b.vk(), a.vk()]
        );
        assert_eq!(meta_marker(meta), Some(11));
        assert_eq!(w.overlaps, 0);
    });
    assert_eq!(
        session.last_modes().get(&key(&b)),
        Some(&NotificationMode::MentionsAndReplies)
    );
    drop(session);

    // A new run (as after restarting the app) reads the pick back from the store.
    let again = Running::start(&node, &dir, quick());
    again.wait_rooms("the rooms are live", |r, _, s| {
        r.len() == 2 && s == LinkStatus::Live
    });
    let modes = again.last_modes();
    assert_eq!(
        modes.get(&key(&b)),
        Some(&NotificationMode::MentionsAndReplies)
    );
    assert_eq!(modes.get(&key(&a)), Some(&NotificationMode::Muted));
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
    // The check after the store follows the store's answer, so it is waited for too: reading
    // the count as soon as the key is stored raced it (1 in 60 on main).
    session.wait_until(
        "the key is stored after registering again, and checked",
        || node.with(|w| w.signing_keys.len() == 1 && w.key_reads.len() >= 3),
    );
    // Time for an extra read, which would be a bug, to show.
    std::thread::sleep(Duration::from_millis(100));
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

// Export ID and Import ID -----------------------------------------------------------------------

fn exports(session: &Running) -> Vec<(String, Result<String, String>)> {
    session
        .updates()
        .into_iter()
        .filter_map(|u| match u {
            RoomsUpdate::Export { room, token } => Some((room, token)),
            _ => None,
        })
        .collect()
}

/// An export asked for in a room is made from the stored room, reported once, and nothing is
/// written or sent for it.
#[test]
fn an_identity_export_is_made_from_the_stored_room_and_nothing_is_written() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "export", quick());
    node.with(|w| w.cas.clear());
    session.act(&a, Action::ExportId);
    session.wait_until("the export is reported", || !exports(&session).is_empty());
    let (room, token) = exports(&session).remove(0);
    assert_eq!(room, bs58::encode(a.vk().to_bytes()).into_string());
    let export = IdentityExport::from_armored_string(&token.unwrap()).unwrap();
    assert_eq!(export.room_owner, a.vk());
    assert_eq!(export.signing_key.to_bytes(), a.owner.to_bytes());
    assert_eq!(export.room_name.as_deref(), Some("A"));
    // Quiescence: a later command is answered, so everything the export could have caused was
    // processed before it (the session handles commands in order).
    session.act(&a, Action::Invite);
    session.wait_until("the later command is answered", || {
        !session.invites().is_empty()
    });
    node.with(|w| assert!(w.cas.is_empty() && w.updates.is_empty() && w.puts.is_empty()));
}

/// An export asked for in a room that has gone is said to have failed, and the report that
/// carries a token never prints it.
#[test]
fn an_identity_export_for_a_room_gone_is_said_to_have_failed() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let gone = TestRoom::new(2, "B", 1, &["b1"]);
    let (session, _dir) = following(&node, &[&a], "export-gone", quick());
    session.act(&gone, Action::ExportId);
    session.wait_until("the export is said to have failed", || {
        exports(&session).iter().any(|(room, t)| {
            *room == bs58::encode(gone.vk().to_bytes()).into_string() && t.is_err()
        })
    });
}

/// A member of room `room` (invited by its owner), as an identity token.
fn member_token(room: &TestRoom, seed: u8, nickname: &str) -> (SigningKey, String) {
    let member = SigningKey::from_bytes(&[seed; 32]);
    let owner_id = MemberId::from(&room.vk());
    let export = IdentityExport {
        room_owner: room.vk(),
        signing_key: member.clone(),
        authorized_member: AuthorizedMember::new(
            Member {
                owner_member_id: owner_id,
                invited_by: owner_id,
                member_vk: member.verifying_key(),
            },
            &room.owner,
        ),
        invite_chain: vec![],
        member_info: None,
        room_name: None,
        self_nickname: Some(nickname.into()),
        invitation_secrets: Default::default(),
    };
    (member, export.to_armored_string())
}

/// Importing the identity of a room this node's store does not hold reads the room from the
/// network, stores it under that identity, and shows it.
#[test]
fn importing_an_identity_adds_the_room_under_that_identity() {
    let node = fake_node(vec![]);
    let room = TestRoom::new(3, "Garden", 1, &["t1"]);
    let real = super::super::create::contract_key(&room.vk());
    node.with(|w| {
        w.contracts
            .insert(*real.id(), (real, Contract::State(cbor(&room.state))));
    });
    let other = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&other], "import-add", quick());
    let (member, token) = member_token(&room, 70, "Bea");
    session.list(Action::ImportId {
        token: IdentityToken(token),
        replace: false,
    });
    session.wait_rooms("the imported room is in the list", |r, _, _| {
        r.iter().any(|r| r.name == "Garden")
    });
    node.with(|w| {
        let StoredRoomSlot::Present(stored) =
            decode_room_slot(&w.slots[&room_storage_key(&room.vk())]).unwrap()
        else {
            panic!("a present room");
        };
        assert_eq!(stored.self_verifying_key(), Some(member.verifying_key()));
        assert_eq!(stored.self_nickname.as_deref(), Some("Bea"));
        assert_eq!(stored.contract_key, real);
        // Nothing was sent to the room: an import is not a join.
        assert!(w.updates.is_empty() && w.puts.is_empty());
        assert_eq!(w.overlaps, 0);
    });
}

/// Replacing keeps the stored room and its fields, and swaps the identity; the room then shows
/// as the imported member.
#[test]
fn replacing_an_identity_swaps_it_in_the_stored_room_and_keeps_the_rest() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "import-replace", quick());
    let slot = room_storage_key(&a.vk());
    let before = node.with(|w| w.slots[&slot].clone());
    let (member, token) = member_token(&a, 71, "Bea");
    session.list(Action::ImportId {
        token: IdentityToken(token),
        replace: true,
    });
    session.wait_until("the identity is replaced", || {
        node.with(|w| {
            matches!(decode_room_slot(&w.slots[&slot]),
                Ok(StoredRoomSlot::Present(r)) if r.self_verifying_key() == Some(member.verifying_key()))
        })
    });
    node.with(|w| {
        let StoredRoomSlot::Present(now) = decode_room_slot(&w.slots[&slot]).unwrap() else {
            panic!("present");
        };
        let StoredRoomSlot::Present(was) = decode_room_slot(&before).unwrap() else {
            panic!("present");
        };
        assert_eq!(now.contract_key, was.contract_key);
        assert_eq!(now.owner_vk, was.owner_vk);
        assert_eq!(now.self_nickname.as_deref(), Some("Bea"));
        assert_eq!(
            cbor(&now.room_state),
            cbor(&was.room_state),
            "the room's state is kept"
        );
        assert_eq!(w.overlaps, 0);
    });
    // The screen follows: the room is still there, and you are the imported member.
    session.wait_rooms("the room is still shown", |r, _, _| {
        r.iter().any(|r| r.name == "A")
    });
}

/// Room "A" with `member` (seed `seed`) already in its members and holding no nickname record,
/// as a member the room has but nobody has heard a name from.
fn room_with_nameless_member(seed: u8) -> (TestRoom, SigningKey) {
    let mut a = TestRoom::new(1, "A", 1, &["a1"]);
    let member = SigningKey::from_bytes(&[seed; 32]);
    let owner_id = MemberId::from(&a.vk());
    a.state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member.verifying_key(),
        },
        &a.owner,
    ));
    // A message of theirs, so the room does not prune them as inactive.
    a.state
        .recent_messages
        .messages
        .push(AuthorizedMessageV1::new(
            MessageV1 {
                room_owner: owner_id,
                author: MemberId::from(&member.verifying_key()),
                time: SystemTime::now() - Duration::from_secs(10),
                content: RoomMessageBody::public("hello".into()),
            },
            &member,
        ));
    (a, member)
}

/// After a replace, the imported member the room holds with no nickname record gets one
/// published, signed with their own key (the web UI's member-info heal, river#414).
#[test]
fn replacing_an_identity_publishes_the_nickname_the_room_lacks_for_it() {
    let node = fake_node(vec![]);
    let (a, member) = room_with_nameless_member(72);
    let (session, _dir) = following(&node, &[&a], "import-heal", quick());
    let (_, token) = member_token(&a, 72, "Bea");
    session.list(Action::ImportId {
        token: IdentityToken(token),
        replace: true,
    });
    session.wait_until("the nickname record is published", || {
        node.with(|w| w.updates.iter().any(|(_, d)| d.member_info.is_some()))
    });
    node.with(|w| {
        let records: Vec<_> = w
            .updates
            .iter()
            .filter_map(|(_, d)| d.member_info.as_ref())
            .flatten()
            .collect();
        assert_eq!(records.len(), 1, "once");
        let r = records[0];
        assert_eq!(
            r.member_info.member_id,
            MemberId::from(&member.verifying_key())
        );
        r.verify_signature_with_key(&member.verifying_key())
            .expect("signed by the imported key");
        assert_eq!(
            r.member_info.preferred_nickname,
            river_core::room_state::privacy::SealedBytes::public(b"Bea".to_vec())
        );
    });
}

/// A stored copy of the room that lacks the member's nickname record while the network's has
/// it: the heal is decided on the network's state, so the newer record is never published over.
#[test]
fn a_stale_stored_state_does_not_make_the_heal_publish_over_the_networks_record() {
    let node = fake_node(vec![]);
    let (stale, member) = room_with_nameless_member(72);
    let mut network = stale.state.clone();
    network.member_info.member_info.push(
        river_core::room_state::member_info::AuthorizedMemberInfo::new_with_member_key(
            river_core::room_state::member_info::MemberInfo {
                member_id: MemberId::from(&member.verifying_key()),
                version: 3,
                preferred_nickname: river_core::room_state::privacy::SealedBytes::public(
                    b"Newer on the network".to_vec(),
                ),
                deputies: Vec::new(),
            },
            &member,
        ),
    );
    let (session, _dir) = following(&node, &[&stale], "import-stale", quick());
    // The network has the record the stored copy lacks.
    node.with(|w| {
        w.contracts
            .insert(stale.id(), (stale.key, Contract::State(cbor(&network))));
    });
    let (_, token) = member_token(&stale, 72, "Bea");
    session.list(Action::ImportId {
        token: IdentityToken(token),
        replace: true,
    });
    session.wait_until("the identity is replaced", || {
        node.with(|w| {
            matches!(decode_room_slot(&w.slots[&room_storage_key(&stale.vk())]),
                Ok(StoredRoomSlot::Present(r)) if r.self_verifying_key() == Some(member.verifying_key()))
        })
    });
    session.act(&stale, Action::Invite);
    session.wait_until("a later command is answered", || {
        !session.invites().is_empty()
    });
    node.with(|w| assert!(w.updates.iter().all(|(_, d)| d.member_info.is_none())));
}

/// In a private room whose secret the imported identity does not have, nothing is published and
/// no failed "Your nickname" entry appears: the heal waits for the secret.
#[test]
fn the_heal_in_a_private_room_without_the_secret_is_skipped_quietly() {
    let node = fake_node(vec![]);
    let mut a = TestRoom::private(1);
    let member = SigningKey::from_bytes(&[72; 32]);
    let owner_id = MemberId::from(&a.vk());
    a.state.members.members.push(AuthorizedMember::new(
        Member {
            owner_member_id: owner_id,
            invited_by: owner_id,
            member_vk: member.verifying_key(),
        },
        &a.owner,
    ));
    let (session, _dir) = following(&node, &[&a], "import-private-heal", quick());
    let (_, token) = member_token(&a, 72, "Bea");
    session.list(Action::ImportId {
        token: IdentityToken(token),
        replace: true,
    });
    session.wait_until("the identity is replaced", || {
        node.with(|w| {
            matches!(decode_room_slot(&w.slots[&room_storage_key(&a.vk())]),
                Ok(StoredRoomSlot::Present(r)) if r.self_verifying_key() == Some(member.verifying_key()))
        })
    });
    session.act(&a, Action::Invite);
    session.wait_until("a later command is answered", || {
        !session.invites().is_empty()
    });
    node.with(|w| assert!(w.updates.iter().all(|(_, d)| d.member_info.is_none())));
    let (rooms, _, _) = session.last_rooms().unwrap();
    assert!(
        rooms.iter().all(|r| r.notices.is_empty()),
        "no unexplained failure"
    );
}

/// A replace asks for the room's current state again, and the heal waits for it: the member-info
/// check is made on a read after the import, not on one from before it.
#[test]
fn a_replace_reads_the_room_again_and_the_heal_waits_for_that_read() {
    let node = fake_node(vec![]);
    let (a, member) = room_with_nameless_member(72);
    let (session, _dir) = following(&node, &[&a], "import-refresh", quick());
    let before = node.with(|w| w.subscribes_to(a.id()));
    let (_, token) = member_token(&a, 72, "Bea");
    session.list(Action::ImportId {
        token: IdentityToken(token),
        replace: true,
    });
    session.wait_until("the room is asked for again", || {
        node.with(|w| w.subscribes_to(a.id()) > before)
    });
    session.wait_until("the nickname record is published", || {
        node.with(|w| w.updates.iter().any(|(_, d)| d.member_info.is_some()))
    });
    let _ = member;
}

/// A heal that never gets its read says so instead of vanishing.
#[test]
fn a_heal_that_cannot_be_decided_says_so() {
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let mut rooms = Rooms::default();
    rooms.list.push(LiveRoom::new(a.stored()));
    rooms.heals.push((
        super::super::work::ImportDone {
            fetched: false,
            room: a.vk(),
            identity: a.vk(),
            nickname: Some("Bea".into()),
        },
        500,
    ));
    // No network state was seen after the import.
    rooms.run_heals();
    assert!(rooms.heals.is_empty());
    let view = rooms.outbox.view(&rooms.list[0], &local_minutes);
    assert!(
        view.notices
            .iter()
            .any(|n| n.text.contains("could not be checked yet")),
        "{:?}",
        view.notices
            .iter()
            .map(|n| n.text.clone())
            .collect::<Vec<_>>()
    );
}

/// The owner has no nickname record to heal, and a member who already has one is left alone.
#[test]
fn replacing_an_identity_heals_nobody_the_room_already_names() {
    let node = fake_node(vec![]);
    let (mut a, member) = room_with_nameless_member(72);
    a.state.member_info.member_info.push(
        river_core::room_state::member_info::AuthorizedMemberInfo::new_with_member_key(
            river_core::room_state::member_info::MemberInfo {
                member_id: MemberId::from(&member.verifying_key()),
                version: 0,
                preferred_nickname: river_core::room_state::privacy::SealedBytes::public(
                    b"Already".to_vec(),
                ),
                deputies: Vec::new(),
            },
            &member,
        ),
    );
    let (session, _dir) = following(&node, &[&a], "import-no-heal", quick());
    let (_, token) = member_token(&a, 72, "Bea");
    session.list(Action::ImportId {
        token: IdentityToken(token),
        replace: true,
    });
    session.wait_until("the identity is replaced", || {
        node.with(|w| {
            matches!(decode_room_slot(&w.slots[&room_storage_key(&a.vk())]),
                Ok(StoredRoomSlot::Present(r)) if r.self_verifying_key() == Some(member.verifying_key()))
        })
    });
    session.wait_rooms("the room shows again", |r, _, _| {
        r.iter().any(|r| r.name == "A")
    });
    node.with(|w| assert!(w.updates.iter().all(|(_, d)| d.member_info.is_none())));
}

/// What the app does to the delegate's key namespace after an import: the key the delegate
/// holds for the room becomes the imported one, over the old one, as the web UI's import does
/// (`migrate_signing_key` with the authoritative flag). Without it the delegate's own signing
/// (rotation of a private room by its owner) would go on with the key the user replaced.
#[test]
fn an_import_replaces_the_delegates_key_for_the_room_with_the_imported_one() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "import-key", quick());
    session.wait_until("the room's own key is in the delegate", || {
        node.with(|w| w.signing_keys.get(&a.vk().to_bytes()) == Some(&a.owner.to_bytes().to_vec()))
    });
    let (member, token) = member_token(&a, 73, "Bea");
    session.list(Action::ImportId {
        token: IdentityToken(token),
        replace: true,
    });
    session.wait_until("the delegate holds the imported key", || {
        node.with(|w| w.signing_keys.get(&a.vk().to_bytes()) == Some(&member.to_bytes().to_vec()))
    });
}

/// A room the user left (its key holds a tombstone) is added by an import, as an explicit
/// rejoin: the web UI's import clears a tombstone too.
#[test]
fn importing_an_identity_for_a_left_room_adds_it_over_the_tombstone() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let real = super::super::create::contract_key(&a.vk());
    node.with(|w| {
        w.contracts
            .insert(*real.id(), (real, Contract::State(cbor(&a.state))));
    });
    let (session, _dir) = following(&node, &[&a, &b], "import-after-leave", quick());
    session.act(&a, Action::LeaveRoom);
    session.wait_until("the leave is stored and done", || {
        tombstoned(&node, &a) && session.last_work().is_empty()
    });
    session.wait_rooms("the room is gone", |r, _, _| {
        r.len() == 1 && r[0].name == "B"
    });
    let (member, token) = member_token(&a, 74, "Bea");
    session.list(Action::ImportId {
        token: IdentityToken(token),
        replace: false,
    });
    session.wait_rooms("the room is back", |r, _, _| {
        r.iter().any(|r| r.name == "A")
    });
    node.with(|w| {
        let StoredRoomSlot::Present(now) =
            decode_room_slot(&w.slots[&room_storage_key(&a.vk())]).unwrap()
        else {
            panic!("the tombstone was replaced by the room");
        };
        assert_eq!(now.self_verifying_key(), Some(member.verifying_key()));
        assert_eq!(w.overlaps, 0);
    });
}

/// An import sent right behind a leave of the same room runs after it, finds the tombstone and
/// adds the room: nothing is refused, and the leave is not tried again over the imported room.
#[test]
fn an_import_queued_behind_a_leave_adds_the_room_after_it() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let real = super::super::create::contract_key(&a.vk());
    node.with(|w| {
        w.contracts
            .insert(*real.id(), (real, Contract::State(cbor(&a.state))));
    });
    let (session, _dir) = following(&node, &[&a, &b], "import-while-leaving", quick());
    let (member, token) = member_token(&a, 75, "Bea");
    session.act(&a, Action::LeaveRoom);
    session.list(Action::ImportId {
        token: IdentityToken(token),
        replace: false,
    });
    session.wait_until("the imported room is stored under the imported key", || {
        node.with(|w| {
            matches!(decode_room_slot(&w.slots[&room_storage_key(&a.vk())]),
                Ok(StoredRoomSlot::Present(r)) if r.self_verifying_key() == Some(member.verifying_key()))
        })
    });
    session.wait_rooms("both rooms show", |r, _, _| r.len() == 2);
    session.wait_until("no task is left", || session.last_work().is_empty());
    node.with(|w| {
        assert_eq!(w.overlaps, 0);
        // The leave wrote its tombstone first, then the import replaced it; nothing came after.
        let stores: Vec<bool> = w
            .cas
            .iter()
            .filter(|(k, _)| *k == room_storage_key(&a.vk()))
            .map(|(_, ok)| *ok)
            .collect();
        assert_eq!(stores, [true, true], "{stores:?}");
    });
}

/// A replace sent behind a leave has no identity left to replace: the room stays left.
#[test]
fn a_replace_queued_behind_a_leave_leaves_the_room_left() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 1, &["b1"]);
    let (session, _dir) = following(&node, &[&a, &b], "replace-while-leaving", quick());
    let (_, token) = member_token(&a, 76, "Bea");
    session.act(&a, Action::LeaveRoom);
    session.list(Action::ImportId {
        token: IdentityToken(token),
        replace: true,
    });
    session.wait_until("the replace is refused", || {
        session.last_work().iter().any(|w| {
            matches!(&w.state, WorkState::Failed { why, retry: false }
                if why.contains("no longer in River's storage"))
        })
    });
    assert!(tombstoned(&node, &a), "the leave stands");
    session.wait_rooms("only B shows", |r, _, _| r.len() == 1 && r[0].name == "B");
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
    // The node keeps refusing: the app's own retries follow the backoff schedule. The stamp
    // is when the node RECEIVED each registration, which is the app's decision to retry plus
    // the setup of the new connection before the registration goes out. That setup varies
    // with load (hundreds of ms on a busy macOS runner), and the first registration is on
    // the first connection while each later one follows a reconnect, so individual gaps do
    // not follow the schedule. Their sum does: the decisions are at least 150 + 300 + 600 ms
    // apart, and each stamp only comes later than its decision. A wait that did not grow
    // (150 ms each time) gives about 450 ms here. The schedule itself is pinned exactly by
    // `the_store_retry_waits_double_from_the_first_wait_up_to_the_max`.
    session.wait_until("four registrations", || {
        node.with(|w| w.register_times.len() >= 4)
    });
    node.with(|w| {
        let times = &w.register_times;
        let span = times[3].duration_since(times[0]);
        assert!(
            span >= Duration::from_millis(150 + 300 + 600),
            "registrations 1 to 4 took {span:?}, under the backoff schedule 150/300/600 ms"
        );
    });
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
            _: &dyn Fn(ContractInstanceId, Fetch),
        ) {
            std::thread::sleep(Duration::from_secs(60));
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
    // From "setting up" to registered, the screen never said the store could not be set up
    // (the reconnect in between included).
    let shown: Vec<Issues> = session
        .updates()
        .into_iter()
        .filter_map(|u| match u {
            RoomsUpdate::Rooms { issues, .. } => Some(issues),
            _ => None,
        })
        .skip_while(|i| !i.setting_up)
        .take_while(|i| i.store_missing)
        .collect();
    assert!(!shown.is_empty());
    assert!(
        shown.iter().all(|i| i.setting_up),
        "\"could not set up\" flashed before registering: {shown:?}"
    );
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

/// The connection drops while a registration waits for the first check. The next connection
/// decides afresh: River's storage is registered once, not twice.
#[test]
fn a_drop_while_registration_waits_registers_once() {
    let node = fake_node(vec![]);
    node.with(|w| w.unregistered = true);
    let dir = temp_dir("deferred-drop");
    let source = Arc::new(FakePointers::default());
    *source.delay.lock().unwrap() = Duration::from_millis(500);
    let watch = watched(&source, Some(1));
    // The next connection starts after the check has settled: the case where a stale "put
    // off" from the dropped connection would register a second time.
    let pace = Pace {
        backoff_first: Duration::from_millis(900),
        backoff_max: Duration::from_millis(900),
        ..quick()
    };
    let session = Running::start_watched(&node, &dir, pace, watch);
    session.wait_rooms("setting up", |_, issues, _| issues.setting_up);
    node.with(|w| w.drop_live = true);
    session.wait_rooms("registered and read", |r, issues, _| {
        r.is_empty() && !issues.store_missing && !issues.setting_up
    });
    // Time for a second, needless registration to show: past another reconnect's backoff.
    std::thread::sleep(Duration::from_millis(2000));
    let connections = node.with(|w| w.connections.len());
    assert_eq!(connections, 2, "no needless reconnect");
    let registers = node.with(|w| w.sent.iter().filter(|(_, k)| *k == "register").count());
    assert_eq!(registers, 1);
}

/// The first read of the stored rooms takes longer than `first_wait` and finds no store. A
/// newer record arrived while the session waited on that read: it was judged as it arrived,
/// so no registration is sent, although the startup wait has run out.
#[test]
fn a_newer_record_during_a_slow_first_read_stops_the_registration() {
    let node = fake_node(vec![]);
    let source = Arc::new(FakePointers::default());
    *source.delay.lock().unwrap() = Duration::from_millis(100);
    let watch = watched(&source, Some(2));
    let slow = Arc::new(AtomicBool::new(true));
    node.with(|w| {
        w.unregistered = true;
        let (slow, watch) = (slow.clone(), watch.clone());
        // The first listing is answered only once the newer record has closed the gate (at
        // most 5 s), and past `first_wait`: the session is blocked in that read meanwhile.
        w.on_list = Some(Box::new(move || {
            if slow.swap(false, Ordering::SeqCst) {
                let started = Instant::now();
                while !watch.gate().is_closed() && started.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        }));
    });
    let dir = temp_dir("slow-read-newer");
    save_token(&dir, "kept-token");
    let pace = Pace {
        pointers: CheckPace {
            first_wait: Duration::from_millis(250),
            ..quick().pointers
        },
        ..quick()
    };
    let session = Running::start_watched(&node, &dir, pace, watch);
    session.wait_rooms("no store, and the update asked for", |_, issues, _| {
        issues.store_missing && issues.update_required
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(writes_taken(&node), (0, 0, 0, 0, 0));
    assert!(node.with(|w| w.unregistered));
}

/// A rename settled in memory is not undone, and does not make the next read rebuild the room:
/// the cache it writes (`self_member_info`, `self_nickname`) is ours until the store has a
/// newer one.
#[test]
fn a_settled_rename_survives_a_store_read_and_a_newer_stored_one_wins() {
    use river_core::room_state::member::MemberId;
    use river_core::room_state::member_info::{AuthorizedMemberInfo, MemberInfo};
    use river_core::room_state::privacy::SealedBytes;
    let mut rooms = Rooms::default();
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    rooms.replace(loaded(vec![a.stored()]));
    let record = |version: u32, name: &str| {
        AuthorizedMemberInfo::new_with_member_key(
            MemberInfo {
                member_id: MemberId::from(&a.owner.verifying_key()),
                version,
                preferred_nickname: SealedBytes::public(name.as_bytes().to_vec()),
                deputies: Vec::new(),
            },
            &a.owner,
        )
    };
    // The rename settled here.
    {
        let kept = rooms.list.first_mut().unwrap();
        kept.stored.self_member_info = Some(record(6, "Renamed"));
        kept.stored.self_nickname = Some("Renamed".into());
    }
    // The store, which has not caught up, is read again: the room is kept, with the rename.
    assert!(rooms.replace(loaded(vec![a.stored()])).is_empty());
    let kept = shown(&rooms, &a);
    assert_eq!(kept.stored.self_nickname.as_deref(), Some("Renamed"));
    assert_eq!(
        kept.stored
            .self_member_info
            .as_ref()
            .unwrap()
            .member_info
            .version,
        6
    );

    // The web UI renamed since: its record is newer, so the store's is taken.
    let mut web = a.stored();
    web.self_member_info = Some(record(9, "From The Web"));
    web.self_nickname = Some("From The Web".into());
    assert!(rooms.replace(loaded(vec![web])).is_empty());
    assert_eq!(
        shown(&rooms, &a).stored.self_nickname.as_deref(),
        Some("From The Web")
    );
    // A record of someone else's, at any version, is not ours to cache.
    let mut foreign = a.stored();
    let other = SigningKey::from_bytes(&[55; 32]);
    foreign.self_member_info = Some(AuthorizedMemberInfo::new_with_member_key(
        MemberInfo {
            member_id: MemberId::from(&other.verifying_key()),
            version: 99,
            preferred_nickname: SealedBytes::public(b"Stranger".to_vec()),
            deputies: Vec::new(),
        },
        &other,
    ));
    foreign.self_nickname = Some("Stranger".into());
    rooms.replace(loaded(vec![foreign]));
    assert_eq!(
        shown(&rooms, &a).stored.self_nickname.as_deref(),
        Some("From The Web")
    );
    // An older stored one is not.
    let mut old = a.stored();
    old.self_member_info = Some(record(2, "Old"));
    old.self_nickname = Some("Old".into());
    rooms.replace(loaded(vec![old]));
    assert_eq!(
        shown(&rooms, &a).stored.self_nickname.as_deref(),
        Some("From The Web")
    );
}

// Leaving a room -----------------------------------------------------------------------------------

/// A `rooms_meta` as the web UI wrote it: `open` is the open room, `order` the room order, and
/// `muted` has a mode; plus a field this app does not know.
fn web_ui_meta_open(open: VerifyingKey, order: &[VerifyingKey], muted: VerifyingKey) -> Vec<u8> {
    use ciborium::Value;
    let mut out = Vec::new();
    ciborium::ser::into_writer(
        &Value::Map(vec![
            (
                Value::Text("current_room_key".into()),
                Value::serialized(&open).unwrap(),
            ),
            (
                Value::Text("notification_modes".into()),
                Value::Map(vec![(
                    Value::serialized(&muted).unwrap(),
                    Value::serialized(&NotificationMode::Muted).unwrap(),
                )]),
            ),
            (
                Value::Text("room_order".into()),
                Value::serialized(&order.to_vec()).unwrap(),
            ),
            (Value::Text("web_ui_marker".into()), Value::from(11)),
        ]),
        &mut out,
    )
    .unwrap();
    out
}

fn is_tombstone(bytes: &[u8]) -> bool {
    matches!(decode_room_slot(bytes), Ok(StoredRoomSlot::Tombstone))
}

/// Leaving writes what the web UI's leave writes, shows the room gone at once, keeps what it
/// does not own, and the room stays gone when the app starts again.
#[test]
fn a_room_left_here_is_a_tombstone_for_both_apps_and_stays_gone_on_the_next_run() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 2, &["b1"]);
    node.with(|w| {
        w.slots.insert(
            ROOMS_META_KEY.to_vec(),
            web_ui_meta_open(b.vk(), &[b.vk(), a.vk()], b.vk()),
        );
    });
    let (session, dir) = following(&node, &[&a, &b], "leave-roundtrip", quick());
    let a_slot_before = node.with(|w| w.slots.get(&room_storage_key(&a.vk())).cloned());
    session.act(&b, Action::LeaveRoom);
    session.wait_rooms("the room is gone from the list", |r, _, _| {
        r.len() == 1 && r[0].name == "A"
    });
    session.wait_until("the leave is done", || {
        session.last_work().is_empty()
            && node.with(|w| {
                w.slots
                    .get(ROOMS_META_KEY)
                    .and_then(|m| decode_rooms_meta(m).ok())
                    .is_some_and(|m| !m.room_order.contains(&b.vk()))
            })
    });
    node.with(|w| {
        // The web UI's tombstone, byte for byte, and the other room untouched.
        let slot = w.slots.get(&room_storage_key(&b.vk())).unwrap();
        assert!(is_tombstone(slot));
        assert_eq!(slot, &[&[0x69][..], b"Tombstone"].concat());
        assert_eq!(
            w.slots.get(&room_storage_key(&a.vk())),
            a_slot_before.as_ref()
        );
        let meta = w.slots.get(ROOMS_META_KEY).unwrap();
        let read = decode_rooms_meta(meta).unwrap();
        assert_eq!(read.room_order, vec![a.vk()]);
        assert_eq!(read.current_room_key, None);
        // The web UI keeps the mode of a room left, and the unknown field.
        assert_eq!(
            crate::rooms::modes::decode_modes(meta)[&b.vk()],
            NotificationMode::Muted
        );
        assert_eq!(meta_marker(meta), Some(11));
        assert_eq!(w.overlaps, 0);
    });
    drop(session);

    // A new run, as after restarting the app, reads the store and does not bring it back.
    let again = Running::start(&node, &dir, quick());
    again.wait_rooms("the rooms are live", |r, _, s| {
        r.len() == 1 && s == LinkStatus::Live
    });
    assert!(again.last_rooms().unwrap().0.iter().all(|r| r.name != "B"));
}

/// The web UI writes the room between this app's read and its store: the store is refused, and
/// the tombstone goes over what the web UI wrote.
#[test]
fn a_room_the_web_ui_changed_meanwhile_is_still_tombstoned() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 2, &["b1"]);
    let key = room_storage_key(&b.vk());
    node.with(|w| {
        let again = b.slot();
        let k = key.clone();
        let mut once = true;
        w.before_cas = Some(Box::new(move |slots, generations, cas_key| {
            if cas_key == k.as_slice() && std::mem::take(&mut once) {
                slots.insert(k.clone(), again.clone());
                *generations.entry(k.clone()).or_default() += 1;
            }
        }));
    });
    let (session, _dir) = following(&node, &[&a, &b], "leave-conflict", quick());
    session.act(&b, Action::LeaveRoom);
    session.wait_until("the tombstone is stored", || {
        node.with(|w| w.slots.get(&key).is_some_and(|s| is_tombstone(s)))
    });
    node.with(|w| {
        let stores: Vec<bool> = w
            .cas
            .iter()
            .filter(|(k, _)| *k == key)
            .map(|(_, ok)| *ok)
            .collect();
        assert_eq!(stores, [false, true], "refused once, then taken");
        assert_eq!(w.overlaps, 0);
    });
    session.wait_rooms("the room is gone", |r, _, _| {
        r.len() == 1 && r[0].name == "A"
    });
}

/// Leaving the only room leaves an empty list, not a store that reads as missing.
#[test]
fn leaving_the_last_room_empties_the_list() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "leave-last", quick());
    session.act(&a, Action::LeaveRoom);
    session.wait_rooms("no rooms are shown", |r, i, _| {
        r.is_empty() && !i.store_missing
    });
    session.wait_until("the leave is done", || {
        session.last_work().is_empty()
            && node.with(|w| {
                w.slots
                    .get(&room_storage_key(&a.vk()))
                    .is_some_and(|s| is_tombstone(s))
            })
    });
}

/// Go offline: every new connection is dropped until `node.plays` is cleared.
fn go_offline(node: &FakeNode, session: &Running) {
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
}

fn tombstoned(node: &FakeNode, room: &TestRoom) -> bool {
    node.with(|w| {
        w.slots
            .get(&room_storage_key(&room.vk()))
            .is_some_and(|s| is_tombstone(s))
    })
}

/// A message written for the only room, then the room is left while offline: when the
/// connection returns, nothing is sent to the room or subscribed to, and the room is gone.
#[test]
fn leaving_the_only_room_drops_what_was_unsent_for_good() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let (session, _dir) = following(&node, &[&a], "leave-unsent-only", quick());
    go_offline(&node, &session);
    session.act(&a, say("never to be sent"));
    session.wait_until("shown while disconnected", || {
        matches!(delivery_of(&session, "never to be sent"), Some(Some(_)))
    });
    session.act(&a, Action::LeaveRoom);
    session.wait_rooms("the room is gone at once", |r, _, _| r.is_empty());
    let subscribed = node.with(|w| w.subscribes_to(a.id()));
    node.with(|w| w.plays.clear());
    session.wait_until("the leave is stored", || tombstoned(&node, &a));
    session.wait_until("the leave is done", || session.last_work().is_empty());
    // Live again with no rooms: nothing was sent to the room or asked for.
    session.wait_rooms("live and empty", |r, _, s| {
        r.is_empty() && s == LinkStatus::Live
    });
    node.with(|w| {
        assert!(
            w.updates.is_empty(),
            "an update was sent: {:?}",
            w.updates.len()
        );
        assert_eq!(
            w.subscribes_to(a.id()),
            subscribed,
            "the room was subscribed again"
        );
    });
}

/// A message queued before Confirm, with another task working ahead of the leave, is not
/// published once the connection returns.
#[test]
fn a_message_queued_before_leaving_behind_another_task_is_not_sent() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 2, &["b1"]);
    let (session, _dir) = following(&node, &[&a, &b], "leave-unsent-queued", quick());
    go_offline(&node, &session);
    session.act(&a, say("queued before confirm"));
    session.list(Action::CreateRoom {
        name: "Book club".into(),
        nickname: "Ada".into(),
        private: false,
    });
    session.act(&a, Action::LeaveRoom);
    node.with(|w| w.plays.clear());
    session.wait_until("the leave is stored", || tombstoned(&node, &a));
    session.wait_rooms("the new room is shown and A is not", |r, _, _| {
        r.iter().any(|r| r.name == "Book club") && r.iter().all(|r| r.name != "A")
    });
    assert!(node.with(|w| w.updates.is_empty()), "an update was sent");
}

/// Leaving the last room, then accepting an invitation to it: the join is not refused as "you
/// are already a member".
#[test]
fn a_room_left_can_be_joined_again_at_once() {
    let node = fake_node(vec![]);
    let room = TestRoom::new(3, "Theirs", 1, &["t1"]);
    let real = super::super::create::contract_key(&room.vk());
    let (_, link) =
        super::super::create::invitation(&LiveRoom::new(room.stored()), "http://x/").unwrap();
    node.with(|w| {
        w.contracts
            .insert(*real.id(), (real, Contract::State(cbor(&room.state))));
    });
    let (session, _dir) = following(&node, &[&room], "leave-rejoin", quick());
    session.act(&room, Action::LeaveRoom);
    session.wait_rooms("no rooms are shown", |r, _, _| r.is_empty());
    session.wait_until("the leave is stored", || tombstoned(&node, &room));
    session.list(Action::Join {
        code: InviteCode(link),
        nickname: "Bea".into(),
    });
    session.wait_rooms("the room is back", |r, _, _| {
        r.iter().any(|r| r.name == "Theirs")
    });
    assert!(
        !session
            .last_work()
            .iter()
            .any(|w| matches!(&w.state, WorkState::Failed { .. }))
    );
}

/// A leave the store refuses puts the room back on screen, says why, and Try again takes it
/// off again once the store takes the write.
#[test]
fn a_failed_leave_restores_the_room_and_try_again_finishes_it() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 2, &["b1"]);
    let key = room_storage_key(&b.vk());
    node.with(|w| w.fail_cas = Some(key.clone()));
    let (session, _dir) = following(&node, &[&a, &b], "leave-fails", quick());
    session.act(&b, Action::LeaveRoom);
    session.wait_until("the leave failed and the room is shown again", || {
        session.last_work().iter().any(
            |w| matches!(&w.state, WorkState::Failed { why, retry: true } if why.contains("disk")),
        ) && session
            .last_rooms()
            .is_some_and(|(r, _, _)| r.iter().any(|r| r.name == "B"))
    });
    assert!(!tombstoned(&node, &b));
    node.with(|w| w.fail_cas = None);
    let id = session.last_work()[0].id;
    session.list(Action::RetryWork { id });
    session.wait_until("the tombstone is stored", || tombstoned(&node, &b));
    session.wait_rooms("the room is gone", |r, _, _| {
        r.len() == 1 && r[0].name == "A"
    });
}

/// The leave timed out though its tombstone was written; the room is joined again (here, by the
/// web UI, under another key); Try again on the old row leaves the new room alone.
#[test]
fn trying_a_failed_leave_again_never_tombstones_a_room_joined_since() {
    let node = fake_node(vec![]);
    let a = TestRoom::new(1, "A", 1, &["a1"]);
    let b = TestRoom::new(2, "B", 2, &["b1"]);
    let key = room_storage_key(&b.vk());
    node.with(|w| w.fail_cas = Some(key.clone()));
    let (session, _dir) = following(&node, &[&a, &b], "leave-stale-retry", quick());
    session.act(&b, Action::LeaveRoom);
    session.wait_until("the leave failed with Try again", || {
        session
            .last_work()
            .iter()
            .any(|w| matches!(&w.state, WorkState::Failed { retry: true, .. }))
    });
    // The room is joined again under another key before Try again is pressed.
    let rejoined = cbor(&Slot::Present(Box::new(SlotData {
        owner_vk: b.vk(),
        room_state: b.state.clone(),
        self_sk: Some(SigningKey::from_bytes(&[77; 32])),
        self_vk: None,
        contract_key: b.key,
        last_read_message_id: None,
        self_nickname: None,
        invitation_secrets: BTreeMap::new(),
    })));
    node.with(|w| {
        w.fail_cas = None;
        w.slots.insert(key.clone(), rejoined.clone());
    });
    let id = session.last_work()[0].id;
    session.list(Action::RetryWork { id });
    session.wait_until("the retry is refused", || {
        session.last_work().iter().any(|w| {
            matches!(&w.state, WorkState::Failed { why, retry: false } if why.contains("joined this room again"))
        })
    });
    // The rejoined room is intact and shown again.
    node.with(|w| assert_eq!(w.slots.get(&key), Some(&rejoined)));
    session.wait_rooms("the room is shown", |r, _, _| {
        r.iter().any(|r| r.name == "B")
    });
}

// The sent direct messages are read with the rooms, and only read -----------------------------

fn dm_store_bytes(text: &str) -> Vec<u8> {
    use river_core::chat_delegate::{OutboundDmEntry, OutboundDmStore};
    use river_core::room_state::direct_messages::PurgeToken;
    let store = OutboundDmStore {
        entries: vec![OutboundDmEntry {
            room_owner_vk: vk(1).to_bytes(),
            sender: MemberId::from(&vk(2)),
            recipient: MemberId::from(&vk(3)),
            purge_token: PurgeToken([5; 16]),
            timestamp: 1,
            plaintext: text.to_string(),
        }],
        hidden_threads: vec![],
    };
    let mut out = Vec::new();
    ciborium::ser::into_writer(&store, &mut out).unwrap();
    out
}

#[test]
fn the_loader_reads_outbound_dms_when_the_store_lists_them() {
    let (mut loader, _) = Loader::start();
    let keys = vec![
        ChatDelegateKey::new(room_storage_key(&vk(1))),
        ChatDelegateKey::new(crate::rooms::dm_cache::KEY.to_vec()),
    ];
    let Step::Send(reads) = loader.on_answer(ChatDelegateResponseMsg::ListResponse { keys }) else {
        panic!("gets follow the list");
    };
    assert_eq!(reads.len(), 2);
    assert!(
        reads
            .iter()
            .any(|r| matches!(r, Read::Get(k) if k == b"outbound_dms"))
    );
    assert!(matches!(
        loader.on_answer(get_answer(
            b"outbound_dms".to_vec(),
            Some(dm_store_bytes("sent words"))
        )),
        Step::Wait
    ));
    let Step::Done(loaded) = loader.on_answer(get_answer(room_storage_key(&vk(1)), None)) else {
        panic!("all answered");
    };
    let DmRead::Read(cache) = loaded.dms else {
        panic!("the store decoded");
    };
    assert_eq!(cache.len(), 1);
    // Rooms are counted as before: the sent messages are not one.
    assert_eq!(loaded.listed, 1);
}

#[test]
fn a_store_without_outbound_dms_asks_for_none_and_an_unreadable_one_is_unreadable() {
    let (mut loader, _) = Loader::start();
    let keys = vec![ChatDelegateKey::new(ROOMS_META_KEY.to_vec())];
    let Step::Send(reads) = loader.on_answer(ChatDelegateResponseMsg::ListResponse { keys }) else {
        panic!("a get follows the list");
    };
    assert!(
        !reads
            .iter()
            .any(|r| matches!(r, Read::Get(k) if k == b"outbound_dms"))
    );
    let Step::Done(loaded) = loader.on_answer(get_answer(ROOMS_META_KEY.to_vec(), None)) else {
        panic!("all answered");
    };
    assert_eq!(loaded.dms, DmRead::NotListed);

    let (mut loader, _) = Loader::start();
    let keys = vec![ChatDelegateKey::new(b"outbound_dms".to_vec())];
    loader.on_answer(ChatDelegateResponseMsg::ListResponse { keys });
    let Step::Done(loaded) = loader.on_answer(get_answer(
        b"outbound_dms".to_vec(),
        Some(vec![0xff, 0x01, 0x02]),
    )) else {
        panic!("all answered");
    };
    assert_eq!(loaded.dms, DmRead::Unreadable);
}

#[test]
fn a_listed_key_with_no_value_is_unreadable_not_empty() {
    for value in [None, Some(vec![])] {
        let (mut loader, _) = Loader::start();
        let keys = vec![ChatDelegateKey::new(b"outbound_dms".to_vec())];
        loader.on_answer(ChatDelegateResponseMsg::ListResponse { keys });
        let Step::Done(loaded) = loader.on_answer(get_answer(b"outbound_dms".to_vec(), value))
        else {
            panic!("all answered");
        };
        assert_eq!(loaded.dms, DmRead::Unreadable);
    }
}

#[test]
fn reading_the_rooms_hydrates_the_cache_and_a_missing_store_leaves_it_alone() {
    let mut rooms = Rooms::default();
    assert!(!rooms.dm_cache().hydrated());
    let read = |bytes: &[u8]| DmRead::Read(Box::new(OutboundCache::from_bytes(bytes).unwrap()));
    let mut first = loaded(vec![]);
    first.dms = read(&dm_store_bytes("one"));
    rooms.replace(first);
    assert!(rooms.dm_cache().hydrated());
    assert_eq!(rooms.dm_cache().len(), 1);

    // The store is gone from this read: that says nothing about what was in it.
    rooms.replace(Loaded::missing());
    assert!(rooms.dm_cache().hydrated());
    assert_eq!(rooms.dm_cache().len(), 1);

    // An empty list of rooms that did not list the key (a transient empty read) neither wipes
    // the cache nor changes its latch.
    rooms.replace(loaded(vec![]));
    assert!(rooms.dm_cache().hydrated());
    assert_eq!(rooms.dm_cache().len(), 1);
    // Nor does a list that named rooms but not the key clear what was read.
    rooms.replace(loaded(vec![TestRoom::new(1, "A", 1, &["a1"]).stored()]));
    assert!(rooms.dm_cache().hydrated());
    assert_eq!(rooms.dm_cache().len(), 1);

    // An unreadable value keeps the entries and unlatches; the next good read latches again.
    let mut bad = loaded(vec![]);
    bad.dms = DmRead::Unreadable;
    rooms.replace(bad);
    assert!(!rooms.dm_cache().hydrated());
    assert_eq!(rooms.dm_cache().len(), 1);
    let mut good = loaded(vec![]);
    good.dms = read(&dm_store_bytes("two"));
    rooms.replace(good);
    assert!(rooms.dm_cache().hydrated());
}

#[test]
fn a_key_not_listed_hydrates_an_empty_cache_only_from_a_list_that_named_rooms() {
    // A read that listed nothing proves nothing: not hydrated.
    let mut rooms = Rooms::default();
    rooms.replace(loaded(vec![]));
    assert!(!rooms.dm_cache().hydrated());
    // The same for a store that was not there.
    rooms.replace(Loaded::missing());
    assert!(!rooms.dm_cache().hydrated());
    // A list that named a room was read, so the key's absence is real.
    rooms.replace(loaded(vec![TestRoom::new(1, "A", 1, &["a1"]).stored()]));
    assert!(rooms.dm_cache().hydrated());
    assert!(rooms.dm_cache().is_empty());
}

// The direct-message threads sent to the window -----------------------------------------------

#[test]
fn threads_are_sent_when_they_change_and_a_room_s_messages_are_opened_only_then() {
    use river_core::room_state::direct_messages::compose_direct_message;
    let f = crate::rooms::room_tests::Fixture::new();
    let mut state = f.private_state();
    let (owner, me) = (f.owner.verifying_key(), f.me.verifying_key());
    for (from, to, at, text) in [(&f.owner, &me, 100, "hello"), (&f.me, &owner, 160, "mine")] {
        state
            .direct_messages
            .messages
            .push(compose_direct_message(from, to, &owner, at, at, text.as_bytes()).unwrap());
    }
    let mut rooms = Rooms::default();
    rooms.list.push(LiveRoom::new(f.stored(state)));
    rooms.network_seen.insert(owner);

    let first = rooms.dm_snapshot().expect("the first report sends them");
    assert_eq!(
        (first.threads.len(), first.room_keys.len(), first.hydrated),
        (1, 1, false)
    );
    let thread = &first.threads[0];
    assert_eq!(
        (thread.room_name.as_str(), thread.peer_name.as_str()),
        ("Secret garden", "Olive")
    );
    assert_eq!(thread.messages.len(), 2);
    // The user's own message has no cached text yet.
    assert!(matches!(
        thread.messages[1].body,
        crate::chat::dm_model::DmBody::Placeholder(
            crate::chat::dm_model::Placeholder::CiphertextOnly
        )
    ));
    let builds = |rooms: &Rooms| rooms.dm_memo[&owner].builds;
    assert_eq!(builds(&rooms), 1);

    // Nothing changed: nothing is sent, and nothing is opened again.
    assert!(rooms.dm_snapshot().is_none());
    assert!(rooms.dm_snapshot().is_none());
    assert_eq!(builds(&rooms), 1);

    // Another room arrives: the rooms an invitation can name changed, so the threads are built
    // under a new key and sent again.
    let other = TestRoom::new(5, "Other", 1, &["x"]).stored();
    rooms.network_seen.insert(other.owner_vk);
    rooms.list.push(LiveRoom::new(other));
    let second = rooms.dm_snapshot().expect("the list changed");
    assert_eq!(second.room_keys.len(), 2);
    // The room that did not change keeps the very thread it had (nothing is cloned again).
    assert!(
        !std::sync::Arc::ptr_eq(&first.threads[0], &second.threads[0]),
        "its key changed with the other room's name, so it was built again"
    );
    let again = rooms.dm_snapshot();
    assert!(again.is_none());
    assert_eq!(builds(&rooms), 2);
    assert!(rooms.dm_snapshot().is_none());

    // Reading the sent messages changes what the user's own rows show.
    rooms.dm_cache.note_read(
        DmRead::Read(Box::new(
            OutboundCache::from_bytes(&dm_store_bytes("x")).unwrap(),
        )),
        false,
    );
    let third = rooms.dm_snapshot().expect("the cache was read");
    assert!(third.hydrated);
    assert_eq!(builds(&rooms), 3);
    assert!(rooms.dm_snapshot().is_none());

    // A room with no direct messages costs nothing and is never in the memo.
    assert_eq!(rooms.dm_memo.len(), 1);
    // Its messages going away empties the thread list, and says so once.
    rooms.list[0]
        .stored
        .room_state
        .direct_messages
        .messages
        .clear();
    let gone = rooms.dm_snapshot().expect("the threads went");
    assert!(gone.threads.is_empty());
    assert!(rooms.dm_memo.is_empty());
}

#[test]
fn the_threads_key_follows_the_peer_s_nickname_and_the_names_of_the_rooms_an_invitation_can_name() {
    use river_core::room_state::direct_messages::compose_direct_message;
    let f = crate::rooms::room_tests::Fixture::new();
    let (owner, me) = (f.owner.verifying_key(), f.me.verifying_key());
    // Composed once: sealing is randomised, so a second composition is a different message.
    let message = compose_direct_message(&f.owner, &me, &owner, 100, 100, b"hi").unwrap();
    let room = |nick: &str| {
        let mut state = f.private_state();
        state.direct_messages.messages.push(message.clone());
        state.member_info.member_info[0] = f.nickname(&f.owner, nick);
        LiveRoom::new(f.stored(state))
    };
    let cache = OutboundCache::default();
    let label = |_: &VerifyingKey| None;
    let names = |others: &[&str]| others.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let key = |r: &LiveRoom, others: &[&str]| r.dm_key(&cache, &label, &names(others));

    let base = key(&room("Olive"), &["Secret garden", "Other"]);
    assert_eq!(
        base,
        key(&room("Olive"), &["Secret garden", "Other"]),
        "stable"
    );
    assert_ne!(
        base,
        key(&room("Oliver"), &["Secret garden", "Other"]),
        "peer renamed"
    );
    assert_ne!(
        base,
        key(&room("Olive"), &["Secret garden", "Renamed"]),
        "another room renamed"
    );
}

#[test]
fn replacing_a_room_with_another_is_sent_even_when_neither_has_messages() {
    let mut rooms = Rooms::default();
    let (one, two) = (
        TestRoom::new(5, "One", 1, &["x"]).stored(),
        TestRoom::new(6, "Two", 1, &["y"]).stored(),
    );
    rooms.network_seen.extend([one.owner_vk, two.owner_vk]);
    rooms.list.push(LiveRoom::new(one));
    let first = rooms.dm_snapshot().expect("first");
    assert!(rooms.dm_snapshot().is_none());
    rooms.list[0] = LiveRoom::new(two);
    let second = rooms
        .dm_snapshot()
        .expect("the list changed, though its size did not");
    assert_ne!(first.room_keys, second.room_keys);
}

#[test]
fn a_room_is_listed_as_loaded_only_once_the_network_has_given_its_state() {
    let mut rooms = Rooms::default();
    let stored = TestRoom::new(5, "Joined", 1, &["x"]).stored();
    let vk = stored.owner_vk;
    rooms.list.push(LiveRoom::new(stored));
    let first = rooms.dm_snapshot().expect("first");
    assert!(first.room_keys.is_empty(), "a stub is not loaded");
    assert!(rooms.dm_snapshot().is_none());
    rooms.network_seen.insert(vk);
    let second = rooms.dm_snapshot().expect("loading is a change");
    assert_eq!(second.room_keys.len(), 1);
}

// Sending a direct message ------------------------------------------------------------------------

/// A session's rooms holding the fixture's private room, where the owner is the one to write to.
fn dm_rooms() -> (Rooms, crate::rooms::room_tests::Fixture, String, String) {
    let f = crate::rooms::room_tests::Fixture::new();
    let mut rooms = Rooms::default();
    let room = LiveRoom::new(f.stored(f.private_state()));
    let key = room.key();
    rooms.network_seen.insert(room.stored.owner_vk);
    rooms.list.push(room);
    rooms.loaded = true;
    rooms.complete = true;
    let peer = crate::rooms::dm::peer_key(f.owner_id());
    (rooms, f, key, peer)
}

fn send_dm(room: &str, peer: &str, text: &str, ticket: u64) -> Command {
    Command {
        room: room.to_owned(),
        action: Action::SendDm {
            peer: crate::rooms::dm::peer_from_key(peer).expect("a member key"),
            text: text.to_owned(),
            ticket,
        },
    }
}

fn take(rooms: &mut Rooms, command: Command) -> Vec<(u64, Result<(), String>)> {
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(command).unwrap();
    rooms.take_commands(&rx);
    std::mem::take(&mut rooms.dm_results)
        .into_iter()
        .map(|r| (r.ticket, r.result))
        .collect()
}

#[test]
fn a_direct_message_is_answered_queued_and_shown_in_its_thread_before_it_is_sent() {
    let (mut rooms, f, key, peer) = dm_rooms();
    assert_eq!(
        take(&mut rooms, send_dm(&key, &peer, "hello owner", 7)),
        vec![(7, Ok(()))]
    );
    // Queued, nothing in the room yet.
    assert!(!rooms.outbox.is_empty());
    assert!(
        rooms.list[0]
            .stored
            .room_state
            .direct_messages
            .messages
            .is_empty()
    );

    // The window is sent the thread with the message in it, shown as on its way, with its text.
    let snap = rooms.dm_snapshot().expect("a new thread");
    assert_eq!(snap.threads.len(), 1);
    let t = &snap.threads[0];
    assert_eq!(t.peer_key, peer);
    assert_eq!(t.messages.len(), 1);
    let m = &t.messages[0];
    assert!(m.outgoing);
    assert_eq!(
        m.body,
        crate::chat::dm_model::DmBody::Text("hello owner".into())
    );
    assert_eq!(m.pending.as_ref().map(|p| p.failed.clone()), Some(None));
    assert!(rooms.dm_snapshot().is_none(), "nothing changed");

    // Sent and taken: the room holds it, the entry is done, and the same row (same key) is
    // there with no state and the same text.
    let mut changed = false;
    let delta = rooms
        .outbox
        .next_update(&rooms.list[0], Instant::now(), &mut changed)
        .expect("goes out");
    assert!(delta.direct_messages.is_some());
    assert!(rooms.update_answered(&rooms.list[0].contract_id(), Ok(())));
    let after = rooms.dm_snapshot().expect("the row's state changed");
    let n = &after.threads[0].messages[0];
    assert_eq!(n.key, m.key, "the same row");
    assert!(n.pending.is_none());
    assert_eq!(
        n.body,
        crate::chat::dm_model::DmBody::Text("hello owner".into())
    );
    assert_eq!(after.threads[0].messages.len(), 1, "shown once");
    let _ = f;
}

#[test]
fn a_direct_message_the_session_cannot_take_is_answered_with_why_and_leaves_nothing() {
    let (mut rooms, _f, key, peer) = dm_rooms();
    // River has moved on: nothing is written, and the answer says so.
    rooms.update_required = true;
    let answers = take(&mut rooms, send_dm(&key, &peer, "held", 1));
    assert_eq!(
        answers,
        vec![(
            1,
            Err(crate::rooms::generation::UPDATE_REQUIRED_SEND.to_owned())
        )]
    );
    assert!(rooms.outbox.is_empty());
    rooms.update_required = false;

    // Refusals by the room's own state come with their fixed words.
    let answers = take(&mut rooms, send_dm(&key, &peer, "   ", 2));
    assert_eq!(
        answers,
        vec![(2, Err(crate::rooms::dm_send::Refusal::Empty.text()))]
    );
    let me = rooms.list[0].stored.self_verifying_key().unwrap();
    let own = crate::rooms::dm::peer_key(river_core::room_state::member::MemberId::from(&me));
    let answers = take(&mut rooms, send_dm(&key, &own, "me", 3));
    assert_eq!(
        answers,
        vec![(3, Err(crate::rooms::dm_send::Refusal::SelfMessage.text()))]
    );
    assert!(rooms.outbox.is_empty());
    // Nothing was queued, so nothing shows: the first report has no threads.
    let snap = rooms.dm_snapshot().expect("the first report");
    assert!(snap.threads.is_empty());
    assert!(rooms.dm_results.is_empty());

    // A room that is gone says so; one that may only be unread yet waits.
    let answers = take(&mut rooms, send_dm("nope", &peer, "x", 4));
    assert_eq!(
        answers,
        vec![(4, Err("This room is no longer here.".to_owned()))]
    );
    rooms.complete = false;
    assert!(take(&mut rooms, send_dm("nope", &peer, "x", 5)).is_empty());
    assert_eq!(rooms.outbox.waiting.len(), 1);
}

#[test]
fn a_direct_message_that_waited_for_the_rooms_is_queued_and_answered_once_they_are_read() {
    let (mut rooms, _f, key, peer) = dm_rooms();
    // The rooms are not all read yet: the command waits, and nothing is answered.
    let room = rooms.list.remove(0);
    rooms.complete = false;
    assert!(take(&mut rooms, send_dm(&key, &peer, "patient", 8)).is_empty());
    assert_eq!(rooms.outbox.waiting.len(), 1);
    assert!(rooms.outbox.is_empty());
    // The room arrives and the read is complete: the same command is taken again with no new
    // input, and the answer comes with its own ticket.
    rooms.list.push(room);
    rooms.complete = true;
    let (_tx, rx) = std::sync::mpsc::channel();
    assert!(rooms.take_commands(&rx));
    assert!(rooms.outbox.waiting.is_empty());
    let answers: Vec<_> = std::mem::take(&mut rooms.dm_results)
        .into_iter()
        .map(|r| (r.ticket, r.result))
        .collect();
    assert_eq!(answers, vec![(8, Ok(()))]);
    assert!(!rooms.outbox.is_empty());
    let snap = rooms.dm_snapshot().expect("a thread");
    assert_eq!(snap.threads[0].messages.len(), 1);
}

#[test]
fn a_failed_direct_message_keeps_its_text_in_the_thread_and_a_discard_drops_it() {
    let (mut rooms, _f, key, peer) = dm_rooms();
    assert_eq!(
        take(&mut rooms, send_dm(&key, &peer, "keep me", 1)).len(),
        1
    );
    let mut changed = false;
    rooms
        .outbox
        .next_update(&rooms.list[0], Instant::now(), &mut changed)
        .expect("goes out");
    assert!(rooms.update_answered(&rooms.list[0].contract_id(), Err("refused".into())));
    let snap = rooms.dm_snapshot().expect("the state changed");
    let m = &snap.threads[0].messages[0];
    let pending = m.pending.clone().expect("not sent");
    assert!(pending.failed.unwrap().contains("refused"));
    assert!(pending.retry);
    assert_eq!(
        m.body,
        crate::chat::dm_model::DmBody::Text("keep me".into())
    );

    // The text is kept through a read of the delegate's store (the sent text is not saved yet).
    rooms.replace(loaded(vec![rooms.list[0].stored.clone()]));
    assert_eq!(rooms.dm_cache().sent_this_session(), 1);

    // Discard drops the row and the text kept for it.
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(Command {
        room: key.clone(),
        action: Action::Discard {
            entry: pending.entry,
        },
    })
    .unwrap();
    assert!(rooms.take_commands(&rx));
    assert!(rooms.outbox.is_empty());
    assert_eq!(rooms.dm_cache().sent_this_session(), 0);
    let snap = rooms.dm_snapshot().expect("the row went");
    assert!(snap.threads.is_empty());
}

#[test]
fn the_sent_text_outlives_the_session_while_the_message_is_in_the_outbox() {
    // A session ends and another starts with the same outbox: the thread still shows the text
    // of what is waiting.
    let (mut rooms, _f, key, peer) = dm_rooms();
    assert_eq!(
        take(&mut rooms, send_dm(&key, &peer, "still here", 1)).len(),
        1
    );
    let room = rooms.list.remove(0);
    let outbox = std::mem::take(&mut rooms.outbox);
    let mut next = Rooms {
        outbox,
        loaded: true,
        complete: true,
        ..Rooms::default()
    };
    next.network_seen.insert(room.stored.owner_vk);
    next.list.push(room);
    let snap = next.dm_snapshot().expect("a thread");
    assert_eq!(
        snap.threads[0].messages[0].body,
        crate::chat::dm_model::DmBody::Text("still here".into())
    );
}

#[test]
fn rooms_updates_for_a_direct_message_print_no_text() {
    let update = RoomsUpdate::DmSend {
        room: "r".into(),
        peer: "p".into(),
        ticket: 9,
        result: Err("The room refused it".into()),
    };
    let shown = format!("{update:?}");
    assert!(shown.contains("DmSend") && shown.contains("9"));
    let command = send_dm("r", "123", "PLAINTEXT-SECRET", 1);
    assert!(!format!("{command:?}").contains("PLAINTEXT"));
}

#[test]
fn a_direct_message_answered_before_any_report_still_shows_its_text() {
    let (mut rooms, _f, key, peer) = dm_rooms();
    assert_eq!(take(&mut rooms, send_dm(&key, &peer, "quick", 1)).len(), 1);
    // Sent and taken before the window was ever sent the thread.
    let mut changed = false;
    rooms
        .outbox
        .next_update(&rooms.list[0], Instant::now(), &mut changed)
        .expect("goes out");
    assert!(rooms.update_answered(&rooms.list[0].contract_id(), Ok(())));
    assert!(rooms.outbox.is_empty());
    let snap = rooms.dm_snapshot().expect("a thread");
    assert_eq!(snap.threads[0].messages.len(), 1);
    assert_eq!(
        snap.threads[0].messages[0].body,
        crate::chat::dm_model::DmBody::Text("quick".into())
    );
    assert!(snap.threads[0].messages[0].pending.is_none());
}

// Saving the text of sent direct messages ------------------------------------------------------

mod dm_saves {
    use super::*;
    use crate::rooms::dm_save::{DmSaver, MAX_FAILURES};
    use crate::rooms::dm_store::EntryKey;
    use river_core::chat_delegate::{HiddenDmThreadEntry, OutboundDmEntry, OutboundDmStore};
    use river_core::room_state::direct_messages::{PurgeToken, advance_recipient_purges};

    type BeforeCas = Box<dyn FnOnce(&mut Store)>;

    /// The chat delegate's `outbound_dms` key as the fake holds it: the value and generation, and
    /// what was asked of it.
    #[derive(Default)]
    struct Store {
        value: Option<Vec<u8>>,
        generation: u64,
        /// What the delegate was asked, in order: "read", "store".
        log: Vec<&'static str>,
        /// Answer every store with a refusal.
        refuse: bool,
        /// Refuse a store that holds fewer entries than the value does (a removal).
        refuse_shrink: bool,
        /// Run just before the next store is checked (another writer getting in first).
        before_cas: Option<BeforeCas>,
    }

    impl Store {
        /// What another writer stores, as the web UI's plain `StoreRequest` does: over whatever
        /// is there, one generation higher.
        fn web_writes(&mut self, store: &OutboundDmStore) {
            let mut out = Vec::new();
            ciborium::ser::into_writer(store, &mut out).unwrap();
            self.value = Some(out);
            self.generation += 1;
        }

        fn store(&self) -> OutboundDmStore {
            ciborium::de::from_reader(self.value.as_deref().expect("a stored value")).unwrap()
        }

        fn stores(&self) -> usize {
            self.log.iter().filter(|l| **l == "store").count()
        }
    }

    fn answer(store: &mut Store, request: &ClientRequest<'_>) -> ChatDelegateResponseMsg {
        let ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { inbound, .. }) =
            request
        else {
            panic!("not a delegate request");
        };
        let [InboundDelegateMsg::ApplicationMessage(m)] = inbound.as_slice() else {
            panic!("one message");
        };
        match ciborium::de::from_reader(m.payload.as_slice()).unwrap() {
            ChatDelegateRequestMsg::GetVersionedRequest { key } => {
                assert_eq!(key.as_bytes(), b"outbound_dms");
                store.log.push("read");
                ChatDelegateResponseMsg::GetVersionedResponse {
                    key,
                    value: store.value.clone(),
                    generation: store.generation,
                }
            }
            ChatDelegateRequestMsg::CasStoreRequest {
                key,
                value,
                expected_generation,
            } => {
                assert_eq!(key.as_bytes(), b"outbound_dms");
                store.log.push("store");
                if let Some(hook) = store.before_cas.take() {
                    hook(store);
                }
                let shrinks = store.refuse_shrink
                    && store.value.as_deref().is_some_and(|old| {
                        let count = |b: &[u8]| {
                            ciborium::de::from_reader::<OutboundDmStore, _>(b)
                                .map_or(0, |s| s.entries.len())
                        };
                        count(&value) < count(old)
                    });
                let result = if store.refuse || shrinks {
                    CasStoreResult::Failed("disk".into())
                } else if expected_generation == store.generation {
                    store.value = Some(value);
                    store.generation += 1;
                    CasStoreResult::Stored {
                        generation: store.generation,
                    }
                } else {
                    CasStoreResult::Conflict {
                        current_generation: store.generation,
                        current_value: store.value.clone(),
                    }
                };
                ChatDelegateResponseMsg::CasStoreResponse { key, result }
            }
            other => panic!("the sent-message save sent {other:?}"),
        }
    }

    /// Run the session's save step against the fake until it has nothing more to do.
    fn run(rooms: &mut Rooms, store: &mut Store, now: &mut Instant, writes: bool) {
        for _ in 0..500 {
            rooms.dm_save_tick(writes, *now);
            if let Some(request) = rooms.outbox.work.next_request(false, *now) {
                let a = answer(store, &request);
                assert!(rooms.outbox.work.on_delegate(&a));
                continue;
            }
            if rooms.outbox.work.has_dm() {
                // A wait after a conflict.
                *now += Duration::from_millis(50);
                continue;
            }
            return;
        }
        panic!("the save never settled");
    }

    /// A session's rooms with the fixture's private room, whose sent messages have been read.
    fn rooms_hydrated() -> (Rooms, crate::rooms::room_tests::Fixture, String, String) {
        let (mut rooms, f, key, peer) = dm_rooms();
        rooms.dm_cache.note_read(DmRead::NotListed, true);
        assert!(rooms.dm_cache().hydrated());
        (rooms, f, key, peer)
    }

    fn owner_of(rooms: &Rooms) -> VerifyingKey {
        rooms.list[0].stored.owner_vk
    }

    /// The key of the one direct message in the outbox of the room.
    fn queued_key(rooms: &Rooms) -> EntryKey {
        let owner = owner_of(rooms);
        let rows = rooms.outbox.dm_rows(&owner);
        (owner.to_bytes(), rows[0].recipient, rows[0].token)
    }

    fn deliver(rooms: &mut Rooms) {
        let mut changed = false;
        rooms
            .outbox
            .next_update(&rooms.list[0], Instant::now(), &mut changed)
            .expect("goes out");
        assert!(rooms.update_answered(&rooms.list[0].contract_id(), Ok(())));
    }

    fn send(rooms: &mut Rooms, key: &str, peer: &str, text: &str) {
        assert_eq!(take(rooms, send_dm(key, peer, text, 1)), vec![(1, Ok(()))]);
    }

    fn texts(store: &OutboundDmStore) -> Vec<&str> {
        store.entries.iter().map(|e| e.plaintext.as_str()).collect()
    }

    /// What a web UI tab holds and saves: entries of its own, an archived thread and nothing of
    /// what this app sent since the tab loaded.
    fn web_copy(room: &VerifyingKey) -> OutboundDmStore {
        OutboundDmStore {
            entries: vec![OutboundDmEntry {
                room_owner_vk: room.to_bytes(),
                sender: MemberId::from(&vk(60)),
                recipient: MemberId::from(&vk(61)),
                purge_token: PurgeToken([61; 16]),
                timestamp: 1,
                plaintext: "from the web".into(),
            }],
            hidden_threads: vec![HiddenDmThreadEntry {
                room_owner_vk: room.to_bytes(),
                peer: MemberId::from(&vk(61)),
                hidden_at_ts: 9,
            }],
        }
    }

    /// A read of the store applied to the cache, as the session's re-read does.
    fn read_again(rooms: &mut Rooms, store: &Store) {
        let cache = OutboundCache::from_bytes(store.value.as_deref().unwrap()).unwrap();
        rooms
            .dm_cache
            .note_read(DmRead::Read(Box::new(cache)), true);
    }

    #[test]
    fn a_sent_message_is_saved_and_its_text_is_there_after_a_restart() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        send(&mut rooms, &key, &peer, "hello owner");
        let k = queued_key(&rooms);
        assert_eq!(rooms.dm_cache().sent_this_session(), 1);

        run(&mut rooms, &mut store, &mut now, true);
        // The delegate holds it: one read, one store, the message's own entry.
        assert_eq!(store.log, vec!["read", "store"]);
        let saved = store.store();
        assert_eq!(texts(&saved), vec!["hello owner"]);
        assert_eq!(
            (saved.entries[0].room_owner_vk, saved.entries[0].recipient),
            (k.0, k.1)
        );
        assert_eq!(saved.entries[0].purge_token, k.2);
        // Held apart from the unsaved now, and still shown.
        assert_eq!(rooms.dm_cache().sent_this_session(), 0);
        let owner = owner_of(&rooms);
        assert!(rooms.dm_cache().is_saved(&owner, k.1, &k.2));
        assert_eq!(
            rooms.dm_cache().plaintext(&owner, k.1, &k.2),
            Some("hello owner")
        );
        // Nothing more is written while nothing changes.
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(store.log.len(), 2);

        // The node takes the message; the app restarts: fresh rooms, the same delegate.
        deliver(&mut rooms);
        let room = rooms.list.remove(0);
        let mut again = Rooms {
            loaded: true,
            complete: true,
            ..Rooms::default()
        };
        again.network_seen.insert(room.stored.owner_vk);
        again.list.push(room);
        assert_eq!(again.dm_cache().sent_this_session(), 0);
        read_again(&mut again, &store);
        let snap = again.dm_snapshot().expect("a thread");
        let row = &snap.threads[0].messages[0];
        assert!(row.outgoing);
        assert_eq!(
            row.body,
            crate::chat::dm_model::DmBody::Text("hello owner".into())
        );
        assert!(row.pending.is_none());
    }

    #[test]
    fn nothing_is_written_until_the_sent_messages_were_read_and_decoded() {
        let (mut rooms, _f, key, peer) = dm_rooms();
        let (mut store, mut now) = (Store::default(), Instant::now());
        send(&mut rooms, &key, &peer, "wait for me");
        // Never read: the latch is not set.
        assert!(!rooms.dm_cache().hydrated());
        run(&mut rooms, &mut store, &mut now, true);
        // A list that named no rooms proves nothing about the key.
        rooms.dm_cache.note_read(DmRead::NotListed, false);
        run(&mut rooms, &mut store, &mut now, true);
        // A value that did not decode unlatches it.
        rooms.dm_cache.note_read(DmRead::Unreadable, true);
        assert!(!rooms.dm_cache().hydrated());
        run(&mut rooms, &mut store, &mut now, true);
        assert!(store.log.is_empty(), "no request at all: {:?}", store.log);
        assert_eq!(rooms.dm_cache().sent_this_session(), 1);

        // A read that decoded, after one that did not: now it may.
        store.web_writes(&web_copy(&owner_of(&rooms)));
        read_again(&mut rooms, &store);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(store.stores(), 1);
        assert_eq!(texts(&store.store()), vec!["from the web", "wait for me"]);
    }

    #[test]
    fn a_save_never_starts_while_the_session_may_not_write() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        send(&mut rooms, &key, &peer, "held");

        // The connection's write gate is closed.
        run(&mut rooms, &mut store, &mut now, false);
        // River has moved on.
        rooms.update_required = true;
        run(&mut rooms, &mut store, &mut now, true);
        rooms.update_required = false;
        // The store is gone from this node, or the rooms were not read.
        rooms.store_missing = true;
        run(&mut rooms, &mut store, &mut now, true);
        rooms.store_missing = false;
        rooms.loaded = false;
        run(&mut rooms, &mut store, &mut now, true);
        rooms.loaded = true;
        assert!(store.log.is_empty(), "{:?}", store.log);

        // Open again: it goes.
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(store.stores(), 1);
    }

    #[test]
    fn a_save_working_when_river_moves_on_is_failed_and_stores_nothing() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let now = Instant::now();
        send(&mut rooms, &key, &peer, "held");
        rooms.dm_save_tick(true, now);
        assert!(rooms.outbox.work.has_dm());
        // The session's loop halts what waits with nothing out when it finds River moved on.
        assert!(
            rooms
                .outbox
                .work
                .halt(crate::rooms::generation::UPDATE_REQUIRED_WORK)
        );
        rooms.update_required = true;
        assert!(rooms.outbox.work.next_request(false, now).is_none());
        rooms.dm_save_tick(true, now);
        assert!(!rooms.outbox.work.has_dm());
        assert_eq!(rooms.dm_cache().sent_this_session(), 1, "the text is kept");
        // And nothing starts again while it holds.
        rooms.dm_saver = DmSaver::default();
        rooms.dm_save_tick(true, now + Duration::from_secs(100_000));
        assert!(!rooms.outbox.work.has_dm());
    }

    #[test]
    fn a_merge_keeps_what_the_web_ui_wrote_even_when_it_writes_between_our_read_and_store() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        let owner = owner_of(&rooms);
        store.web_writes(&web_copy(&owner));
        read_again(&mut rooms, &store);
        send(&mut rooms, &key, &peer, "ours");

        // Another tab saves a second entry after we read and before our store is checked.
        let mut second = web_copy(&owner);
        second.entries.push(OutboundDmEntry {
            plaintext: "newer from the web".into(),
            purge_token: PurgeToken([62; 16]),
            ..second.entries[0].clone()
        });
        store.before_cas = Some(Box::new(move |s| s.web_writes(&second)));
        run(&mut rooms, &mut store, &mut now, true);

        // Our store lost the swap, read again, and merged into what the other tab wrote.
        assert_eq!(store.log, vec!["read", "store", "read", "store"]);
        let out = store.store();
        assert_eq!(
            texts(&out),
            vec!["from the web", "newer from the web", "ours"]
        );
        assert_eq!(out.hidden_threads.len(), 1, "the archive list is kept");
        assert_eq!(rooms.dm_cache().sent_this_session(), 0);
    }

    #[test]
    fn a_failed_save_keeps_the_text_tries_later_a_bounded_number_of_times_and_never_blocks_sending()
    {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        rooms.dm_saver = DmSaver::with_waits(Duration::from_secs(10), Duration::from_secs(40));
        let (mut store, mut now) = (Store::default(), Instant::now());
        store.refuse = true;
        send(&mut rooms, &key, &peer, "first");
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(store.stores(), 1);
        assert_eq!(rooms.dm_cache().sent_this_session(), 1, "text kept");
        assert!(!rooms.dm_cache().is_saved(
            &owner_of(&rooms),
            queued_key(&rooms).1,
            &queued_key(&rooms).2
        ));

        // Not again at once, however often the loop turns.
        for _ in 0..20 {
            run(&mut rooms, &mut store, &mut now, true);
        }
        assert_eq!(store.stores(), 1, "waits for its turn");
        // After the wait, once; the wait then doubles.
        now += Duration::from_secs(10);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(store.stores(), 2);
        now += Duration::from_secs(10);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(store.stores(), 2, "the second wait is longer");
        now += Duration::from_secs(10);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(store.stores(), 3);

        // Sending is not held back by any of it, and the next message resets the count.
        send(&mut rooms, &key, &peer, "second");
        assert_eq!(rooms.dm_cache().sent_this_session(), 2);
        store.refuse = false;
        run(&mut rooms, &mut store, &mut now, true);
        // Both sent in one second, so their order in the store is the order of their tokens.
        let both = store.store();
        let mut held = texts(&both);
        held.sort_unstable();
        assert_eq!(held, vec!["first", "second"]);
        assert_eq!(rooms.dm_cache().sent_this_session(), 0);
    }

    #[test]
    fn saves_that_keep_failing_stop_until_the_next_message() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        store.refuse = true;
        send(&mut rooms, &key, &peer, "stuck");
        for _ in 0..MAX_FAILURES + 5 {
            run(&mut rooms, &mut store, &mut now, true);
            now += Duration::from_secs(60 * 60);
        }
        assert_eq!(store.stores(), MAX_FAILURES as usize);
        // A message sent later tries again.
        send(&mut rooms, &key, &peer, "later");
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(store.stores(), MAX_FAILURES as usize + 1);
    }

    /// The room holds the message, as it does once the node took it, so a lost entry may be
    /// put back.
    fn saved_and_delivered() -> (
        Rooms,
        Store,
        Instant,
        EntryKey,
        crate::rooms::room_tests::Fixture,
    ) {
        let (mut rooms, f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        store.web_writes(&web_copy(&owner_of(&rooms)));
        read_again(&mut rooms, &store);
        send(&mut rooms, &key, &peer, "mine");
        let k = queued_key(&rooms);
        run(&mut rooms, &mut store, &mut now, true);
        deliver(&mut rooms);
        assert_eq!(texts(&store.store()), vec!["from the web", "mine"]);
        (rooms, store, now, k, f)
    }

    #[test]
    fn an_entry_a_web_tab_overwrote_is_put_back_a_bounded_number_of_times() {
        let (mut rooms, mut store, mut now, k, _f) = saved_and_delivered();
        let owner = owner_of(&rooms);
        for round in 1..=crate::rooms::dm_cache::MAX_RESAVES {
            // A tab that loaded before our save saves its copy, without our entry.
            store.web_writes(&web_copy(&owner));
            read_again(&mut rooms, &store);
            assert_eq!(rooms.dm_cache().sent_this_session(), 1, "round {round}");
            // The text is still shown while it is put back.
            assert_eq!(rooms.dm_cache().plaintext(&owner, k.1, &k.2), Some("mine"));
            run(&mut rooms, &mut store, &mut now, true);
            let out = store.store();
            assert_eq!(texts(&out), vec!["from the web", "mine"], "round {round}");
            assert_eq!(out.hidden_threads.len(), 1);
            read_again(&mut rooms, &store);
        }
        // It keeps happening: past the bound, the entry is left out.
        store.web_writes(&web_copy(&owner));
        read_again(&mut rooms, &store);
        assert_eq!(rooms.dm_cache().sent_this_session(), 0);
        let stores = store.stores();
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(store.stores(), stores);
        assert_eq!(texts(&store.store()), vec!["from the web"]);
    }

    #[test]
    fn a_lost_entry_is_not_put_back_when_it_was_purged_or_is_not_the_users_now() {
        // Purged by its recipient: the web UI drops such an entry on purpose.
        let (mut rooms, mut store, mut now, k, f) = saved_and_delivered();
        let owner = owner_of(&rooms);
        let envelope = advance_recipient_purges(&f.owner, &owner, None, [k.2]).unwrap();
        rooms.list[0]
            .stored
            .room_state
            .direct_messages
            .purges
            .push(envelope);
        store.web_writes(&web_copy(&owner));
        read_again(&mut rooms, &store);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(texts(&store.store()), vec!["from the web"]);
        assert_eq!(rooms.dm_cache().sent_this_session(), 0, "dropped, not kept");

        // The user's identity in the room was replaced: the old one's text is not put back.
        let (mut rooms, mut store, mut now, _k, _f) = saved_and_delivered();
        rooms.list[0].stored.self_sk = Some(SigningKey::from_bytes(&[77; 32]));
        store.web_writes(&web_copy(&owner));
        read_again(&mut rooms, &store);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(texts(&store.store()), vec!["from the web"]);

        // The message is no longer in the room (the contract's cap dropped it).
        let (mut rooms, mut store, mut now, _k, _f) = saved_and_delivered();
        rooms.list[0]
            .stored
            .room_state
            .direct_messages
            .messages
            .clear();
        store.web_writes(&web_copy(&owner));
        read_again(&mut rooms, &store);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(texts(&store.store()), vec!["from the web"]);
    }

    #[test]
    fn a_lost_entry_waits_for_the_room_to_be_read_from_the_network() {
        let (mut rooms, mut store, mut now, _k, _f) = saved_and_delivered();
        let owner = owner_of(&rooms);
        rooms.network_seen.clear();
        store.web_writes(&web_copy(&owner));
        read_again(&mut rooms, &store);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(texts(&store.store()), vec!["from the web"], "not yet");
        assert_eq!(rooms.dm_cache().sent_this_session(), 1, "and not given up");
        rooms.network_seen.insert(owner);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(texts(&store.store()), vec!["from the web", "mine"]);
    }

    #[test]
    fn discarding_a_saved_message_removes_only_its_entry() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        let owner = owner_of(&rooms);
        store.web_writes(&web_copy(&owner));
        read_again(&mut rooms, &store);
        send(&mut rooms, &key, &peer, "will fail");
        send(&mut rooms, &key, &peer, "will stay");
        run(&mut rooms, &mut store, &mut now, true);
        let both = store.store();
        let mut held = texts(&both);
        held.sort_unstable();
        assert_eq!(held, vec!["from the web", "will fail", "will stay"]);

        // The first one's update is refused; the user discards it.
        let mut changed = false;
        rooms
            .outbox
            .next_update(&rooms.list[0], Instant::now(), &mut changed)
            .expect("goes out");
        assert!(rooms.update_answered(&rooms.list[0].contract_id(), Err("refused".into())));
        let failed = rooms
            .outbox
            .dm_rows(&owner)
            .into_iter()
            .find(|r| r.pending.failed.is_some())
            .expect("the failed one");
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Command {
            room: key.clone(),
            action: Action::Discard {
                entry: failed.pending.entry,
            },
        })
        .unwrap();
        assert!(rooms.take_commands(&rx));
        run(&mut rooms, &mut store, &mut now, true);

        let out = store.store();
        assert_eq!(texts(&out), vec!["from the web", "will stay"]);
        assert_eq!(out.hidden_threads.len(), 1);
    }

    #[test]
    fn discarding_a_message_that_was_never_saved_writes_nothing() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        let owner = owner_of(&rooms);
        send(&mut rooms, &key, &peer, "never saved");
        let row = rooms.outbox.dm_rows(&owner).remove(0);
        let (tx, rx) = std::sync::mpsc::channel();
        // The room refuses it first, so there is something to discard.
        let mut changed = false;
        rooms
            .outbox
            .next_update(&rooms.list[0], Instant::now(), &mut changed)
            .expect("goes out");
        assert!(rooms.update_answered(&rooms.list[0].contract_id(), Err("no".into())));
        tx.send(Command {
            room: key,
            action: Action::Discard {
                entry: row.pending.entry,
            },
        })
        .unwrap();
        assert!(rooms.take_commands(&rx));
        run(&mut rooms, &mut store, &mut now, true);
        assert!(store.log.is_empty(), "no read, no store: {:?}", store.log);
        assert_eq!(rooms.dm_cache().sent_this_session(), 0);
    }

    #[test]
    fn discarding_a_message_whose_save_is_working_removes_it_once_it_lands() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        let owner = owner_of(&rooms);
        send(&mut rooms, &key, &peer, "in flight");
        let row = rooms.outbox.dm_rows(&owner).remove(0);
        // The save has started (its read is out) when the user discards.
        rooms.dm_save_tick(true, now);
        let request = rooms.outbox.work.next_request(false, now).unwrap();
        let a = answer(&mut store, &request);
        assert!(rooms.outbox.work.on_delegate(&a));

        let mut changed = false;
        rooms
            .outbox
            .next_update(&rooms.list[0], Instant::now(), &mut changed)
            .expect("goes out");
        assert!(rooms.update_answered(&rooms.list[0].contract_id(), Err("no".into())));
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Command {
            room: key,
            action: Action::Discard {
                entry: row.pending.entry,
            },
        })
        .unwrap();
        assert!(rooms.take_commands(&rx));
        // The add lands, then the removal follows.
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(store.stores(), 2);
        assert!(store.store().entries.is_empty());
    }

    #[test]
    fn a_value_the_app_cannot_read_is_not_written_over_and_the_text_stays() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        // The cache read nothing; the key then holds something this app cannot decode (a newer
        // web UI's format, or damage).
        store.value = Some(b"\xa1\x67entries\x01".to_vec());
        send(&mut rooms, &key, &peer, "careful");
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(
            store.log,
            vec!["read"],
            "read, found it unreadable, stopped"
        );
        assert_eq!(store.value.as_deref(), Some(&b"\xa1\x67entries\x01"[..]));
        assert_eq!(rooms.dm_cache().sent_this_session(), 1);
    }

    /// The user's member id in the first room.
    fn me_in(rooms: &Rooms) -> MemberId {
        MemberId::from(&rooms.list[0].stored.self_verifying_key().unwrap())
    }

    /// Drive the save's requests until nothing is out, without taking its end.
    fn drive(rooms: &mut Rooms, store: &mut Store, now: Instant) {
        while let Some(request) = rooms.outbox.work.next_request(false, now) {
            let a = answer(store, &request);
            assert!(rooms.outbox.work.on_delegate(&a));
        }
    }

    #[test]
    fn a_read_that_lands_between_the_store_and_the_taking_of_its_end_still_tracks_the_text() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, now) = (Store::default(), Instant::now());
        send(&mut rooms, &key, &peer, "mine");
        let k = queued_key(&rooms);
        rooms.dm_save_tick(true, now);
        drive(&mut rooms, &mut store, now);
        assert_eq!(store.stores(), 1);
        // The re-read (a reconnect's, say) sees the entry before the end of the save was taken.
        read_again(&mut rooms, &store);
        let owner = owner_of(&rooms);
        assert!(rooms.dm_cache().is_saved(&owner, k.1, &k.2));
        assert_eq!(rooms.dm_cache().sent_this_session(), 0);
        // The end is taken: nothing changes.
        rooms.dm_save_tick(true, now);
        assert!(rooms.dm_cache().is_saved(&owner, k.1, &k.2));

        // So a web tab's later overwrite is noticed, and the text put back.
        deliver(&mut rooms);
        store.web_writes(&web_copy(&owner));
        read_again(&mut rooms, &store);
        let mut now = now;
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(texts(&store.store()), vec!["from the web", "mine"]);

        // And a discard of it removes it.
        rooms.dm_saver.owes_removal(k);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(texts(&store.store()), vec!["from the web"]);
    }

    #[test]
    fn a_read_that_began_before_a_save_ended_is_not_a_loss() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        send(&mut rooms, &key, &peer, "first");
        run(&mut rooms, &mut store, &mut now, true);
        let before = store.value.clone();
        // A read begins as the session starts one ...
        rooms.dm_read_began = Some(rooms.dm_cache.save_seq());
        // ... a second message is saved meanwhile ...
        send(&mut rooms, &key, &peer, "second");
        run(&mut rooms, &mut store, &mut now, true);
        // ... and the read, taken before either, is applied after.
        let mut stale = loaded(vec![rooms.list[0].stored.clone()]);
        stale.dms = DmRead::Read(Box::new(
            OutboundCache::from_bytes(&{
                let mut out = Vec::new();
                ciborium::ser::into_writer(&OutboundDmStore::default(), &mut out).unwrap();
                out
            })
            .unwrap(),
        ));
        rooms.replace(stale);
        assert_eq!(rooms.dm_cache().sent_this_session(), 0, "nothing put back");
        let stores = store.stores();
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(store.stores(), stores);
        assert_ne!(before, store.value);
    }

    #[test]
    fn more_than_the_cap_unsaved_for_one_pair_are_cut_to_the_cap_and_not_saved_again() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        store.refuse = true;
        let total = river_core::room_state::direct_messages::MAX_DM_MESSAGES_PER_PAIR + 1;
        for n in 0..total {
            send(&mut rooms, &key, &peer, &format!("message {n}"));
        }
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(rooms.dm_cache().sent_this_session(), total);
        store.refuse = false;
        now += Duration::from_secs(3600);
        run(&mut rooms, &mut store, &mut now, true);
        let out = store.store();
        assert_eq!(out.entries.len(), total - 1, "back at the cap, one batch");
        // The one cut is not kept to be saved again.
        assert_eq!(rooms.dm_cache().sent_this_session(), 0);
        let stores = store.stores();
        read_again(&mut rooms, &store);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(store.stores(), stores, "nothing more to save");
        assert_eq!(store.store().entries.len(), total - 1);
    }

    #[test]
    fn an_entry_the_web_ui_evicted_by_its_own_cap_is_not_fought_over() {
        let (mut rooms, mut store, mut now, k, f) = saved_and_delivered();
        let owner = owner_of(&rooms);
        // The same pair: the user to the room's owner. 100 newer messages are in the store.
        let me = me_in(&rooms);
        let mut web = web_copy(&owner);
        for n in 0..100u64 {
            web.entries.push(OutboundDmEntry {
                room_owner_vk: owner.to_bytes(),
                sender: me,
                recipient: f.owner_id(),
                purge_token: PurgeToken([n as u8; 16]),
                timestamp: u64::MAX / 2 + n,
                plaintext: format!("newer {n}"),
            });
        }
        let stores = store.stores();
        for _ in 0..5 {
            store.web_writes(&web);
            read_again(&mut rooms, &store);
            run(&mut rooms, &mut store, &mut now, true);
        }
        assert_eq!(store.stores(), stores, "never written again");
        assert!(store.store().entries.iter().all(|e| e.plaintext != "mine"));
        // Its text is still shown for the session, and nothing is waiting to be saved.
        assert_eq!(rooms.dm_cache().plaintext(&owner, k.1, &k.2), Some("mine"));
        assert_eq!(rooms.dm_cache().sent_this_session(), 0);
    }

    #[test]
    fn a_purge_by_another_recipient_does_not_stop_the_put_back() {
        let (mut rooms, mut store, mut now, k, _f) = saved_and_delivered();
        let owner = owner_of(&rooms);
        // Someone else's purge envelope naming the same token: it is not this message's
        // recipient's, so it says nothing about it.
        let other = SigningKey::from_bytes(&[55; 32]);
        let envelope = advance_recipient_purges(&other, &owner, None, [k.2]).unwrap();
        assert_ne!(envelope.recipient_id, k.1);
        rooms.list[0]
            .stored
            .room_state
            .direct_messages
            .purges
            .push(envelope);
        store.web_writes(&web_copy(&owner));
        read_again(&mut rooms, &store);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(texts(&store.store()), vec!["from the web", "mine"]);
    }

    #[test]
    fn a_lost_entry_is_wanted_only_for_the_message_with_its_recipient_sender_and_token() {
        use river_core::room_state::direct_messages::compose_direct_message;
        let (mut rooms, _f, k_sender_recipient) = {
            let (rooms, _s, _n, k, f) = saved_and_delivered();
            (rooms, f, k)
        };
        let k = k_sender_recipient;
        let me = me_in(&rooms);
        assert_eq!(rooms.resave_wanted(&k, me), Some(true));
        // Another recipient with the same token is not that message.
        let other_recipient = MemberId::from(&vk(70));
        assert_eq!(
            rooms.resave_wanted(&(k.0, other_recipient, k.2), me),
            Some(false)
        );
        // A message of someone else's to the user, asked after as if the user sent it: the
        // recipient is the user's own id, but the sender is not.
        let f = crate::rooms::room_tests::Fixture::new();
        let owner = owner_of(&rooms);
        let me_vk = rooms.list[0].stored.self_verifying_key().unwrap();
        let inbound = compose_direct_message(&f.owner, &me_vk, &owner, 777, 777, b"hi").unwrap();
        let token = inbound.purge_token();
        rooms.list[0]
            .stored
            .room_state
            .direct_messages
            .messages
            .push(inbound);
        assert_eq!(rooms.resave_wanted(&(k.0, me, token), me), Some(false));
        // And an identity that is not the user's now.
        assert_eq!(
            rooms.resave_wanted(&k, MemberId::from(&vk(71))),
            Some(false)
        );
    }

    #[test]
    fn a_store_that_vanished_is_asked_about_again_and_created_once_the_list_says_it_is_gone() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        let owner = owner_of(&rooms);
        store.web_writes(&web_copy(&owner));
        read_again(&mut rooms, &store);
        assert!(rooms.dm_cache().listed());
        send(&mut rooms, &key, &peer, "ours");
        // The value has gone from the delegate.
        store.value = None;
        store.generation = 0;
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(store.log, vec!["read"], "refused, written over nothing");
        assert!(store.value.is_none());
        assert!(
            rooms.outbox.work.reload_pending(),
            "the rooms are read again"
        );

        // The read of the rooms: a list that names rooms and not the key. It is gone.
        rooms.dm_cache.note_read(DmRead::NotListed, true);
        assert!(!rooms.dm_cache().listed());
        now += Duration::from_secs(3600);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(texts(&store.store()), vec!["ours"]);
    }

    #[test]
    fn a_removal_is_made_alone_and_first_so_an_add_that_is_refused_cannot_hold_it_back() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        let owner = owner_of(&rooms);
        send(&mut rooms, &key, &peer, "will fail");
        run(&mut rooms, &mut store, &mut now, true);
        // Its update is refused, and the user discards it.
        let mut changed = false;
        rooms
            .outbox
            .next_update(&rooms.list[0], Instant::now(), &mut changed)
            .expect("goes out");
        assert!(rooms.update_answered(&rooms.list[0].contract_id(), Err("no".into())));
        let row = rooms.outbox.dm_rows(&owner).remove(0);
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Command {
            room: key.clone(),
            action: Action::Discard {
                entry: row.pending.entry,
            },
        })
        .unwrap();
        assert!(rooms.take_commands(&rx));
        // Another message waits as well.
        send(&mut rooms, &key, &peer, "next");
        // The first write is the removal alone.
        rooms.dm_save_tick(true, now);
        drive(&mut rooms, &mut store, now);
        assert_eq!(
            store.store().entries.len(),
            0,
            "removed, and nothing added yet"
        );
        rooms.dm_save_tick(true, now);
        drive(&mut rooms, &mut store, now);
        assert_eq!(texts(&store.store()), vec!["next"]);

        // A removal still goes when the adds are refused for size or shape: the add fails the
        // task it is in, and that is not the removal's.
        let k = queued_key(&rooms);
        rooms.dm_saver.owes_removal(k);
        rooms.dm_save_tick(true, now);
        drive(&mut rooms, &mut store, now);
        assert!(store.store().entries.is_empty());
    }

    #[test]
    fn leaving_a_room_while_its_text_is_being_saved_leaves_no_entry_behind() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, now) = (Store::default(), Instant::now());
        send(&mut rooms, &key, &peer, "from a room about to be left");
        rooms.dm_save_tick(true, now);
        // The read is out ...
        let request = rooms.outbox.work.next_request(false, now).unwrap();
        // ... and the room is left before it is answered.
        rooms.list.clear();
        rooms.dm_save_tick(true, now);
        let a = answer(&mut store, &request);
        assert!(rooms.outbox.work.on_delegate(&a));
        assert!(
            rooms.outbox.work.next_request(false, now).is_none(),
            "no store"
        );
        assert_eq!(store.log, vec!["read"]);
        assert!(store.value.is_none());
        // The save ends well with nothing added, and the text goes with the room.
        rooms.dm_save_tick(true, now);
        rooms.dm_snapshot();
        assert_eq!(rooms.dm_cache().sent_this_session(), 0);
    }

    #[test]
    fn a_removal_that_keeps_failing_does_not_hold_the_adds_back_for_good() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        send(&mut rooms, &key, &peer, "saved, then discarded");
        run(&mut rooms, &mut store, &mut now, true);
        let k = queued_key(&rooms);
        // Its removal is owed, and the delegate refuses every write that removes something.
        rooms.dm_saver.owes_removal(k);
        store.refuse_shrink = true;
        send(&mut rooms, &key, &peer, "waiting behind it");
        for _ in 0..8 {
            run(&mut rooms, &mut store, &mut now, true);
            now += Duration::from_secs(3600);
        }
        let both = store.store();
        let held = texts(&both);
        assert!(held.contains(&"waiting behind it"), "{held:?}");
        assert!(
            held.contains(&"saved, then discarded"),
            "the removal did not work"
        );
        // It was tried again after the adds had their turn.
        let after = store.stores();
        store.refuse_shrink = false;
        now += Duration::from_secs(3600);
        run(&mut rooms, &mut store, &mut now, true);
        assert!(store.stores() > after);
        assert_eq!(texts(&store.store()), vec!["waiting behind it"]);
    }

    #[test]
    fn a_partial_read_of_the_rooms_prunes_no_unsaved_text_and_it_is_saved_when_the_room_is_back() {
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        let (mut store, mut now) = (Store::default(), Instant::now());
        send(&mut rooms, &key, &peer, "keep me");
        let stored = rooms.list[0].stored.clone();
        let owner = owner_of(&rooms);

        // A read in which the room's slot could not be decoded: it is not in the list, and the
        // read is not complete.
        let mut partial = loaded(vec![]);
        partial.listed = 1;
        partial.unreadable = 1;
        rooms.replace(partial);
        assert!(rooms.list.is_empty() && !rooms.complete);
        rooms.dm_snapshot();
        assert_eq!(rooms.dm_cache().sent_this_session(), 1, "nothing pruned");
        // And nothing is written for a room that is not in the list.
        run(&mut rooms, &mut store, &mut now, true);
        assert!(store.log.is_empty(), "{:?}", store.log);

        // The room is back, in a complete read: the text is still there, and is saved.
        rooms.replace(loaded(vec![stored]));
        assert!(rooms.complete);
        rooms.dm_snapshot();
        assert_eq!(rooms.dm_cache().sent_this_session(), 1);
        run(&mut rooms, &mut store, &mut now, true);
        assert_eq!(texts(&store.store()), vec!["keep me"]);
        assert!(rooms.dm_cache().is_saved(
            &owner,
            rooms.outbox.dm_rows(&owner)[0].recipient,
            &rooms.outbox.dm_rows(&owner)[0].token
        ));

        // A complete read that does not have the room (it was left) does prune.
        let (mut rooms, _f, key, peer) = rooms_hydrated();
        send(&mut rooms, &key, &peer, "left");
        rooms.list.clear();
        rooms.dm_snapshot();
        assert_eq!(rooms.dm_cache().sent_this_session(), 0);
    }

    #[test]
    fn a_new_session_starts_with_a_cache_that_has_read_and_saved_nothing() {
        // A node switch ends the session and starts another: `Rooms` is built afresh from the
        // outbox alone, so `listed`, the saved set and the put-back counts of the old node are
        // not carried to the new one.
        let norm = |s: &str| s.replace("\r\n", "\n");
        let session = norm(include_str!("session.rs"));
        let start = session
            .find("let mut kept = KeepOutbox {")
            .expect("run_with");
        let built = &session[start..start + 400];
        assert!(built.contains("..Rooms::default()"), "{built}");
        assert!(!built.contains("dm_cache"));
        // A read that was out when the connection dropped leaves no stamp for the next one.
        let follow = session.split("fn follow_on").nth(1).unwrap();
        assert!(follow.contains("rooms.dm_read_began = None;"));
        // And the failures are named, without a word of what was written.
        let tick = session.split("fn dm_save_tick").nth(1).unwrap();
        assert!(tick.contains("saving sent direct messages failed ({})"));
        assert!(tick.contains("r.kind()"));
    }

    #[test]
    fn the_save_is_the_only_writer_of_the_key_and_is_gated_in_the_session_loop() {
        let norm = |s: &str| s.replace("\r\n", "\n");
        let session = norm(include_str!("session.rs"));
        let loop_body = session
            .split("#[cfg(test)]\n#[path = \"session_tests.rs\"]")
            .next()
            .unwrap();
        let tick = loop_body
            .find("rooms.dm_save_tick(writes, Instant::now())")
            .expect("the loop starts the save");
        let send = loop_body[tick..]
            .find("rooms.send_updates(socket)")
            .expect("before updates");
        assert!(send < 400, "the tick sits with the other writes");
        // The tick is given the write gate and checks River's hold itself.
        let tick_fn = loop_body.split("fn dm_save_tick").nth(1).unwrap();
        let tick_fn = tick_fn.split("\n    }\n").next().unwrap();
        for guard in [
            "!writes",
            "self.update_required",
            "!self.dm_cache.hydrated()",
            "self.store_missing",
        ] {
            assert!(tick_fn.contains(guard), "{guard}");
        }
        // No unconditional store request exists in this crate's delegate requests.
        let delegate = norm(include_str!("delegate.rs"));
        assert!(!delegate.contains("ChatDelegateRequestMsg::StoreRequest"));
        // The key is only written through the work queue's compare-and-swap.
        assert!(norm(include_str!("work.rs")).contains("dm_cache::KEY.to_vec()"));
    }

    // Archiving a thread ------------------------------------------------------------------------

    mod archiving {
        use super::*;
        use crate::rooms::dm_archive::{ArchiveOp, ThreadKey};
        use river_core::room_state::direct_messages::{
            MAX_DM_FUTURE_SKEW_SECS, compose_direct_message,
        };

        /// The session's code without its tests and its comments. Fails if the marker that ends
        /// the code is not there, rather than reading the whole file.
        fn session_code() -> String {
            include_str!("session.rs")
                .replace("\r\n", "\n")
                .split_once("#[cfg(test)]\n#[path = \"session_tests.rs\"]")
                .expect("the marker of the end of the code")
                .0
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn archive_command(room: &str, peer: &str, row_inbound_ts: u64) -> Command {
            Command {
                room: room.to_owned(),
                action: Action::ArchiveDm {
                    peer: crate::rooms::dm::peer_from_key(peer).expect("a member key"),
                    row_inbound_ts,
                },
            }
        }

        fn unarchive_command(room: &str, peer: &str) -> Command {
            Command {
                room: room.to_owned(),
                action: Action::UnarchiveDm {
                    peer: crate::rooms::dm::peer_from_key(peer).expect("a member key"),
                },
            }
        }

        fn ask(rooms: &mut Rooms, command: Command) -> bool {
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(command).unwrap();
            rooms.take_commands(&rx)
        }

        /// The room's one peer (its owner) writes to the user at `at`.
        fn inbound(rooms: &mut Rooms, at: u64) {
            let f = crate::rooms::room_tests::Fixture::new();
            let owner = owner_of(rooms);
            let me = rooms.list[0].stored.self_verifying_key().unwrap();
            let m = compose_direct_message(&f.owner, &me, &owner, at, at, b"hi").unwrap();
            rooms.list[0]
                .stored
                .room_state
                .direct_messages
                .messages
                .push(m);
        }

        fn thread_of(rooms: &Rooms, peer: &str) -> ThreadKey {
            (
                owner_of(rooms).to_bytes(),
                crate::rooms::dm::peer_from_key(peer).unwrap(),
            )
        }

        /// The archive list the window would be sent now. A fresh snapshot is forced, so what
        /// this reads is the state and not whether the last one was sent: that is
        /// `the_window_is_sent_a_new_snapshot_when_the_archive_list_changes`.
        fn archived(rooms: &mut Rooms) -> Vec<(String, u64)> {
            rooms.dm_sent = None;
            rooms
                .dm_snapshot()
                .expect("a forced snapshot")
                .archived
                .into_iter()
                .map(|a| (a.peer_key, a.hidden_at_ts))
                .collect()
        }

        /// The room's one peer's message to nobody in particular is not what this is: the USER
        /// writes to the peer at `at`, in the room's own state, as a message the room holds.
        fn outbound(rooms: &mut Rooms, at: u64) {
            let f = crate::rooms::room_tests::Fixture::new();
            let owner = owner_of(rooms);
            let m = compose_direct_message(&f.me, &owner, &owner, at, at, b"mine").unwrap();
            rooms.list[0]
                .stored
                .room_state
                .direct_messages
                .messages
                .push(m);
        }

        /// The store as a newer web UI writes it, with archive entries of other threads and a
        /// field this build does not know, read by the cache.
        fn web_store(rooms: &mut Rooms, store: &mut Store, peers: &[(u8, u64)]) {
            let owner = owner_of(rooms);
            let mut web = web_copy(&owner);
            web.hidden_threads = peers
                .iter()
                .map(|(p, ts)| HiddenDmThreadEntry {
                    room_owner_vk: owner.to_bytes(),
                    peer: MemberId::from(&vk(*p)),
                    hidden_at_ts: *ts,
                })
                .collect();
            store.web_writes(&web);
            read_again(rooms, store);
        }

        #[test]
        fn archiving_shows_at_once_at_the_peers_newest_time_of_the_row_or_the_room() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            inbound(&mut rooms, 100);
            inbound(&mut rooms, 160);
            // The window's row lags: it saw 100. The room has 160, which is the cutoff.
            assert!(ask(&mut rooms, archive_command(&key, &peer, 100)));
            assert_eq!(archived(&mut rooms), vec![(peer.clone(), 160)]);
            // Nothing written yet: only owed.
            assert_eq!(rooms.dm_cache().archive_owed().len(), 1);
            // The row ahead of the room wins instead.
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            inbound(&mut rooms, 100);
            ask(&mut rooms, archive_command(&key, &peer, 130));
            assert_eq!(archived(&mut rooms), vec![(peer, 130)]);
        }

        #[test]
        fn our_own_messages_never_set_the_cutoff() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            inbound(&mut rooms, 100);
            send(&mut rooms, &key, &peer, "my reply, stamped by my clock");
            ask(&mut rooms, archive_command(&key, &peer, 100));
            // `send` unarchived nothing (it was not archived), and the cutoff is the peer's 100.
            assert_eq!(archived(&mut rooms), vec![(peer, 100)]);
        }

        #[test]
        fn an_archived_thread_is_written_to_the_store_keeping_everything_else() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            web_store(&mut rooms, &mut store, &[(61, 9)]);
            inbound(&mut rooms, 100);
            ask(&mut rooms, archive_command(&key, &peer, 0));
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.log, vec!["read", "store"]);
            let out = store.store();
            assert_eq!(texts(&out), vec!["from the web"], "the entries are kept");
            let ours = HiddenDmThreadEntry {
                room_owner_vk: owner_of(&rooms).to_bytes(),
                peer: crate::rooms::dm::peer_from_key(&peer).unwrap(),
                hidden_at_ts: 100,
            };
            assert_eq!(out.hidden_threads.len(), 2);
            assert!(out.hidden_threads.contains(&ours));
            // It is no longer owed, and the list shown is the same.
            assert!(rooms.dm_cache().archive_owed().is_empty());
            let shown = archived(&mut rooms);
            assert!(shown.contains(&(peer, 100)));
            // Nothing more is written while nothing changes.
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.log.len(), 2);
        }

        #[test]
        fn sending_into_the_thread_just_archived_leaves_only_the_text_to_write() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            ask(&mut rooms, archive_command(&key, &peer, 0));
            send(&mut rooms, &key, &peer, "reply");
            // The send unarchives (it was archived in the same breath), so the net change is the
            // text alone.
            run(&mut rooms, &mut store, &mut now, true);
            let out = store.store();
            assert_eq!(texts(&out), vec!["reply"]);
            assert!(out.hidden_threads.is_empty());
        }

        #[test]
        fn nothing_is_written_while_the_gate_is_shut_or_the_value_unread_and_the_archive_still_shows()
         {
            let (mut rooms, _f, key, peer) = dm_rooms();
            let (mut store, mut now) = (Store::default(), Instant::now());
            assert!(!rooms.dm_cache().hydrated());
            ask(&mut rooms, archive_command(&key, &peer, 0));
            run(&mut rooms, &mut store, &mut now, true);
            assert!(store.log.is_empty(), "never read: {:?}", store.log);
            assert_eq!(
                archived(&mut rooms),
                vec![(peer.clone(), 0)],
                "shown anyway"
            );
            // Read at last (nothing stored): it goes, but not while the write gate is shut or
            // River has moved on.
            rooms.dm_cache.note_read(DmRead::NotListed, true);
            run(&mut rooms, &mut store, &mut now, false);
            rooms.update_required = true;
            run(&mut rooms, &mut store, &mut now, true);
            rooms.update_required = false;
            assert!(store.log.is_empty(), "{:?}", store.log);
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.stores(), 1);
            assert_eq!(store.store().hidden_threads.len(), 1);
        }

        #[test]
        fn a_value_this_app_cannot_read_is_not_written_over_and_the_archive_stays_owed() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            store.value = Some(b"\xa1\x67entries\x01".to_vec());
            ask(&mut rooms, archive_command(&key, &peer, 0));
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.log, vec!["read"]);
            assert_eq!(store.value.as_deref(), Some(&b"\xa1\x67entries\x01"[..]));
            assert_eq!(rooms.dm_cache().archive_owed().len(), 1);
            assert_eq!(archived(&mut rooms), vec![(peer, 0)]);
            assert_eq!(rooms.archive_saver.failures(), 1);
            assert_eq!(rooms.dm_saver.failures(), 0, "the text's pacing is its own");
        }

        #[test]
        fn a_store_that_conflicts_is_read_again_and_the_web_uis_archive_survives() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            web_store(&mut rooms, &mut store, &[(61, 9)]);
            ask(&mut rooms, archive_command(&key, &peer, 0));
            // The web UI archives another thread and sends a text after we read and before our
            // store is checked.
            let owner = owner_of(&rooms);
            let mut newer = web_copy(&owner);
            newer.hidden_threads.push(HiddenDmThreadEntry {
                room_owner_vk: owner.to_bytes(),
                peer: MemberId::from(&vk(62)),
                hidden_at_ts: 77,
            });
            store.before_cas = Some(Box::new(move |s| s.web_writes(&newer)));
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.log, vec!["read", "store", "read", "store"]);
            let out = store.store();
            assert_eq!(out.hidden_threads.len(), 3, "{:?}", out.hidden_threads);
            assert!(
                out.hidden_threads
                    .iter()
                    .any(|h| h.peer == MemberId::from(&vk(62)) && h.hidden_at_ts == 77)
            );
            assert_eq!(texts(&out), vec!["from the web"]);
        }

        #[test]
        fn a_failed_write_is_tried_again_after_a_wait_and_a_bounded_number_of_times() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            rooms.archive_saver =
                DmSaver::with_waits(Duration::from_secs(10), Duration::from_secs(40));
            let (mut store, mut now) = (Store::default(), Instant::now());
            store.refuse = true;
            ask(&mut rooms, archive_command(&key, &peer, 0));
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.stores(), 1);
            assert!(!rooms.archive_saver.due(now), "it waits");
            assert_eq!(archived(&mut rooms), vec![(peer.clone(), 0)], "still shown");
            // Later it goes again and works.
            store.refuse = false;
            now += Duration::from_secs(1_000);
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.store().hidden_threads.len(), 1);
            assert!(rooms.dm_cache().archive_owed().is_empty());
            // A write that keeps failing stops after the bound.
            store.refuse = true;
            ask(&mut rooms, unarchive_command(&key, &peer));
            for _ in 0..(MAX_FAILURES * 2) {
                now += Duration::from_secs(1_000);
                run(&mut rooms, &mut store, &mut now, true);
            }
            assert_eq!(rooms.archive_saver.failures(), MAX_FAILURES);
            assert_eq!(
                rooms.dm_cache().archive_owed().len(),
                1,
                "kept for the session"
            );
        }

        #[test]
        fn the_last_thing_the_user_did_to_a_thread_is_what_is_written() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            ask(&mut rooms, archive_command(&key, &peer, 5));
            ask(&mut rooms, unarchive_command(&key, &peer));
            ask(&mut rooms, archive_command(&key, &peer, 8));
            assert_eq!(rooms.dm_cache().archive_owed().len(), 1, "one per thread");
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.store().hidden_threads[0].hidden_at_ts, 8);
            // And Un-archive removes it from the store, whatever the cutoff.
            ask(&mut rooms, unarchive_command(&key, &peer));
            run(&mut rooms, &mut store, &mut now, true);
            assert!(store.store().hidden_threads.is_empty());
            assert!(archived(&mut rooms).is_empty());
        }

        #[test]
        fn sending_into_an_archived_thread_unarchives_it_unconditionally() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            inbound(&mut rooms, 100);
            ask(&mut rooms, archive_command(&key, &peer, 100));
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.store().hidden_threads.len(), 1);
            // The peer's newest is still 100, at the cutoff: a gated unhide would do nothing.
            send(&mut rooms, &key, &peer, "hello again");
            assert!(archived(&mut rooms).is_empty(), "shown unarchived at once");
            run(&mut rooms, &mut store, &mut now, true);
            let out = store.store();
            assert!(out.hidden_threads.is_empty());
            assert_eq!(texts(&out), vec!["hello again"]);
        }

        #[test]
        fn sending_into_a_thread_that_is_not_archived_writes_no_archive_change() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            send(&mut rooms, &key, &peer, "plain");
            assert!(rooms.dm_cache().archive_owed().is_empty());
            run(&mut rooms, &mut store, &mut now, true);
            // Only the text's own write: a read and a store.
            assert_eq!(store.log, vec!["read", "store"]);
        }

        #[test]
        fn sending_before_the_archive_list_is_read_still_removes_a_stored_archive() {
            let (mut rooms, _f, key, peer) = dm_rooms();
            let (mut store, mut now) = (Store::default(), Instant::now());
            // Not read yet: the thread may be archived in the delegate.
            assert!(!rooms.dm_cache().hydrated());
            send(&mut rooms, &key, &peer, "early");
            assert_eq!(rooms.dm_cache().archive_owed().len(), 1);
            // The delegate has the archive, from the web UI.
            let owner = owner_of(&rooms);
            let mut web = web_copy(&owner);
            web.hidden_threads[0].peer = crate::rooms::dm::peer_from_key(&peer).unwrap();
            store.web_writes(&web);
            read_again(&mut rooms, &store);
            run(&mut rooms, &mut store, &mut now, true);
            assert!(store.store().hidden_threads.is_empty());
        }

        #[test]
        fn what_the_web_ui_archived_shows_and_what_it_unarchived_does_not() {
            let (mut rooms, _f, _key, peer) = rooms_hydrated();
            let mut store = Store::default();
            let member = MemberId::from(&vk(61));
            web_store(&mut rooms, &mut store, &[(61, 9)]);
            let shown = archived(&mut rooms);
            assert_eq!(shown, vec![(crate::rooms::dm::peer_key(member), 9)]);
            // The web UI unarchives (its save drops the entry): the next read does too.
            web_store(&mut rooms, &mut store, &[]);
            assert!(archived(&mut rooms).is_empty());
            let _ = peer;
        }

        /// The documented limit: the web UI's blind save can put back an older list, which
        /// unarchives what this app had archived. It is not fought.
        #[test]
        fn a_blind_save_by_the_web_ui_unarchives_and_is_not_fought() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            ask(&mut rooms, archive_command(&key, &peer, 0));
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(archived(&mut rooms).len(), 1);
            let stores = store.stores();
            // A tab that hydrated before saves its older copy over the value.
            web_store(&mut rooms, &mut store, &[]);
            assert!(
                archived(&mut rooms).is_empty(),
                "the rail shows the thread again"
            );
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.stores(), stores, "and nothing is written back");
        }

        #[test]
        fn a_read_that_began_before_the_archive_was_saved_does_not_drop_what_is_owed() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let mut store = Store::default();
            ask(&mut rooms, archive_command(&key, &peer, 0));
            // A re-read of the delegate lands while the write has not gone yet.
            web_store(&mut rooms, &mut store, &[]);
            assert_eq!(rooms.dm_cache().archive_owed().len(), 1);
            assert_eq!(archived(&mut rooms), vec![(peer, 0)]);
        }

        #[test]
        fn a_command_for_a_room_not_read_yet_waits_and_one_for_a_gone_room_is_dropped() {
            let (mut rooms, _f, key, peer) = dm_rooms();
            let room = rooms.list.remove(0);
            rooms.complete = false;
            assert!(!ask(&mut rooms, archive_command(&key, &peer, 0)));
            assert_eq!(rooms.outbox.waiting.len(), 1);
            assert!(rooms.dm_cache().archive_owed().is_empty());
            rooms.list.push(room);
            rooms.complete = true;
            let (_tx, rx) = std::sync::mpsc::channel();
            assert!(rooms.take_commands(&rx));
            assert!(rooms.outbox.waiting.is_empty());
            assert_eq!(rooms.dm_cache().archive_owed().len(), 1);
            // The room is gone and the rooms were all read: nothing is owed for it.
            let (mut rooms, _f, key, peer) = dm_rooms();
            rooms.list.clear();
            assert!(ask(&mut rooms, archive_command(&key, &peer, 0)));
            assert!(rooms.dm_cache().archive_owed().is_empty());
        }

        // The inbound-sync gate ---------------------------------------------------------------

        const NOW: u64 = 1_700_000_000;

        fn archived_in_store(rooms: &mut Rooms, store: &mut Store, peer: &str, cutoff: u64) {
            let owner = owner_of(rooms);
            let mut web = web_copy(&owner);
            web.hidden_threads = vec![HiddenDmThreadEntry {
                room_owner_vk: owner.to_bytes(),
                peer: crate::rooms::dm::peer_from_key(peer).unwrap(),
                hidden_at_ts: cutoff,
            }];
            store.web_writes(&web);
            read_again(rooms, store);
        }

        #[test]
        fn a_message_past_the_cutoff_drops_the_stored_entry() {
            let (mut rooms, _f, _key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            archived_in_store(&mut rooms, &mut store, &peer, 100);
            inbound(&mut rooms, 100);
            // At the cutoff: the thread's newest is the message that was archived.
            rooms.archive_revival(NOW);
            assert!(rooms.dm_cache().archive_owed().is_empty());
            run(&mut rooms, &mut store, &mut now, true);
            assert!(store.log.is_empty());
            // Strictly past it: the entry goes, through a write that checks the cutoff it finds.
            inbound(&mut rooms, 101);
            rooms.archive_revival(NOW);
            assert_eq!(
                rooms.dm_cache().archive_owed(),
                &[ArchiveOp::UnhideIfPast {
                    thread: thread_of(&rooms, &peer),
                    past: 101
                }]
            );
            run(&mut rooms, &mut store, &mut now, true);
            assert!(store.store().hidden_threads.is_empty());
            assert_eq!(texts(&store.store()), vec!["from the web"]);
        }

        #[test]
        fn re_landed_old_messages_leave_the_archive_alone() {
            let (mut rooms, _f, _key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            archived_in_store(&mut rooms, &mut store, &peer, 1_000);
            // The sweep took the pair's messages and the peer offered them again: original times.
            for at in [800, 1_000] {
                inbound(&mut rooms, at);
                rooms.archive_revival(NOW);
            }
            assert!(rooms.dm_cache().archive_owed().is_empty());
            run(&mut rooms, &mut store, &mut now, true);
            assert!(store.log.is_empty(), "no write: {:?}", store.log);
            // And with no messages at all (swept) the entry stays.
            rooms.list[0]
                .stored
                .room_state
                .direct_messages
                .messages
                .clear();
            rooms.archive_revival(NOW);
            assert!(rooms.dm_cache().archive_owed().is_empty());
            assert_eq!(archived(&mut rooms).len(), 1);
        }

        #[test]
        fn a_forged_future_time_is_bounded_by_the_gate_and_cannot_escalate() {
            let (mut rooms, _f, _key, peer) = rooms_hydrated();
            let (mut store, _now) = (Store::default(), Instant::now());
            // The cutoff is already as high as a bounded stamp can reach.
            let high = NOW + MAX_DM_FUTURE_SKEW_SECS;
            archived_in_store(&mut rooms, &mut store, &peer, high);
            inbound(&mut rooms, u64::MAX / 2);
            rooms.archive_revival(NOW);
            assert!(rooms.dm_cache().archive_owed().is_empty());
            // A first forgery over an ordinary cutoff still revives it (accepted, documented).
            let (mut rooms, _f, _key, peer) = rooms_hydrated();
            archived_in_store(&mut rooms, &mut store, &peer, 100);
            inbound(&mut rooms, u64::MAX / 2);
            rooms.archive_revival(NOW);
            assert_eq!(
                rooms.dm_cache().archive_owed(),
                &[ArchiveOp::UnhideIfPast {
                    thread: thread_of(&rooms, &peer),
                    past: NOW + MAX_DM_FUTURE_SKEW_SECS
                }]
            );
        }

        #[test]
        fn a_stored_entry_with_a_change_owed_is_left_to_the_users_change_and_revived_after_it() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            archived_in_store(&mut rooms, &mut store, &peer, 100);
            inbound(&mut rooms, 150);
            ask(&mut rooms, archive_command(&key, &peer, 150));
            // A message past the user's own cutoff lands before the write has gone: the owed
            // change is the user's and stays (the gate would otherwise replace it).
            inbound(&mut rooms, 200);
            rooms.archive_revival(NOW);
            let hide = ArchiveOp::Hide {
                thread: thread_of(&rooms, &peer),
                hidden_at_ts: 150,
            };
            assert_eq!(rooms.dm_cache().archive_owed(), &[hide]);
            // The write goes, and the entry it leaves is then dropped by the gate: the thread
            // converges on the rail, with nothing lost.
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.store().hidden_threads[0].hidden_at_ts, 150);
            rooms.archive_revival(NOW);
            assert_eq!(
                rooms.dm_cache().archive_owed(),
                &[ArchiveOp::UnhideIfPast {
                    thread: thread_of(&rooms, &peer),
                    past: 200
                }]
            );
            run(&mut rooms, &mut store, &mut now, true);
            assert!(store.store().hidden_threads.is_empty());
        }

        #[test]
        fn the_gate_checks_the_cutoff_a_concurrent_device_stored() {
            let (mut rooms, _f, _key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            archived_in_store(&mut rooms, &mut store, &peer, 100);
            inbound(&mut rooms, 150);
            rooms.archive_revival(NOW);
            // Before the write, another device archived again at a cutoff past the message.
            let owner = owner_of(&rooms);
            let mut newer = web_copy(&owner);
            newer.hidden_threads = vec![HiddenDmThreadEntry {
                room_owner_vk: owner.to_bytes(),
                peer: crate::rooms::dm::peer_from_key(&peer).unwrap(),
                hidden_at_ts: 400,
            }];
            store.before_cas = Some(Box::new(move |s| s.web_writes(&newer)));
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.store().hidden_threads[0].hidden_at_ts, 400, "kept");
        }

        #[test]
        fn our_own_later_message_in_the_room_never_sets_the_cutoff() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            inbound(&mut rooms, 100);
            // The user's own message, stamped by their clock long after the peer's, is in the
            // room's state as the room holds it.
            outbound(&mut rooms, 90_000);
            ask(&mut rooms, archive_command(&key, &peer, 0));
            assert_eq!(archived(&mut rooms), vec![(peer, 100)]);
        }

        #[test]
        fn a_far_future_stamp_is_the_cutoff_as_signed_and_keeps_the_thread_archived() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let forged = u64::MAX / 2;
            inbound(&mut rooms, forged);
            // The window's row carries the time as signed (`last_inbound_signed`), the clamped
            // time is for display only.
            ask(&mut rooms, archive_command(&key, &peer, forged));
            assert_eq!(archived(&mut rooms), vec![(peer.clone(), forged)]);
            let t = rooms.list[0].dm_threads(rooms.dm_cache(), &|_| None);
            assert_eq!(t.threads[0].last_inbound_signed, forged);
            assert!(t.threads[0].last_inbound_ts < forged, "shown clamped");
            assert!(crate::rooms::dm_archive::is_hidden(
                Some(forged),
                t.threads[0].last_inbound_signed
            ));
            // The web UI's gate cannot drop it either.
            assert!(!crate::rooms::dm_archive::should_unhide_for_inbound(
                Some(forged),
                u64::MAX,
                NOW
            ));
        }

        #[test]
        fn a_change_for_a_thread_the_store_already_matches_is_done_and_clears() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            // The store holds nothing about the thread (the web UI took the entry out): the
            // unarchive finds nothing to remove, stores nothing and is done, not a failure.
            web_store(&mut rooms, &mut store, &[]);
            ask(&mut rooms, archive_command(&key, &peer, 0));
            ask(&mut rooms, unarchive_command(&key, &peer));
            let stores = store.stores();
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.stores(), stores, "nothing to store");
            assert!(rooms.dm_cache().archive_owed().is_empty());
            assert_eq!(rooms.dm_saver.failures(), 0);
            assert!(!rooms.outbox.work.has_dm());
        }

        #[test]
        fn the_window_is_sent_a_new_snapshot_when_the_archive_list_changes() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            inbound(&mut rooms, 100);
            let first = rooms.dm_snapshot().expect("the first report");
            assert!(first.archived.is_empty());
            assert!(rooms.dm_snapshot().is_none(), "nothing changed");
            // The user archives: a new snapshot with it.
            ask(&mut rooms, archive_command(&key, &peer, 0));
            let archived_now = rooms.dm_snapshot().expect("an archive is a change");
            assert_eq!(archived_now.archived.len(), 1);
            assert!(rooms.dm_snapshot().is_none());
            // The write ends: the list is as shown, so nothing new to say.
            run(&mut rooms, &mut store, &mut now, true);
            assert!(
                rooms.dm_snapshot().is_none(),
                "what was shown is what was saved"
            );
            // The web UI's blind save takes it away, and a read sees that: a new snapshot.
            web_store(&mut rooms, &mut store, &[]);
            let gone = rooms.dm_snapshot().expect("a read that changed the list");
            assert!(gone.archived.is_empty());
        }

        #[test]
        fn a_sweep_and_re_land_that_keep_the_count_but_raise_the_newest_stamp_are_looked_at() {
            let (mut rooms, _f, _key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            archived_in_store(&mut rooms, &mut store, &peer, 100);
            inbound(&mut rooms, 100);
            rooms.archive_revival(NOW);
            assert!(rooms.dm_cache().archive_owed().is_empty());
            // The count is the same, the one message is newer.
            rooms.list[0].stored.room_state.direct_messages.messages[0]
                .message
                .timestamp = 150;
            rooms.archive_revival(NOW);
            assert_eq!(rooms.dm_cache().archive_owed().len(), 1);
            run(&mut rooms, &mut store, &mut now, true);
            assert!(store.store().hidden_threads.is_empty());
        }

        /// A second room with the same peer: a copy of the room under another owner key (what
        /// these tests need is only that the two are told apart by it).
        fn with_second_room(rooms: &mut Rooms) -> (String, VerifyingKey) {
            let mut other = rooms.list[0].stored.clone();
            other.owner_vk = SigningKey::from_bytes(&[9; 32]).verifying_key();
            let owner = other.owner_vk;
            let room = LiveRoom::new(other);
            let key = room.key();
            rooms.list.push(room);
            (key, owner)
        }

        #[test]
        fn archiving_and_reviving_are_scoped_to_the_room_and_read_that_rooms_messages() {
            let (mut rooms, _f, key_a, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            let (key_b, owner_b) = with_second_room(&mut rooms);
            // The peer wrote in room A at 100 and in room B at 500.
            inbound(&mut rooms, 100);
            let f = crate::rooms::room_tests::Fixture::new();
            let me = rooms.list[0].stored.self_verifying_key().unwrap();
            let m = compose_direct_message(&f.owner, &me, &owner_b, 500, 500, b"b").unwrap();
            rooms.list[1].stored.room_state.direct_messages.messages = vec![m];

            // Archive in A: the cutoff is A's 100, and B's thread is not archived.
            ask(&mut rooms, archive_command(&key_a, &peer, 0));
            let snap = rooms.dm_snapshot().expect("a change");
            let listed: Vec<(&str, u64)> = snap
                .archived
                .iter()
                .map(|a| (a.room.as_str(), a.hidden_at_ts))
                .collect();
            assert_eq!(listed, vec![(key_a.as_str(), 100)]);
            run(&mut rooms, &mut store, &mut now, true);
            let stored = store.store().hidden_threads;
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].room_owner_vk, owner_of(&rooms).to_bytes());

            // An entry for B at 300 (B's peer wrote at 500, past it) is dropped, A's stays.
            let mut web = web_copy(&owner_of(&rooms));
            web.hidden_threads = vec![
                stored[0].clone(),
                HiddenDmThreadEntry {
                    room_owner_vk: owner_b.to_bytes(),
                    peer: stored[0].peer,
                    hidden_at_ts: 300,
                },
            ];
            store.web_writes(&web);
            read_again(&mut rooms, &store);
            rooms.archive_revival(NOW);
            run(&mut rooms, &mut store, &mut now, true);
            let left = store.store().hidden_threads;
            assert_eq!(left.len(), 1);
            assert_eq!(
                left[0].room_owner_vk,
                owner_of(&rooms).to_bytes(),
                "A stays"
            );
            let _ = key_b;
        }

        #[test]
        fn an_entry_for_a_room_that_is_not_listed_is_not_sent_to_the_window() {
            let (mut rooms, _f, _key, peer) = rooms_hydrated();
            let mut store = Store::default();
            let gone = SigningKey::from_bytes(&[8; 32]).verifying_key();
            let mut web = web_copy(&owner_of(&rooms));
            web.hidden_threads = vec![HiddenDmThreadEntry {
                room_owner_vk: gone.to_bytes(),
                peer: crate::rooms::dm::peer_from_key(&peer).unwrap(),
                hidden_at_ts: 5,
            }];
            store.web_writes(&web);
            read_again(&mut rooms, &store);
            assert!(archived(&mut rooms).is_empty());
            // And it is left in the store, for the room's other devices.
            assert_eq!(store.store().hidden_threads.len(), 1);
        }

        #[test]
        fn the_loop_revives_before_it_saves() {
            let body = session_code();
            let revive = body
                .find("rooms.archive_revival(unix_secs());")
                .expect("the loop revives archived threads");
            let save = body
                .find("rooms.dm_save_tick(writes, Instant::now());")
                .expect("the loop saves");
            assert!(revive < save, "the entry is owed before the save is built");
        }

        #[test]
        fn a_store_that_keeps_refusing_archive_changes_does_not_hold_back_the_text() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            web_store(&mut rooms, &mut store, &[]);
            // The value now holds an archive entry this build cannot read (a newer web UI's).
            let mut blob = Vec::new();
            let bad = ciborium::Value::Map(vec![
                (
                    ciborium::Value::Text("entries".into()),
                    ciborium::Value::Array(vec![]),
                ),
                (
                    ciborium::Value::Text("hidden_threads".into()),
                    ciborium::Value::Array(vec![ciborium::Value::from(1u32)]),
                ),
            ]);
            ciborium::ser::into_writer(&bad, &mut blob).unwrap();
            store.value = Some(blob.clone());
            store.generation += 1;
            ask(&mut rooms, archive_command(&key, &peer, 0));
            send(&mut rooms, &key, &peer, "still goes");
            run(&mut rooms, &mut store, &mut now, true);
            // The text went in, over a list it did not touch; the archive change was refused,
            // alone, and counted against its own pacing.
            assert!(
                store
                    .value
                    .as_deref()
                    .is_some_and(|v| v != blob.as_slice() && v.len() > blob.len())
            );
            assert_eq!(rooms.dm_saver.failures(), 0);
            assert_eq!(rooms.archive_saver.failures(), 1);
            assert!(!rooms.archive_saver.due(now));
            assert!(rooms.dm_saver.due(now), "the text's own pacing is not held");
            // One change per thread: the send's unarchive replaced the archive. Still owed.
            assert_eq!(rooms.dm_cache().archive_owed().len(), 1);
        }

        #[test]
        fn an_archive_change_found_already_made_asks_for_the_stored_rooms_to_be_read_again() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            // Another device already archived it, at a higher cutoff than this app asks for.
            archived_in_store(&mut rooms, &mut store, &peer, 900);
            rooms.dm_cache.note_read(
                DmRead::Read(Box::new(
                    OutboundCache::from_bytes(&{
                        let mut out = Vec::new();
                        ciborium::ser::into_writer(&OutboundDmStore::default(), &mut out).unwrap();
                        out
                    })
                    .unwrap(),
                )),
                true,
            );
            ask(&mut rooms, archive_command(&key, &peer, 0));
            assert!(!rooms.outbox.work.reload_pending());
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.stores(), 0, "nothing to store");
            assert!(
                rooms.outbox.work.reload_pending(),
                "this copy was out of date"
            );
        }

        #[test]
        fn an_archive_with_no_signing_key_here_uses_the_threads_own_row() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            inbound(&mut rooms, 100);
            rooms.list[0].stored.self_sk = None;
            rooms.list[0].stored.self_vk = None;
            ask(&mut rooms, archive_command(&key, &peer, 7));
            assert_eq!(archived(&mut rooms), vec![(peer, 7)]);
        }

        #[test]
        fn a_new_entry_that_does_not_fit_does_not_hold_back_an_unarchive() {
            use crate::rooms::dm_store::MAX_WRITE_BYTES;
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            let owner = owner_of(&rooms);
            let enc = |s: &OutboundDmStore| {
                let mut out = Vec::new();
                ciborium::ser::into_writer(s, &mut out).unwrap();
                out
            };
            let mut web = web_copy(&owner);
            let x_peer = web.hidden_threads[0].peer;
            let without = {
                let mut w = web.clone();
                w.hidden_threads.clear();
                enc(&w).len()
            };
            let x_size = enc(&web).len() - without;
            // Pad the sent text so that the value is over the bound with the entry and one byte
            // under it without: one new entry cannot fit, and taking the old one out always can.
            let target = MAX_WRITE_BYTES - 1 + x_size;
            web.entries[0].plaintext = "x".repeat(100_000);
            let at = enc(&web).len();
            web.entries[0].plaintext = "x".repeat(100_000 + target - at);
            assert_eq!(enc(&web).len(), target);
            store.web_writes(&web);
            read_again(&mut rooms, &store);
            let x_key = crate::rooms::dm::peer_key(x_peer);
            ask(&mut rooms, unarchive_command(&key, &x_key));
            ask(&mut rooms, archive_command(&key, &peer, 0));
            run(&mut rooms, &mut store, &mut now, true);
            // The Un-archive was written, alone and first.
            assert!(store.store().hidden_threads.is_empty(), "the entry is gone");
            // The new entry did not fit: refused, still owed, on the archive pacing only.
            assert_eq!(rooms.dm_cache().archive_owed().len(), 1);
            assert_eq!(rooms.archive_saver.failures(), 1);
            assert_eq!(rooms.dm_saver.failures(), 0);
        }

        #[test]
        fn thousands_of_archived_entries_still_revive_the_right_threads() {
            let (mut rooms, _f, _key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            let owner = owner_of(&rooms);
            let me_peer = crate::rooms::dm::peer_from_key(&peer).unwrap();
            let mut web = web_copy(&owner);
            web.hidden_threads = (0..4000u64)
                .map(|n| HiddenDmThreadEntry {
                    room_owner_vk: owner.to_bytes(),
                    peer: MemberId(freenet_scaffold::util::FastHash(n as i64 + 1_000_000)),
                    hidden_at_ts: 500,
                })
                .collect();
            web.hidden_threads.push(HiddenDmThreadEntry {
                room_owner_vk: owner.to_bytes(),
                peer: me_peer,
                hidden_at_ts: 100,
            });
            store.web_writes(&web);
            read_again(&mut rooms, &store);
            assert_eq!(rooms.dm_cache().archived().len(), 4001);
            inbound(&mut rooms, 150);
            rooms.archive_revival(NOW);
            // Only the thread with a message past its cutoff is dropped.
            assert_eq!(
                rooms.dm_cache().archive_owed(),
                &[ArchiveOp::UnhideIfPast {
                    thread: thread_of(&rooms, &peer),
                    past: 150
                }]
            );
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.store().hidden_threads.len(), 4000);
        }

        #[test]
        fn a_pair_at_its_cap_revives_when_a_new_message_evicts_an_old_one() {
            let (mut rooms, _f, _key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            for at in 1..=100u64 {
                inbound(&mut rooms, at);
            }
            archived_in_store(&mut rooms, &mut store, &peer, 100);
            rooms.archive_revival(NOW);
            assert!(rooms.dm_cache().archive_owed().is_empty());
            // The contract keeps 100 a pair: the newest replaces the oldest, the count stays.
            let msgs = &mut rooms.list[0].stored.room_state.direct_messages.messages;
            msgs.remove(0);
            inbound(&mut rooms, 101);
            assert_eq!(
                rooms.list[0]
                    .stored
                    .room_state
                    .direct_messages
                    .messages
                    .len(),
                100
            );
            rooms.archive_revival(NOW);
            assert_eq!(rooms.dm_cache().archive_owed().len(), 1);
            run(&mut rooms, &mut store, &mut now, true);
            assert!(store.store().hidden_threads.is_empty());
        }

        #[test]
        fn a_send_into_a_thread_this_copy_does_not_hold_queues_no_unhide_and_asks_for_one_reread() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            web_store(&mut rooms, &mut store, &[]);
            assert!(rooms.dm_cache().hydrated());
            send(&mut rooms, &key, &peer, "hello");
            assert!(
                rooms.dm_cache().archive_owed().is_empty(),
                "the declined behaviour"
            );
            assert!(!rooms.outbox.work.reload_pending());
            rooms.dm_save_tick(true, now);
            assert!(rooms.outbox.work.take_reload(), "one re-read is asked for");
            // And not again at once, however many sends: at most one in the window.
            send(&mut rooms, &key, &peer, "again");
            rooms.dm_save_tick(true, now);
            assert!(!rooms.outbox.work.reload_pending());
            now += super::super::REREAD_EVERY;
            rooms.dm_save_tick(true, now);
            assert!(rooms.outbox.work.take_reload());
            rooms.dm_save_tick(true, now);
            assert!(!rooms.outbox.work.reload_pending());
        }

        #[test]
        fn a_conditional_removal_the_store_skipped_is_not_applied_to_the_copy() {
            let (mut rooms, _f, _key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            archived_in_store(&mut rooms, &mut store, &peer, 100);
            inbound(&mut rooms, 150);
            rooms.archive_revival(NOW);
            // Another device raised the cutoff past the message before the write.
            let owner = owner_of(&rooms);
            let mut newer = web_copy(&owner);
            newer.hidden_threads = vec![HiddenDmThreadEntry {
                room_owner_vk: owner.to_bytes(),
                peer: crate::rooms::dm::peer_from_key(&peer).unwrap(),
                hidden_at_ts: 400,
            }];
            store.before_cas = Some(Box::new(move |s| s.web_writes(&newer)));
            run(&mut rooms, &mut store, &mut now, true);
            // The store kept the entry; this copy still holds it (at the cutoff it knew), and
            // the revival does not repeat the same removal.
            assert!(
                rooms.outbox.work.reload_pending(),
                "read again to see what it holds"
            );
            assert_eq!(rooms.dm_cache().archived().len(), 1);
            assert!(rooms.dm_cache().archive_owed().is_empty());
            // The same stale copy does not ask for the same removal again.
            rooms.archive_revival(NOW);
            assert!(rooms.dm_cache().archive_owed().is_empty());
            // A read shows what the store holds (400, past the message): nothing to revive.
            read_again(&mut rooms, &store);
            rooms.archive_revival(NOW);
            assert!(rooms.dm_cache().archive_owed().is_empty());
        }

        #[test]
        fn a_write_whose_only_change_was_left_out_is_booked_to_the_archive_pacing() {
            let (mut rooms, _f, _key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            web_store(&mut rooms, &mut store, &[]);
            let gone = SigningKey::from_bytes(&[8; 32]).verifying_key();
            rooms.dm_cache.archive(ArchiveOp::Hide {
                thread: (
                    gone.to_bytes(),
                    crate::rooms::dm::peer_from_key(&peer).unwrap(),
                ),
                hidden_at_ts: 5,
            });
            // The text saver has failures of its own, and a wait.
            rooms.dm_saver.ended(false, now);
            assert_eq!(rooms.dm_saver.failures(), 1);
            assert!(!rooms.dm_saver.due(now));
            run(&mut rooms, &mut store, &mut now, true);
            assert!(
                rooms.dm_cache().archive_owed().is_empty(),
                "left out, no longer owed"
            );
            assert_eq!(rooms.dm_saver.failures(), 1, "the text saver is untouched");
            assert!(!rooms.dm_saver.due(now));
            assert_eq!(rooms.archive_saver.failures(), 0);
            assert!(
                store.store().hidden_threads.is_empty(),
                "nothing written for it"
            );
            assert_eq!(store.stores(), 0);
        }

        #[test]
        fn a_new_entry_for_a_room_missing_from_a_partial_list_waits_for_a_complete_read() {
            let (mut rooms, _f, _key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            web_store(&mut rooms, &mut store, &[]);
            let missing = SigningKey::from_bytes(&[8; 32]).verifying_key();
            rooms.dm_cache.archive(ArchiveOp::Hide {
                thread: (
                    missing.to_bytes(),
                    crate::rooms::dm::peer_from_key(&peer).unwrap(),
                ),
                hidden_at_ts: 5,
            });
            rooms.complete = false;
            let reads = store.log.len();
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(
                store.log.len(),
                reads,
                "no write while the list may be partial"
            );
            assert_eq!(rooms.dm_cache().archive_owed().len(), 1);
            rooms.complete = true;
            run(&mut rooms, &mut store, &mut now, true);
            assert!(
                rooms.dm_cache().archive_owed().is_empty(),
                "now it is known to be gone"
            );
        }

        #[test]
        fn the_reread_after_a_send_is_not_used_up_by_a_tick_that_cannot_run_it() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, now) = (Store::default(), Instant::now());
            web_store(&mut rooms, &mut store, &[]);
            send(&mut rooms, &key, &peer, "hello");
            rooms.dm_save_tick(false, now);
            rooms.update_required = true;
            rooms.dm_save_tick(true, now);
            rooms.update_required = false;
            assert!(
                !rooms.outbox.work.reload_pending(),
                "nothing asked while it cannot run"
            );
            rooms.dm_save_tick(true, now);
            assert!(rooms.outbox.work.take_reload(), "asked once it can");
        }

        // Privacy and the shape of the code --------------------------------------------------

        #[test]
        fn the_archive_code_names_no_peer_and_prints_nothing_of_a_thread() {
            let code_only = |src: &str| -> String {
                src.replace("\r\n", "\n")
                    .split_once("#[cfg(test)]\n#[path")
                    .expect("the marker of the end of the code")
                    .0
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("//"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            for file in [
                code_only(include_str!("dm_archive.rs")),
                code_only(include_str!("dm_store.rs")),
            ] {
                for line in file.lines() {
                    assert!(
                        !line.contains("println!")
                            && !line.contains("dbg!")
                            && !line.contains("tracing::"),
                        "{line}"
                    );
                }
            }
            let body = session_code();
            for name in ["fn archive_dm", "fn archive_revival"] {
                let f = body.split(name).nth(1).expect("the function");
                let f = f.split("\n    }\n").next().unwrap();
                // At most one fixed message, with no format argument: nothing of a thread.
                for line in f.lines().filter(|l| l.contains("println!")) {
                    assert!(!line.contains('{') && !line.contains(','), "{name}: {line}");
                }
                assert!(!f.contains("dbg!") && !f.contains("peer_key"), "{name}");
            }
        }

        #[test]
        fn an_owed_removal_and_an_archive_change_go_in_their_own_writes_removal_first() {
            let (mut rooms, _f, key, peer) = rooms_hydrated();
            let (mut store, mut now) = (Store::default(), Instant::now());
            let owner = owner_of(&rooms);
            web_store(&mut rooms, &mut store, &[]);
            let entry = web_copy(&owner).entries[0].clone();
            let k: EntryKey = (owner.to_bytes(), entry.recipient, entry.purge_token);
            rooms.dm_saver.owes_removal(k);
            ask(&mut rooms, archive_command(&key, &peer, 0));
            // The removal goes alone and first; the archive change is the next write, so a store
            // that refuses one cannot refuse the other.
            rooms.dm_save_tick(true, now);
            assert!(rooms.outbox.work.has_dm());
            run(&mut rooms, &mut store, &mut now, true);
            assert_eq!(store.stores(), 2, "{:?}", store.log);
            let out = store.store();
            assert!(out.entries.is_empty(), "the removal was made");
            assert_eq!(out.hidden_threads.len(), 1, "and the archive");
            assert!(rooms.dm_saver.removals().is_empty());
            assert!(rooms.dm_cache().archive_owed().is_empty());
        }
    }
}

// Deleting their messages ---------------------------------------------------------------------

mod deleting_their_messages {
    use super::*;
    use crate::rooms::dm_purge;
    use river_core::room_state::ChatRoomStateV1Delta;
    use river_core::room_state::direct_messages::{
        DirectMessagesDelta, PurgeToken, advance_recipient_purges, compose_direct_message,
    };

    /// The room of `dm_rooms`, with `n` messages from the owner to the user taken by it, and
    /// their tokens.
    fn with_inbound(
        n: u64,
    ) -> (
        Rooms,
        crate::rooms::room_tests::Fixture,
        String,
        String,
        Vec<PurgeToken>,
    ) {
        let (mut rooms, f, key, peer) = dm_rooms();
        let mut tokens = Vec::new();
        for i in 0..n {
            let ts = 1_700_000_000 + i;
            let m = compose_direct_message(
                &f.owner,
                &f.me.verifying_key(),
                &f.owner.verifying_key(),
                ts,
                ts,
                format!("owner says {i}").as_bytes(),
            )
            .unwrap();
            tokens.push(m.purge_token());
            rooms.list[0]
                .apply_delta(ChatRoomStateV1Delta {
                    direct_messages: Some(DirectMessagesDelta {
                        new_messages: vec![m],
                        advanced_purges: Vec::new(),
                    }),
                    ..Default::default()
                })
                .unwrap();
        }
        (rooms, f, key, peer, tokens)
    }

    fn purge(room: &str, peer: &str, tokens: &[PurgeToken], ticket: u64) -> Command {
        Command {
            room: room.to_owned(),
            action: Action::PurgeDm {
                peer: crate::rooms::dm::peer_from_key(peer).expect("a member key"),
                tokens: tokens.to_vec(),
                ticket,
            },
        }
    }

    fn take_purge(rooms: &mut Rooms, command: Command) -> Vec<(u64, Result<(), String>)> {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(command).unwrap();
        rooms.take_commands(&rx);
        std::mem::take(&mut rooms.dm_purge_results)
            .into_iter()
            .map(|r| (r.ticket, r.result))
            .collect()
    }

    #[test]
    fn a_deletion_is_answered_shown_as_under_way_and_takes_the_messages_out_once_the_room_holds_it()
    {
        let (mut rooms, _f, key, peer, tokens) = with_inbound(2);
        let before = rooms.dm_snapshot().expect("the thread");
        assert_eq!(before.threads[0].messages.len(), 2);
        assert!(before.purges.is_empty());

        assert_eq!(
            take_purge(&mut rooms, purge(&key, &peer, &tokens, 4)),
            vec![(4, Ok(()))]
        );
        // On its way: the messages are still the room's, the thread says a deletion is out.
        let under = rooms.dm_snapshot().expect("the state changed");
        assert_eq!(under.threads[0].messages.len(), 2);
        assert_eq!(under.purges.len(), 1);
        assert_eq!(under.purges[0].room, key);
        assert_eq!(under.purges[0].peer_key, peer);
        assert_eq!(under.purges[0].count, 2);
        assert_eq!(under.purges[0].pending.failed, None);
        assert!(rooms.dm_snapshot().is_none(), "nothing changed");

        // Sent and taken: the room holds it, the messages are gone, and so is the line.
        let mut changed = false;
        let delta = rooms
            .outbox
            .next_update(&rooms.list[0], Instant::now(), &mut changed)
            .expect("goes out");
        assert_eq!(
            delta
                .direct_messages
                .as_ref()
                .unwrap()
                .advanced_purges
                .len(),
            1
        );
        assert!(rooms.update_answered(&rooms.list[0].contract_id(), Ok(())));
        let after = rooms.dm_snapshot().expect("the thread changed");
        assert!(after.threads.is_empty(), "{:?}", after.threads);
        assert!(after.purges.is_empty());
        assert!(rooms.outbox.is_empty());
    }

    #[test]
    fn a_deletion_the_session_cannot_take_is_answered_with_why_and_leaves_nothing() {
        let (mut rooms, f, key, peer, tokens) = with_inbound(1);
        // River has moved on: nothing is written.
        rooms.update_required = true;
        assert_eq!(
            take_purge(&mut rooms, purge(&key, &peer, &tokens, 1)),
            vec![(
                1,
                Err(crate::rooms::generation::UPDATE_REQUIRED_PURGE.to_owned())
            )]
        );
        rooms.update_required = false;
        assert!(rooms.outbox.is_empty());

        // A room only the store has read may lack a deletion made elsewhere, which this one
        // would lift: it is refused until the network has given the state.
        let owner = rooms.list[0].stored.owner_vk;
        rooms.network_seen.remove(&owner);
        let answers = take_purge(&mut rooms, purge(&key, &peer, &tokens, 2));
        assert_eq!(answers.len(), 1);
        assert!(answers[0].1.as_ref().unwrap_err().contains("hasn't loaded"));
        assert!(rooms.outbox.is_empty());
        rooms.network_seen.insert(owner);

        // Nothing of theirs left to delete, and a token that is not theirs.
        let answers = take_purge(&mut rooms, purge(&key, &peer, &[], 3));
        assert_eq!(answers, vec![(3, Err(dm_purge::Refusal::Nothing.text()))]);
        let stranger = PurgeToken([9; 16]);
        assert_eq!(
            take_purge(&mut rooms, purge(&key, &peer, &[stranger], 4)),
            vec![(4, Err(dm_purge::Refusal::Nothing.text()))]
        );
        assert!(rooms.outbox.is_empty());

        // A room that is gone says so; one that may only be unread yet waits.
        assert_eq!(
            take_purge(&mut rooms, purge("nope", &peer, &tokens, 5)),
            vec![(5, Err("This room is no longer here.".to_owned()))]
        );
        rooms.complete = false;
        assert!(take_purge(&mut rooms, purge("nope", &peer, &tokens, 6)).is_empty());
        assert_eq!(rooms.outbox.waiting.len(), 1);
        rooms.complete = true;

        // No key on this computer: the room's own words.
        rooms.list[0].stored.self_sk = None;
        let answers = take_purge(&mut rooms, purge(&key, &peer, &tokens, 7));
        // (The command that waited is taken again now that the read is complete.)
        assert_eq!(
            answers,
            vec![
                (6, Err("This room is no longer here.".to_owned())),
                (
                    7,
                    Err(crate::rooms::outbox::Blocked::NoKey.reason().to_owned())
                )
            ]
        );
        let _ = f;
    }

    #[test]
    fn a_deletion_waiting_when_river_moves_on_fails_with_why_and_is_kept_to_retry_or_discard() {
        let (mut rooms, _f, key, peer, tokens) = with_inbound(1);
        take_purge(&mut rooms, purge(&key, &peer, &tokens, 1));
        rooms.dm_snapshot().expect("first");
        // The held gate fails what waits, as for a message.
        assert!(
            rooms
                .outbox
                .fail_queued(crate::rooms::generation::UPDATE_REQUIRED)
        );
        let snap = rooms.dm_snapshot().expect("the state changed");
        let p = &snap.purges[0];
        assert!(
            p.pending
                .failed
                .as_deref()
                .is_some_and(|w| w.contains("newer version of River"))
        );
        // Its messages are still shown, and it is not sent while River is ahead.
        assert_eq!(snap.threads[0].messages.len(), 1);
        let mut changed = false;
        assert!(
            rooms
                .outbox
                .next_update(&rooms.list[0], Instant::now(), &mut changed)
                .is_none()
        );
    }

    #[test]
    fn a_message_the_recipient_deleted_loses_its_saved_text_only_by_the_envelope_that_names_it() {
        let (mut rooms, f, _key, _peer, _tokens) = with_inbound(0);
        let owner_vk = rooms.list[0].stored.owner_vk;
        let owner_id = f.owner_id();
        let named = PurgeToken([1; 16]);
        let unnamed = PurgeToken([2; 16]);
        let elsewhere = PurgeToken([3; 16]);
        rooms
            .dm_cache
            .insert_for_test(&owner_vk, owner_id, named, "named");
        rooms
            .dm_cache
            .insert_for_test(&owner_vk, owner_id, unnamed, "unnamed");
        // The same token to someone else is another entry.
        let third = river_core::room_state::member::MemberId::from(
            &ed25519_dalek::SigningKey::from_bytes(&[9; 32]).verifying_key(),
        );
        rooms
            .dm_cache
            .insert_for_test(&owner_vk, third, named, "other recipient");
        rooms
            .dm_cache
            .insert_for_test(&owner_vk, owner_id, elsewhere, "gone from the room");

        // No envelope: a message missing from the room says nothing, so nothing is dropped.
        rooms.prune_purged_texts();
        assert_eq!(rooms.dm_cache.len(), 4);
        assert!(rooms.dm_saver.removals().is_empty());

        // The owner's envelope names two tokens of the pair, in a room read from the network.
        let envelope =
            advance_recipient_purges(&f.owner, &f.owner.verifying_key(), None, [named]).unwrap();
        rooms.list[0]
            .apply_delta(dm_purge::delta(&envelope))
            .unwrap();
        rooms.network_seen.remove(&owner_vk);
        rooms.prune_purged_texts();
        assert_eq!(
            rooms.dm_cache.len(),
            4,
            "a room not read from the network is not trusted"
        );
        rooms.network_seen.insert(owner_vk);
        rooms.prune_purged_texts();
        assert_eq!(rooms.dm_cache.plaintext(&owner_vk, owner_id, &named), None);
        for (who, token, text) in [
            (owner_id, unnamed, "unnamed"),
            (third, named, "other recipient"),
            (owner_id, elsewhere, "gone from the room"),
        ] {
            assert_eq!(rooms.dm_cache.plaintext(&owner_vk, who, &token), Some(text));
        }
        // The store is owed the removal of that one entry, once.
        assert_eq!(
            rooms.dm_saver.removals(),
            &[(owner_vk.to_bytes(), owner_id, named)]
        );
        rooms.prune_purged_texts();
        assert_eq!(rooms.dm_saver.removals().len(), 1);
    }

    #[test]
    fn a_text_a_save_is_adding_is_removed_after_the_save_when_its_recipient_deletes_it() {
        let (mut rooms, f, _key, _peer, _tokens) = with_inbound(0);
        let owner_vk = rooms.list[0].stored.owner_vk;
        let owner_id = f.owner_id();
        let token = PurgeToken([4; 16]);
        let key = (owner_vk.to_bytes(), owner_id, token);
        // The user sent it, its text is unsaved, and a write that adds it is out.
        rooms
            .dm_cache
            .remember_sent(&owner_vk, owner_id, token, f.me_id(), 5, "text".into());
        rooms.dm_saver.started(vec![key], Vec::new());
        // Its recipient deletes it.
        let envelope =
            advance_recipient_purges(&f.owner, &f.owner.verifying_key(), None, [token]).unwrap();
        rooms.list[0]
            .apply_delta(dm_purge::delta(&envelope))
            .unwrap();
        rooms.prune_purged_texts();
        assert_eq!(rooms.dm_cache.plaintext(&owner_vk, owner_id, &token), None);
        // The write puts it in the store, so its removal is owed, to follow the write.
        assert_eq!(rooms.dm_saver.removals(), &[key]);
        // The save's end does not bring it back or lose track of the removal.
        rooms.dm_cache.note_saved(&[key], &[]);
        assert_eq!(rooms.dm_cache.plaintext(&owner_vk, owner_id, &token), None);
        assert_eq!(rooms.dm_saver.removals(), &[key]);
    }

    #[test]
    fn the_prune_runs_again_only_when_an_envelope_or_the_cache_changed() {
        let (mut rooms, f, _key, _peer, _tokens) = with_inbound(0);
        let owner_vk = rooms.list[0].stored.owner_vk;
        rooms.prune_purged_texts();
        let first = rooms.purge_pruned;
        assert_eq!(first, Some(rooms.purge_fingerprint()));
        // Nothing changed: the same fingerprint, so the next tick skips.
        rooms.prune_purged_texts();
        assert_eq!(rooms.purge_pruned, first);
        // A new envelope changes it.
        let envelope = advance_recipient_purges(
            &f.owner,
            &f.owner.verifying_key(),
            None,
            [PurgeToken([1; 16])],
        )
        .unwrap();
        rooms.list[0]
            .apply_delta(dm_purge::delta(&envelope))
            .unwrap();
        assert_ne!(Some(rooms.purge_fingerprint()), first);
        // So does the cache (a read that brings an entry back).
        rooms.prune_purged_texts();
        let second = rooms.purge_fingerprint();
        rooms
            .dm_cache
            .insert_for_test(&owner_vk, f.owner_id(), PurgeToken([5; 16]), "x");
        assert_ne!(rooms.purge_fingerprint(), second);
    }

    #[test]
    fn nothing_here_logs_a_token_and_a_deletion_is_refused_for_a_room_the_network_has_not_given() {
        let norm = |s: &str| s.replace("\r\n", "\n");
        let session = norm(include_str!("session.rs"));
        let body = session
            .split("#[cfg(test)]\n#[path = \"session_tests.rs\"]")
            .next()
            .unwrap();
        // The deletion is refused for a room the network has not given.
        let purge_fn = body.split("fn purge_dm").nth(1).unwrap();
        let purge_fn = purge_fn.split("\n    }\n").next().unwrap();
        assert!(purge_fn.contains("self.network_seen.contains"));
        assert!(purge_fn.contains("self.update_required"));
        for file in [
            include_str!("dm_purge.rs"),
            include_str!("../chat/dm_purge.rs"),
        ] {
            let file = norm(file);
            for line in file.lines().filter(|l| !l.trim_start().starts_with("//")) {
                assert!(
                    !line.contains("println!")
                        && !line.contains("dbg!")
                        && !line.contains("tracing::"),
                    "{line}"
                );
            }
        }
    }
}

/// The app's own retry of a store that could not be set up: the first re-read arms the first
/// wait and does not retry; nothing is due before the wait is over; each retry taken doubles
/// the next wait, up to the max. Exact instants, no clock.
#[test]
fn the_store_retry_waits_double_from_the_first_wait_up_to_the_max() {
    let pace = Pace {
        register_retry_first: Duration::from_millis(150),
        register_retry_max: Duration::from_millis(600),
        ..quick()
    };
    let mut rooms = Rooms::default();
    let t0 = Instant::now();
    let ms = |n: u64| t0 + Duration::from_millis(n);
    assert!(
        !rooms.register_retry_due(t0, &pace),
        "the first re-read only arms the wait"
    );
    assert!(!rooms.register_retry_due(ms(149), &pace));
    assert!(rooms.register_retry_due(ms(150), &pace));
    assert!(!rooms.register_retry_due(ms(150 + 299), &pace));
    assert!(rooms.register_retry_due(ms(150 + 300), &pace));
    assert!(!rooms.register_retry_due(ms(450 + 599), &pace));
    assert!(rooms.register_retry_due(ms(450 + 600), &pace));
    // Capped: the next wait is 600 ms again, not 1200.
    assert!(!rooms.register_retry_due(ms(1050 + 599), &pace));
    assert!(rooms.register_retry_due(ms(1050 + 600), &pace));
}

// Carrying rooms across a re-key (freenet/river#889) ---------------------------------------------

mod rekey {
    use super::*;
    use crate::rooms::carry::predecessors;

    fn work_lines(session: &Running) -> Vec<String> {
        session
            .updates()
            .into_iter()
            .filter_map(|u| match u {
                RoomsUpdate::Rooms { work, .. } => Some(work),
                _ => None,
            })
            .flatten()
            .map(|w| w.what)
            .collect()
    }

    /// The regression: a node whose current chat delegate is empty (the delegate was re-keyed
    /// and only the web UI's walk would carry the rooms) and whose earlier delegates hold the
    /// rooms. Before #889 the app showed an empty list.
    #[test]
    fn rooms_kept_only_by_earlier_chat_delegates_are_carried_over_and_shown() {
        let node = fake_node(vec![]);
        let a = TestRoom::new(1, "A", 1, &["a1"]);
        let b = TestRoom::new(2, "B", 1, &["b1"]);
        let left = TestRoom::new(3, "Left", 1, &["l1"]);
        let older = TestRoom::new(4, "Older", 1, &["o1"]);
        let gone_here = TestRoom::new(5, "Left here", 1, &["g1"]);
        let preds = predecessors();
        let (newest, next) = (preds[0].key.clone(), preds[2].key.clone());
        let tombstone = crate::rooms::leave::tombstone_write(None).unwrap().unwrap();
        node.with(|w| {
            for r in [&a, &b, &left, &older, &gone_here] {
                w.contracts
                    .insert(r.id(), (r.key, Contract::State(cbor(&r.state))));
            }
            // The current store holds only the tombstone of a room left in this generation.
            w.slots
                .insert(room_storage_key(&gone_here.vk()), tombstone.clone());
            // The newest earlier delegate: A, B, the room left in it, and the room left here.
            w.legacy.insert(
                newest.clone(),
                BTreeMap::from([
                    (room_storage_key(&a.vk()), a.slot()),
                    (room_storage_key(&b.vk()), b.slot()),
                    (room_storage_key(&left.vk()), tombstone.clone()),
                    (room_storage_key(&gone_here.vk()), gone_here.slot()),
                ]),
            );
            // An older one (a release skipped between them): the left room still present, and
            // a room only it holds.
            w.legacy.insert(
                next.clone(),
                BTreeMap::from([
                    (room_storage_key(&left.vk()), left.slot()),
                    (room_storage_key(&older.vk()), older.slot()),
                ]),
            );
        });
        let dir = temp_dir("carry-delegate");
        save_token(&dir, "kept-token");
        let session = Running::start(&node, &dir, quick());
        session.wait_rooms("the carried rooms are shown", |r, _, s| {
            names(r) == ["A", "B", "Older"] && s == LinkStatus::Live
        });
        // Written to the current store: the rooms, and the tombstone, so the room left stays
        // left; the room left in this generation is not brought back.
        node.with(|w| {
            for r in [&a, &b, &older] {
                let StoredRoomSlot::Present(s) =
                    decode_room_slot(&w.slots[&room_storage_key(&r.vk())]).unwrap()
                else {
                    panic!("not carried");
                };
                assert_eq!(s.self_verifying_key(), Some(r.vk()));
            }
            assert!(matches!(
                decode_room_slot(&w.slots[&room_storage_key(&left.vk())]),
                Ok(StoredRoomSlot::Tombstone)
            ));
            assert_eq!(w.slots[&room_storage_key(&gone_here.vk())], tombstone);
            // Every earlier delegate was asked, newest first, each only on what it holds.
            let asked: Vec<DelegateKey> = w.legacy_asks.iter().map(|(_, k)| k.clone()).collect();
            let mut order: Vec<DelegateKey> = Vec::new();
            for k in &asked {
                if order.last() != Some(k) {
                    order.push(k.clone());
                }
            }
            let expected: Vec<DelegateKey> = preds.iter().map(|p| p.key.clone()).collect();
            assert_eq!(order, expected);
        });
        // While it worked the empty list said why.
        assert!(
            work_lines(&session)
                .iter()
                .any(|l| l.contains("from an earlier version of River")),
            "{:?}",
            work_lines(&session)
        );
        // Once done, nothing about it stays on screen.
        let (_, _, _) = session.last_rooms().unwrap();
        session.wait_until("the row is gone", || {
            session
                .last_work()
                .iter()
                .all(|w| !w.what.contains("earlier version"))
        });
    }

    /// A browser tab that stores a room between the walk's read and its write wins: the write
    /// finds the key taken and stores nothing.
    #[test]
    fn a_room_another_writer_stored_meanwhile_is_not_overwritten() {
        let node = fake_node(vec![]);
        let a = TestRoom::new(1, "A", 1, &["a1"]);
        let newest = predecessors()[0].key.clone();
        // The same room under another identity, as a browser tab stored it.
        let their_slot = cbor(&Slot::Present(Box::new(SlotData {
            owner_vk: a.vk(),
            room_state: a.state.clone(),
            self_sk: Some(SigningKey::from_bytes(&[99; 32])),
            self_vk: None,
            contract_key: a.key,
            last_read_message_id: None,
            self_nickname: None,
            invitation_secrets: BTreeMap::new(),
        })));
        let slot_key = room_storage_key(&a.vk());
        node.with(|w| {
            w.contracts
                .insert(a.id(), (a.key, Contract::State(cbor(&a.state))));
            w.legacy
                .insert(newest, BTreeMap::from([(slot_key.clone(), a.slot())]));
            // Another writer stores the room just before the carry's compare-and-swap.
            let (k, v) = (slot_key.clone(), their_slot.clone());
            w.before_cas = Some(Box::new(move |slots, generations, key| {
                if key == k.as_slice() && !slots.contains_key(&k) {
                    slots.insert(k.clone(), v.clone());
                    generations.insert(k.clone(), 1);
                }
            }));
        });
        let dir = temp_dir("carry-race");
        save_token(&dir, "kept-token");
        let session = Running::start(&node, &dir, quick());
        session.wait_rooms("the room is shown", |r, _, _| names(r) == ["A"]);
        session.wait_until("the carry is over", || {
            session.last_work().iter().all(|w| !w.what.contains("earlier version"))
        });
        assert_eq!(node.with(|w| w.slots[&slot_key].clone()), their_slot);
    }

    /// An earlier store that never answers is passed over (on a new connection, so its answer
    /// can't be taken for another's), the others are still carried, and the screen says what
    /// could not be read, with Try again.
    #[test]
    fn an_earlier_store_that_does_not_answer_is_said_and_try_again_reads_it() {
        let node = fake_node(vec![]);
        let a = TestRoom::new(1, "A", 1, &["a1"]);
        let b = TestRoom::new(2, "B", 1, &["b1"]);
        let preds = predecessors();
        let (silent, other) = (preds[0].key.clone(), preds[1].key.clone());
        node.with(|w| {
            for r in [&a, &b] {
                w.contracts
                    .insert(r.id(), (r.key, Contract::State(cbor(&r.state))));
            }
            w.legacy.insert(
                silent.clone(),
                BTreeMap::from([(room_storage_key(&a.vk()), a.slot())]),
            );
            w.legacy.insert(
                other,
                BTreeMap::from([(room_storage_key(&b.vk()), b.slot())]),
            );
            w.silence_legacy = Some(silent.clone());
        });
        let dir = temp_dir("carry-silent");
        save_token(&dir, "kept-token");
        let pace = Pace {
            delegate_deadline: Duration::from_millis(300),
            ..quick()
        };
        let session = Running::start(&node, &dir, pace);
        session.wait_until("the failure is shown", || {
            session.last_work().iter().any(|w| {
                matches!(&w.state, WorkState::Failed { why, retry: true } if why.contains(&preds[0].version))
            })
        });
        session.wait_rooms("the other store's room is shown", |r, _, _| names(r) == ["B"]);
        node.with(|w| w.silence_legacy = None);
        let id = session
            .last_work()
            .iter()
            .find(|w| w.what.contains("earlier version"))
            .unwrap()
            .id;
        session.list(Action::RetryWork { id });
        session.wait_rooms("both rooms are shown", |r, _, _| names(r) == ["A", "B"]);
    }

    /// A room stored under an earlier room-contract key whose current key has no state: the
    /// app follows the current key and carries the room there from the earlier one (merged
    /// with its own copy), then follows it.
    #[test]
    fn a_room_under_an_earlier_contract_key_is_moved_to_the_current_one() {
        let node = fake_node(vec![]);
        let owner = SigningKey::from_bytes(&[1; 32]);
        let earlier = river_core::migration::legacy_contract_keys_for_owner(&owner.verifying_key())[0];
        let current = crate::rooms::create::contract_key(&owner.verifying_key());
        let mut room = TestRoom::new(1, "A", 1, &["in my copy"]);
        room.key = earlier;
        let mut network = room.state.clone();
        network
            .recent_messages
            .messages
            .push(message(&room.owner, SystemTime::now(), "on the earlier key"));
        node.with(|w| {
            w.slots.insert(room_storage_key(&room.vk()), room.slot());
            w.contracts
                .insert(*earlier.id(), (earlier, Contract::State(cbor(&network))));
            w.contracts
                .insert(*current.id(), (current, Contract::NotFound));
        });
        let dir = temp_dir("rekey-room");
        save_token(&dir, "kept-token");
        let pace = Pace {
            resubscribe_first: Duration::from_millis(50),
            resubscribe_max: Duration::from_millis(100),
            ..quick()
        };
        let session = Running::start(&node, &dir, pace);
        session.wait_until("the room is PUT under the current key", || {
            node.with(|w| w.puts.contains(&current))
        });
        session.wait_rooms("the room is followed under the current key", |r, _, s| {
            r.len() == 1
                && texts(&r[0]).contains(&"on the earlier key".to_owned())
                && texts(&r[0]).contains(&"in my copy".to_owned())
                && s == LinkStatus::Live
        });
        node.with(|w| {
            assert!(w.subscribes_to(*current.id()) >= 2, "asked again once moved");
            // The earlier key is only read, never subscribed to or written.
            assert!(w.reads.iter().any(|(id, _)| id == earlier.id()));
            assert!(w.updates.iter().all(|(k, _)| k != &earlier));
            assert_eq!(w.puts, vec![current]);
            // What went to the current key carries no pointer elsewhere.
            let Contract::State(bytes) = &w.contracts[current.id()].1 else {
                panic!("no state under the current key");
            };
            let put: ChatRoomStateV1 = ciborium::de::from_reader(bytes.as_slice()).unwrap();
            assert!(put.upgrade.0.is_none());
        });
    }
}
