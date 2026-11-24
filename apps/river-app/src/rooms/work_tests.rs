use super::*;
use crate::rooms::create::invitation;
use crate::rooms::room::LiveRoom;
use crate::rooms::room_tests::Fixture;
use crate::rooms::work::wiped;
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
    assert!(matches!(state(&w), WorkState::Failed { retry: false, .. }));
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
    wiped::take();
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
    // Done: the invitation and the plan (the invitee's key and the room's secrets) were wiped
    // when the join finished, and the line still names the room.
    assert!(w.views()[0].what.contains("Secret garden"));
    assert!(w.join_credentials_wiped());
    let (invitation_secrets, plan_secrets) = wiped::take();
    assert!(
        invitation_secrets > 0 && plan_secrets > 0,
        "{invitation_secrets} {plan_secrets}"
    );
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
        (link.as_str(), "Bea", true, "already a member"),
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

/// A join of a public room, run up to the slot read it sends after reading the room: the work,
/// the room's owner and slot key, and the invitee's identity.
fn joining_to_slot(w: &mut Work) -> (VerifyingKey, Vec<u8>, SigningKey) {
    joining_to_slot_with(w, false)
}

/// The same, with the room as read already holding the invitee as a member when
/// `already_member` (another tab accepted the same invitation and joined).
fn joining_to_slot_with(w: &mut Work, already_member: bool) -> (VerifyingKey, Vec<u8>, SigningKey) {
    let owner_sk = SigningKey::from_bytes(&[60; 32]);
    let plan = crate::rooms::create::new_room(owner_sk.clone(), "Open", "Olive", false).unwrap();
    let room = LiveRoom::new(stored(&plan.room));
    let (inv, link) = invitation(&room, "http://x/").unwrap();
    w.join(&link, "Bea", &|_| false);
    let ClientRequest::ContractOp(ContractRequest::Get { .. }) = next(w) else {
        panic!("a GET");
    };
    let owner = owner_sk.verifying_key();
    let key = crate::rooms::create::contract_key(&owner);
    let mut state = plan.room.room_state.clone();
    if already_member {
        state.members.members.push(inv.invitee.clone());
    }
    get_answer(w, key, &state);
    let slot = room_storage_key(&owner);
    expect_read(w, &slot);
    (owner, slot, inv.invitee_signing_key)
}

fn get_answer(
    w: &mut Work,
    key: freenet_stdlib::prelude::ContractKey,
    state: &river_core::room_state::ChatRoomStateV1,
) {
    let mut state_bytes = Vec::new();
    ciborium::ser::into_writer(state, &mut state_bytes).unwrap();
    assert!(w.on_contract(&ContractResponse::GetResponse {
        key,
        contract: None,
        state: WrappedState::new(state_bytes),
    }));
}

/// The join event's time in an UPDATE.
fn join_time(r: &ClientRequest<'_>) -> SystemTime {
    let ClientRequest::ContractOp(ContractRequest::Update { data, .. }) = r else {
        panic!("an UPDATE, got {r:?}");
    };
    let UpdateData::Delta(d) = data else {
        panic!("a delta");
    };
    let d: river_core::room_state::ChatRoomStateV1Delta =
        ciborium::de::from_reader(d.as_ref()).unwrap();
    d.recent_messages.unwrap()[0].message.time
}

/// A browser tab accepted the same invitation and joined first: the room as read already has
/// the invitee, so this join sends nothing and only opens the room.
#[test]
fn a_join_the_room_already_has_sends_nothing() {
    let mut w = Work::default();
    let (owner, slot, me) = joining_to_slot_with(&mut w, true);
    // The tab's copy: the same identity, other bytes.
    let mut theirs =
        crate::rooms::create::new_room(SigningKey::from_bytes(&[61; 32]), "T", "T", false)
            .unwrap()
            .room;
    theirs.owner_vk = owner;
    theirs.self_vk = Some(me.verifying_key());
    theirs.self_sk = Some(me);
    w.on_delegate(&read_answer(
        &slot,
        Some(encode_new_room(&theirs).unwrap()),
        3,
    ));
    expect_read(&mut w, ROOMS_META_KEY);
}

/// A join started from a direct message must add a new member: an invitation for a key the room
/// holds already would otherwise make the user store that member's identity.
#[test]
fn a_join_from_a_message_for_a_member_of_the_room_is_refused_and_one_for_a_new_member_goes_on() {
    for (already, expect_failed) in [(true, true), (false, false)] {
        let mut w = Work::default();
        let owner_sk = SigningKey::from_bytes(&[60; 32]);
        let plan =
            crate::rooms::create::new_room(owner_sk.clone(), "Open", "Olive", false).unwrap();
        let room = LiveRoom::new(stored(&plan.room));
        let (inv, _) = invitation(&room, "http://x/").unwrap();
        let member = inv.invitee.clone();
        w.join_invitation(inv, "Bea", true, &|_| false).unwrap();
        let ClientRequest::ContractOp(ContractRequest::Get { .. }) = next(&mut w) else {
            panic!("a GET");
        };
        let mut state = plan.room.room_state.clone();
        if already {
            state.members.members.push(member);
        }
        get_answer(
            &mut w,
            crate::rooms::create::contract_key(&owner_sk.verifying_key()),
            &state,
        );
        let failed = w
            .views()
            .iter()
            .any(|v| matches!(v.state, WorkState::Failed { retry: false, .. }));
        assert_eq!(failed, expect_failed);
    }
}

/// This join's own store landed but its answer was lost: the room holds exactly its bytes, so
/// the join still goes out.
#[test]
fn a_join_whose_own_store_landed_still_sends_the_join() {
    let mut w = Work::default();
    let (_, slot, _) = joining_to_slot(&mut w);
    w.on_delegate(&read_answer(&slot, None, 0));
    let bytes = expect_store(&mut w, &slot, 0);
    w.connection_lost();
    expect_read(&mut w, &slot);
    w.on_delegate(&read_answer(&slot, Some(bytes), 1));
    assert!(matches!(
        next(&mut w),
        ClientRequest::ContractOp(ContractRequest::Update { .. })
    ));
}

/// The node refused the join's UPDATE (the room may have filled up since): Try again reads the
/// room again and builds the join against it now, rather than sending what was refused.
#[test]
fn trying_a_join_again_after_its_update_was_refused_reads_the_room_again() {
    let mut w = Work::default();
    let (_, slot, _) = joining_to_slot(&mut w);
    w.on_delegate(&read_answer(&slot, None, 0));
    expect_store(&mut w, &slot, 0);
    w.on_delegate(&store_answer(
        &slot,
        CasStoreResult::Stored { generation: 1 },
    ));
    let ClientRequest::ContractOp(ContractRequest::Update { key, .. }) = next(&mut w) else {
        panic!("an UPDATE");
    };
    let refused: ClientError =
        ErrorKind::RequestError(RequestError::ContractError(ContractError::Update {
            key,
            cause: "busy".into(),
        }))
        .into();
    assert!(w.on_error(&refused, false, &|_| false));
    let id = w.views()[0].id;
    assert!(w.retry(id));
    assert!(matches!(
        next(&mut w),
        ClientRequest::ContractOp(ContractRequest::Get { .. })
    ));
}

/// The join's UPDATE went unanswered until the task gave up, so whether the node took it is
/// unknown: Try again sends the same join (same message id), not a second join event.
#[test]
fn trying_a_join_again_after_no_answer_sends_the_same_join() {
    let mut w = Work::default();
    let (_, slot, _) = joining_to_slot(&mut w);
    w.on_delegate(&read_answer(&slot, None, 0));
    expect_store(&mut w, &slot, 0);
    w.on_delegate(&store_answer(
        &slot,
        CasStoreResult::Stored { generation: 1 },
    ));
    let first = join_time(&next(&mut w));
    let d = Duration::from_secs(1);
    let start = Instant::now();
    for n in 1..=MAX_RECONNECTS + 1 {
        assert!(w.expire(d, d, start + d * 3 * n));
        if n <= MAX_RECONNECTS {
            assert_eq!(join_time(&next(&mut w)), first, "resent unchanged");
        }
    }
    assert!(matches!(state(&w), WorkState::Failed { retry: true, .. }));
    assert!(w.retry(w.views()[0].id));
    assert_eq!(join_time(&next(&mut w)), first);
}

/// The slot store landed but its answer was lost, and the join failed there; the user tries
/// again. Who stored the slot does not matter: the room, read again, lacks the invitee, so the
/// join goes out, built now, with a fresh time.
#[test]
fn a_join_retried_after_its_store_landed_still_sends_the_join() {
    let mut w = Work::default();
    let (owner, slot, _) = joining_to_slot(&mut w);
    w.on_delegate(&read_answer(&slot, None, 0));
    let landed = expect_store(&mut w, &slot, 0);
    let StoredRoomSlot::Present(earlier) = decode_room_slot(&landed).unwrap() else {
        panic!("present");
    };
    let earlier_time = earlier
        .room_state
        .recent_messages
        .messages
        .iter()
        .map(|m| m.message.time)
        .max()
        .unwrap();
    // No answer: the store fails (as after the reconnects run out); then Try again.
    let refused: ClientError = ErrorKind::RequestError(RequestError::DelegateError(
        DelegateError::Missing(crate::rooms::delegate::chat_delegate_key().clone()),
    ))
    .into();
    assert!(w.on_error(&refused, false, &|_| false));
    std::thread::sleep(Duration::from_millis(5));
    assert!(w.retry(w.views()[0].id));
    // The room is read again, as it was before (without the invitee).
    let ClientRequest::ContractOp(ContractRequest::Get { .. }) = next(&mut w) else {
        panic!("the room is read again");
    };
    let key = crate::rooms::create::contract_key(&owner);
    let mut state = earlier.room_state.clone();
    state.members.members.clear();
    state.recent_messages.messages.retain(|m| {
        m.message.content != river_core::room_state::message::RoomMessageBody::join_event()
    });
    state.member_info.member_info.retain(|i| {
        i.member_info.member_id
            == earlier
                .room_state
                .configuration
                .configuration
                .owner_member_id
    });
    get_answer(&mut w, key, &state);
    // The slot holds the earlier store's bytes: already stored, and the join still goes out.
    expect_read(&mut w, &slot);
    w.on_delegate(&read_answer(&slot, Some(landed), 1));
    let update = next(&mut w);
    assert!(join_time(&update) > earlier_time, "a fresh join time");
}

/// A join that failed with Try again blocks a second paste of the same invitation.
#[test]
fn a_failed_join_waiting_for_try_again_blocks_a_second_one() {
    let mut w = Work::default();
    let (_, slot, _) = joining_to_slot(&mut w);
    w.on_delegate(&read_answer(&slot, None, 0));
    expect_store(&mut w, &slot, 0);
    w.on_delegate(&store_answer(&slot, CasStoreResult::Failed("busy".into())));
    let link = w_link(&w);
    w.join(&link, "Bea", &|_| false);
    let views = w.views();
    assert!(
        matches!(&views[1].state, WorkState::Failed { why, retry: false } if why.contains("Try again"))
    );
}

#[test]
fn a_room_just_joined_is_not_joined_again() {
    let mut w = Work::default();
    let f = Fixture::new();
    let room = LiveRoom::new(f.stored(f.private_state()));
    let (_, link) = invitation(&room, "http://x/").unwrap();
    w.join(&link, "Bea", &|_| false);
    // Pretend it finished.
    w.tasks[0].step = Step::Done;
    w.join(&link, "Bea", &|_| false);
    let views = w.views();
    assert!(
        matches!(&views[1].state, WorkState::Failed { why, .. } if why.contains("just joined"))
    );
    // A finished task's row can be put away.
    assert!(w.dismiss(views[0].id));
}

#[test]
fn a_store_the_delegate_refuses_is_said_and_can_be_tried_again() {
    let mut w = Work::default();
    let (_, slot) = creating(&mut w);
    w.on_delegate(&read_answer(&slot, None, 0));
    expect_store(&mut w, &slot, 0);
    w.on_delegate(&store_answer(
        &slot,
        CasStoreResult::Failed("disk full".into()),
    ));
    let WorkState::Failed { why, retry } = state(&w) else {
        panic!("failed");
    };
    assert!(retry && why.contains("disk full"), "{why}");
}

/// Up to the meta step of a create.
fn creating_to_meta(w: &mut Work) -> VerifyingKey {
    let (owner, slot) = creating(w);
    w.on_delegate(&read_answer(&slot, None, 0));
    expect_store(w, &slot, 0);
    w.on_delegate(&store_answer(
        &slot,
        CasStoreResult::Stored { generation: 1 },
    ));
    let key = match next(w) {
        ClientRequest::ContractOp(ContractRequest::Put { contract, .. }) => contract.key(),
        _ => panic!("a PUT"),
    };
    w.on_contract(&ContractResponse::PutResponse { key });
    expect_read(w, ROOMS_META_KEY);
    owner
}

