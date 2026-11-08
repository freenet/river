use super::*;
use crate::rooms::create::invitation;
use crate::rooms::room::LiveRoom;
use crate::rooms::room_tests::Fixture;
use ed25519_dalek::SigningKey;
use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::prelude::{InboundDelegateMsg, StateSummary, WrappedState};
use river_core::chat_delegate::{ChatDelegateKey, ChatDelegateRequestMsg};
use river_core::stored_rooms::{StoredRoomsMeta, decode_rooms_meta};

/// What the task asked the chat delegate, if it asked it something.
fn delegate_msg(r: &ClientRequest<'_>) -> Option<ChatDelegateRequestMsg> {
    let ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { inbound, .. }) = r else {
        return None;
    };
    let [InboundDelegateMsg::ApplicationMessage(m)] = inbound.as_slice() else {
        return None;
    };
    ciborium::de::from_reader(m.payload.as_slice()).ok()
}

fn next(w: &mut Work) -> ClientRequest<'static> {
    w.next_request(false, Instant::now())
        .expect("a request to send")
}

/// The task's next request is a versioned read of `key`.
fn expect_read(w: &mut Work, key: &[u8]) {
    match delegate_msg(&next(w)) {
        Some(ChatDelegateRequestMsg::GetVersionedRequest { key: k }) => {
            assert_eq!(k.as_bytes(), key)
        }
        other => panic!("expected a versioned read, got {other:?}"),
    }
}

/// The task's next request is a CAS store of `key` at `generation`; its value.
fn expect_store(w: &mut Work, key: &[u8], generation: u64) -> Vec<u8> {
    match delegate_msg(&next(w)) {
        Some(ChatDelegateRequestMsg::CasStoreRequest {
            key: k,
            value,
            expected_generation,
        }) => {
            assert_eq!(k.as_bytes(), key);
            assert_eq!(expected_generation, generation);
            value
        }
        other => panic!("expected a CAS store, got {other:?}"),
    }
}

fn read_answer(key: &[u8], value: Option<Vec<u8>>, generation: u64) -> ChatDelegateResponseMsg {
    ChatDelegateResponseMsg::GetVersionedResponse {
        key: ChatDelegateKey::new(key.to_vec()),
        value,
        generation,
    }
}

fn store_answer(key: &[u8], result: CasStoreResult) -> ChatDelegateResponseMsg {
    ChatDelegateResponseMsg::CasStoreResponse {
        key: ChatDelegateKey::new(key.to_vec()),
        result,
    }
}

fn state(w: &Work) -> WorkState {
    w.views().last().expect("a task").state.clone()
}

/// A created room's task, up to the slot write it sends first: its owner and slot key.
fn creating(w: &mut Work) -> (VerifyingKey, Vec<u8>) {
    w.create("Book club", "Ada", false);
    assert_eq!(state(w), WorkState::Working);
    let req = next(w);
    let Some(ChatDelegateRequestMsg::GetVersionedRequest { key }) = delegate_msg(&req) else {
        panic!("the slot is read first");
    };
    let owner = river_core::stored_rooms::parse_room_storage_key(key.as_bytes()).unwrap();
    (owner, key.as_bytes().to_vec())
}

fn meta_with(field: &str, value: u32, current: Option<VerifyingKey>) -> Vec<u8> {
    use ciborium::Value;
    let mut entries = vec![(Value::Text(field.into()), Value::from(value))];
    if let Some(vk) = current {
        entries.push((
            Value::Text("current_room_key".into()),
            Value::serialized(&vk).unwrap(),
        ));
    }
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(entries), &mut out).unwrap();
    out
}

