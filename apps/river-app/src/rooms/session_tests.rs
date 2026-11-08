use super::*;
use ed25519_dalek::SigningKey;
use freenet_stdlib::client_api::HostResponse;
use freenet_stdlib::prelude::{CodeHash, ContractCode, ContractKey, Parameters};
use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tungstenite::Message;
use tungstenite::protocol::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;

fn vk(seed: u8) -> VerifyingKey {
    SigningKey::from_bytes(&[seed; 32]).verifying_key()
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
        key: river_core::chat_delegate::ChatDelegateKey::new(key),
        value,
    }
}

#[test]
fn the_loader_takes_answers_in_any_order_and_ignores_strays() {
    let (mut loader, first) = Loader::start();
    assert!(matches!(first.as_slice(), [Read::List]));
    // A Get answer before the list is not ours.
    assert!(matches!(
        loader.on_answer(get_answer(b"room:x".to_vec(), None)),
        Step::Wait
    ));
    let keys = vec![
        river_core::chat_delegate::ChatDelegateKey::new(room_storage_key(&vk(1))),
        river_core::chat_delegate::ChatDelegateKey::new(ROOMS_META_KEY.to_vec()),
    ];
    let Step::Send(reads) = loader.on_answer(ChatDelegateResponseMsg::ListResponse { keys }) else {
        panic!("gets follow the list");
    };
    assert_eq!(reads.len(), 2);
    // The meta answer first, then a stray, then the room (missing: the user left it on a
    // device that deleted the key).
    assert!(matches!(
        loader.on_answer(get_answer(ROOMS_META_KEY.to_vec(), None)),
        Step::Wait
    ));
    assert!(matches!(
        loader.on_answer(get_answer(b"other".to_vec(), Some(vec![1]))),
        Step::Wait
    ));
    let Step::Done(loaded) = loader.on_answer(get_answer(room_storage_key(&vk(1)), None)) else {
        panic!("all answered");
    };
    assert!(loaded.slots.is_empty());
    assert!(!loaded.store_missing);

    // An empty store is done at once.
    let (mut loader, _) = Loader::start();
    assert!(matches!(
        loader.on_answer(ChatDelegateResponseMsg::ListResponse { keys: vec![] }),
        Step::Done(_)
    ));
}

fn stored(owner: u8, code: u8) -> StoredRoom {
    let owner_vk = vk(owner);
    let mut state = ChatRoomStateV1::default();
    state.configuration.configuration.owner_member_id =
        river_core::room_state::member::MemberId::from(&owner_vk);
    StoredRoom {
        owner_vk,
        room_state: state,
        self_sk: Some(SigningKey::from_bytes(&[owner; 32])),
        self_vk: None,
        contract_key: ContractKey::from_params_and_code(
            Parameters::from(vec![owner]),
            ContractCode::from(vec![code]),
        ),
        last_read_message_id: None,
        self_nickname: None,
        invitation_secrets: Default::default(),
    }
}

fn loaded(slots: Vec<StoredRoom>) -> Loaded {
    Loaded {
        slots,
        meta: StoredRoomsMeta::default(),
        store_missing: false,
    }
}

#[test]
fn reading_again_adds_joined_rooms_drops_left_ones_and_follows_a_rekey() {
    let mut rooms = Rooms::default();
    let a = stored(1, 1);
    let b = stored(2, 1);
    let first = rooms.replace(loaded(vec![a.clone(), b.clone()]));
    assert_eq!(first.len(), 2, "both rooms need a subscription");

    // The same rooms again: nothing new to subscribe.
    assert!(rooms.replace(loaded(vec![a.clone(), b.clone()])).is_empty());

    // Room b was left, room c joined, and room a moved to a new contract.
    let a2 = stored(1, 2);
    let c = stored(3, 1);
    let new = rooms.replace(loaded(vec![a2.clone(), c.clone()]));
    let mut expected = vec![*a2.contract_key.id(), *c.contract_key.id()];
    expected.sort_by_key(|id| id.as_bytes().to_vec());
    let mut new = new;
    new.sort_by_key(|id| id.as_bytes().to_vec());
    assert_eq!(new, expected);
    let mut shown = rooms.contract_ids();
    shown.sort_by_key(|id| id.as_bytes().to_vec());
    assert_eq!(shown, expected);

    // A changed identity (rejoined with a new key) takes the stored slot.
    let mut a3 = a2.clone();
    a3.self_sk = Some(SigningKey::from_bytes(&[77; 32]));
    assert!(rooms.replace(loaded(vec![a3, c])).is_empty());
    assert_eq!(
        rooms.list[0].stored.self_sk.as_ref().unwrap().to_bytes(),
        [77; 32]
    );
}