/// A room list this app can't read is never overwritten, and trying again would read the same:
/// it says so, with nothing to try again.
#[test]
fn an_unreadable_room_list_is_left_alone_and_said() {
    let mut w = Work::default();
    creating_to_meta(&mut w);
    let mut list = Vec::new();
    ciborium::ser::into_writer(&vec![1u8, 2], &mut list).unwrap();
    w.on_delegate(&read_answer(ROOMS_META_KEY, Some(list), 2));
    let WorkState::Failed { why, retry } = state(&w) else {
        panic!("failed");
    };
    assert!(!retry && why.contains("room list"), "{why}");
    assert!(w.next_request(false, Instant::now()).is_none());
}

#[test]
fn a_room_list_that_keeps_changing_fails_after_the_web_uis_number_of_attempts() {
    let mut w = Work::default();
    creating_to_meta(&mut w);
    for attempt in 0..CAS_ATTEMPTS {
        if attempt > 0 {
            expect_read(&mut w, ROOMS_META_KEY);
        }
        w.on_delegate(&read_answer(ROOMS_META_KEY, None, u64::from(attempt)));
        expect_store(&mut w, ROOMS_META_KEY, u64::from(attempt));
        w.on_delegate(&store_answer(
            ROOMS_META_KEY,
            CasStoreResult::Conflict {
                current_generation: u64::from(attempt) + 1,
                current_value: None,
            },
        ));
    }
    assert!(matches!(state(&w), WorkState::Failed { retry: true, .. }));
}

/// Another invitation into the room `joining_to_slot` joins (its owner is fixed).
fn w_link(_: &Work) -> String {
    let plan =
        crate::rooms::create::new_room(SigningKey::from_bytes(&[60; 32]), "Open", "Olive", false)
            .unwrap();
    invitation(&LiveRoom::new(stored(&plan.room)), "http://x/")
        .unwrap()
        .1
}

// Notification modes -----------------------------------------------------------------------------

fn room_vk(seed: u8) -> VerifyingKey {
    SigningKey::from_bytes(&[seed; 32]).verifying_key()
}

/// A `rooms_meta` as the web UI wrote it: an order, the open room and another room's mode.
fn web_meta_bytes() -> Vec<u8> {
    use ciborium::Value;
    let entries = vec![
        (
            Value::Text("current_room_key".into()),
            Value::serialized(&room_vk(1)).unwrap(),
        ),
        (
            Value::Text("notification_modes".into()),
            Value::Map(vec![(
                Value::serialized(&room_vk(2)).unwrap(),
                Value::serialized(&NotificationMode::Muted).unwrap(),
            )]),
        ),
        (
            Value::Text("room_order".into()),
            Value::serialized(&vec![room_vk(2), room_vk(1)]).unwrap(),
        ),
    ];
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(entries), &mut out).unwrap();
    out
}

#[test]
fn a_mode_is_stored_by_reading_rooms_meta_then_a_cas_store_that_keeps_the_rest() {
    let mut w = Work::default();
    w.set_mode(room_vk(1), NotificationMode::MentionsAndReplies);
    // Not about a room being created or joined: it holds no room, and shows no row yet it works.
    assert!(w.busy());
    assert!(!w.holds(&room_vk(1)));
    expect_read(&mut w, ROOMS_META_KEY);
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, Some(web_meta_bytes()), 4)));
    let value = expect_store(&mut w, ROOMS_META_KEY, 4);
    let after = crate::rooms::modes::decode_modes(&value);
    assert_eq!(after[&room_vk(1)], NotificationMode::MentionsAndReplies);
    assert_eq!(after[&room_vk(2)], NotificationMode::Muted);
    let meta = decode_rooms_meta(&value).unwrap();
    assert_eq!(meta.room_order, vec![room_vk(2), room_vk(1)]);
    assert_eq!(meta.current_room_key, Some(room_vk(1)));
    assert!(w.on_delegate(&store_answer(
        ROOMS_META_KEY,
        CasStoreResult::Stored { generation: 5 }
    )));
    // Done: no row, no re-read of the rooms, and nothing left to override a later read.
    assert!(!w.busy());
    assert!(w.views().is_empty());
    assert!(!w.take_reload());
    assert!(w.pending_modes().is_empty());
    // Reported once as stored.
    assert_eq!(
        w.settle_modes(),
        vec![(room_vk(1), NotificationMode::MentionsAndReplies, true)]
    );
    assert!(w.settle_modes().is_empty());
    w.forget_done(&|_| false);
    assert!(w.next_request(false, Instant::now()).is_none());
}

#[test]
fn a_mode_conflict_reads_again_and_stores_over_what_the_web_ui_wrote() {
    let mut w = Work::default();
    w.set_mode(room_vk(1), NotificationMode::Muted);
    expect_read(&mut w, ROOMS_META_KEY);
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, None, 0)));
    let first = expect_store(&mut w, ROOMS_META_KEY, 0);
    assert!(crate::rooms::modes::decode_modes(&first).contains_key(&room_vk(1)));
    // The web UI stored first, with another room's mode and an order.
    assert!(w.on_delegate(&store_answer(
        ROOMS_META_KEY,
        CasStoreResult::Conflict {
            current_generation: 3,
            current_value: None,
        }
    )));
    expect_read(&mut w, ROOMS_META_KEY);
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, Some(web_meta_bytes()), 3)));
    let second = expect_store(&mut w, ROOMS_META_KEY, 3);
    let modes = crate::rooms::modes::decode_modes(&second);
    assert_eq!(modes[&room_vk(1)], NotificationMode::Muted);
    assert_eq!(modes[&room_vk(2)], NotificationMode::Muted);
    assert_eq!(
        decode_rooms_meta(&second).unwrap().room_order,
        vec![room_vk(2), room_vk(1)]
    );
}

#[test]
fn a_mode_the_store_already_holds_writes_nothing() {
    let mut w = Work::default();
    w.set_mode(room_vk(2), NotificationMode::Muted);
    expect_read(&mut w, ROOMS_META_KEY);
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, Some(web_meta_bytes()), 4)));
    assert!(!w.busy());
}

#[test]
fn picking_quickly_writes_the_last_pick_once() {
    let mut w = Work::default();
    w.set_mode(room_vk(1), NotificationMode::MentionsAndReplies);
    w.set_mode(room_vk(1), NotificationMode::Muted);
    // Another room is its own save.
    w.set_mode(room_vk(2), NotificationMode::All);
    assert_eq!(
        w.pending_modes(),
        vec![
            (room_vk(1), NotificationMode::Muted),
            (room_vk(2), NotificationMode::All)
        ]
    );
    expect_read(&mut w, ROOMS_META_KEY);
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, None, 0)));
    let value = expect_store(&mut w, ROOMS_META_KEY, 0);
    assert_eq!(
        crate::rooms::modes::decode_modes(&value)[&room_vk(1)],
        NotificationMode::Muted
    );
}

#[test]
fn a_mode_that_cannot_be_stored_is_said_and_can_be_tried_again() {
    let mut w = Work::default();
    w.set_mode(room_vk(1), NotificationMode::Muted);
    expect_read(&mut w, ROOMS_META_KEY);
    // A room list this app does not understand is left alone.
    let mut text = Vec::new();
    ciborium::ser::into_writer(&"not a map", &mut text).unwrap();
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, Some(text), 1)));
    let WorkState::Failed { why, retry } = state(&w) else {
        panic!("failed");
    };
    assert!(!retry);
    assert!(why.contains("was not saved"), "{why}");
    // Not saved: not pending, and reported once as ended without being stored, so the
    // screen goes back to what the store holds. Dismissing changes nothing more.
    assert!(w.pending_modes().is_empty());
    assert_eq!(
        w.settle_modes(),
        vec![(room_vk(1), NotificationMode::Muted, false)]
    );
    assert!(w.settle_modes().is_empty());
    let id = w.views()[0].id;
    assert!(w.dismiss(id));
    assert!(w.settle_modes().is_empty());
}

// Leaving a room ---------------------------------------------------------------------------------

/// A room as the web UI would have it stored: its owner and slot bytes.
fn present_room(seed: u8) -> (VerifyingKey, Vec<u8>) {
    let plan = crate::rooms::create::new_room(
        SigningKey::from_bytes(&[seed; 32]),
        "Book club",
        "Ada",
        false,
    )
    .unwrap();
    (
        plan.room.owner_vk,
        river_core::stored_rooms::encode_new_room(&plan.room).unwrap(),
    )
}

fn tombstone_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(&"Tombstone", &mut out).unwrap();
    out
}

/// Leaving, up to the point where the tombstone has been stored and `rooms_meta` is read.
fn leaving_to_meta(w: &mut Work, owner: VerifyingKey, slot: &[u8]) {
    let key = river_core::stored_rooms::room_storage_key(&owner);
    assert!(w.leave(owner, "Book club".into(), None));
    expect_read(w, &key);
    assert!(w.on_delegate(&read_answer(&key, Some(slot.to_vec()), 6)));
    let value = expect_store(w, &key, 6);
    assert_eq!(value, tombstone_bytes());
    assert!(w.on_delegate(&store_answer(
        &key,
        CasStoreResult::Stored { generation: 7 }
    )));
    expect_read(w, ROOMS_META_KEY);
}

#[test]
fn leaving_tombstones_the_room_then_takes_it_out_of_rooms_meta_keeping_the_rest() {
    let (owner, slot) = present_room(1);
    let mut w = Work::default();
    assert!(w.leave(owner, "Book club".into(), None));
    // Working: the room is already gone from the screen, and nothing else goes to it.
    assert!(w.leaving(&owner));
    assert!(w.holds(&owner));
    assert_eq!(state(&w), WorkState::Working);
    assert_eq!(w.views()[0].what, "Leaving “Book club”");
    // A second leave of the same room queues nothing.
    assert!(!w.leave(owner, "Book club".into(), None));

    let key = river_core::stored_rooms::room_storage_key(&owner);
    expect_read(&mut w, &key);
    assert!(w.on_delegate(&read_answer(&key, Some(slot), 6)));
    assert_eq!(expect_store(&mut w, &key, 6), tombstone_bytes());
    assert!(w.on_delegate(&store_answer(
        &key,
        CasStoreResult::Stored { generation: 7 }
    )));

    // rooms_meta as the web UI wrote it (room 1 open, rooms 2 and 1 ordered, room 2 muted) plus a
    // field from a newer build. The room left here is room 1.
    assert_eq!(owner, room_vk(1));
    use ciborium::Value;
    let mut entries = match ciborium::de::from_reader::<Value, _>(&web_meta_bytes()[..]).unwrap() {
        Value::Map(e) => e,
        _ => unreachable!(),
    };
    entries.push((Value::Text("from_a_newer_build".into()), Value::from(7u32)));
    let mut meta = Vec::new();
    ciborium::ser::into_writer(&Value::Map(entries), &mut meta).unwrap();
    expect_read(&mut w, ROOMS_META_KEY);
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, Some(meta), 9)));
    let stored = expect_store(&mut w, ROOMS_META_KEY, 9);
    let read = decode_rooms_meta(&stored).unwrap();
    assert_eq!(read.current_room_key, None);
    assert_eq!(read.room_order, vec![room_vk(2)]);
    assert_eq!(
        crate::rooms::modes::decode_modes(&stored)[&room_vk(2)],
        NotificationMode::Muted
    );
    assert!(w.on_delegate(&store_answer(
        ROOMS_META_KEY,
        CasStoreResult::Stored { generation: 10 }
    )));
    // Done, but the stored rooms have not been read since: still hidden, no row shown.
    assert!(w.views().is_empty());
    assert!(w.leaving(&owner));
    assert!(w.take_reload(), "the stored rooms are read again");
    w.rooms_read();
    assert!(!w.leaving(&owner));
    assert!(w.views().is_empty());
}

#[test]
fn leaving_the_open_room_clears_it_from_the_stored_meta_and_keeps_other_fields() {
    let (owner, slot) = present_room(1);
    let mut w = Work::default();
    leaving_to_meta(&mut w, owner, &slot);
    use ciborium::Value;
    let entries = vec![
        (
            Value::Text("current_room_key".into()),
            Value::serialized(&owner).unwrap(),
        ),
        (
            Value::Text("room_order".into()),
            Value::serialized(&vec![room_vk(2), owner]).unwrap(),
        ),
        (
            Value::Text("notification_modes".into()),
            Value::Map(vec![(
                Value::serialized(&owner).unwrap(),
                Value::serialized(&NotificationMode::Muted).unwrap(),
            )]),
        ),
        (Value::Text("future".into()), Value::from(3u32)),
    ];
    let mut meta = Vec::new();
    ciborium::ser::into_writer(&Value::Map(entries), &mut meta).unwrap();
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, Some(meta), 2)));
    let stored = expect_store(&mut w, ROOMS_META_KEY, 2);
    let read = decode_rooms_meta(&stored).unwrap();
    assert_eq!(read.current_room_key, None);
    assert_eq!(read.room_order, vec![room_vk(2)]);
    // The mode stays, as the web UI keeps it; an unknown field stays.
    assert_eq!(
        crate::rooms::modes::decode_modes(&stored)[&owner],
        NotificationMode::Muted
    );
    let Value::Map(after) = ciborium::de::from_reader::<Value, _>(&stored[..]).unwrap() else {
        panic!("a map")
    };
    assert!(
        after
            .iter()
            .any(|(k, v)| k.as_text() == Some("future") && *v == Value::from(3u32))
    );
}