#[test]
fn creating_stores_the_room_publishes_it_then_opens_it_in_the_web_ui() {
    let mut w = Work::default();
    let (owner, slot) = creating(&mut w);
    // Nothing more goes out while the read is unanswered.
    assert!(w.next_request(false, Instant::now()).is_none());
    assert!(w.busy() && w.has_out());
    assert!(w.on_delegate(&read_answer(&slot, None, 0)));
    let stored_bytes = expect_store(&mut w, &slot, 0);
    let StoredRoomSlot::Present(room) = decode_room_slot(&stored_bytes).unwrap() else {
        panic!("a present room");
    };
    assert_eq!(room.owner_vk, owner);
    assert!(w.on_delegate(&store_answer(
        &slot,
        CasStoreResult::Stored { generation: 1 }
    )));
    // Then the seed PUT, of this room's contract only.
    let ClientRequest::ContractOp(ContractRequest::Put { contract, .. }) = next(&mut w) else {
        panic!("the room is published next");
    };
    assert_eq!(contract.key(), room.contract_key);
    assert!(w.on_contract(&ContractResponse::PutResponse {
        key: room.contract_key
    }));
    // Then rooms_meta, keeping what the web UI stored there.
    expect_read(&mut w, ROOMS_META_KEY);
    let web_ui_meta = meta_with("something_new", 7, None);
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, Some(web_ui_meta), 4)));
    let meta = expect_store(&mut w, ROOMS_META_KEY, 4);
    let read: StoredRoomsMeta = decode_rooms_meta(&meta).unwrap();
    assert_eq!(read.current_room_key, Some(owner));
    assert!(w.on_delegate(&store_answer(
        ROOMS_META_KEY,
        CasStoreResult::Stored { generation: 5 }
    )));
    let key = bs58::encode(owner.to_bytes()).into_string();
    assert_eq!(state(&w), WorkState::Done { room: key });
    assert!(!w.busy());
    assert!(w.take_reload(), "the stored rooms are read now");
    assert!(!w.take_reload());
    // Dropped once the room is shown.
    w.forget_done(&|vk| *vk == owner);
    assert!(w.views().is_empty());
}

/// Another writer (the web UI) stored first: the write is worked out again from what it
/// stored, never stored over it.
#[test]
fn a_conflict_reads_again_and_never_overwrites() {
    let mut w = Work::default();
    let (owner, slot) = creating(&mut w);
    w.on_delegate(&read_answer(&slot, None, 0));
    expect_store(&mut w, &slot, 0);
    assert!(w.on_delegate(&store_answer(
        &slot,
        CasStoreResult::Conflict {
            current_generation: 1,
            current_value: None,
        }
    )));
    // Read again (the web UI's `cas_write_key` reads again too).
    expect_read(&mut w, &slot);
    // What got in first is someone else's room under this key: refused, nothing stored.
    let other = {
        let mut r =
            crate::rooms::create::new_room(SigningKey::from_bytes(&[9; 32]), "X", "Y", false)
                .unwrap()
                .room;
        r.owner_vk = owner;
        encode_new_room(&r).unwrap()
    };
    assert!(w.on_delegate(&read_answer(&slot, Some(other), 1)));
    assert!(matches!(state(&w), WorkState::Failed { retry: true, .. }));
    assert!(w.next_request(false, Instant::now()).is_none());

    // The meta: a conflict re-reads, and the store keeps what the other writer added.
    let mut w = Work::default();
    let (owner, slot) = creating(&mut w);
    w.on_delegate(&read_answer(&slot, None, 0));
    expect_store(&mut w, &slot, 0);
    w.on_delegate(&store_answer(
        &slot,
        CasStoreResult::Stored { generation: 1 },
    ));
    let key = match next(&mut w) {
        ClientRequest::ContractOp(ContractRequest::Put { contract, .. }) => contract.key(),
        _ => panic!("a PUT"),
    };
    w.on_contract(&ContractResponse::PutResponse { key });
    expect_read(&mut w, ROOMS_META_KEY);
    w.on_delegate(&read_answer(
        ROOMS_META_KEY,
        Some(meta_with("a", 1, None)),
        3,
    ));
    expect_store(&mut w, ROOMS_META_KEY, 3);
    w.on_delegate(&store_answer(
        ROOMS_META_KEY,
        CasStoreResult::Conflict {
            current_generation: 4,
            current_value: None,
        },
    ));
    expect_read(&mut w, ROOMS_META_KEY);
    w.on_delegate(&read_answer(
        ROOMS_META_KEY,
        Some(meta_with("b", 2, None)),
        4,
    ));
    let stored = expect_store(&mut w, ROOMS_META_KEY, 4);
    let value: ciborium::Value = ciborium::de::from_reader(stored.as_slice()).unwrap();
    let entries = value.as_map().unwrap();
    assert!(entries.iter().any(|(k, _)| k.as_text() == Some("b")));
    assert_eq!(
        decode_rooms_meta(&stored).unwrap().current_room_key,
        Some(owner)
    );
}