#[test]
fn a_room_the_network_update_failed_for_is_named_until_one_works() {
    let mut rooms = Rooms::default();
    let a = stored(1, 1);
    let id = *a.contract_key.id();
    rooms.replace(loaded(vec![a.clone()]));
    assert!(rooms.issues().not_updated.is_empty());
    assert!(rooms.merge_bytes(&id, b"not cbor"));
    assert_eq!(rooms.issues().not_updated, ["Default Room Name"]);
    // A second failure changes nothing on screen.
    assert!(!rooms.merge_bytes(&id, b"still not cbor"));
    let mut good = Vec::new();
    ciborium::ser::into_writer(&a.room_state, &mut good).unwrap();
    assert!(rooms.merge_bytes(&id, &good));
    assert!(rooms.issues().not_updated.is_empty());
    // A room that is not shown is ignored.
    assert!(!rooms.merge_bytes(&ContractInstanceId::new([9; 32]), &good));
}

// Against a fake node -------------------------------------------------------------------------

/// What the fake node does with one WebSocket connection.
#[derive(Clone, Debug)]
enum Play {
    /// The node's real sequence for a rejected token: the error, then the close frame.
    Reject(&'static str, u16),
    /// Refuse the upgrade with this HTTP status.
    Handshake(u16),
    /// Accept, and answer every delegate read with "missing delegate".
    NoDelegate,
}

struct FakeNode {
    authority: String,
    /// "METHOD path" of each plain HTTP request.
    http: Arc<Mutex<Vec<String>>>,
    /// Bearer tokens presented on WebSocket upgrades.
    bearers: Arc<Mutex<Vec<String>>>,
}

fn head_of(stream: &TcpStream) -> String {
    let mut buf = [0u8; 8192];
    loop {
        let n = stream.peek(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        if text.contains("\r\n\r\n") || n == buf.len() {
            return text;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn fake_node(plays: Vec<Play>) -> FakeNode {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    let http = Arc::new(Mutex::new(Vec::new()));
    let bearers = Arc::new(Mutex::new(Vec::new()));
    let node = FakeNode {
        authority: authority.clone(),
        http: http.clone(),
        bearers: bearers.clone(),
    };
    let plays = Arc::new(Mutex::new(VecDeque::from(plays)));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            let head = head_of(&stream);
            let lower = head.to_ascii_lowercase();
            if lower.contains("upgrade: websocket") {
                if let Some(bearer) = lower
                    .lines()
                    .find_map(|l| l.strip_prefix("authorization: bearer "))
                {
                    // Lowercased; the tests use lowercase tokens.
                    bearers.lock().unwrap().push(bearer.trim().to_owned());
                }
                let play = plays.lock().unwrap().pop_front();
                std::thread::spawn(move || serve_ws(stream, play));
            } else {
                let first = head.lines().next().unwrap_or("").to_owned();
                let mut parts = first.split(' ');
                let call = format!(
                    "{} {}",
                    parts.next().unwrap_or(""),
                    parts.next().unwrap_or("")
                );
                http.lock().unwrap().push(call.clone());
                let (status, body) = if call.starts_with("POST /v1/app-grants/request") {
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
                // Consume the request, then answer.
                let mut sink = vec![0u8; head.len()];
                let _ = stream.read(&mut sink);
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

fn serve_ws(mut stream: TcpStream, play: Option<Play>) {
    let play = play.expect("the app opened more connections than the test expected");
    if let Play::Handshake(status) = play {
        let mut buf = [0u8; 8192];
        let _ = stream.read(&mut buf);
        let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Length: 0\r\n\r\n");
        return;
    }
    let mut ws = tungstenite::accept(stream).unwrap();
    match play {
        Play::Reject(prefix, code) => {
            let error: Result<HostResponse, ClientError> = Err(ErrorKind::Unhandled {
                cause: format!("{prefix}: explanation from the node").into(),
            }
            .into());
            let _ = ws.send(Message::Binary(bincode::serialize(&error).unwrap().into()));
            let _ = ws.send(Message::Close(Some(CloseFrame {
                code: CloseCode::from(code),
                reason: prefix.into(),
            })));
            while ws.read().is_ok() {}
        }
        Play::NoDelegate => {
            while let Ok(m) = ws.read() {
                if let Message::Binary(_) = m {
                    let error: Result<HostResponse, ClientError> = Err(missing_delegate());
                    let _ = ws.send(Message::Binary(bincode::serialize(&error).unwrap().into()));
                }
            }
        }
        Play::Handshake(_) => unreachable!(),
    }
}

fn quick() -> Pace {
    Pace {
        backoff_first: Duration::from_millis(10),
        backoff_max: Duration::from_millis(20),
        stable: Duration::from_secs(60),
        tick: Duration::from_millis(20),
        idle_check: Duration::from_secs(60),
        pong_wait: Duration::from_secs(60),
        catch_up: Duration::from_millis(100),
        refresh: Duration::from_secs(3600),
        delegate_deadline: Duration::from_secs(5),
        grant: GrantPacing {
            pause: Duration::ZERO,
            margin: Duration::ZERO,
        },
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("river-session-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run the session against `node` until it stops by itself or `done` holds, then stop it.
fn drive(
    node: &FakeNode,
    dir: &Path,
    done: impl Fn(&[RoomsUpdate]) -> bool,
) -> (Option<Access>, Vec<RoomsUpdate>) {
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
                &quick(),
                &|| stop.load(Ordering::Relaxed),
            )
        })
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !handle.is_finished() && !done(&updates.lock().unwrap()) {
        assert!(Instant::now() < deadline, "the session never settled");
        std::thread::sleep(Duration::from_millis(5));
    }
    let stopped_by_itself = handle.is_finished();
    stop.store(true, Ordering::Relaxed);
    let end = handle.join().unwrap();
    let updates = updates.lock().unwrap().clone();
    (stopped_by_itself.then_some(end), updates)
}

fn save_token(dir: &Path, token: &str) {
    TokenStore::in_dir(dir)
        .save(&GrantToken::new(token.into()))
        .unwrap();
}

fn saved(dir: &Path) -> Option<GrantToken> {
    TokenStore::in_dir(dir).load()
}

#[test]
fn a_saved_token_the_node_rejects_is_discarded_and_nothing_is_reopened() {
    let node = fake_node(vec![Play::Reject("AUTH_TOKEN_INVALID", 4401)]);
    let dir = temp_dir("4401-saved");
    save_token(&dir, "old-token");
    let (end, updates) = drive(&node, &dir, |_| false);
    assert_eq!(end, Some(Access::PermissionRemoved));
    assert_eq!(saved(&dir), None);
    // No new grant was asked for, so no approval page opened.
    assert!(node.http.lock().unwrap().is_empty());
    assert!(
        !updates
            .iter()
            .any(|u| matches!(u, RoomsUpdate::Access(Access::WaitingForApproval { .. })))
    );
    assert_eq!(*node.bearers.lock().unwrap(), ["old-token"]);
}

#[test]
fn a_token_the_node_rejects_as_soon_as_it_issued_it_stops() {
    let node = fake_node(vec![Play::Reject("AUTH_TOKEN_INVALID", 4401)]);
    let dir = temp_dir("4401-fresh");
    let (end, updates) = drive(&node, &dir, |_| false);
    assert!(
        matches!(&end, Some(Access::Refused(m)) if m.contains("just gave")),
        "{end:?}"
    );
    assert_eq!(saved(&dir), None);
    assert_eq!(
        *node.http.lock().unwrap(),
        [
            "POST /v1/app-grants/request",
            "GET /v1/app-grants/request/req1"
        ]
    );
    assert!(updates.iter().any(|u| matches!(
        u,
        RoomsUpdate::Access(Access::WaitingForApproval { approve_url })
            if approve_url.starts_with(&format!("http://{}/", node.authority))
    )));
}

#[test]
fn a_connection_that_cannot_use_a_grant_keeps_the_token_and_says_why() {
    let node = fake_node(vec![Play::Reject("AUTH_TOKEN_NOT_USABLE_HERE", 4403)]);
    let dir = temp_dir("4403");
    save_token(&dir, "kept-token");
    let (end, _) = drive(&node, &dir, |_| false);
    assert!(
        matches!(&end, Some(Access::Refused(m)) if m.contains("directly")),
        "{end:?}"
    );
    assert_eq!(saved(&dir), Some(GrantToken::new("kept-token".into())));
}

#[test]
fn a_node_that_cannot_check_the_token_yet_is_retried_with_the_same_token() {
    let node = fake_node(vec![
        Play::Reject("AUTH_TOKEN_UNAVAILABLE", 4503),
        Play::NoDelegate,
    ]);
    let dir = temp_dir("4503");
    save_token(&dir, "kept-token");
    let (end, updates) = drive(&node, &dir, |u| {
        u.iter().any(|u| matches!(u, RoomsUpdate::Rooms { .. }))
    });
    assert_eq!(end, None, "still following");
    assert_eq!(saved(&dir), Some(GrantToken::new("kept-token".into())));
    assert_eq!(*node.bearers.lock().unwrap(), ["kept-token", "kept-token"]);
    // The node has no chat delegate: no rooms, and the screen is told why.
    let Some(RoomsUpdate::Rooms { rooms, issues, .. }) = updates
        .iter()
        .find(|u| matches!(u, RoomsUpdate::Rooms { .. }))
    else {
        unreachable!()
    };
    assert!(rooms.is_empty());
    assert!(issues.store_missing);
    assert!(
        updates
            .iter()
            .any(|u| matches!(u, RoomsUpdate::Access(Access::Ready)))
    );
}

#[test]
fn a_refused_upgrade_stops_instead_of_retrying_forever() {
    let node = fake_node(vec![Play::Handshake(403)]);
    let dir = temp_dir("handshake");
    save_token(&dir, "kept-token");
    let (end, _) = drive(&node, &dir, |_| false);
    assert!(
        matches!(&end, Some(Access::Refused(m)) if m.contains("403")),
        "{end:?}"
    );
    assert_eq!(saved(&dir), Some(GrantToken::new("kept-token".into())));
}