#[test]
fn a_tombstone_already_stored_skips_to_the_meta_and_a_meta_without_the_room_writes_nothing() {
    // Room 5 is in neither the order nor the open room of `web_meta_bytes`.
    let (owner, _) = present_room(5);
    let key = river_core::stored_rooms::room_storage_key(&owner);
    let mut w = Work::default();
    assert!(w.leave(owner, "Book club".into(), None));
    expect_read(&mut w, &key);
    assert!(w.on_delegate(&read_answer(&key, Some(tombstone_bytes()), 3)));
    // No store of the slot: straight to the meta.
    expect_read(&mut w, ROOMS_META_KEY);
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, Some(web_meta_bytes()), 4)));
    assert!(!w.busy());
    assert!(w.leaving(&owner));
    assert!(w.take_reload());
}

#[test]
fn leaving_when_the_slot_changes_reads_again_and_tombstones_what_is_there_now() {
    let (owner, slot) = present_room(1);
    let key = river_core::stored_rooms::room_storage_key(&owner);
    let mut w = Work::default();
    assert!(w.leave(owner, "Book club".into(), None));
    expect_read(&mut w, &key);
    assert!(w.on_delegate(&read_answer(&key, Some(slot.clone()), 6)));
    expect_store(&mut w, &key, 6);
    // The web UI wrote the room first (a read-position update, say).
    assert!(w.on_delegate(&store_answer(
        &key,
        CasStoreResult::Conflict {
            current_generation: 7,
            current_value: Some(slot.clone()),
        }
    )));
    expect_read(&mut w, &key);
    assert!(w.on_delegate(&read_answer(&key, Some(slot), 7)));
    assert_eq!(expect_store(&mut w, &key, 7), tombstone_bytes());
    // Another writer tombstoned it meanwhile: nothing more to store there.
    assert!(w.on_delegate(&store_answer(
        &key,
        CasStoreResult::Conflict {
            current_generation: 8,
            current_value: Some(tombstone_bytes()),
        }
    )));
    expect_read(&mut w, &key);
    assert!(w.on_delegate(&read_answer(&key, Some(tombstone_bytes()), 8)));
    expect_read(&mut w, ROOMS_META_KEY);
}

#[test]
fn a_slot_that_keeps_changing_fails_and_the_room_is_shown_again_until_it_is_tried_again() {
    let (owner, slot) = present_room(1);
    let key = river_core::stored_rooms::room_storage_key(&owner);
    let mut w = Work::default();
    assert!(w.leave(owner, "Book club".into(), None));
    for generation in 0..u64::from(CAS_ATTEMPTS) {
        expect_read(&mut w, &key);
        assert!(w.on_delegate(&read_answer(&key, Some(slot.clone()), generation)));
        expect_store(&mut w, &key, generation);
        assert!(w.on_delegate(&store_answer(
            &key,
            CasStoreResult::Conflict {
                current_generation: generation + 1,
                current_value: Some(slot.clone()),
            }
        )));
    }
    let WorkState::Failed { retry, .. } = state(&w) else {
        panic!("failed");
    };
    assert!(retry);
    // Nothing was tombstoned: the room is still the user's, so the stored rooms are read to
    // show it again.
    assert!(!w.leaving(&owner));
    assert!(w.take_reload());
    assert!(w.next_request(false, Instant::now()).is_none());
    // Try again starts over, and the room is on its way out again.
    let id = w.views()[0].id;
    assert!(w.retry(id));
    assert!(w.leaving(&owner));
    expect_read(&mut w, &key);
}

#[test]
fn a_refused_store_fails_with_try_again() {
    let (owner, slot) = present_room(1);
    let key = river_core::stored_rooms::room_storage_key(&owner);
    let mut w = Work::default();
    assert!(w.leave(owner, "Book club".into(), None));
    expect_read(&mut w, &key);
    assert!(w.on_delegate(&read_answer(&key, Some(slot), 1)));
    expect_store(&mut w, &key, 1);
    assert!(w.on_delegate(&store_answer(&key, CasStoreResult::Failed("disk".into()))));
    let WorkState::Failed { why, retry } = state(&w) else {
        panic!("failed");
    };
    assert!(retry);
    assert!(why.contains("disk"), "{why}");
    assert!(!w.leaving(&owner));
}

#[test]
fn an_unreadable_meta_after_the_tombstone_is_said_and_the_room_stays_gone() {
    let (owner, slot) = present_room(1);
    let mut w = Work::default();
    leaving_to_meta(&mut w, owner, &slot);
    let mut text = Vec::new();
    ciborium::ser::into_writer(&"not a map", &mut text).unwrap();
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, Some(text), 1)));
    let WorkState::Failed { why, retry } = state(&w) else {
        panic!("failed");
    };
    assert!(!retry);
    assert!(why.contains("You left the room"), "{why}");
    // The room's key holds the tombstone: a read lists nothing for it, and the failed row
    // does not hide a room rejoined since.
    assert!(!w.leaving(&owner));
}

#[test]
fn a_dropped_connection_starts_the_step_again_from_its_read() {
    let (owner, slot) = present_room(1);
    let key = river_core::stored_rooms::room_storage_key(&owner);
    let mut w = Work::default();
    assert!(w.leave(owner, "Book club".into(), None));
    expect_read(&mut w, &key);
    assert!(w.on_delegate(&read_answer(&key, Some(slot), 1)));
    expect_store(&mut w, &key, 1);
    w.connection_lost();
    expect_read(&mut w, &key);
}

#[test]
fn a_finished_leave_does_not_stop_the_room_being_joined_again() {
    let (owner, slot) = present_room(1);
    let mut w = Work::default();
    leaving_to_meta(&mut w, owner, &slot);
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, None, 0)));
    assert!(!w.busy());
    // The room's invitation, accepted again: not refused for the leave that just finished.
    let (_, link) = {
        let plan = crate::rooms::create::new_room(
            SigningKey::from_bytes(&[1; 32]),
            "Book club",
            "Ada",
            false,
        )
        .unwrap();
        invitation(&LiveRoom::new(stored(&plan.room)), "http://x/").unwrap()
    };
    w.join(&link, "Bea", &|_| false);
    let last = w.views().last().cloned().expect("a task");
    assert!(
        !matches!(&last.state, WorkState::Failed { why, .. } if why.contains("just joined")),
        "{last:?}"
    );
}

#[test]
fn a_conflict_at_the_meta_step_reads_it_again_and_edits_what_the_web_ui_stored() {
    let (owner, slot) = present_room(1);
    let mut w = Work::default();
    leaving_to_meta(&mut w, owner, &slot);
    // The first read has the room in its order; the store is refused, and the web UI has
    // since put another room in the order.
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, Some(web_meta_bytes()), 2)));
    expect_store(&mut w, ROOMS_META_KEY, 2);
    assert!(w.on_delegate(&store_answer(
        ROOMS_META_KEY,
        CasStoreResult::Conflict {
            current_generation: 3,
            current_value: None,
        }
    )));
    expect_read(&mut w, ROOMS_META_KEY);
    let newer = {
        use ciborium::Value;
        let entries = vec![(
            Value::Text("room_order".into()),
            Value::serialized(&vec![room_vk(7), owner]).unwrap(),
        )];
        let mut out = Vec::new();
        ciborium::ser::into_writer(&Value::Map(entries), &mut out).unwrap();
        out
    };
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, Some(newer), 3)));
    let stored = expect_store(&mut w, ROOMS_META_KEY, 3);
    assert_eq!(
        decode_rooms_meta(&stored).unwrap().room_order,
        vec![room_vk(7)]
    );
}

/// Fail a leave of `owner` at its slot step, the way a refused store does.
fn failed_leave(w: &mut Work, owner: VerifyingKey, slot: &[u8], identity: Option<VerifyingKey>) {
    let key = river_core::stored_rooms::room_storage_key(&owner);
    assert!(w.leave(owner, "Book club".into(), identity));
    expect_read(w, &key);
    assert!(w.on_delegate(&read_answer(&key, Some(slot.to_vec()), 1)));
    expect_store(w, &key, 1);
    assert!(w.on_delegate(&store_answer(&key, CasStoreResult::Failed("disk".into()))));
}

#[test]
fn a_failed_leave_says_unsent_messages_were_discarded() {
    let (owner, slot) = present_room(1);
    let mut w = Work::default();
    failed_leave(&mut w, owner, &slot, None);
    let WorkState::Failed { why, .. } = state(&w) else {
        panic!("failed");
    };
    assert!(
        why.contains("Unsent messages for this room were discarded"),
        "{why}"
    );
}

#[test]
fn leaving_again_replaces_a_failed_leave_instead_of_adding_a_row() {
    let (owner, slot) = present_room(1);
    let mut w = Work::default();
    failed_leave(&mut w, owner, &slot, None);
    assert!(w.leave(owner, "Book club".into(), None));
    assert_eq!(w.views().len(), 1);
    assert_eq!(state(&w), WorkState::Working);
}

#[test]
fn joining_a_room_dismisses_its_failed_leave() {
    let (owner, slot) = present_room(1);
    let mut w = Work::default();
    failed_leave(&mut w, owner, &slot, None);
    let (_, link) = {
        let plan = crate::rooms::create::new_room(
            SigningKey::from_bytes(&[1; 32]),
            "Book club",
            "Ada",
            false,
        )
        .unwrap();
        invitation(&LiveRoom::new(stored(&plan.room)), "http://x/").unwrap()
    };
    w.join(&link, "Bea", &|_| false);
    // No failed leave left to try again over the room joined now.
    assert!(w.views().iter().all(|v| !v.what.starts_with("Leaving")));
}

#[test]
fn trying_a_leave_again_over_a_room_joined_since_changes_nothing() {
    let (owner, slot) = present_room(1);
    let key = river_core::stored_rooms::room_storage_key(&owner);
    let mut w = Work::default();
    // Left as the identity of seed 1 (the stored room's own key).
    failed_leave(&mut w, owner, &slot, Some(owner));
    let id = w.views()[0].id;
    assert!(w.retry(id));
    expect_read(&mut w, &key);
    // The store now holds the room under another identity: joined again since.
    let plan =
        crate::rooms::create::new_room(SigningKey::from_bytes(&[1; 32]), "Book club", "Ada", false)
            .unwrap();
    let mut room = plan.room;
    room.self_sk = Some(SigningKey::from_bytes(&[99; 32]));
    room.self_vk = None;
    let rejoined = river_core::stored_rooms::encode_new_room(&room).unwrap();
    assert!(w.on_delegate(&read_answer(&key, Some(rejoined), 2)));
    let WorkState::Failed { why, retry } = state(&w) else {
        panic!("refused");
    };
    assert!(!retry);
    assert!(why.contains("joined this room again"), "{why}");
    // Nothing was stored.
    assert!(w.next_request(false, Instant::now()).is_none());
}

#[test]
fn halting_a_leave_shows_its_room_again_at_once() {
    let (owner, _) = present_room(1);
    let mut w = Work::default();
    assert!(w.leave(owner, "Book club".into(), None));
    assert!(w.halt("River has moved on."));
    assert!(!w.leaving(&owner));
    assert!(w.take_reload());
}

// Importing an identity ----------------------------------------------------------------------

mod importing {
    use super::*;
    use crate::rooms::identity::IdentityToken;
    use crate::rooms::identity::tests::{World, member_export, world};

    fn token(w: &World) -> IdentityToken {
        IdentityToken(member_export(w).to_armored_string())
    }

    fn slot_of(w: &World) -> Vec<u8> {
        room_slot(w, &member_export(w))
    }

    /// The slot a room holds for `export`'s identity, as a join stores it.
    fn room_slot(w: &World, export: &river_core::room_state::identity::IdentityExport) -> Vec<u8> {
        river_core::stored_rooms::encode_new_room(
            &crate::rooms::identity::new_room(export, &w.state, w.key).unwrap(),
        )
        .unwrap()
    }

    /// The owner's own identity in the same room: a different one than the token's.
    fn owners_slot(w: &World) -> Vec<u8> {
        let plan = crate::rooms::create::new_room(w.owner.clone(), "Open", "Olive", false).unwrap();
        river_core::stored_rooms::encode_new_room(&plan.room).unwrap()
    }