#[test]
fn a_write_that_keeps_conflicting_fails_after_the_web_uis_number_of_attempts() {
    let mut w = Work::default();
    let (_, slot) = creating(&mut w);
    for attempt in 0..CAS_ATTEMPTS {
        if attempt > 0 {
            expect_read(&mut w, &slot);
        }
        w.on_delegate(&read_answer(&slot, None, u64::from(attempt)));
        expect_store(&mut w, &slot, u64::from(attempt));
        w.on_delegate(&store_answer(
            &slot,
            CasStoreResult::Conflict {
                current_generation: u64::from(attempt) + 1,
                current_value: None,
            },
        ));
    }
    assert!(matches!(state(&w), WorkState::Failed { retry: true, .. }));
    // Try again starts the write over.
    let id = w.views()[0].id;
    assert!(w.retry(id));
    expect_read(&mut w, &slot);
}

/// A store whose answer was lost landed: reading again finds the room already there and goes
/// on, without storing again.
#[test]
fn a_room_already_stored_is_not_stored_again() {
    let mut w = Work::default();
    let (_, slot) = creating(&mut w);
    w.on_delegate(&read_answer(&slot, None, 0));
    let bytes = expect_store(&mut w, &slot, 0);
    // The connection drops with the store unanswered.
    w.connection_lost();
    expect_read(&mut w, &slot);
    w.on_delegate(&read_answer(&slot, Some(bytes), 1));
    assert!(matches!(
        next(&mut w),
        ClientRequest::ContractOp(ContractRequest::Put { .. })
    ));
}

/// Answers carry no request id, so only the kind and key awaited are taken.
#[test]
fn only_the_answer_awaited_is_taken() {
    let mut w = Work::default();
    let (_, slot) = creating(&mut w);
    // A plain read's answer for the same key (the stored-rooms reader's) is not the task's.
    assert!(!w.on_delegate(&ChatDelegateResponseMsg::GetResponse {
        key: ChatDelegateKey::new(slot.clone()),
        value: None,
    }));
    // Nor a versioned answer for another key, nor a store answer while a read is out.
    assert!(!w.on_delegate(&read_answer(ROOMS_META_KEY, None, 0)));
    assert!(!w.on_delegate(&store_answer(
        &slot,
        CasStoreResult::Stored { generation: 1 }
    )));
    // Nor a contract answer.
    let other =
        crate::rooms::create::contract_key(&SigningKey::from_bytes(&[3; 32]).verifying_key());
    assert!(!w.on_contract(&ContractResponse::PutResponse { key: other }));
    assert_eq!(state(&w), WorkState::Working);
    // The awaited one is.
    assert!(w.on_delegate(&read_answer(&slot, None, 0)));
    // With the store out, a store answer for another key is not taken.
    expect_store(&mut w, &slot, 0);
    assert!(!w.on_delegate(&store_answer(
        ROOMS_META_KEY,
        CasStoreResult::Stored { generation: 1 }
    )));
    assert!(w.has_out());
    // On a new connection the read is sent again.
    w.connection_lost();
    expect_read(&mut w, &slot);
    assert!(w.on_delegate(&read_answer(&slot, None, 0)));
    // During the PUT: an UPDATE answer or a PUT answer for another contract is not taken.
    expect_store(&mut w, &slot, 0);
    w.on_delegate(&store_answer(
        &slot,
        CasStoreResult::Stored { generation: 1 },
    ));
    let mine = match next(&mut w) {
        ClientRequest::ContractOp(ContractRequest::Put { contract, .. }) => contract.key(),
        _ => panic!("a PUT"),
    };
    assert!(!w.on_contract(&ContractResponse::PutResponse { key: other }));
    assert!(!w.on_contract(&ContractResponse::UpdateResponse {
        key: mine,
        summary: StateSummary::from(vec![]),
    }));
    assert!(w.on_contract(&ContractResponse::PutResponse { key: mine }));
}