    fn slot_key(w: &World) -> Vec<u8> {
        room_storage_key(&w.owner.verifying_key())
    }

    fn view_state(w: &Work) -> WorkState {
        w.views().last().expect("a task").state.clone()
    }

    fn failed(w: &Work) -> (String, bool) {
        match view_state(w) {
            WorkState::Failed { why, retry } => (why, retry),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    fn stored_identity(bytes: &[u8]) -> river_core::stored_rooms::StoredRoom {
        match decode_room_slot(bytes).unwrap() {
            StoredRoomSlot::Present(r) => *r,
            StoredRoomSlot::Tombstone => panic!("a present room"),
        }
    }

    /// Run an add-room import up to its slot read: the room was read from the network first.
    fn adding_to_slot(w: &mut Work, world: &World) {
        w.import_identity(&token(world), false, &|_| false);
        let ClientRequest::ContractOp(ContractRequest::Get { key: id, .. }) = next(w) else {
            panic!("the room is read from the network first");
        };
        assert_eq!(id, *world.key.id());
        get_answer(w, world.key, &world.state);
        expect_read(w, &slot_key(world));
    }

    fn finish_meta(w: &mut Work, world: &World) {
        expect_read(w, ROOMS_META_KEY);
        assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, None, 0)));
        let meta = expect_store(w, ROOMS_META_KEY, 0);
        let m = decode_rooms_meta_for_test(&meta);
        assert_eq!(m, Some(world.owner.verifying_key()), "the room opens first");
        assert!(w.on_delegate(&store_answer(
            ROOMS_META_KEY,
            CasStoreResult::Stored { generation: 1 }
        )));
    }

    fn decode_rooms_meta_for_test(bytes: &[u8]) -> Option<VerifyingKey> {
        decode_rooms_meta(bytes).unwrap().current_room_key
    }

    #[test]
    fn adding_reads_the_room_stores_it_with_the_identity_then_opens_it() {
        let world = world();
        let mut w = Work::default();
        adding_to_slot(&mut w, &world);
        assert!(w.on_delegate(&read_answer(&slot_key(&world), None, 0)));
        let bytes = expect_store(&mut w, &slot_key(&world), 0);
        let room = stored_identity(&bytes);
        assert_eq!(
            room.self_verifying_key(),
            Some(world.member.verifying_key())
        );
        assert_eq!(room.self_nickname.as_deref(), Some("Bea"));
        assert_eq!(room.self_authorized_member, Some(world.invitee.clone()));
        assert_eq!(room.contract_key, world.key);
        // The room's real state from the network, not an empty placeholder.
        assert_eq!(
            room.room_state.configuration.configuration.display,
            world.state.configuration.configuration.display
        );
        assert!(w.on_delegate(&store_answer(
            &slot_key(&world),
            CasStoreResult::Stored { generation: 1 }
        )));
        finish_meta(&mut w, &world);
        assert!(matches!(view_state(&w), WorkState::Done { .. }));
        assert!(w.take_reload(), "the stored rooms are read again");
        // Nothing but the room's GET and the delegate's slot and meta was sent.
        assert!(w.next_request(false, Instant::now()).is_none());
    }

    #[test]
    fn replacing_goes_straight_to_the_slot_and_changes_only_the_identity() {
        let world = world();
        let mut w = Work::default();
        w.import_identity(&token(&world), true, &|_| true);
        // No GET: the stored room's state is kept.
        expect_read(&mut w, &slot_key(&world));
        let held = owners_slot(&world);
        assert!(w.on_delegate(&read_answer(&slot_key(&world), Some(held.clone()), 4)));
        let bytes = expect_store(&mut w, &slot_key(&world), 4);
        let room = stored_identity(&bytes);
        assert_eq!(
            room.self_verifying_key(),
            Some(world.member.verifying_key())
        );
        assert_eq!(room.self_nickname.as_deref(), Some("Bea"));
        assert_eq!(
            room.room_state.configuration,
            stored_identity(&held).room_state.configuration
        );
        assert!(w.on_delegate(&store_answer(
            &slot_key(&world),
            CasStoreResult::Stored { generation: 5 }
        )));
        finish_meta(&mut w, &world);
        assert!(matches!(view_state(&w), WorkState::Done { .. }));
    }

    #[test]
    fn a_room_that_is_shown_is_not_replaced_unless_the_user_said_so() {
        let world = world();
        let mut w = Work::default();
        w.import_identity(&token(&world), false, &|_| true);
        let (why, retry) = failed(&w);
        assert_eq!(
            why,
            "You already have this room. Import again and choose to replace your identity in it."
        );
        assert!(!retry);
        assert!(w.next_request(false, Instant::now()).is_none());
    }

    #[test]
    fn a_bad_token_is_refused_before_anything_is_sent() {
        let mut w = Work::default();
        w.import_identity(&IdentityToken("garbage".into()), false, &|_| false);
        let (why, retry) = failed(&w);
        assert!(why.starts_with("Invalid token"), "{why}");
        assert!(!retry);
        assert!(w.next_request(false, Instant::now()).is_none());
    }

    #[test]
    fn a_second_import_of_the_same_room_is_refused_while_the_first_works() {
        let world = world();
        let mut w = Work::default();
        w.import_identity(&token(&world), true, &|_| true);
        w.import_identity(&token(&world), true, &|_| true);
        let views = w.views();
        assert_eq!(views.len(), 2);
        assert!(matches!(
            &views[1].state,
            WorkState::Failed { retry: false, .. }
        ));
    }

    #[test]
    fn replacing_a_slot_that_is_gone_or_left_writes_nothing() {
        let world = world();
        for held in [
            None,
            Some({
                let mut t = Vec::new();
                ciborium::ser::into_writer(&ciborium::Value::Text("Tombstone".into()), &mut t)
                    .unwrap();
                t
            }),
        ] {
            let mut w = Work::default();
            w.import_identity(&token(&world), true, &|_| true);
            expect_read(&mut w, &slot_key(&world));
            assert!(w.on_delegate(&read_answer(&slot_key(&world), held, 2)));
            let (why, retry) = failed(&w);
            assert!(why.contains("no longer in River's storage"), "{why}");
            assert!(!retry);
            assert!(
                w.next_request(false, Instant::now()).is_none(),
                "nothing was stored"
            );
        }
    }

    #[test]
    fn a_slot_this_app_cannot_read_is_never_overwritten() {
        let world = world();
        let mut w = Work::default();
        w.import_identity(&token(&world), true, &|_| true);
        expect_read(&mut w, &slot_key(&world));
        assert!(w.on_delegate(&read_answer(
            &slot_key(&world),
            Some(b"\xff\xfe".to_vec()),
            2
        )));
        let (why, retry) = failed(&w);
        assert!(why.contains("can't read"), "{why}");
        assert!(!retry);
        assert!(w.next_request(false, Instant::now()).is_none());
    }

    #[test]
    fn a_conflict_is_read_again_and_decided_on_what_the_other_writer_stored() {
        let world = world();
        let mut w = Work::default();
        w.import_identity(&token(&world), true, &|_| true);
        expect_read(&mut w, &slot_key(&world));
        assert!(w.on_delegate(&read_answer(
            &slot_key(&world),
            Some(owners_slot(&world)),
            1
        )));
        expect_store(&mut w, &slot_key(&world), 1);
        assert!(w.on_delegate(&store_answer(
            &slot_key(&world),
            CasStoreResult::Conflict {
                current_generation: 2,
                current_value: None,
            }
        )));
        // Read again. The other writer (the web UI) left the room meanwhile.
        expect_read(&mut w, &slot_key(&world));
        let mut tomb = Vec::new();
        ciborium::ser::into_writer(&ciborium::Value::Text("Tombstone".into()), &mut tomb).unwrap();
        assert!(w.on_delegate(&read_answer(&slot_key(&world), Some(tomb), 2)));
        let (why, retry) = failed(&w);
        assert!(why.contains("no longer in River's storage"), "{why}");
        assert!(!retry);
        assert!(
            w.next_request(false, Instant::now()).is_none(),
            "the tombstone stands"
        );
    }

    #[test]
    fn a_conflict_where_the_other_writer_stored_this_identity_is_done_without_a_write() {
        let world = world();
        let mut w = Work::default();
        w.import_identity(&token(&world), true, &|_| true);
        expect_read(&mut w, &slot_key(&world));
        assert!(w.on_delegate(&read_answer(
            &slot_key(&world),
            Some(owners_slot(&world)),
            1
        )));
        let ours = expect_store(&mut w, &slot_key(&world), 1);
        assert!(w.on_delegate(&store_answer(
            &slot_key(&world),
            CasStoreResult::Conflict {
                current_generation: 2,
                current_value: None,
            }
        )));
        expect_read(&mut w, &slot_key(&world));
        // It holds exactly what we meant to store.
        assert!(w.on_delegate(&read_answer(&slot_key(&world), Some(ours), 2)));
        // On to the meta, with no second slot store.
        expect_read(&mut w, ROOMS_META_KEY);
    }

    #[test]
    fn a_storage_that_keeps_changing_fails_with_try_again() {
        let world = world();
        let mut w = Work::default();
        w.import_identity(&token(&world), true, &|_| true);
        for n in 0..CAS_ATTEMPTS {
            expect_read(&mut w, &slot_key(&world));
            assert!(w.on_delegate(&read_answer(
                &slot_key(&world),
                Some(owners_slot(&world)),
                n as u64
            )));
            expect_store(&mut w, &slot_key(&world), n as u64);
            assert!(w.on_delegate(&store_answer(
                &slot_key(&world),
                CasStoreResult::Conflict {
                    current_generation: n as u64 + 1,
                    current_value: None,
                }
            )));
        }
        let (_, retry) = failed(&w);
        assert!(retry);
        // Try again starts from the slot.
        let id = w.views()[0].id;
        assert!(w.retry(id));
        expect_read(&mut w, &slot_key(&world));
    }

    #[test]
    fn a_refusal_by_the_storage_fails_with_try_again_and_stores_nothing_more() {
        let world = world();
        let mut w = Work::default();
        adding_to_slot(&mut w, &world);
        assert!(w.on_delegate(&read_answer(&slot_key(&world), None, 0)));
        expect_store(&mut w, &slot_key(&world), 0);
        assert!(w.on_delegate(&store_answer(
            &slot_key(&world),
            CasStoreResult::Failed("disk full".into())
        )));
        let (why, retry) = failed(&w);
        assert!(why.contains("disk full"), "{why}");
        assert!(retry);
        // Try again reads the room again (the add path builds the slot from the room as it is).
        let id = w.views()[0].id;
        assert!(w.retry(id));
        let ClientRequest::ContractOp(ContractRequest::Get { .. }) = next(&mut w) else {
            panic!("the room is read again");
        };
    }

    #[test]
    fn a_room_freenet_cannot_find_fails_with_try_again_and_stores_nothing() {
        let world = world();
        let mut w = Work::default();
        w.import_identity(&token(&world), false, &|_| false);
        let ClientRequest::ContractOp(ContractRequest::Get { key: id, .. }) = next(&mut w) else {
            panic!("a GET");
        };
        assert!(w.on_contract(&ContractResponse::NotFound { instance_id: id }));
        // The current key has no state: the room's earlier contract keys are read, newest
        // first, and none holds it either (freenet/river#889).
        let owner = world.owner.verifying_key();
        let earlier: Vec<ContractInstanceId> =
            river_core::migration::legacy_contract_keys_for_owner(&owner)
                .iter()
                .map(|k| *k.id())
                .collect();
        let mut read = Vec::new();
        while let Some(ClientRequest::ContractOp(ContractRequest::Get { key, subscribe, .. })) =
            w.next_request(false, Instant::now())
        {
            assert!(!subscribe);
            read.push(key);
            assert!(w.on_contract(&ContractResponse::NotFound { instance_id: key }));
        }
        assert_eq!(read, earlier);
        let (why, retry) = failed(&w);
        assert!(why.contains("couldn't find this room"), "{why}");
        assert!(retry);
        assert!(w.next_request(false, Instant::now()).is_none());
    }

    #[test]
    fn a_different_room_from_the_network_is_refused() {
        let world = world();
        let mut w = Work::default();
        w.import_identity(&token(&world), false, &|_| false);
        let _ = next(&mut w);
        let other =
            crate::rooms::create::new_room(SigningKey::from_bytes(&[55; 32]), "Other", "O", false)
                .unwrap();
        get_answer(&mut w, world.key, &other.room.room_state);
        let (why, retry) = failed(&w);
        assert!(why.contains("isn't the one"), "{why}");
        assert!(!retry);
        assert!(w.next_request(false, Instant::now()).is_none());
    }

    #[test]
    fn adding_never_overwrites_a_room_another_writer_stored_meanwhile() {
        let world = world();
        let mut w = Work::default();
        adding_to_slot(&mut w, &world);
        // The web UI stored this room under the OWNER's identity since the screen last looked.
        assert!(w.on_delegate(&read_answer(
            &slot_key(&world),
            Some(owners_slot(&world)),
            3
        )));
        let (why, retry) = failed(&w);
        assert!(why.contains("choose to replace"), "{why}");
        assert!(!retry);
        assert!(
            w.next_request(false, Instant::now()).is_none(),
            "nothing stored"
        );
    }

    #[test]
    fn a_room_that_has_banned_the_identity_is_not_added() {
        let world = world();
        let mut w = Work::default();
        w.import_identity(&token(&world), false, &|_| false);
        let _ = next(&mut w);
        let mut state = world.state.clone();
        let owner_id = river_core::room_state::member::MemberId::from(&world.owner.verifying_key());
        state
            .bans
            .0
            .push(river_core::room_state::ban::AuthorizedUserBan::new(
                river_core::room_state::ban::UserBan {
                    owner_member_id: owner_id,
                    banned_at: SystemTime::now(),
                    banned_user: river_core::room_state::member::MemberId::from(
                        &world.member.verifying_key(),
                    ),
                },
                owner_id,
                &world.owner,
            ));
        get_answer(&mut w, world.key, &state);
        let (why, retry) = failed(&w);
        assert_eq!(why, crate::rooms::identity::BANNED);
        assert!(
            retry,
            "the copy read may be stale: Try again reads it again"
        );
        assert!(
            w.next_request(false, Instant::now()).is_none(),
            "nothing stored"
        );
    }

    #[test]
    fn a_finished_import_is_reported_once_with_its_identity_and_nickname() {
        let world = world();
        let mut w = Work::default();
        adding_to_slot(&mut w, &world);
        assert!(w.take_done_imports().is_empty(), "not while it works");
        assert!(w.on_delegate(&read_answer(&slot_key(&world), None, 0)));
        expect_store(&mut w, &slot_key(&world), 0);
        assert!(w.on_delegate(&store_answer(
            &slot_key(&world),
            CasStoreResult::Stored { generation: 1 }
        )));
        finish_meta(&mut w, &world);
        let done = w.take_done_imports();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].room, world.owner.verifying_key());
        assert_eq!(done[0].identity, world.member.verifying_key());
        assert_eq!(done[0].nickname.as_deref(), Some("Bea"));
        assert!(w.take_done_imports().is_empty(), "once");
    }

    #[test]
    fn adding_over_a_room_with_this_very_identity_stores_nothing() {
        let world = world();
        let mut w = Work::default();
        adding_to_slot(&mut w, &world);
        assert!(w.on_delegate(&read_answer(&slot_key(&world), Some(slot_of(&world)), 3)));
        expect_read(&mut w, ROOMS_META_KEY);
    }

    #[test]
    fn a_failed_leave_of_the_room_is_dropped_so_it_cannot_run_over_the_import() {
        let world = world();
        let mut w = Work::default();
        assert!(w.leave(world.owner.verifying_key(), "Open".into(), None));
        assert!(w.halt("River has moved on."));
        w.import_identity(&token(&world), false, &|_| false);
        let views = w.views();
        assert_eq!(views.len(), 1, "{views:?}");
        assert_eq!(views[0].what, "Importing your ID");
        assert_eq!(views[0].state, WorkState::Working);
    }

    #[test]
    fn a_failed_leave_is_kept_when_the_import_replaces_and_dropped_only_when_it_adds() {
        let world = world();
        // Replace: the user's leave intent stays visible as a failed row.
        let mut w = Work::default();
        assert!(w.leave(world.owner.verifying_key(), "Open".into(), None));
        assert!(w.halt("River has moved on."));
        w.import_identity(&token(&world), true, &|_| true);
        assert_eq!(
            w.views().len(),
            2,
            "the failed leave stays beside the replace"
        );
        // Add: the failed leave is over.
        let mut w = Work::default();
        assert!(w.leave(world.owner.verifying_key(), "Open".into(), None));
        assert!(w.halt("River has moved on."));
        w.import_identity(&token(&world), false, &|_| false);
        assert_eq!(w.views().len(), 1);
    }

    #[test]
    fn an_import_waits_behind_a_leave_that_is_working() {
        let world = world();
        let mut w = Work::default();
        assert!(w.leave(world.owner.verifying_key(), "Open".into(), None));
        w.import_identity(&token(&world), false, &|_| false);
        let views = w.views();
        assert_eq!(views.len(), 2);
        assert_eq!(views[1].state, WorkState::Working, "queued, not refused");
        // The leave goes first.
        expect_read(&mut w, &slot_key(&world));
    }

    #[test]
    fn the_row_shown_says_what_it_is_and_hides_the_token() {
        let world = world();
        let mut w = Work::default();
        w.import_identity(&token(&world), true, &|_| true);
        assert_eq!(w.views()[0].what, "Replacing your ID in a room");
        let mut w = Work::default();
        w.import_identity(&token(&world), false, &|_| false);
        assert_eq!(w.views()[0].what, "Importing your ID");
        let _ = room_slot;
    }
}