#[test]
fn errors_are_taken_only_when_they_can_be_the_tasks() {
    let mut w = Work::default();
    let (_, _slot) = creating(&mut w);
    let unnamed: ClientError = ErrorKind::EmptyRing.into();
    // Something else could have caused it: not taken.
    assert!(!w.on_error(&unnamed, false, &|_| false));
    assert_eq!(state(&w), WorkState::Working);
    // A contract error is not a delegate request's.
    let other: ClientError = ErrorKind::RequestError(RequestError::ContractError(
        ContractError::MissingContract {
            key: ContractInstanceId::new([1; 32]),
        },
    ))
    .into();
    assert!(!w.on_error(&other, true, &|_| false));
    // A delegate error is, and it is said.
    let refused: ClientError = ErrorKind::RequestError(RequestError::DelegateError(
        DelegateError::Missing(crate::rooms::delegate::chat_delegate_key().clone()),
    ))
    .into();
    assert!(w.on_error(&refused, false, &|_| false));
    let WorkState::Failed { why, retry } = state(&w) else {
        panic!("failed");
    };
    assert!(retry && why.contains("storage"), "{why}");

    // A join's GET: `MissingContract` while a subscribe of the room is out is not taken.
    let mut w = Work::default();
    let f = Fixture::new();
    let room = LiveRoom::new(f.stored(f.private_state()));
    let (inv, link) = invitation(&room, "http://x/").unwrap();
    w.join(&link, "Bea", &|_| false);
    let ClientRequest::ContractOp(ContractRequest::Get {
        key: id, subscribe, ..
    }) = next(&mut w)
    else {
        panic!("the room is read first");
    };
    assert!(!subscribe);
    assert_eq!(id, *crate::rooms::create::contract_key(&inv.room).id());
    let missing: ClientError = ErrorKind::RequestError(RequestError::ContractError(
        ContractError::MissingContract { key: id },
    ))
    .into();
    assert!(!w.on_error(&missing, true, &|k| *k == id));
    assert!(w.on_error(&missing, true, &|_| false));
}

#[test]
fn an_unanswered_request_drops_the_connection_and_fails_after_a_few() {
    let mut w = Work::default();
    let (_, slot) = creating(&mut w);
    let d = Duration::from_secs(5);
    let start = Instant::now();
    assert!(!w.expire(d, d, start));
    for n in 1..=MAX_RECONNECTS {
        assert!(w.expire(d, d, start + d * 2 * n));
        assert_eq!(state(&w), WorkState::Working);
        // The next connection asks again from the start of the step.
        expect_read(&mut w, &slot);
    }
    assert!(w.expire(d, d, start + d * 100));
    assert!(matches!(state(&w), WorkState::Failed { retry: true, .. }));
    assert!(w.next_request(false, Instant::now()).is_none());
}

#[test]
fn nothing_is_sent_while_the_stored_rooms_are_being_read() {
    let mut w = Work::default();
    w.create("Book club", "Ada", false);
    assert!(w.next_request(true, Instant::now()).is_none());
    assert!(w.next_request(false, Instant::now()).is_some());
}

#[test]
fn joining_reads_the_room_stores_it_over_a_tombstone_then_sends_the_join() {
    let f = Fixture::new();
    let mut state0 = f.private_state();
    // An owner-side copy, to invite from.
    let mut owner_side = f.stored(state0.clone());
    owner_side.self_sk = Some(f.owner.clone());
    crate::rooms::room_tests::add_secret(
        &f,
        &mut state0,
        0,
        &crate::rooms::room_tests::SECRET,
        &f.owner,
    );
    owner_side.room_state = state0.clone();
    let (inv, link) = invitation(&LiveRoom::new(owner_side), "http://x/").unwrap();
    let mut w = Work::default();
    w.join(&link, "Bea", &|_| false);
    let owner = f.owner.verifying_key();
    assert!(w.holds(&owner));
    let ClientRequest::ContractOp(ContractRequest::Get { key: id, .. }) = next(&mut w) else {
        panic!("a GET");
    };
    let key = crate::rooms::create::contract_key(&owner);
    let mut state_bytes = Vec::new();
    ciborium::ser::into_writer(&state0, &mut state_bytes).unwrap();
    assert!(w.on_contract(&ContractResponse::GetResponse {
        key,
        contract: None,
        state: WrappedState::new(state_bytes),
    }));
    assert_eq!(id, *key.id());
    // The room is stored, over the tombstone a left room leaves.
    let slot = room_storage_key(&owner);
    expect_read(&mut w, &slot);
    let mut tomb = Vec::new();
    ciborium::ser::into_writer(&"Tombstone", &mut tomb).unwrap();
    w.on_delegate(&read_answer(&slot, Some(tomb), 6));
    let bytes = expect_store(&mut w, &slot, 6);
    let StoredRoomSlot::Present(stored_room) = decode_room_slot(&bytes).unwrap() else {
        panic!("present");
    };
    assert_eq!(
        stored_room.self_verifying_key(),
        Some(inv.invitee_signing_key.verifying_key())
    );
    w.on_delegate(&store_answer(
        &slot,
        CasStoreResult::Stored { generation: 7 },
    ));
    // Then the join, as an UPDATE (never a PUT).
    let ClientRequest::ContractOp(ContractRequest::Update { key: k, data }) = next(&mut w) else {
        panic!("the join goes as an UPDATE");
    };
    assert_eq!(k, key);
    let UpdateData::Delta(delta) = data else {
        panic!("a delta");
    };
    let delta: river_core::room_state::ChatRoomStateV1Delta =
        ciborium::de::from_reader(delta.as_ref()).unwrap();
    assert!(delta.members.is_some() && delta.recent_messages.is_some());
    assert!(w.on_contract(&ContractResponse::UpdateResponse {
        key,
        summary: StateSummary::from(vec![]),
    }));
    expect_read(&mut w, ROOMS_META_KEY);
    w.on_delegate(&read_answer(ROOMS_META_KEY, None, 0));
    expect_store(&mut w, ROOMS_META_KEY, 0);
    w.on_delegate(&store_answer(
        ROOMS_META_KEY,
        CasStoreResult::Stored { generation: 1 },
    ));
    assert!(matches!(state(&w), WorkState::Done { .. }));
    assert!(!w.holds(&owner));
    assert!(w.views()[0].what.contains("Secret garden"));
}

#[test]
fn a_join_that_cant_start_says_why_at_once() {
    let f = Fixture::new();
    let room = LiveRoom::new(f.stored(f.private_state()));
    let (_, link) = invitation(&room, "http://x/").unwrap();
    for (pasted, nick, shown, says) in [
        ("", "Bea", false, "Paste"),
        ("not a code!", "Bea", false, "valid invite code"),
        (link.as_str(), "", false, "nickname"),
        (link.as_str(), "Bea", true, "already have"),
    ] {
        let mut w = Work::default();
        w.join(pasted, nick, &|_| shown);
        let WorkState::Failed { why, retry } = state(&w) else {
            panic!("refused: {pasted:?}");
        };
        assert!(why.contains(says), "{why}");
        assert!(!retry);
        assert!(!w.busy() && w.next_request(false, Instant::now()).is_none());
        // Dismissed, it is gone.
        let id = w.views()[0].id;
        assert!(w.dismiss(id));
        assert!(w.views().is_empty());
    }
    // A second join of a room being joined.
    let mut w = Work::default();
    w.join(&link, "Bea", &|_| false);
    w.join(&link, "Bea", &|_| false);
    let views = w.views();
    assert_eq!(views.len(), 2);
    assert!(
        matches!(&views[1].state, WorkState::Failed { why, .. } if why.contains("already joining"))
    );
    // A working task can't be dismissed.
    assert!(!w.dismiss(views[0].id));
}

#[test]
fn a_new_room_reads_back_as_itself() {
    let plan =
        crate::rooms::create::new_room(SigningKey::from_bytes(&[5; 32]), "R", "N", true).unwrap();
    let mut room = plan.room;
    room.self_nickname = Some("N".into());
    room.invitation_secrets.insert(3, [1; 32]);
    let read = stored(&room);
    assert_eq!(read.owner_vk, room.owner_vk);
    assert_eq!(read.room_state, room.room_state);
    assert_eq!(read.self_vk, room.self_vk);
    assert_eq!(read.self_nickname, room.self_nickname);
    assert_eq!(read.invitation_secrets, room.invitation_secrets);
    assert_eq!(read.contract_key, room.contract_key);
}