// Saving sent messages ---------------------------------------------------------------------------

mod dm_saves {
    use super::*;
    use crate::rooms::dm_cache::Plaintext;
    use river_core::chat_delegate::{HiddenDmThreadEntry, OutboundDmEntry, OutboundDmStore};
    use river_core::room_state::direct_messages::PurgeToken;
    use river_core::room_state::member::MemberId;

    const KEY: &[u8] = b"outbound_dms";

    fn vk(n: u8) -> VerifyingKey {
        SigningKey::from_bytes(&[n; 32]).verifying_key()
    }

    fn key(room: u8, recipient: u8, token: u8) -> EntryKey {
        (
            vk(room).to_bytes(),
            MemberId::from(&vk(recipient)),
            PurgeToken([token; 16]),
        )
    }

    fn entry(room: u8, recipient: u8, token: u8, text: &str) -> OutboundDmEntry {
        let (room_owner_vk, recipient, purge_token) = key(room, recipient, token);
        OutboundDmEntry {
            room_owner_vk,
            sender: MemberId::from(&vk(99)),
            recipient,
            purge_token,
            timestamp: 100 + u64::from(token),
            plaintext: text.to_owned(),
        }
    }

    /// Start a save, with room 1 shown.
    fn save(w: &mut Work, ops: Vec<DmOp>, listed: bool) -> bool {
        w.set_dm_rooms(vec![vk(1).to_bytes()]);
        w.dm_save(ops, listed)
    }

    fn add(room: u8, recipient: u8, token: u8, text: &str) -> DmOp {
        let e = entry(room, recipient, token, text);
        DmOp::Add {
            key: key(room, recipient, token),
            sender: e.sender,
            timestamp: e.timestamp,
            text: Plaintext::new(text.to_owned()),
        }
    }

    fn blob(entries: Vec<OutboundDmEntry>) -> Vec<u8> {
        let store = OutboundDmStore {
            entries,
            hidden_threads: vec![HiddenDmThreadEntry {
                room_owner_vk: vk(1).to_bytes(),
                peer: MemberId::from(&vk(2)),
                hidden_at_ts: 77,
            }],
        };
        let mut out = Vec::new();
        ciborium::ser::into_writer(&store, &mut out).unwrap();
        out
    }

    fn read(bytes: &[u8]) -> OutboundDmStore {
        ciborium::de::from_reader(bytes).unwrap()
    }

    /// The next request at `now`, which must be a versioned read of the store.
    fn read_at(w: &mut Work, now: Instant) {
        match delegate_msg(&w.next_request(false, now).expect("a request")) {
            Some(ChatDelegateRequestMsg::GetVersionedRequest { key }) => {
                assert_eq!(key.as_bytes(), KEY)
            }
            other => panic!("expected a versioned read, got {other:?}"),
        }
    }

    fn store_at(w: &mut Work, now: Instant, generation: u64) -> Vec<u8> {
        match delegate_msg(&w.next_request(false, now).expect("a request")) {
            Some(ChatDelegateRequestMsg::CasStoreRequest {
                key,
                value,
                expected_generation,
            }) => {
                assert_eq!(key.as_bytes(), KEY);
                assert_eq!(expected_generation, generation);
                value
            }
            other => panic!("expected a CAS store, got {other:?}"),
        }
    }

    #[test]
    fn a_save_reads_the_key_merges_into_what_is_there_and_stores_at_that_generation() {
        let mut w = Work::default();
        let now = Instant::now();
        assert!(save(&mut w, vec![add(1, 4, 9, "native")], true));
        // Nothing of it is shown, working or not.
        assert!(w.views().is_empty());
        assert!(w.has_dm());
        assert!(w.busy());

        read_at(&mut w, now);
        let web = blob(vec![entry(1, 2, 1, "web one"), entry(1, 3, 2, "web two")]);
        assert!(w.on_delegate(&read_answer(KEY, Some(web.clone()), 6)));
        let value = store_at(&mut w, now, 6);
        let store = read(&value);
        assert_eq!(store.entries.len(), 3);
        assert_eq!(store.entries[2], entry(1, 4, 9, "native"));
        assert_eq!(store.hidden_threads.len(), 1, "the archive list is kept");

        assert!(w.on_delegate(&store_answer(KEY, CasStoreResult::Stored { generation: 7 })));
        assert!(!w.busy());
        let ends = w.take_dm_ends();
        assert_eq!(
            ends,
            vec![DmEnd {
                ok: true,
                added: vec![key(1, 4, 9)],
                removed: vec![],
                archived: vec![],
                archive_skipped: vec![],
                archive_dropped: vec![],
                evicted: vec![],
                refusal: None,
            }]
        );
        assert!(!w.has_dm(), "taken once, and the task is gone");
        assert!(w.take_dm_ends().is_empty());
        assert!(w.views().is_empty());
    }

    #[test]
    fn a_new_archive_entry_for_a_room_not_shown_is_left_out_but_one_taken_out_is_made() {
        use crate::rooms::dm_archive::ArchiveOp;
        let mut w = Work::default();
        let now = Instant::now();
        let peer = MemberId::from(&vk(2));
        let hide = |room: u8| {
            DmOp::Archive(ArchiveOp::Hide {
                thread: (vk(room).to_bytes(), peer),
                hidden_at_ts: 500,
            })
        };
        let unhide = DmOp::Archive(ArchiveOp::Unhide {
            thread: (vk(1).to_bytes(), peer),
        });
        // Room 1 is shown, room 5 is gone.
        assert!(save(&mut w, vec![hide(1), hide(5), unhide], true));
        read_at(&mut w, now);
        assert!(w.on_delegate(&read_answer(KEY, Some(blob(vec![])), 3)));
        let store = read(&store_at(&mut w, now, 3));
        // The blob held (room 1, peer, 77): hide(1) at 500 raised it, then unhide removed it;
        // nothing was added for room 5.
        assert!(
            store.hidden_threads.is_empty(),
            "{:?}",
            store.hidden_threads
        );
        assert!(w.on_delegate(&store_answer(KEY, CasStoreResult::Stored { generation: 4 })));
        let ends = w.take_dm_ends();
        assert!(ends[0].ok);
        assert_eq!(ends[0].archived.len(), 2, "the two that were written");
        assert_eq!(
            ends[0].archive_dropped.len(),
            1,
            "the one for room 5 is not written"
        );
    }

    #[test]
    fn a_conflict_waits_then_reads_again_and_merges_into_what_the_other_writer_stored() {
        let mut w = Work::default();
        w.fix_jitter(0.5);
        let t0 = Instant::now();
        assert!(save(&mut w, vec![add(1, 4, 9, "native")], false));
        read_at(&mut w, t0);
        assert!(w.on_delegate(&read_answer(KEY, None, 0)));
        let first = store_at(&mut w, t0, 0);
        assert_eq!(read(&first).entries.len(), 1);

        // A web tab stored first.
        let web = blob(vec![entry(1, 2, 1, "web one")]);
        let conflict = CasStoreResult::Conflict {
            current_generation: 1,
            current_value: Some(web.clone()),
        };
        assert!(w.on_delegate(&store_answer(KEY, conflict)));
        // It waits before asking again, by the clock it is given.
        assert!(w.next_request(false, t0).is_none());
        assert!(
            w.next_request(false, t0 + Duration::from_millis(10))
                .is_none()
        );
        let t1 = t0 + Duration::from_millis(60);
        read_at(&mut w, t1);
        assert!(w.on_delegate(&read_answer(KEY, Some(web), 1)));
        let second = store_at(&mut w, t1, 1);
        let store = read(&second);
        assert_eq!(store.entries.len(), 2, "the web UI's entry is kept");
        assert_eq!(store.entries[0], entry(1, 2, 1, "web one"));
        assert_eq!(store.entries[1], entry(1, 4, 9, "native"));
        assert_eq!(store.hidden_threads.len(), 1);
        assert!(w.on_delegate(&store_answer(KEY, CasStoreResult::Stored { generation: 2 })));
        assert!(w.take_dm_ends()[0].ok);
    }

    #[test]
    fn the_wait_after_a_conflict_doubles_to_a_most_and_the_attempts_are_bounded() {
        let mut w = Work::default();
        w.fix_jitter(0.5);
        let mut now = Instant::now();
        assert!(save(&mut w, vec![add(1, 4, 9, "native")], false));
        let mut waits = Vec::new();
        for n in 0..CAS_ATTEMPTS {
            // The wait for this conflict, found by asking until it lets a request through.
            let asked = now;
            let mut waited = Duration::ZERO;
            while w.next_request(false, now).is_none() {
                now += Duration::from_millis(5);
                waited = now - asked;
                assert!(
                    waited < Duration::from_secs(2),
                    "attempt {n} never went out"
                );
            }
            waits.push(waited);
            // That was the read; answer it and the store, which conflicts.
            assert!(w.on_delegate(&read_answer(KEY, None, u64::from(n))));
            assert!(w.next_request(false, now).is_some());
            let conflict = CasStoreResult::Conflict {
                current_generation: u64::from(n) + 1,
                current_value: None,
            };
            assert!(w.on_delegate(&store_answer(KEY, conflict)));
        }
        assert!(!w.busy(), "failed after the web UI's number of attempts");
        let ends = w.take_dm_ends();
        assert!(!ends[0].ok);
        assert!(w.views().is_empty(), "and nothing is shown");
        // The first attempt had no wait; later ones wait longer, up to a most.
        assert_eq!(waits[0], Duration::ZERO);
        assert!(waits.windows(2).all(|p| p[0] <= p[1]), "{waits:?}");
        assert!(waits[3] > waits[1], "{waits:?}");
        assert!(waits.iter().all(|w| *w <= Duration::from_millis(405)));
        assert!(waits[CAS_ATTEMPTS as usize - 1] >= Duration::from_millis(300));
    }

    #[test]
    fn a_value_that_cannot_be_read_or_has_vanished_is_not_written_over() {
        for (listed, value) in [
            (true, Some(b"not cbor at all".to_vec())),
            (true, Some(Vec::new())),
            (true, None),
        ] {
            let mut w = Work::default();
            let now = Instant::now();
            assert!(save(&mut w, vec![add(1, 4, 9, "native")], listed));
            read_at(&mut w, now);
            assert!(w.on_delegate(&read_answer(KEY, value, 3)));
            // No store request ever follows.
            assert!(w.next_request(false, now).is_none());
            assert!(!w.busy());
            let ends = w.take_dm_ends();
            assert_eq!(ends.len(), 1);
            assert!(!ends[0].ok);
        }
    }

    #[test]
    fn a_store_that_already_holds_the_entry_writes_nothing_and_counts_as_saved() {
        let mut w = Work::default();
        let now = Instant::now();
        assert!(save(&mut w, vec![add(1, 4, 9, "native")], true));
        read_at(&mut w, now);
        let held = blob(vec![entry(1, 4, 9, "native")]);
        assert!(w.on_delegate(&read_answer(KEY, Some(held), 2)));
        assert!(w.next_request(false, now).is_none(), "no store request");
        let ends = w.take_dm_ends();
        assert!(ends[0].ok);
        assert_eq!(ends[0].added, vec![key(1, 4, 9)]);
    }

    #[test]
    fn a_save_is_one_at_a_time_and_halts_with_the_rest_when_river_has_moved_on() {
        let mut w = Work::default();
        assert!(save(&mut w, vec![add(1, 4, 9, "a")], true));
        assert!(
            !save(&mut w, vec![add(1, 4, 10, "b")], true),
            "one at a time"
        );
        assert!(!save(&mut w, vec![], true), "nothing to do is not a task");
        assert!(w.halt("update"));
        assert!(!w.busy());
        assert!(!w.take_dm_ends()[0].ok);
        assert!(w.views().is_empty());
    }

    #[test]
    fn a_save_that_lost_its_connection_reads_again_and_never_stores_blind() {
        let mut w = Work::default();
        let now = Instant::now();
        assert!(save(&mut w, vec![add(1, 4, 9, "native")], false));
        read_at(&mut w, now);
        assert!(w.on_delegate(&read_answer(KEY, None, 0)));
        let _ = store_at(&mut w, now, 0);
        // The answer never came: the connection dropped. The next one starts at the read.
        w.connection_lost();
        read_at(&mut w, now);
        // The store did land, as it turns out: the entry is there, so nothing is written again.
        let landed = blob(vec![entry(1, 4, 9, "native")]);
        assert!(w.on_delegate(&read_answer(KEY, Some(landed), 1)));
        assert!(w.next_request(false, now).is_none());
        assert!(w.take_dm_ends()[0].ok);
    }

    #[test]
    fn a_refused_store_is_a_failed_save_and_shows_nothing() {
        let mut w = Work::default();
        let now = Instant::now();
        assert!(save(&mut w, vec![add(1, 4, 9, "native")], false));
        read_at(&mut w, now);
        assert!(w.on_delegate(&read_answer(KEY, None, 0)));
        let _ = store_at(&mut w, now, 0);
        assert!(w.on_delegate(&store_answer(KEY, CasStoreResult::Failed("disk".into()))));
        assert!(w.views().is_empty());
        let ends = w.take_dm_ends();
        assert!(!ends[0].ok);
        assert_eq!(
            ends[0].added,
            vec![key(1, 4, 9)],
            "so it can be tried again"
        );
    }

    #[test]
    fn the_wait_after_a_conflict_is_spread_by_a_fifth_either_way() {
        let wait_for = |unit: f64| {
            let mut w = Work::default();
            w.fix_jitter(unit);
            let t0 = Instant::now();
            assert!(save(&mut w, vec![add(1, 4, 9, "native")], false));
            read_at(&mut w, t0);
            assert!(w.on_delegate(&read_answer(KEY, None, 0)));
            let _ = store_at(&mut w, t0, 0);
            let conflict = CasStoreResult::Conflict {
                current_generation: 1,
                current_value: None,
            };
            assert!(w.on_delegate(&store_answer(KEY, conflict)));
            assert!(w.next_request(false, t0).is_none());
            // The first wait is 25 ms before jitter: find where it ends.
            (0..400u64)
                .find(|ms| {
                    w.next_request(false, t0 + Duration::from_millis(*ms))
                        .is_some()
                })
                .expect("it ends")
        };
        assert_eq!(wait_for(0.5), 25);
        assert_eq!(wait_for(0.0), 20);
        assert_eq!(wait_for(1.0), 30);
    }

    #[test]
    fn an_add_for_a_room_that_is_no_longer_shown_is_not_written_and_not_counted_saved() {
        let mut w = Work::default();
        let now = Instant::now();
        let ops = vec![add(1, 4, 9, "left"), add(2, 4, 10, "kept")];
        w.set_dm_rooms(vec![vk(1).to_bytes(), vk(2).to_bytes()]);
        assert!(w.dm_save(ops, false));
        read_at(&mut w, now);
        // Room 1 is left while the read is out.
        w.set_dm_rooms(vec![vk(2).to_bytes()]);
        assert!(w.on_delegate(&read_answer(KEY, None, 0)));
        let value = store_at(&mut w, now, 0);
        let store = read(&value);
        assert_eq!(store.entries, vec![entry(2, 4, 10, "kept")]);
        assert!(w.on_delegate(&store_answer(KEY, CasStoreResult::Stored { generation: 1 })));
        let ends = w.take_dm_ends();
        assert_eq!(ends[0].added, vec![key(2, 4, 10)]);

        // With every room gone, nothing is stored at all.
        let mut w = Work::default();
        w.set_dm_rooms(vec![vk(1).to_bytes()]);
        assert!(w.dm_save(vec![add(1, 4, 9, "left")], false));
        read_at(&mut w, now);
        w.set_dm_rooms(Vec::new());
        assert!(w.on_delegate(&read_answer(KEY, None, 0)));
        assert!(w.next_request(false, now).is_none());
        assert!(w.take_dm_ends()[0].added.is_empty());
        // And with no room ever named, an add is refused the same way.
        let mut w = Work::default();
        assert!(w.dm_save(vec![add(1, 4, 9, "unknown")], false));
        read_at(&mut w, now);
        assert!(w.on_delegate(&read_answer(KEY, None, 0)));
        assert!(w.next_request(false, now).is_none());
    }

    #[test]
    fn a_removal_is_made_whether_or_not_its_room_is_shown() {
        let mut w = Work::default();
        let now = Instant::now();
        let held = blob(vec![entry(1, 4, 9, "gone")]);
        assert!(w.dm_save(vec![DmOp::Remove { key: key(1, 4, 9) }], true));
        read_at(&mut w, now);
        assert!(w.on_delegate(&read_answer(KEY, Some(held), 3)));
        assert!(read(&store_at(&mut w, now, 3)).entries.is_empty());
    }

    #[test]
    fn a_refusal_is_named_and_a_save_that_goes_unanswered_costs_one_connection_at_most() {
        let mut w = Work::default();
        let now = Instant::now();
        assert!(save(&mut w, vec![add(1, 4, 9, "x")], true));
        read_at(&mut w, now);
        assert!(w.on_delegate(&read_answer(KEY, None, 0)));
        let ends = w.take_dm_ends();
        assert_eq!(ends[0].refusal, Some(dm_store::Refusal::Vanished));

        // No answer: one new connection, then the save fails (and is tried later by its saver).
        let mut w = Work::default();
        assert!(save(&mut w, vec![add(1, 4, 9, "x")], false));
        read_at(&mut w, now);
        let late = now + Duration::from_secs(60);
        let wait = Duration::from_secs(10);
        assert!(w.expire(wait, wait, late));
        assert!(w.expired_dm());
        assert!(w.busy(), "once");
        read_at(&mut w, late);
        assert!(w.expire(wait, wait, late + Duration::from_secs(60)));
        assert!(!w.busy(), "failed after one more connection");
        assert!(!w.take_dm_ends()[0].ok);

        // Another task's expiry is not a save's.
        let mut w = Work::default();
        w.create("Book club", "Ada", false);
        let _ = next(&mut w);
        assert!(w.expire(wait, wait, now + Duration::from_secs(60)));
        assert!(!w.expired_dm());
    }

    #[test]
    fn a_save_prints_only_the_length_of_what_it_is_about_to_store() {
        let mut w = Work::default();
        let now = Instant::now();
        assert!(save(&mut w, vec![add(1, 4, 9, "PLAINTEXT-SECRET")], false));
        read_at(&mut w, now);
        assert!(w.on_delegate(&read_answer(KEY, None, 0)));
        let phase = &w.tasks[0].phase;
        let shown = format!("{phase:?}");
        assert!(shown.contains("bytes"), "{shown}");
        assert!(!shown.contains("PLAINTEXT") && !shown.contains("112"));
        // Once the write ends the bytes are gone.
        let _ = store_at(&mut w, now, 0);
        assert!(w.on_delegate(&store_answer(KEY, CasStoreResult::Stored { generation: 1 })));
        assert_eq!(w.tasks[0].phase, Phase::Read);
    }
}

// Joining from an invitation already decoded ([`Work::join_invitation`]) -----------------------------

fn an_invitation() -> (Invitation, SigningKey) {
    let owner_sk = SigningKey::from_bytes(&[62; 32]);
    let plan = crate::rooms::create::new_room(owner_sk.clone(), "Open", "Olive", false).unwrap();
    let room = LiveRoom::new(stored(&plan.room));
    let (inv, _) = invitation(&room, "http://x/").unwrap();
    (inv, owner_sk)
}

#[test]
fn join_invitation_says_why_at_once_and_queues_nothing_for_each_refusal() {
    let mut w = Work::default();
    // A nickname that can't be used.
    let (inv, _) = an_invitation();
    assert!(w.join_invitation(inv, "  ", false, &|_| false).is_err());
    // A room already in the list.
    let (inv, _) = an_invitation();
    let why = w.join_invitation(inv, "Bea", false, &|_| true).unwrap_err();
    assert!(why.contains("already a member"), "{why}");
    assert!(
        w.views().is_empty(),
        "nothing is shown as failed: the caller says it"
    );
    // A join of the room under way.
    let (inv, _) = an_invitation();
    w.join_invitation(inv, "Bea", false, &|_| false).unwrap();
    let (inv, _) = an_invitation();
    let why = w
        .join_invitation(inv, "Bea", false, &|_| false)
        .unwrap_err();
    assert!(why.contains("already joining"), "{why}");
    assert_eq!(w.views().len(), 1);
}

#[test]
fn only_a_join_names_its_room_so_a_leave_does_not_look_like_one() {
    let mut w = Work::default();
    let (inv, _) = an_invitation();
    let room = inv.room;
    w.join_invitation(inv, "Bea", false, &|_| false).unwrap();
    assert_eq!(
        w.views()[0].room,
        Some(bs58::encode(room.to_bytes()).into_string())
    );
    let mut w = Work::default();
    assert!(w.leave(room, "Open".into(), None));
    assert_eq!(w.views()[0].room, None);
}

/// A private room's state with an owner-side copy that can make an invitation for it.
fn private_join() -> (Fixture, river_core::room_state::ChatRoomStateV1, Invitation) {
    let f = Fixture::new();
    let mut state = f.private_state();
    let mut owner_side = f.stored(state.clone());
    owner_side.self_sk = Some(f.owner.clone());
    crate::rooms::room_tests::add_secret(
        &f,
        &mut state,
        0,
        &crate::rooms::room_tests::SECRET,
        &f.owner,
    );
    owner_side.room_state = state.clone();
    let (inv, _) = invitation(&LiveRoom::new(owner_side), "http://x/").unwrap();
    (f, state, inv)
}

#[test]
fn a_refused_plan_and_a_dropped_task_are_wiped_too() {
    // A join from a message for a key the room holds: the plan made for it (with the room's
    // secrets and the invitee's key) is refused, and is wiped with the invitation.
    let (f, mut state, inv) = private_join();
    state.members.members.push(inv.invitee.clone());
    let mut w = Work::default();
    wiped::take();
    w.join_invitation(inv, "Bea", true, &|_| false).unwrap();
    next(&mut w);
    get_answer(
        &mut w,
        crate::rooms::create::contract_key(&f.owner.verifying_key()),
        &state,
    );
    assert!(
        w.views()
            .iter()
            .any(|v| matches!(v.state, WorkState::Failed { retry: false, .. }))
    );
    assert!(w.join_credentials_wiped());
    let (invitation_secrets, plan_secrets) = wiped::take();
    assert!(invitation_secrets > 0, "the invitation's secrets");
    assert!(plan_secrets > 0, "the refused plan's secrets");

    // Dropping a task that holds an invitation wipes it. (The dropped value cannot be looked at;
    // the count is what shows the secrets were reached.)
    let (_, _, inv) = private_join();
    let mut w = Work::default();
    w.join_invitation(inv, "Bea", false, &|_| false).unwrap();
    wiped::take();
    drop(w);
    assert!(wiped::take().0 > 0);
}

#[test]
fn a_generated_nickname_is_cut_to_the_rooms_limit_and_the_cut_is_what_is_joined_with() {
    use river_core::room_state::configuration::AuthorizedConfigurationV1;
    let run = |nickname: &str, limit: usize, only_new: bool| {
        let mut w = Work::default();
        let (inv, owner_sk) = an_invitation();
        w.join_invitation(inv, nickname, only_new, &|_| false)
            .unwrap();
        next(&mut w);
        let plan =
            crate::rooms::create::new_room(owner_sk.clone(), "Open", "Olive", false).unwrap();
        let mut state = plan.room.room_state;
        let mut config = state.configuration.configuration.clone();
        config.max_nickname_size = limit;
        state.configuration = AuthorizedConfigurationV1::new(config, &owner_sk);
        get_answer(
            &mut w,
            crate::rooms::create::contract_key(&owner_sk.verifying_key()),
            &state,
        );
        let joined = match &w.tasks[0].kind {
            Kind::Join { plan: Some(p), .. } => p.room.self_nickname.clone(),
            _ => None,
        };
        let failed = w.views().into_iter().find_map(|v| match v.state {
            WorkState::Failed { why, retry } => Some((why, retry)),
            _ => None,
        });
        (joined, failed)
    };
    // Cut on a character boundary, trailing space dropped.
    let (joined, failed) = run("Cipher Daylight \u{e9}\u{e9}\u{e9}", 7, true);
    assert_eq!((joined.as_deref(), failed), (Some("Cipher"), None));
    // A typed nickname is refused when too long, not changed.
    let (joined, failed) = run("Cipher Daylight", 7, false);
    assert!(
        joined.is_none()
            && failed.is_some_and(|(w, _)| w.contains("too long") || w.contains("at most"))
    );
    // A first character wider than the limit leaves nothing, and a limit under the fixed
    // fallback's length is said, for good.
    let (joined, failed) = run("\u{e9}Bea", 1, true);
    assert!(joined.is_none());
    let (why, retry) = failed.expect("refused");
    assert!(
        why.contains("nickname limit is too small") && !retry,
        "{why}"
    );
}

/// After a room-contract re-key the room's current key may have no state yet: a join reads the
/// room's earlier keys, newest first, PUTs the newest that holds the room under the current key,
/// then joins from it (freenet/river#889).
#[test]
fn a_join_after_a_room_contract_rekey_carries_the_room_to_its_current_key_first() {
    let owner_sk = SigningKey::from_bytes(&[60; 32]);
    let plan = crate::rooms::create::new_room(owner_sk.clone(), "Open", "Olive", false).unwrap();
    let room = LiveRoom::new(stored(&plan.room));
    let (_inv, link) = invitation(&room, "http://x/").unwrap();
    let mut w = Work::default();
    w.join(&link, "Bea", &|_| false);
    let owner = owner_sk.verifying_key();
    let current = crate::rooms::create::contract_key(&owner);
    let ClientRequest::ContractOp(ContractRequest::Get { key: id, .. }) = next(&mut w) else {
        panic!("a GET");
    };
    assert_eq!(id, *current.id());
    assert!(w.on_contract(&ContractResponse::NotFound { instance_id: id }));
    // The earlier keys, newest first: the newest holds nothing, the next holds the room.
    let earlier = river_core::migration::legacy_contract_keys_for_owner(&owner);
    let ClientRequest::ContractOp(ContractRequest::Get { key: first, .. }) = next(&mut w) else {
        panic!("a GET of an earlier key");
    };
    assert_eq!(first, *earlier[0].id());
    assert!(w.on_contract(&ContractResponse::NotFound { instance_id: first }));
    let ClientRequest::ContractOp(ContractRequest::Get { key: second, .. }) = next(&mut w) else {
        panic!("a GET of the next earlier key");
    };
    assert_eq!(second, *earlier[1].id());
    get_answer(&mut w, earlier[1], &plan.room.room_state);
    // Nothing older is read once one holds the room: it is PUT under the current key.
    let ClientRequest::ContractOp(ContractRequest::Put {
        contract, state, ..
    }) = next(&mut w)
    else {
        panic!("a PUT");
    };
    assert_eq!(contract.key(), current);
    let put: ChatRoomStateV1 = ciborium::de::from_reader(state.as_ref()).unwrap();
    assert!(put.upgrade.0.is_none());
    // The node took it: the join goes on from that state, to the store.
    assert!(w.on_contract(&ContractResponse::PutResponse { key: current }));
    expect_read(&mut w, &room_storage_key(&owner));
}

fn ids(keys: &[freenet_stdlib::prelude::ContractKey]) -> Vec<ContractInstanceId> {
    keys.iter().map(|k| *k.id()).collect()
}

/// A move of a stored room folds every earlier key down to the one it was stored under (the web
/// UI's `start_rekey_merge`); a PUT the node refuses ends the move unmoved, for the session to
/// try again later, and a new move reads the earlier keys again.
#[test]
fn a_move_folds_every_earlier_key_and_a_refused_put_ends_it_unmoved() {
    let owner_sk = SigningKey::from_bytes(&[61; 32]);
    let plan = crate::rooms::create::new_room(owner_sk.clone(), "Open", "Olive", false).unwrap();
    let owner = owner_sk.verifying_key();
    let earlier = river_core::migration::legacy_contract_keys_for_owner(&owner);
    let current = crate::rooms::create::contract_key(&owner);
    let candidates = ids(&earlier[..3]);
    let mut w = Work::default();
    assert!(w.seed(
        owner,
        "Open".into(),
        candidates.clone(),
        plan.room.room_state.clone()
    ));
    // One move of a room at a time.
    assert!(!w.seed(
        owner,
        "Open".into(),
        candidates.clone(),
        plan.room.room_state.clone()
    ));
    assert!(w.holds(&owner));
    // It is shown while it works.
    assert!(w.views()[0].what.contains("Moving"));
    for (i, k) in earlier[..3].iter().enumerate() {
        let ClientRequest::ContractOp(ContractRequest::Get { key, subscribe, .. }) = next(&mut w)
        else {
            panic!("a GET of an earlier key");
        };
        assert!(!subscribe);
        assert_eq!(key, *k.id());
        if i == 1 {
            get_answer(&mut w, *k, &plan.room.room_state);
        } else {
            assert!(w.on_contract(&ContractResponse::NotFound { instance_id: key }));
        }
    }
    assert!(matches!(
        next(&mut w),
        ClientRequest::ContractOp(ContractRequest::Put { .. })
    ));
    let refusal: ClientError =
        ErrorKind::RequestError(RequestError::ContractError(ContractError::Put {
            key: current,
            cause: "no".into(),
        }))
        .into();
    assert!(w.on_error(&refusal, true, &|_| false));
    // Not shown once it ended: the session tries it again after a wait.
    assert!(w.views().is_empty());
    assert_eq!(
        w.take_seed_ends(),
        vec![SeedEnd {
            room: owner,
            moved: false,
            unresolved: Vec::new()
        }]
    );
    assert!(w.take_seed_ends().is_empty());
    assert!(w.seed(
        owner,
        "Open".into(),
        candidates,
        plan.room.room_state.clone()
    ));
    let ClientRequest::ContractOp(ContractRequest::Get { key, .. }) = next(&mut w) else {
        panic!("a GET");
    };
    assert_eq!(key, *earlier[0].id());
}

/// Every earlier key answered and none holds the room: the user's copy is PUT under the current
/// key, as the web UI seeds it, and the move ends moved.
#[test]
fn a_room_no_earlier_key_holds_is_seeded_from_the_users_copy() {
    let owner_sk = SigningKey::from_bytes(&[62; 32]);
    let plan = crate::rooms::create::new_room(owner_sk.clone(), "Open", "Olive", false).unwrap();
    let owner = owner_sk.verifying_key();
    let earlier = river_core::migration::legacy_contract_keys_for_owner(&owner);
    let current = crate::rooms::create::contract_key(&owner);
    let mut w = Work::default();
    w.seed(
        owner,
        "Open".into(),
        ids(&earlier[..1]),
        plan.room.room_state.clone(),
    );
    let ClientRequest::ContractOp(ContractRequest::Get { key, .. }) = next(&mut w) else {
        panic!("a GET");
    };
    assert!(w.on_contract(&ContractResponse::NotFound { instance_id: key }));
    let ClientRequest::ContractOp(ContractRequest::Put {
        contract, state, ..
    }) = next(&mut w)
    else {
        panic!("a PUT of the user's copy");
    };
    assert_eq!(contract.key(), current);
    let put: ChatRoomStateV1 = ciborium::de::from_reader(state.as_ref()).unwrap();
    assert_eq!(
        put.configuration.configuration.display,
        plan.room.room_state.configuration.configuration.display
    );
    assert!(w.on_contract(&ContractResponse::PutResponse { key: current }));
    assert!(!w.busy());
    assert_eq!(
        w.take_seed_ends(),
        vec![SeedEnd {
            room: owner,
            moved: true,
            unresolved: Vec::new()
        }]
    );
}

/// An earlier key that does not answer is passed over without a new connection, and its late
/// answer is taken by nobody; with nothing found, the move ends unmoved (nothing is PUT while
/// it is not known whether a key holds the room).
#[test]
fn a_silent_earlier_key_costs_no_connection_and_its_late_answer_is_dropped() {
    let owner_sk = SigningKey::from_bytes(&[63; 32]);
    let plan = crate::rooms::create::new_room(owner_sk.clone(), "Open", "Olive", false).unwrap();
    let owner = owner_sk.verifying_key();
    let earlier = river_core::migration::legacy_contract_keys_for_owner(&owner);
    let mut w = Work::default();
    w.seed(
        owner,
        "Open".into(),
        ids(&earlier[..2]),
        plan.room.room_state.clone(),
    );
    let t0 = Instant::now();
    let ClientRequest::ContractOp(ContractRequest::Get { key: silent, .. }) = next(&mut w) else {
        panic!("a GET");
    };
    let wait = Duration::from_secs(60);
    assert!(
        !w.expire(
            Duration::from_secs(20),
            wait,
            t0 + wait + Duration::from_secs(1)
        ),
        "no new connection for an earlier key that does not answer"
    );
    assert!(w.has_late());
    let ClientRequest::ContractOp(ContractRequest::Get { key, .. }) = next(&mut w) else {
        panic!("the next earlier key");
    };
    assert_eq!(key, *earlier[1].id());
    // The late answer is nobody's: not the read now out.
    assert!(w.on_contract(&ContractResponse::NotFound {
        instance_id: silent
    }));
    assert!(!w.has_late());
    assert!(w.on_contract(&ContractResponse::NotFound { instance_id: key }));
    assert!(w.next_request(false, Instant::now()).is_none());
    assert_eq!(
        w.take_seed_ends(),
        vec![SeedEnd {
            room: owner,
            moved: false,
            unresolved: Vec::new()
        }]
    );
}

/// Try again after a join's forwarding PUT failed reads the room again from the start
/// (Codex, review of #997).
#[test]
fn trying_a_join_again_after_its_forward_put_failed_reads_the_room_again() {
    let owner_sk = SigningKey::from_bytes(&[64; 32]);
    let plan = crate::rooms::create::new_room(owner_sk.clone(), "Open", "Olive", false).unwrap();
    let room = LiveRoom::new(stored(&plan.room));
    let (_inv, link) = invitation(&room, "http://x/").unwrap();
    let owner = owner_sk.verifying_key();
    let current = crate::rooms::create::contract_key(&owner);
    let earlier = river_core::migration::legacy_contract_keys_for_owner(&owner);
    let mut w = Work::default();
    w.join(&link, "Bea", &|_| false);
    let ClientRequest::ContractOp(ContractRequest::Get { key: id, .. }) = next(&mut w) else {
        panic!("a GET");
    };
    assert!(w.on_contract(&ContractResponse::NotFound { instance_id: id }));
    let _ = next(&mut w);
    get_answer(&mut w, earlier[0], &plan.room.room_state);
    assert!(matches!(
        next(&mut w),
        ClientRequest::ContractOp(ContractRequest::Put { .. })
    ));
    let refusal: ClientError =
        ErrorKind::RequestError(RequestError::ContractError(ContractError::Put {
            key: current,
            cause: "no".into(),
        }))
        .into();
    assert!(w.on_error(&refusal, true, &|_| false));
    let view = w.views().pop().unwrap();
    assert!(matches!(view.state, WorkState::Failed { retry: true, .. }));
    assert!(w.retry(view.id));
    let ClientRequest::ContractOp(ContractRequest::Get { key, .. }) = next(&mut w) else {
        panic!("Try again reads the room's current key again");
    };
    assert_eq!(key, *current.id());
}

/// A join reads the earlier keys for at most [`PROBE_BUDGET`], then fails with Try again.
#[test]
fn a_joins_reading_of_earlier_keys_is_bounded() {
    let owner_sk = SigningKey::from_bytes(&[65; 32]);
    let plan = crate::rooms::create::new_room(owner_sk.clone(), "Open", "Olive", false).unwrap();
    let room = LiveRoom::new(stored(&plan.room));
    let (_inv, link) = invitation(&room, "http://x/").unwrap();
    let mut w = Work::default();
    w.join(&link, "Bea", &|_| false);
    let ClientRequest::ContractOp(ContractRequest::Get { key: id, .. }) = next(&mut w) else {
        panic!("a GET");
    };
    assert!(w.on_contract(&ContractResponse::NotFound { instance_id: id }));
    let t0 = Instant::now();
    let Some(ClientRequest::ContractOp(ContractRequest::Get { key, .. })) =
        w.next_request(false, t0)
    else {
        panic!("a GET of an earlier key");
    };
    assert!(w.on_contract(&ContractResponse::NotFound { instance_id: key }));
    // Within the budget, the next key is read.
    let Some(ClientRequest::ContractOp(ContractRequest::Get { key, .. })) =
        w.next_request(false, t0 + Duration::from_secs(1))
    else {
        panic!("the next earlier key");
    };
    assert!(w.on_contract(&ContractResponse::NotFound { instance_id: key }));
    // Past it, the join gives up, with Try again.
    assert!(w.next_request(false, t0 + PROBE_BUDGET).is_none());
    let WorkState::Failed { why, retry } = state(&w) else {
        panic!("not failed");
    };
    assert!(why.contains("couldn't find this room"), "{why}");
    assert!(retry);
}

fn carry_task(w: &mut Work) -> crate::rooms::carry::Predecessor {
    let pred = crate::rooms::carry::Predecessor {
        version: "V39".into(),
        key: freenet_stdlib::prelude::DelegateKey::new(
            [39; 32],
            freenet_stdlib::prelude::CodeHash::new([39; 32]),
        ),
        rank: 39,
    };
    w.carry(crate::rooms::carry::Carry::new(
        vec![pred.clone()],
        Vec::new(),
        true,
    ));
    pred
}

/// A create the user asks for goes ahead of the carry at the carry's next request, instead of
/// waiting for the whole walk (review of #997).
#[test]
fn a_users_create_goes_ahead_of_the_carry() {
    let mut w = Work::default();
    let pred = carry_task(&mut w);
    // The carry's first request is out: its answer is taken before anything else goes.
    let ClientRequest::DelegateOp(DelegateRequest::ApplicationMessages { key, .. }) = next(&mut w)
    else {
        panic!("the carry's listing");
    };
    assert_eq!(key, pred.key);
    w.create("Book club", "Ada", false);
    assert!(
        w.next_request(false, Instant::now()).is_none(),
        "one request out at a time"
    );
    assert!(w.on_predecessor(
        &pred.key,
        &[ChatDelegateResponseMsg::ListResponse { keys: Vec::new() }]
    ));
    // Next: the create's read of its slot, not the carry's next read.
    let req = next(&mut w);
    assert!(matches!(
        delegate_msg(&req),
        Some(ChatDelegateRequestMsg::GetVersionedRequest { .. })
    ));
}

/// A typed delegate error that names no delegate is taken for the carry's read only when no
/// other delegate request can be out; `Missing` of another delegate never is.
#[test]
fn a_delegate_error_is_the_carrys_only_when_nothing_else_could_have_caused_it() {
    let mut w = Work::default();
    let pred = carry_task(&mut w);
    let _ = next(&mut w);
    let trap: ClientError = ErrorKind::RequestError(RequestError::DelegateError(
        DelegateError::ExecutionError("trap".into()),
    ))
    .into();
    let other_missing: ClientError = ErrorKind::RequestError(RequestError::DelegateError(
        DelegateError::Missing(crate::rooms::delegate::chat_delegate_key().clone()),
    ))
    .into();
    assert!(!w.on_error(&other_missing, true, &|_| false));
    w.set_delegate_shared(true);
    assert!(
        !w.on_error(&trap, true, &|_| false),
        "the key sweep's, maybe"
    );
    w.set_delegate_shared(false);
    assert!(w.on_error(&trap, true, &|_| false));
    let view = w.views().pop().unwrap();
    let WorkState::Failed { why, retry: true } = view.state else {
        panic!("not failed: {view:?}");
    };
    assert!(why.contains("V39") && why.contains("trap"), "{why}");
    // Its own `Missing` is "not installed", never a failure.
    let mut w = Work::default();
    let pred2 = carry_task(&mut w);
    assert_eq!(pred2.key, pred.key);
    let _ = next(&mut w);
    let missing: ClientError = ErrorKind::RequestError(RequestError::DelegateError(
        DelegateError::Missing(pred.key.clone()),
    ))
    .into();
    assert!(w.on_error(&missing, true, &|_| false));
    assert!(!w.busy());
    assert!(w.views().is_empty());
}

/// The walk over earlier delegates stops at [`WALK_BUDGET`], says so with Try again, and Try
/// again starts a new budget.
#[test]
fn the_walk_stops_at_its_budget_and_try_again_starts_a_new_one() {
    let mut w = Work::default();
    let pred = carry_task(&mut w);
    let t0 = Instant::now();
    assert!(w.next_request(false, t0).is_some());
    assert!(w.on_predecessor(
        &pred.key,
        &[ChatDelegateResponseMsg::ListResponse { keys: Vec::new() }]
    ));
    assert!(w.next_request(false, t0 + WALK_BUDGET).is_none());
    let view = w.views().pop().unwrap();
    let WorkState::Failed { why, retry: true } = view.state else {
        panic!("not failed: {view:?}");
    };
    assert!(why.contains("too long"), "{why}");
    assert!(w.retry(view.id));
    let later = t0 + WALK_BUDGET + Duration::from_secs(1);
    assert!(w.next_request(false, later).is_some(), "a new budget");
}

/// The user's rows come before the background ones; a carry halted because River moved on,
/// and a move that ended, are not shown.
#[test]
fn the_users_rows_come_first_and_ended_background_work_is_hidden() {
    let mut w = Work::default();
    carry_task(&mut w);
    w.create("Book club", "Ada", false);
    let views = w.views();
    assert_eq!(views.len(), 2);
    assert!(views[0].what.contains("Book club"), "{views:?}");
    assert!(views[1].what.contains("earlier version"), "{views:?}");
    assert!(w.halt("River moved on"));
    assert!(
        w.views()
            .iter()
            .all(|v| !v.what.contains("earlier version")),
        "a halted carry is the newer build's to finish"
    );
    // A move is shown while it works, not once it failed (the session tries it again).
    let owner_sk = SigningKey::from_bytes(&[66; 32]);
    let plan = crate::rooms::create::new_room(owner_sk.clone(), "Open", "Olive", false).unwrap();
    let owner = owner_sk.verifying_key();
    let earlier = river_core::migration::legacy_contract_keys_for_owner(&owner);
    let mut w = Work::default();
    w.seed(
        owner,
        "Open".into(),
        ids(&earlier[..1]),
        plan.room.room_state.clone(),
    );
    assert!(w.views()[0].what.contains("Moving"));
    assert!(w.halt("River moved on"));
    assert!(w.views().is_empty());
}

/// The carry writes a room's rank before its slot; `rooms_meta` it can't read stops it before
/// the slot, with Try again, and a conflict on it is read again.
#[test]
fn the_carrys_rank_write_conflicts_and_refusals() {
    let pred = crate::rooms::carry::Predecessor {
        version: "V39".into(),
        key: freenet_stdlib::prelude::DelegateKey::new(
            [39; 32],
            freenet_stdlib::prelude::CodeHash::new([39; 32]),
        ),
        rank: 39,
    };
    let owner_sk = SigningKey::from_bytes(&[67; 32]);
    let plan = crate::rooms::create::new_room(owner_sk.clone(), "Open", "Olive", false).unwrap();
    let slot = river_core::stored_rooms::encode_new_room(&plan.room).unwrap();
    let slot_key = room_storage_key(&owner_sk.verifying_key());
    let mut w = Work::default();
    w.carry(crate::rooms::carry::Carry::new(
        vec![pred.clone()],
        Vec::new(),
        true,
    ));
    // Walk V39: one room.
    #[derive(serde::Serialize)]
    struct Index {
        keys: Vec<ChatDelegateKey>,
    }
    let index = {
        let mut b = Vec::new();
        ciborium::ser::into_writer(
            &Index {
                keys: vec![ChatDelegateKey::new(slot_key.clone())],
            },
            &mut b,
        )
        .unwrap();
        b
    };
    let get = |k: &[u8], v: Option<Vec<u8>>| ChatDelegateResponseMsg::GetResponse {
        key: ChatDelegateKey::new(k.to_vec()),
        value: v,
    };
    let _ = next(&mut w);
    assert!(w.on_predecessor(
        &pred.key,
        &[ChatDelegateResponseMsg::ListResponse {
            keys: vec![ChatDelegateKey::new(slot_key.clone())]
        }]
    ));
    for answer in [
        get(crate::rooms::carry::KEY_INDEX_KEY, Some(index.clone())),
        get(crate::rooms::carry::KEY_INDEX_KEY, Some(index.clone())),
        get(ROOMS_META_KEY, None),
        get(b"rooms_data", None),
        get(b"outbound_dms", None),
        get(&slot_key, Some(slot.clone())),
    ] {
        let _ = next(&mut w);
        assert!(w.on_predecessor(&pred.key, &[answer]));
    }
    // The rank first: rooms_meta is read, and someone else wrote it meanwhile.
    expect_read(&mut w, ROOMS_META_KEY);
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, None, 4)));
    let _ = expect_store(&mut w, ROOMS_META_KEY, 4);
    assert!(w.on_delegate(&store_answer(
        ROOMS_META_KEY,
        CasStoreResult::Conflict {
            current_generation: 5,
            current_value: Some(b"garbage".to_vec())
        }
    )));
    // Read again: now it can't be read; the slot is never written, and Try again is offered.
    expect_read(&mut w, ROOMS_META_KEY);
    assert!(w.on_delegate(&read_answer(ROOMS_META_KEY, Some(b"garbage".to_vec()), 5)));
    let view = w.views().pop().unwrap();
    let WorkState::Failed { retry: true, .. } = view.state else {
        panic!("not failed with Try again: {view:?}");
    };
    assert!(w.next_request(false, Instant::now()).is_none());
}
